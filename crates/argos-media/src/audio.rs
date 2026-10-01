use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use argos_core::metrics::AudioMetrics;
use opus_rs::{Application, OpusDecoder, OpusEncoder};
use wasapi::{
    initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_SAMPLES: usize = 960;
const BYTES_PER_SAMPLE: usize = 4;
const BLOCK_ALIGN: usize = CHANNELS * BYTES_PER_SAMPLE;
/// How often the capture thread re-checks which process to exclude from the mix.
const EXCLUSION_RESCAN_INTERVAL: Duration = Duration::from_secs(2);
/// Buffer duration for the process-loopback client (20 ms). The device period cannot
/// be queried in process-loopback mode and the passed value is ignored by the engine anyway.
const PROCESS_LOOPBACK_PERIOD_HNS: i64 = 200_000;

/// How long to wait on the WASAPI event before re-checking the stop flag and
/// liveness.
///
/// The unit is milliseconds and the previous value was `1_000_000`, i.e. about
/// 16.7 minutes. That is harmless while the device keeps signalling — the wait
/// returns early each period — but if the event ever stops firing (device
/// unplugged, default device changed, stream invalidated) the thread hangs for
/// that entire window and never notices its own stop flag. A short timeout
/// makes that failure mode bounded.
const EVENT_WAIT_MS: u32 = 20;

/// First backoff between attempts to open an audio device.
const RETRY_BACKOFF: Duration = Duration::from_millis(250);
/// Ceiling for the exponential backoff, so a device that comes back after a
/// minute is picked up promptly instead of after a 16 s wait.
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Sleeps for `duration`, returning `true` if the stop flag was set instead.
fn sleep_interrupted(duration: Duration, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        thread::sleep(Duration::from_millis(10).min(duration));
    }
    stop.load(Ordering::Relaxed)
}

/// Health of an audio worker thread, shared with the UI.
///
/// This exists because of a specific failure: the worker used to report its
/// first error through a one-shot channel that `start` had already consumed, so
/// every error after startup was discarded. The thread exited, and because the
/// handle was still `Some` every "is audio running?" check kept reporting yes
/// for the rest of the session. Nothing downstream could tell the difference.
///
/// `alive` is the single source of truth for that question, and it is only ever
/// cleared by the worker itself — on entry to a retry backoff, or when it exits.
#[derive(Debug, Default)]
pub struct AudioState {
    metrics: Arc<AudioMetrics>,
    /// The most recent error or notice, waiting to be picked up by the UI.
    last_error: Mutex<Option<String>>,
    /// True only while a device client is open and being serviced.
    alive: AtomicBool,
    /// Bumped every time the worker rebuilds its device client, so the UI can
    /// tell a fresh failure from one it has already shown.
    generation: AtomicU64,
}

impl AudioState {
    pub fn metrics(&self) -> &AudioMetrics {
        &self.metrics
    }

    /// A handle to the shared counters, for workers that rebuild their buffers
    /// but must keep accumulating into one place.
    pub fn metrics_handle(&self) -> Arc<AudioMetrics> {
        Arc::clone(&self.metrics)
    }

    /// True while a device client is open. False during a retry backoff and
    /// after the worker has given up, which is exactly when the UI must say so.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Which attempt of the current worker produced [`Self::last_error`].
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Records a failure and marks the worker as not currently serving audio.
    pub fn fail(&self, generation: u64, error: impl Into<String>) {
        self.alive.store(false, Ordering::Relaxed);
        self.metrics.device_errors.incr();
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(error.into());
        }
        self.generation.store(generation, Ordering::Relaxed);
    }

    /// Records a non-fatal notice (e.g. falling back to full loopback) without
    /// claiming the worker is unhealthy.
    pub fn notice(&self, notice: impl Into<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(notice.into());
        }
    }

    /// Takes the pending error or notice, leaving the slot empty.
    pub fn take_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|mut slot| slot.take())
    }

    pub fn clear_error(&self) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = None;
        }
    }
}

/// A bounded, self-correcting buffer between the network and the audio device.
///
/// The device always consumes exactly as many frames as it asks for, and the
/// network delivers samples paced by a *different machine's* clock. The two
/// disagree by a few hundred parts per million, which is nothing per second and
/// everything over an hour: uncorrected, the level walks out of the safe band
/// and then every device period is padded with silence, so the audio degrades
/// into a permanent stutter that no restart fixes.
///
/// The correction is therefore continuous and tiny. Holding the level near the
/// target costs one duplicated or discarded sample every few periods — a rate
/// change of well under 0.1%, which is inaudible — instead of the audible
/// alternative of dropping a whole frame.
pub struct JitterBuffer {
    queue: VecDeque<f32>,
    /// Do not write to the device until this much audio is queued. Starting
    /// mid-stream is what produces the first burst of underruns otherwise.
    target: usize,
    high: usize,
    low: usize,
    max: usize,
    /// Consecutive periods observed outside the band before a correction is
    /// applied, so one late packet does not nudge the clock.
    high_streak: u32,
    low_streak: u32,
    /// `-1` while shedding samples, `0` while idle, `+1` while padding. Held
    /// across periods until the level is back at the target, so a correction
    /// can outrun the deficit that started it.
    correcting: i8,
    /// True once the buffer has been filled to the target at least once.
    primed: bool,
    metrics: Arc<AudioMetrics>,
}

