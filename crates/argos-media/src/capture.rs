//! Screen capture over DXGI Desktop Duplication.
//!
//! The public surface is deliberately identical to the previous `xcap`-based
//! backend: a [`CaptureSession`] owns a thread that keeps a duplication alive
//! and hands whole frames to [`CaptureSession::latest`], and [`list_monitors`]
//! enumerates the displays a sharer can pick.
//!
//! Desktop Duplication has two rules that shape the recorder:
//!
//! 1. At most one frame may be held at a time. Every acquire must be matched by
//!    a release before the next one. The recorder releases on *every* path,
//!    including the ones where it throws the frame away. Failing to do so is
//!    what eventually surfaces as `DXGI_ERROR_ACCESS_LOST`, and it is why the
//!    old backend could stall for good.
//! 2. `AcquireNextFrame` is the pacer. Waiting on a full handoff queue instead
//!    of on the compositor means stale frames pile up and the release is
//!    delayed behind the consumer. The recorder therefore never blocks on the
//!    channel; it drops and counts.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use argos_core::metrics::SenderMetrics;

use crate::dxgi::{Control, Recorder};

const DEFAULT_CAPTURE_INTERVAL_MICROS: u64 = 33_000;
/// How long a streak of capture failures may last before the session is
/// reported as failed. Transient failures (e.g. DXGI access loss when the
/// desktop composition changes) recover on their own; a streak longer than
/// this is treated as fatal and surfaced to the UI.
const MAX_FAILURE_WINDOW: Duration = Duration::from_secs(30);
/// A capture session that has been healthy for at least this long starts a
/// fresh failure streak on its next failure, so occasional transient
/// interruptions never accumulate into a fatal error.
const HEALTHY_PERIOD: Duration = Duration::from_secs(10);
/// Initial delay between capture recovery attempts.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
/// Upper bound for the recovery backoff.
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(2);
const STOP_POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct CaptureSession {
    rx: Receiver<Frame>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    interval: Arc<AtomicU64>,
    /// Set only when the capture fails fatally (or the capture thread
    /// panics). Recoverable interruptions are retried internally and do not
    /// surface here.
    error: Arc<Mutex<Option<String>>>,
    /// Shared with the capture thread so the UI can read stage timings and drop
    /// counts without touching the frame channel.
    metrics: Arc<SenderMetrics>,
    cursor: Arc<Mutex<Option<argos_core::lan::CursorUpdate>>>,
    join: Option<JoinHandle<()>>,
}

impl CaptureSession {
    pub fn new() -> Self {
        let (_tx, rx) = channel();
        Self {
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(true)),
            interval: Arc::new(AtomicU64::new(DEFAULT_CAPTURE_INTERVAL_MICROS)),
            error: Arc::new(Mutex::new(None)),
            metrics: Arc::new(SenderMetrics::default()),
            cursor: Arc::new(Mutex::new(None)),
            join: None,
        }
    }

    /// Stage timings and drop counters for this session.
    pub fn metrics(&self) -> &Arc<SenderMetrics> {
        &self.metrics
    }

    /// The shared slot holding the last cursor state, for the app to forward to
    /// viewers. Polling is driven by the capture thread; the app reads this at
    /// its repaint cadence.
    pub fn cursor(&self) -> Arc<Mutex<Option<argos_core::lan::CursorUpdate>>> {
        Arc::clone(&self.cursor)
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    pub fn set_interval(&self, interval: Duration) {
        self.interval
            .store(interval.as_micros().max(1) as u64, Ordering::Relaxed);
    }

    /// Reason the capture failed fatally, if it did. `None` while the capture
    /// is running or is recovering from a transient interruption.
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn start(&mut self, source: &MonitorInfo) -> Result<(), String> {
        if self.join.is_some() {
            return Err("capture already running".to_string());
        }
        let wanted = source.name.clone();
        let (tx, rx) = sync_channel::<Frame>(1);
        let stop = self.stop.clone();
        let active = self.active.clone();
        let interval = self.interval.clone();
        let error = Arc::new(Mutex::new(None));
        self.error = Arc::clone(&error);
        let error_reporter = Arc::clone(&error);
        let stop_reporter = Arc::clone(&stop);
        let metrics = Arc::clone(&self.metrics);
        let cursor = self.cursor();
        self.join = Some(
            thread::Builder::new()
                .name("argos-capture".to_string())
                .spawn(move || {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        pump(wanted, tx, stop, active, interval, error, metrics, cursor)
                    }));
                    if let Err(payload) = outcome {
                        // A panicking capture thread used to die silently (the
                        // join handle was discarded). Report it instead so the
                        // failure is diagnosable.
                        if !stop_reporter.load(Ordering::Relaxed) {
                            let detail = if let Some(message) = payload.downcast_ref::<&str>() {
                                (*message).to_owned()
                            } else if let Some(message) = payload.downcast_ref::<String>() {
                                message.clone()
                            } else {
                                "unknown panic payload".to_owned()
                            };
                            set_capture_error(
                                &error_reporter,
                                format!("capture thread panicked: {detail}"),
                            );
                        }
                    }
                })
                .map_err(|error| error.to_string())?,
        );
        self.rx = rx;
        Ok(())
    }

    pub fn latest(&self) -> Option<Frame> {
        let mut latest = None;
        while let Ok(frame) = self.rx.try_recv() {
            latest = Some(frame);
        }
        latest
    }
}

