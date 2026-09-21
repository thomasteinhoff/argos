use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
<<<<<<< HEAD
use std::sync::mpsc::{
    channel, sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError,
};
use std::sync::{Arc, Mutex};
=======
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
>>>>>>> origin/main
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use xcap::Monitor;

const DEFAULT_CAPTURE_INTERVAL_MICROS: u64 = 33_000;
<<<<<<< HEAD
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
const INACTIVE_SLEEP: Duration = Duration::from_millis(20);
const STOP_POLL: Duration = Duration::from_millis(20);
=======
>>>>>>> origin/main

#[derive(Clone)]
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
<<<<<<< HEAD
    /// Set only when the capture fails fatally (or the capture thread
    /// panics). Recoverable interruptions are retried internally and do not
    /// surface here.
    error: Arc<Mutex<Option<String>>>,
=======
>>>>>>> origin/main
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
<<<<<<< HEAD
            error: Arc::new(Mutex::new(None)),
=======
>>>>>>> origin/main
            join: None,
        }
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    pub fn set_interval(&self, interval: Duration) {
        self.interval
            .store(interval.as_micros().max(1) as u64, Ordering::Relaxed);
    }

<<<<<<< HEAD
    /// Reason the capture failed fatally, if it did. `None` while the capture
    /// is running or is recovering from a transient interruption.
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|slot| slot.clone())
    }

=======
>>>>>>> origin/main
    pub fn start(&mut self, source: &MonitorInfo) -> Result<(), String> {
        if self.join.is_some() {
            return Err("capture already running".to_string());
        }
        let wanted = source.name.clone();
        let (tx, rx) = sync_channel::<Frame>(1);
        let stop = self.stop.clone();
        let active = self.active.clone();
        let interval = self.interval.clone();
<<<<<<< HEAD
        let error = Arc::new(Mutex::new(None));
        self.error = Arc::clone(&error);
        let error_reporter = Arc::clone(&error);
        let stop_reporter = Arc::clone(&stop);
        self.join = Some(
            thread::Builder::new()
                .name("argos-capture".to_string())
                .spawn(move || {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        pump(wanted, tx, stop, active, interval, error)
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
=======
        self.join = Some(thread::spawn(move || {
            let Ok(monitors) = Monitor::all() else {
                return;
            };
            let Some(monitor) = monitors
                .into_iter()
                .find(|candidate| candidate.name().ok().as_deref() == Some(wanted.as_str()))
            else {
                return;
            };
            let Ok((recorder, frames)) = monitor.video_recorder() else {
                return;
            };
            let _ = recorder.start();
            pump(recorder, frames, tx, stop, active, interval);
        }));
>>>>>>> origin/main
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

<<<<<<< HEAD
/// (Re)connect the video recorder for the given monitor name. This is the
/// documented recovery from DXGI "access lost" failures: drop the old
/// duplication and create a fresh one for the same output.
fn open_recorder(wanted: &str) -> Result<(xcap::VideoRecorder, Receiver<xcap::Frame>), String> {
    let monitors = Monitor::all().map_err(|error| format!("list monitors: {error}"))?;
    let monitor = monitors
        .into_iter()
        .find(|candidate| candidate.name().ok().as_deref() == Some(wanted))
        .ok_or_else(|| format!("monitor '{wanted}' not found"))?;
    let (recorder, frames) = monitor
        .video_recorder()
        .map_err(|error| format!("create video recorder: {error}"))?;
    recorder
        .start()
        .map_err(|error| format!("start video recorder: {error}"))?;
    Ok((recorder, frames))
}

/// Pump frames from the recorder into `tx`, obeying the active/interval
/// pacing. Returns `true` when the recorder died unexpectedly (its frame
/// channel disconnected while the session is still supposed to be running)
/// and `false` when the pump exited for a normal reason (session stopped or
/// torn down).
fn pump_once(
    recorder: xcap::VideoRecorder,
    frames: Receiver<xcap::Frame>,
    tx: &SyncSender<Frame>,
    stop: &Arc<AtomicBool>,
    active: &Arc<AtomicBool>,
    interval: &Arc<AtomicU64>,
) -> bool {
    let mut last = Instant::now() - Duration::from_secs(1);
    let died = loop {
        if stop.load(Ordering::Relaxed) {
            break false;
        }
        if !active.load(Ordering::Relaxed) {
            thread::sleep(INACTIVE_SLEEP);
            last = Instant::now();
            // Discard frames that arrived while paused and detect a recorder
            // that died while the session was inactive.
            match frames.try_recv() {
                Ok(_) => {}
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break true,
            }
=======
fn pump(
    recorder: xcap::VideoRecorder,
    frames: Receiver<xcap::Frame>,
    tx: SyncSender<Frame>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    interval: Arc<AtomicU64>,
) {
    let mut last = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        if !active.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
            last = Instant::now();
>>>>>>> origin/main
            continue;
        }
        let target = Duration::from_micros(interval.load(Ordering::Relaxed).max(1));
        let elapsed = last.elapsed();
        if elapsed < target {
            thread::sleep(target - elapsed);
        }
        if stop.load(Ordering::Relaxed) {
<<<<<<< HEAD
            break false;
=======
            break;
>>>>>>> origin/main
        }
        match frames.recv_timeout(target) {
            Ok(frame) => {
                last = Instant::now();
                let outgoing = Frame {
                    width: frame.width,
                    height: frame.height,
                    rgba: frame.raw,
                };
                match tx.try_send(outgoing) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {}
<<<<<<< HEAD
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break false,
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break true,
        }
    };
    let _ = recorder.stop();
    died
}

/// Keeps the capture alive across recoverable source failures (desktop
/// composition changes, mode switches, monitor flaps). When the recorder dies
/// or cannot be created, it retries with a bounded backoff and recreates the
/// recorder, which is how the DXGI API itself prescribes handling access
/// loss. Only a persistent failure streak is reported as fatal.
fn pump(
    wanted: String,
    tx: SyncSender<Frame>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    interval: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
) {
    let mut failed_since: Option<Instant> = None;
    let mut connected_at: Option<Instant> = None;
    let mut backoff = RETRY_BACKOFF;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let (recorder, frames) = match open_recorder(&wanted) {
            Ok(ready) => ready,
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
        let recorder_died = pump_once(recorder, frames, &tx, &stop, &active, &interval);
        if stop.load(Ordering::Relaxed) || !recorder_died {
            // Stopped, or the session was torn down by the UI. Not a failure.
            return;
        }
        // The capture source stopped delivering frames. Recreate the recorder
        // and retry.
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
=======
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = recorder.stop();
>>>>>>> origin/main
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    Monitor::all()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|monitor| {
            let name = monitor.name().ok()?;
            let width = monitor.width().ok()?;
            let height = monitor.height().ok()?;
            let is_primary = monitor.is_primary().ok()?;
            Some(MonitorInfo {
                name,
                width,
                height,
                is_primary,
            })
        })
        .collect()
}