/// Consecutive out-of-band periods required before the level is nudged.
const DRIFT_STREAK: u32 = 8;
/// Samples inserted or discarded per correction. At 960 frames per 20 ms
/// period, 8 samples is a 0.04% rate change: below the threshold of hearing.
const DRIFT_STEP: usize = 8;

impl JitterBuffer {
    pub fn new(metrics: Arc<AudioMetrics>) -> Self {
        let target = FRAME_SAMPLES * CHANNELS * 5; // 100 ms
        let max = FRAME_SAMPLES * CHANNELS * 15; // 300 ms
        Self {
            queue: VecDeque::with_capacity(max),
            target,
            high: target + FRAME_SAMPLES * CHANNELS / 2,
            low: target - FRAME_SAMPLES * CHANNELS / 2,
            max,
            high_streak: 0,
            low_streak: 0,
            correcting: 0,
            primed: false,
            metrics,
        }
    }

    /// Samples currently held.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Adds decoded samples, discarding the oldest if the buffer is full.
    ///
    /// Trimming from the front keeps the freshest audio: a late burst should
    /// delay playback, not rewind it.
    pub fn push(&mut self, samples: &[f32]) {
        let overflow = self
            .queue
            .len()
            .saturating_add(samples.len())
            .saturating_sub(self.max);
        if overflow > 0 {
            self.queue.drain(..overflow.min(self.queue.len()));
            self.metrics.overruns.drop_frame();
        } else {
            self.metrics.overruns.record();
        }
        self.queue.extend(samples.iter().copied());
    }

    /// Fills `out` with `want` samples of audio, padding with silence if the
    /// buffer ran short.
    ///
    /// Returns `true` if the device period was served from real audio. `false`
    /// means the period was silence used only to keep the device's event loop
    /// turning during the initial prebuffer, which is not an underrun.
    pub fn fill(&mut self, out: &mut Vec<f32>, want: usize) -> bool {
        out.clear();
        out.reserve(want);

        if !self.primed {
            // Wait for a full target depth *plus* the period about to be
            // written, so the first real period still leaves the buffer at
            // target. Starting a period short is what produces the first burst
            // of underruns.
            if self.queue.len() < self.target + want {
                // Still filling. Hand the device silence so its buffer keeps
                // advancing, but do not pretend real audio was lost.
                out.resize(want, 0.0);
                return false;
            }
            self.primed = true;
        }

        let available = self.queue.len();
        if available < want {
            self.metrics.underruns.drop_frame();
        } else {
            self.metrics.underruns.record();
        }
        let take = want.min(available);
        out.extend(self.queue.drain(..take));
        out.resize(want, 0.0);

        // Drift is judged on the level left *behind*, which is the buffer depth
        // the device actually experiences. Judging the pre-drain level would
        // compare against a figure inflated by one period of arriving audio,
        // shifting the whole band down by a period and making the thresholds
        // mean something other than what they say.
        self.correct_drift(available - take);
        true
    }

    /// Nudges the level back toward the target.
    ///
    /// Onset is gated by a streak so that one late packet does not move the
    /// clock. Once correcting, it keeps going until the level is back at the
    /// target: gating every individual correction by the streak as well would
    /// cap the correction at one nudge per window, which is smaller than the
    /// deficit it exists to cancel, so the level would keep sliding and the
    /// correction would never actually win.
    fn correct_drift(&mut self, depth: usize) {
        let step: i64 = match self.correcting {
            0 => {
                if depth > self.high {
                    self.high_streak += 1;
                    self.low_streak = 0;
                    if self.high_streak >= DRIFT_STREAK {
                        // Too much queued: the sender's clock is ahead, or the
                        // network bursted. Skip ahead so the buffer shrinks.
                        self.correcting = -1;
                        -(DRIFT_STEP as i64)
                    } else {
                        0
                    }
                } else if depth < self.low {
                    self.low_streak += 1;
                    self.high_streak = 0;
                    if self.low_streak >= DRIFT_STREAK {
                        // Running dry: the sender's clock is behind. Repeat a
                        // sliver of audio to slow playback by a hair.
                        self.correcting = 1;
                        DRIFT_STEP as i64
                    } else {
                        0
                    }
                } else {
                    self.high_streak = 0;
                    self.low_streak = 0;
                    0
                }
            }
            1 => {
                if depth >= self.target {
                    self.correcting = 0;
                }
                DRIFT_STEP as i64
            }
            _ => {
                if depth <= self.target {
                    self.correcting = 0;
                }
                -(DRIFT_STEP as i64)
            }
        };

        if step < 0 {
            let discard = ((-step) as usize).min(self.queue.len());
            self.queue.drain(..discard);
            self.metrics.drift_samples.add(discard as u64);
        } else if step > 0 {
            let repeat = (step as usize).min(self.queue.len());
            if repeat > 0 {
                let head: Vec<f32> = self.queue.iter().take(repeat).copied().collect();
                for sample in head.into_iter().rev() {
                    self.queue.push_front(sample);
                }
                self.metrics.drift_samples.add(repeat as u64);
            }
        }
    }
}