impl Default for CaptureSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Keeps the capture alive across recoverable source failures (desktop
/// composition changes, mode switches, monitor flaps). When the recorder dies
/// or cannot be created, it retries with a bounded backoff and recreates the
/// duplication, which is how the DXGI API itself prescribes handling access
/// loss. Only a persistent failure streak is reported as fatal.
#[allow(clippy::too_many_arguments)]
fn pump(
    wanted: String,
    tx: SyncSender<Frame>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    interval: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
    metrics: Arc<SenderMetrics>,
    cursor: Arc<Mutex<Option<argos_core::lan::CursorUpdate>>>,
) {
    let mut failed_since: Option<Instant> = None;
    let mut connected_at: Option<Instant> = None;
    let mut backoff = RETRY_BACKOFF;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Resolving the monitor and creating the duplication happens on this
        // thread, so a failure to connect is reportable before we commit to a
        // long-lived recorder.
        let control = Control {
            stop: Arc::clone(&stop),
            active: Arc::clone(&active),
            interval: Arc::clone(&interval),
            metrics: Arc::clone(&metrics),
            cursor: Arc::clone(&cursor),
        };
        let mut recorder = match Recorder::start(&wanted, tx.clone(), control) {
            Ok(recorder) => recorder,
            Err(reason) => {
                if failure_exhausted(&mut failed_since, connected_at) {
                    set_capture_error(&error, format!("capture failed: {reason}"));
                    return;
                }
                if sleep_interrupted(backoff, &stop) {
                    return;
                }
                backoff = (backoff * 2).min(MAX_RETRY_BACKOFF);
                continue;
            }
        };
        connected_at = Some(Instant::now());
        backoff = RETRY_BACKOFF;
        let recorder_died = recorder.wait();
        if stop.load(Ordering::Relaxed) || !recorder_died {
            // Stopped, or the session was torn down by the UI. Not a failure.
            return;
        }
        // The duplication went away (access lost, mode switch, GPU reset).
        // Recreate it and retry.
        if failure_exhausted(&mut failed_since, connected_at) {
            set_capture_error(
                &error,
                "capture stopped: the capture source did not recover".to_string(),
            );
            return;
        }
        connected_at = None;
        if sleep_interrupted(backoff, &stop) {
            return;
        }
        backoff = (backoff * 2).min(MAX_RETRY_BACKOFF);
    }
}

/// Registers a capture failure and returns `true` when the failure streak has
/// lasted long enough to be treated as fatal. A failure following a stretch of
/// healthy capture (`healthy_since` older than `HEALTHY_PERIOD`) starts a
/// fresh streak, so occasional interruptions never accumulate.
fn failure_exhausted(failed_since: &mut Option<Instant>, healthy_since: Option<Instant>) -> bool {
    if healthy_since.is_some_and(|start| start.elapsed() >= HEALTHY_PERIOD) {
        *failed_since = None;
    }
    let now = Instant::now();
    let since = *failed_since.get_or_insert(now);
    now.duration_since(since) >= MAX_FAILURE_WINDOW
}

/// Sleeps for `duration`, polling `stop` so the session can be torn down
/// promptly. Returns `true` if the session was stopped during the sleep.
fn sleep_interrupted(duration: Duration, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(STOP_POLL));
    }
    stop.load(Ordering::Relaxed)
}

fn set_capture_error(error: &Arc<Mutex<Option<String>>>, message: String) {
    if let Ok(mut slot) = error.lock() {
        if slot.is_none() {
            *slot = Some(message);
        }
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The attached displays, in the same shape the rest of the app expects.
pub fn list_monitors() -> Vec<MonitorInfo> {
    crate::dxgi::list_monitors()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the real duplication path: enumerate, duplicate, acquire,
    /// read back. Ignored by default because it needs an interactive desktop
    /// and a GPU. Run it explicitly with:
    ///
    /// ```text
    /// cargo test -p argos-media --no-default-features --features capture-native -- --ignored
    /// ```
    #[test]
    #[ignore = "requires an interactive desktop and a GPU"]
    fn captures_real_frames() {
        let monitors = list_monitors();
        assert!(!monitors.is_empty(), "no monitors were enumerated");
        let primary = monitors
            .iter()
            .find(|monitor| monitor.is_primary)
            .unwrap_or(&monitors[0]);
        assert!(
            primary.width > 0 && primary.height > 0,
            "monitor reported no size: {primary:?}"
        );

        let mut session = CaptureSession::new();
        session.start(primary).expect("capture should start");

        let deadline = Instant::now() + Duration::from_secs(5);
        let frame = loop {
            if let Some(frame) = session.latest() {
                break frame;
            }
            assert!(Instant::now() < deadline, "no frame arrived within 5s");
            thread::sleep(Duration::from_millis(20));
        };

        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(
            frame.rgba.len(),
            (frame.width as usize) * (frame.height as usize) * 4,
            "frame buffer is not tightly packed RGBA"
        );
        assert!(
            session.metrics().captured.frames() > 0,
            "the capture metric never recorded a frame"
        );
    }
}