/// Converts one 20 ms block of f32 device bytes into interleaved samples.
///
/// Works in contiguous runs rather than four bytes at a time. The obvious
/// `pop_front` loop costs four bounds-checked deque operations per sample, which
/// at 48 kHz is roughly 380,000 calls per second on the capture thread's
/// critical path, all of it overhead.
fn pop_frame(bytes: &mut VecDeque<u8>, samples: &mut Vec<f32>) -> bool {
    let frame_bytes = FRAME_SAMPLES * BLOCK_ALIGN;
    if bytes.len() < frame_bytes {
        return false;
    }
    samples.clear();
    samples.reserve(FRAME_SAMPLES * CHANNELS);
    let mut remaining = frame_bytes;
    {
        // `as_slices` hands back the two contiguous runs of a wrapped deque, so
        // the conversion is a straight walk over slices with no per-byte branch.
        let (head, tail) = bytes.as_slices();
        for run in [head, tail] {
            let take = remaining.min(run.len());
            let (samples_bytes, _) = run[..take].as_chunks::<BYTES_PER_SAMPLE>();
            samples.extend(samples_bytes.iter().copied().map(f32::from_le_bytes));
            remaining -= take;
            if remaining == 0 {
                break;
            }
        }
    }
    bytes.drain(..frame_bytes);
    true
}

pub struct AudioCapture {
    rx: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    state: Arc<AudioState>,
    join: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = sync_channel::<Vec<f32>>(16);
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(AudioState::default());
        let thread_stop = Arc::clone(&stop);
        let thread_state = Arc::clone(&state);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let join = thread::Builder::new()
            .name("argos-audio-capture".to_string())
            .spawn(move || capture_loop(tx, thread_stop, thread_state, ready_tx))
            .map_err(|error| error.to_string())?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx,
                stop,
                state,
                join: Some(join),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("audio capture thread exited early".to_string()),
        }
    }

    /// Health of the capture thread, for the UI to display.
    pub fn state(&self) -> &Arc<AudioState> {
        &self.state
    }

    pub fn try_frame(&self) -> Option<Vec<f32>> {
        self.rx.try_recv().ok()
    }

    /// Takes the next pending notice (e.g. Discord exclusion falling back to
    /// full loopback, or a device failure the worker has already recovered
    /// from).
    pub fn try_error(&self) -> Option<String> {
        self.state.take_error()
    }

    /// True while the capture thread is servicing a device.
    pub fn is_alive(&self) -> bool {
        self.state.is_alive()
    }

    pub fn last_frame(&self) -> Option<Vec<f32>> {
        let mut latest = None;
        while let Ok(frame) = self.rx.try_recv() {
            latest = Some(frame);
        }
        latest
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn capture_loop(
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    state: Arc<AudioState>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    // COM initialisation is per-thread and unrecoverable: if it fails there is
    // no point retrying, so this is the one failure that legitimately reports
    // through `ready` and stops. Everything else retries below.
    if let Err(error) = initialize_mta().ok() {
        let message = format!("COM init failed: {error}");
        state.fail(0, message.clone());
        let _ = ready.send(Err(message));
        return;
    }

    let mut backoff = RETRY_BACKOFF;
    let mut generation = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match run_capture(&tx, &stop, &state, generation) {
            Ok(()) => return,
            Err(error) => {
                state.fail(generation, error);
                generation += 1;
                state.metrics().restarts.incr();
            }
        }
        if sleep_interrupted(backoff, &stop) {
            return;
        }
        backoff = (backoff * 2).min(MAX_RETRY_BACKOFF);
    }
}

fn run_capture(
    tx: &SyncSender<Vec<f32>>,
    stop: &AtomicBool,
    state: &AudioState,
    generation: u64,
) -> Result<(), String> {
    let format = WaveFormat::new(
        BYTES_PER_SAMPLE * 8,
        BYTES_PER_SAMPLE * 8,
        &SampleType::Float,
        SAMPLE_RATE as usize,
        CHANNELS,
        None,
    );
    let frame_bytes = FRAME_SAMPLES * BLOCK_ALIGN;
    // The root Discord process currently excluded from the mix, if any.
    let mut exclusion_target: Option<u32> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Pick what to exclude: the root Discord process when it is running.
        // A scan failure is non-fatal: fall back to a normal full loopback.
        let target = match find_discord_root_pid() {
            Ok(pid) => pid,
            Err(error) => {
                state.notice(format!("Discord detection failed: {error}"));
                None
            }
        };
        if target != exclusion_target {
            if let Some(pid) = target {
                state.notice(format!(
                    "Discord audio excluded from the stream (pid {pid})"
                ));
            } else {
                state.notice("Discord not running: sharing full system audio".to_string());
            }
        }
        exclusion_target = target;

        // Build a loopback client. With a Discord pid available we ask WASAPI for a
        // process-loopback stream in EXCLUDE mode (everything except Discord's tree);
        // otherwise we capture the plain device mix.
        let (mut audio_client, buffer_duration_hns) = build_capture_client(target, state)?;

        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns,
        };
        audio_client
            .initialize_client(&format, &Direction::Capture, &mode)
            .map_err(|error| format!("loopback init failed: {error}"))?;

        let event = audio_client
            .set_get_eventhandle()
            .map_err(|error| error.to_string())?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .map_err(|error| error.to_string())?;
        audio_client
            .start_stream()
            .map_err(|error| error.to_string())?;
        state.alive.store(true, Ordering::Relaxed);
        state.clear_error();

        let mut bytes: VecDeque<u8> = VecDeque::with_capacity(frame_bytes * 4);
        let mut samples: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * CHANNELS);
        let mut next_rescan = Instant::now() + EXCLUSION_RESCAN_INTERVAL;
        let mut rebuild = false;

        while !stop.load(Ordering::Relaxed) {
            // Periodically re-check whether the exclusion target changed (Discord
            // started / quit / restarted) and rebuild the capture if it did.
            if Instant::now() >= next_rescan {
                next_rescan = Instant::now() + EXCLUSION_RESCAN_INTERVAL;
                match find_discord_root_pid() {
                    Ok(current) if current != exclusion_target => {
                        rebuild = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        state.notice(format!("Discord detection failed: {error}"));
                    }
                }
            }

            if let Err(error) = capture_client.read_from_device_to_deque(&mut bytes) {
                // A device change invalidates the client. Report it, then fall out
                // of this inner loop so the outer one can build a fresh client
                // against the new default device.
                state.fail(generation, format!("audio capture read failed: {error}"));
                rebuild = true;
                break;
            }
            while pop_frame(&mut bytes, &mut samples) {
                let _ = tx.try_send(samples.clone());
            }
            state.metrics().periods.incr();
            if event.wait_for_event(EVENT_WAIT_MS).is_err() {
                rebuild = true;
                break;
            }
        }

        let _ = audio_client.stop_stream();
        if !rebuild || stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }
}

fn build_capture_client(
    target: Option<u32>,
    state: &AudioState,
) -> Result<(AudioClient, i64), String> {
    if let Some(pid) = target {
        match AudioClient::new_application_loopback_client(pid, false) {
            Ok(client) => return Ok((client, PROCESS_LOOPBACK_PERIOD_HNS)),
            Err(error) => {
                state.notice(format!(
                    "Discord exclusion unavailable ({error}); sharing full system audio"
                ));
            }
        }
    }
    // Plain WASAPI loopback of the default output device.
    let enumerator = DeviceEnumerator::new().map_err(|error| error.to_string())?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|error| format!("no default output device: {error}"))?;
    let audio_client = device
        .get_iaudioclient()
        .map_err(|error| error.to_string())?;
    let (_, min_time) = audio_client
        .get_device_period()
        .map_err(|error| error.to_string())?;
    Ok((audio_client, min_time))
}

/// Find the root process of the Discord process tree, if Discord is running.
/// Returns `None` when no Discord process is active. The root is the process whose
/// parent is not itself a Discord process, excluding that root excludes the whole tree.
fn find_discord_root_pid() -> Result<Option<u32>, String> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|error| error.to_string())?;
    let mut found = vec![];
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry).is_ok() };
    while ok {
        let name = image_name(&entry.szExeFile);
        if is_discord_image(&name) {
            found.push((entry.th32ProcessID, entry.th32ParentProcessID));
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry).is_ok() };
    }
    let _ = unsafe { CloseHandle(snapshot) };
    Ok(select_discord_root(&found))
}

/// Derived from a snapshot of (pid, parent_pid) pairs of Discord processes.
fn select_discord_root(processes: &[(u32, u32)]) -> Option<u32> {
    if processes.is_empty() {
        return None;
    }
    let pids: std::collections::HashSet<u32> = processes.iter().map(|(pid, _)| *pid).collect();
    // Roots are processes whose parent is not a Discord process itself.
    let mut roots: Vec<u32> = processes
        .iter()
        .filter(|(_, parent)| !pids.contains(parent))
        .map(|(pid, _)| *pid)
        .collect();
    roots.sort_unstable();
    // Prefer the root with the largest subtree (handles a second Discord instance).
    roots
        .into_iter()
        .max_by_key(|root| count_descendants(*root, processes))
}

fn count_descendants(root: u32, processes: &[(u32, u32)]) -> usize {
    let pids: std::collections::HashSet<u32> = processes.iter().map(|(pid, _)| *pid).collect();
    processes
        .iter()
        .filter(|&&(_, parent)| {
            if parent == root {
                return true;
            }
            // Walk up the tree to see if this process descends from root.
            let mut current = parent;
            while pids.contains(&current) {
                if current == root {
                    break;
                }
                match processes.iter().find(|&&(p, _)| p == current) {
                    Some(&(_, next_parent)) => {
                        if next_parent == root {
                            return true;
                        }
                        current = next_parent;
                    }
                    None => return false,
                }
            }
            false
        })
        .count()
}

/// Read an executable image name from a null-terminated Win32 wide string, lowercased.
fn image_name(name: &[u16]) -> String {
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    name[..end]
        .iter()
        .map(|&c| char::from_u32(c as u32).unwrap_or('?').to_ascii_lowercase())
        .collect()
}

fn is_discord_image(name: &str) -> bool {
    matches!(name, "discord.exe" | "discordptb.exe" | "discordcanary.exe")
}

pub struct OpusAudioEncoder {
    encoder: OpusEncoder,
    buffer: Vec<u8>,
}

impl OpusAudioEncoder {
    pub fn new() -> Result<Self, String> {
        let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, CHANNELS, Application::Audio)
            .map_err(|error| error.to_string())?;
        encoder.bitrate_bps = 96_000;
        Ok(Self {
            encoder,
            buffer: vec![0u8; 4000],
        })
    }

    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, String> {
        let written = self
            .encoder
            .encode(samples, FRAME_SAMPLES, &mut self.buffer)
            .map_err(|error| error.to_string())?;
        Ok(self.buffer[..written].to_vec())
    }
}

pub struct OpusAudioDecoder {
    decoder: OpusDecoder,
}

impl OpusAudioDecoder {
    pub fn new() -> Result<Self, String> {
        let decoder =
            OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS).map_err(|error| error.to_string())?;
        Ok(Self { decoder })
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>, String> {
        let mut samples = vec![0f32; FRAME_SAMPLES * CHANNELS];
        let written = self
            .decoder
            .decode(packet, FRAME_SAMPLES, &mut samples)
            .map_err(|error| error.to_string())?;
        samples.truncate(written * CHANNELS);
        Ok(samples)
    }
}

pub struct AudioPlayback {
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    state: Arc<AudioState>,
    join: Option<JoinHandle<()>>,
}

impl AudioPlayback {
    pub fn start(volume: f32) -> Result<Self, String> {
        let (tx, rx) = sync_channel::<Vec<f32>>(32);
        let stop = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_muted = Arc::clone(&muted);
        let state = Arc::new(AudioState::default());
        let thread_state = Arc::clone(&state);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let join = thread::Builder::new()
            .name("argos-audio-playback".to_string())
            .spawn(move || {
                playback_loop(
                    rx,
                    thread_stop,
                    thread_muted,
                    thread_state,
                    ready_tx,
                    volume,
                )
            })
            .map_err(|error| error.to_string())?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                stop,
                muted,
                state,
                join: Some(join),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("audio playback thread exited early".to_string()),
        }
    }

    pub fn push(&self, frame: Vec<f32>) {
        let _ = self.tx.try_send(frame);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Health of the playback thread, for the UI to display.
    pub fn state(&self) -> &Arc<AudioState> {
        &self.state
    }

    /// True while a device client is open and being fed. False during a retry
    /// backoff, which is the only reliable way for the UI to know audio is not
    /// coming out.
    pub fn is_alive(&self) -> bool {
        self.state.is_alive()
    }

    /// Takes the next pending error, leaving the slot empty.
    pub fn try_error(&self) -> Option<String> {
        self.state.take_error()
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn playback_loop(
    rx: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    state: Arc<AudioState>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
    volume: f32,
) {
    // COM initialisation is per-thread and unrecoverable, so it is the only
    // failure that reports through `ready` and gives up. A device that is
    // missing or invalid at startup is *not* fatal: the worker retries in the
    // background, so plugging in headphones brings the audio back without
    // restarting the app.
    if let Err(error) = initialize_mta().ok() {
        let message = format!("COM init failed: {error}");
        state.fail(0, message.clone());
        let _ = ready.send(Err(message));
        return;
    }

    let mut backoff = RETRY_BACKOFF;
    let mut generation = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match run_playback(&rx, &stop, &muted, volume, &state, generation) {
            Ok(()) => return,
            Err(error) => {
                state.fail(generation, error);
                generation += 1;
                state.metrics().restarts.incr();
            }
        }
        if sleep_interrupted(backoff, &stop) {
            return;
        }
        backoff = (backoff * 2).min(MAX_RETRY_BACKOFF);
    }
}

fn run_playback(
    rx: &Receiver<Vec<f32>>,
    stop: &AtomicBool,
    muted: &AtomicBool,
    volume: f32,
    state: &AudioState,
    generation: u64,
) -> Result<(), String> {
    let enumerator = DeviceEnumerator::new().map_err(|error| error.to_string())?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|error| error.to_string())?;
    let mut audio_client = device
        .get_iaudioclient()
        .map_err(|error| error.to_string())?;

    let format = WaveFormat::new(
        BYTES_PER_SAMPLE * 8,
        BYTES_PER_SAMPLE * 8,
        &SampleType::Float,
        SAMPLE_RATE as usize,
        CHANNELS,
        None,
    );
    let (_, min_time) = audio_client
        .get_device_period()
        .map_err(|error| error.to_string())?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: min_time,
    };
    audio_client
        .initialize_client(&format, &Direction::Render, &mode)
        .map_err(|error| format!("playback init failed: {error}"))?;

    let event = audio_client
        .set_get_eventhandle()
        .map_err(|error| error.to_string())?;
    let render_client = audio_client
        .get_audiorenderclient()
        .map_err(|error| error.to_string())?;
    audio_client
        .start_stream()
        .map_err(|error| error.to_string())?;
    state.alive.store(true, Ordering::Relaxed);
    state.clear_error();

    // A fresh jitter buffer per client: the samples queued for a device that
    // just went away are the wrong length for the new one, and starting with a
    // full buffer is the whole point of having one. The counters it writes into
    // are shared with the state, so they keep accumulating across restarts.
    let mut jitter = JitterBuffer::new(state.metrics_handle());
    let mut out: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 4);
    let mut queue: VecDeque<u8> =
        VecDeque::with_capacity(FRAME_SAMPLES * CHANNELS * 4 * BYTES_PER_SAMPLE);
    // Frames we refuse to write in one go, so a single period cannot drain the
    // whole jitter buffer and leave the next one starving.
    let max_frames_per_period = FRAME_SAMPLES * 4;

    while !stop.load(Ordering::Relaxed) {
        // Top the buffer up from the network first, non-blocking. The channel is
        // the backpressure: when it is full, `push` drops, and the jitter
        // buffer's high-water correction is what pulls the level back.
        while let Ok(frame) = rx.try_recv() {
            jitter.push(&frame);
        }

        let available = audio_client
            .get_available_space_in_frames()
            .map_err(|error| error.to_string())? as usize;
        let want_frames = available.min(max_frames_per_period);
        if want_frames == 0 {
            if event.wait_for_event(EVENT_WAIT_MS).is_err() {
                break;
            }
            continue;
        }

        let started = Instant::now();
        let _real = jitter.fill(&mut out, want_frames * CHANNELS);
        // Mute is applied by scaling to zero rather than by stopping the stream,
        // so the device keeps advancing in real time and unmuting resumes in
        // sync instead of dumping a backlog.
        let gain = if muted.load(Ordering::Relaxed) {
            0.0
        } else {
            volume
        };
        queue.clear();
        for sample in &out {
            queue.extend((sample * gain).to_le_bytes());
        }
        // `write_to_device_from_deque` refuses a short buffer, so the length
        // invariant has to hold no matter what the jitter buffer did.
        let needed = want_frames * BLOCK_ALIGN;
        if queue.len() < needed {
            queue.resize(needed, 0);
        }
        render_client
            .write_to_device_from_deque(want_frames, &mut queue, None)
            .map_err(|error| {
                // The client is dead: the device was changed, invalidated, or
                // removed. Report it so the UI can say so, then let the caller
                // rebuild against whatever the default device is now.
                state.fail(generation, format!("audio playback failed: {error}"));
                error.to_string()
            })?;
        state.metrics().device_io.record(started.elapsed());
        state.metrics().periods.incr();

        if event.wait_for_event(EVENT_WAIT_MS).is_err() {
            // The event stopped firing. Treat it as a dead client rather than
            // spinning on a dead handle.
            state.fail(generation, "audio device stopped signalling".to_string());
            break;
        }
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{pop_frame, select_discord_root, JitterBuffer, CHANNELS, FRAME_SAMPLES};
    use argos_core::metrics::AudioMetrics;
    use std::collections::VecDeque;
    use std::sync::Arc;

    /// One 20 ms frame of interleaved stereo.
    const FRAME: usize = FRAME_SAMPLES * CHANNELS;
    /// The buffer primes at five frames and corrects drift outside ±half a frame
    /// of that.
    const TARGET: usize = FRAME * 5;
    const HIGH: usize = TARGET + FRAME / 2;
    const LOW: usize = TARGET - FRAME / 2;
    const MAX: usize = FRAME * 15;

    fn constant(value: f32) -> Vec<f32> {
        vec![value; FRAME]
    }

    fn buffer() -> (JitterBuffer, Arc<AudioMetrics>) {
        let metrics = Arc::new(AudioMetrics::default());
        (JitterBuffer::new(Arc::clone(&metrics)), metrics)
    }

    /// Fills the buffer past the priming depth and primes it, so a test can
    /// start from a known-good state: one full target depth left on the books.
    fn primed(value: f32) -> (JitterBuffer, Arc<AudioMetrics>, Vec<f32>) {
        let (mut buffer, metrics) = buffer();
        // Six frames: the target depth plus the period about to be written.
        for _ in 0..6 {
            buffer.push(&constant(value));
        }
        let mut out = Vec::new();
        assert!(buffer.fill(&mut out, FRAME));
        assert_eq!(out, constant(value));
        assert_eq!(buffer.len(), TARGET);
        (buffer, metrics, out)
    }

    #[test]
    fn discord_root_single_process() {
        assert_eq!(select_discord_root(&[(100, 4)]), Some(100));
    }

    #[test]
    fn discord_root_skips_child_processes() {
        // The renderer (101) is a child of the main process (100).
        let processes = [(100, 4), (101, 100), (102, 101)];
        assert_eq!(select_discord_root(&processes), Some(100));
    }

    #[test]
    fn discord_root_empty_processes() {
        assert_eq!(select_discord_root(&[]), None);
    }

    #[test]
    fn discord_root_prefers_largest_tree() {
        // Two Discord instances: tree A and the larger tree B.
        let processes = [
            (100, 4),
            (101, 100),
            (200, 4),
            (201, 200),
            (202, 201),
            (203, 202),
        ];
        assert_eq!(select_discord_root(&processes), Some(200));
    }

    #[test]
    fn discord_root_no_parent_match_falls_back() {
        // Parent pids are not Discord processes: every pid is its own root.
        let processes = [(100, 4), (101, 5), (102, 6)];
        let root = select_discord_root(&processes).unwrap();
        assert!(processes.iter().any(|(pid, _)| *pid == root));
    }

    /// One device frame's worth of little-endian f32.
    fn device_frame(samples: &[f32]) -> VecDeque<u8> {
        let mut bytes = VecDeque::new();
        for sample in samples {
            bytes.extend(sample.to_le_bytes());
        }
        bytes
    }

    /// Share of the frame that ends up ahead of the wrap point in
    /// [`wrapped_device_frame`].
    const WRAP_LEAD: usize = 8;
    /// Bytes of filler left queued behind the frame, standing in for the rest of
    /// a device period's payload.
    const WRAP_TRAILER: usize = 16;

    /// The same frame, written so that it straddles the wrap point of the
    /// deque's storage and therefore has to be read out of both of `as_slices`'
    /// runs. The frame still starts at the logical front, which is where a
    /// device read always puts it.
    ///
    /// Filling the storage first would not do: the bytes already at the head
    /// would still be sitting between the wrap point and the frame. Instead the
    /// head of the frame is pushed in from the front, which walks the deque's
    /// head off the start of the storage and around to the end.
    fn wrapped_device_frame(samples: &[f32]) -> VecDeque<u8> {
        let frame: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut bytes = VecDeque::new();
        bytes.reserve(frame.len() + WRAP_TRAILER);
        // `push_front` places each byte ahead of the previous one, so the head of
        // the frame has to go in last.
        for byte in frame[..WRAP_LEAD].iter().rev() {
            bytes.push_front(*byte);
        }
        for byte in &frame[WRAP_LEAD..] {
            bytes.push_back(*byte);
        }
        for _ in 0..WRAP_TRAILER {
            bytes.push_back(0xAA);
        }
        bytes
    }

    #[test]
    fn pop_frame_decodes_interleaved_float32() {
        let samples: Vec<f32> = (0..FRAME as i32).map(|i| i as f32 * 0.001).collect();
        let mut bytes = device_frame(&samples);
        let mut out = Vec::new();
        assert!(pop_frame(&mut bytes, &mut out));
        assert_eq!(out, samples);
        assert!(bytes.is_empty());
    }

    #[test]
    fn pop_frame_handles_a_wrapped_deque() {
        let samples: Vec<f32> = (0..FRAME as i32).map(|i| i as f32 * 0.001).collect();
        let mut bytes = wrapped_device_frame(&samples);
        let (head, tail) = bytes.as_slices();
        assert!(
            !head.is_empty() && !tail.is_empty(),
            "frame did not straddle"
        );
        let mut out = Vec::new();
        assert!(pop_frame(&mut bytes, &mut out));
        assert_eq!(out, samples);
        // Only the filler behind the frame is left.
        assert_eq!(bytes.len(), WRAP_TRAILER);
        assert!(bytes.iter().all(|byte| *byte == 0xAA));
    }

    #[test]
    fn pop_frame_refuses_a_partial_block_and_leaves_it_intact() {
        let mut bytes = VecDeque::new();
        bytes.extend((1.0f32).to_le_bytes());
        let mut out = Vec::new();
        assert!(!pop_frame(&mut bytes, &mut out));
        // A partial read must survive to be completed by the next call; losing
        // it would drop a quarter of a frame and click.
        assert_eq!(bytes.len(), 4);
    }

    #[test]
    fn pop_frame_consumes_exactly_one_frame_and_leaves_the_rest() {
        let mut bytes = VecDeque::new();
        bytes.extend((1.0f32).to_le_bytes());
        bytes.extend((2.0f32).to_le_bytes());
        let lead = bytes.len();
        bytes.extend(vec![0u8; FRAME * 4 - lead]);
        let mut out = Vec::new();
        assert!(pop_frame(&mut bytes, &mut out));
        assert_eq!(out[0], 1.0);
        assert_eq!(out[1], 2.0);
        assert!(bytes.is_empty());
        // Anything past a full frame must not be touched.
        bytes.extend((9.0f32).to_le_bytes());
        assert!(!pop_frame(&mut bytes, &mut out));
        assert_eq!(bytes.len(), 4);
    }

    #[test]
    fn jitter_buffer_pads_with_silence_until_primed() {
        let (mut buffer, metrics) = buffer();
        let mut out = Vec::new();
        // Below target: silence, and explicitly not counted as an underrun,
        // because nothing has been lost yet.
        buffer.push(&constant(1.0));
        assert!(!buffer.fill(&mut out, FRAME));
        assert_eq!(out, vec![0.0; FRAME]);
        assert_eq!(metrics.underruns.dropped(), 0);
    }

    #[test]
    fn jitter_buffer_preserves_frame_order() {
        let (mut buffer, _, mut out) = primed(-1.0);
        for i in 0..5 {
            buffer.push(&constant(i as f32));
        }
        // Drain everything queued in one request, then collapse each constant
        // run back to its value. Draining in a single shot keeps the depth out
        // of the drift band entirely, so nothing perturbs the frame boundaries
        // and the only thing under test is the order audio comes out in.
        let queued = buffer.len();
        assert!(buffer.fill(&mut out, queued));
        let mut runs = Vec::new();
        for sample in &out {
            if runs.last() != Some(sample) {
                runs.push(*sample);
            }
        }
        assert_eq!(runs, vec![-1.0, 0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn jitter_buffer_counts_a_real_underrun_and_pads_the_tail() {
        let (mut buffer, metrics, mut out) = primed(1.0);
        // Five frames remain; ask for eight, so the last three must be silence.
        assert!(buffer.fill(&mut out, FRAME * 8));
        assert!(out[..TARGET].iter().all(|sample| *sample == 1.0));
        assert!(out[TARGET..].iter().all(|sample| *sample == 0.0));
        assert_eq!(metrics.underruns.dropped(), 1);
        assert_eq!(metrics.underruns.frames(), 2);
    }

    #[test]
    fn jitter_buffer_discards_the_oldest_audio_on_overflow() {
        let (mut buffer, metrics, mut out) = primed(-1.0);
        for i in 0..40 {
            buffer.push(&constant(i as f32));
        }
        assert!(metrics.overruns.dropped() > 0);
        assert!(buffer.len() <= MAX);
        buffer.fill(&mut out, buffer.len());
        // The newest frame must survive...
        assert_eq!(*out.last().expect("some audio retained"), 39.0);
        // ...and the oldest must not: an overflow should delay playback, never
        // rewind it to stale audio.
        assert!(
            *out.first().expect("some audio retained") > 24.0,
            "oldest audio survived the overflow"
        );
    }

    /// The reported long-session failure, reproduced in miniature: a sender
    /// whose clock runs slightly slow, so the buffer drains by a fraction of a
    /// sample per period. Uncorrected it walks out of the safe band within
    /// seconds of audio and then every period is padded with silence. Corrected,
    /// the level settles inside the band and no period is ever silent.
    #[test]
    fn jitter_buffer_corrects_a_slow_clock_without_a_single_underrun() {
        let (mut buffer, metrics, mut out) = primed(1.0);
        // 0.999x: two samples short per 20 ms period, about 43 ms of buffer per
        // second of audio.
        let produced = (FRAME as f64 * 0.999) as usize;
        for _ in 0..1500 {
            buffer.push(&vec![1.0f32; produced]);
            buffer.fill(&mut out, FRAME);
        }
        assert!(metrics.drift_samples.get() > 0, "no correction was applied");
        assert_eq!(metrics.underruns.dropped(), 0);
        assert_eq!(metrics.overruns.dropped(), 0);
        // The depth must stay in the band, not merely stay non-empty: settling
        // anywhere lower is the same failure one period away from audible.
        assert!(
            (LOW..=HIGH).contains(&buffer.len()),
            "level {} outside {LOW}..={HIGH}",
            buffer.len()
        );
    }

    /// The mirror case: a sender whose clock runs fast must not grow without
    /// bound, and must not need to discard a single sample at the ceiling.
    #[test]
    fn jitter_buffer_corrects_a_fast_clock_without_overflowing() {
        let (mut buffer, metrics, mut out) = primed(1.0);
        // 1.001x: two samples per period too many.
        let produced = (FRAME as f64 * 1.001) as usize;
        for _ in 0..1500 {
            buffer.push(&vec![1.0f32; produced]);
            buffer.fill(&mut out, FRAME);
        }
        assert!(metrics.drift_samples.get() > 0, "no correction was applied");
        assert_eq!(metrics.overruns.dropped(), 0);
        assert_eq!(metrics.underruns.dropped(), 0);
        assert!(
            (LOW..=HIGH).contains(&buffer.len()),
            "level {} outside {LOW}..={HIGH}",
            buffer.len()
        );
    }

    /// Jitter in either direction must not cost more than it has to: the buffer
    /// should absorb a burst, not turn it into silence.
    #[test]
    fn jitter_buffer_absorbs_a_network_burst() {
        let (mut buffer, metrics, mut out) = primed(1.0);
        // Five periods arrive at once.
        for _ in 0..5 {
            buffer.push(&constant(1.0));
        }
        for _ in 0..10 {
            assert!(buffer.fill(&mut out, FRAME));
            assert!(out.iter().all(|s| *s == 1.0), "burst produced silence");
        }
        assert_eq!(metrics.underruns.dropped(), 0);
    }

    /// A one-off glitch should not move the level: the streak requirement is
    /// what separates a late packet from a genuine clock offset.
    #[test]
    fn jitter_buffer_ignores_a_single_late_period() {
        let (mut buffer, metrics, mut out) = primed(1.0);
        for _ in 0..4 {
            // Consume without replenishing: four periods below the band.
            buffer.fill(&mut out, FRAME);
        }
        assert_eq!(metrics.drift_samples.get(), 0, "corrected too eagerly");
        buffer.push(&constant(1.0));
        buffer.fill(&mut out, FRAME);
        assert_eq!(metrics.underruns.dropped(), 0);
    }
}
