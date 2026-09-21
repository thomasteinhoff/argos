use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

pub struct AudioCapture {
    rx: Receiver<Vec<f32>>,
    errors: Receiver<String>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let (error_tx, error_rx) = sync_channel::<String>(8);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let join = thread::Builder::new()
            .name("argos-audio-capture".to_string())
            .spawn(move || capture_loop(tx, error_tx, thread_stop, ready_tx))
            .map_err(|error| error.to_string())?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx,
                errors: error_rx,
                stop,
                join: Some(join),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("audio capture thread exited early".to_string()),
        }
    }

    pub fn try_frame(&self) -> Option<Vec<f32>> {
        self.rx.try_recv().ok()
    }

    /// Surface non-fatal capture problems (e.g. Discord exclusion falling back to full loopback).
    pub fn try_error(&self) -> Option<String> {
        self.errors.try_recv().ok()
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
    errors: SyncSender<String>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    if let Err(error) = initialize_mta().ok() {
        let _ = ready.send(Err(format!("COM init failed: {error}")));
        return;
    }
    if let Err(error) = run_capture(&tx, &errors, &stop, &ready) {
        let _ = ready.send(Err(error));
    }
}

fn run_capture(
    tx: &SyncSender<Vec<f32>>,
    errors: &SyncSender<String>,
    stop: &AtomicBool,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
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
    let mut ready_sent = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Pick what to exclude: the root Discord process when it is running.
        // A scan failure is non-fatal: fall back to a normal full loopback.
        let target = match find_discord_root_pid() {
            Ok(pid) => pid,
            Err(error) => {
                let _ = errors.try_send(format!("Discord detection failed: {error}"));
                None
            }
        };
        if target != exclusion_target {
            if let Some(pid) = target {
                let _ = errors.try_send(format!(
                    "Discord audio excluded from the stream (pid {pid})"
                ));
            } else {
                let _ =
                    errors.try_send("Discord not running: sharing full system audio".to_string());
            }
        }
        exclusion_target = target;

        // Build a loopback client. With a Discord pid available we ask WASAPI for a
        // process-loopback stream in EXCLUDE mode (everything except Discord's tree);
        // otherwise we capture the plain device mix.
        let (mut audio_client, buffer_duration_hns) = match build_capture_client(target, errors) {
            Ok(pair) => pair,
            Err(error) => {
                let _ = errors.try_send(error);
                return Err("audio capture failed".to_string());
            }
        };

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
        if !ready_sent {
            ready_sent = true;
            let _ = ready.send(Ok(()));
        }

        let mut bytes: VecDeque<u8> = VecDeque::with_capacity(frame_bytes * 4);
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
                        let _ = errors.try_send(format!("Discord detection failed: {error}"));
                    }
                }
            }

            if let Err(error) = capture_client.read_from_device_to_deque(&mut bytes) {
                let _ = errors.try_send(format!("audio capture read failed: {error}"));
                rebuild = true;
                break;
            }
            while bytes.len() >= frame_bytes {
                let mut samples = Vec::with_capacity(FRAME_SAMPLES * CHANNELS);
                for _ in 0..FRAME_SAMPLES * CHANNELS {
                    let b0 = bytes.pop_front().unwrap_or(0);
                    let b1 = bytes.pop_front().unwrap_or(0);
                    let b2 = bytes.pop_front().unwrap_or(0);
                    let b3 = bytes.pop_front().unwrap_or(0);
                    samples.push(f32::from_le_bytes([b0, b1, b2, b3]));
                }
                let _ = tx.try_send(samples);
            }
            if event.wait_for_event(1_000_000).is_err() {
                rebuild = true;
                break;
            }
        }

        let _ = audio_client.stop_stream();
        if !rebuild {
            return Ok(());
        }
    }
}

fn build_capture_client(
    target: Option<u32>,
    errors: &SyncSender<String>,
) -> Result<(AudioClient, i64), String> {
    if let Some(pid) = target {
        match AudioClient::new_application_loopback_client(pid, false) {
            Ok(client) => return Ok((client, PROCESS_LOOPBACK_PERIOD_HNS)),
            Err(error) => {
                let _ = errors.try_send(format!(
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
    join: Option<JoinHandle<()>>,
}

impl AudioPlayback {
    pub fn start(volume: f32) -> Result<Self, String> {
        let (tx, rx) = sync_channel::<Vec<f32>>(32);
        let stop = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_muted = muted.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let join = thread::Builder::new()
            .name("argos-audio-playback".to_string())
            .spawn(move || playback_loop(rx, thread_stop, thread_muted, ready_tx, volume))
            .map_err(|error| error.to_string())?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                stop,
                muted,
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
    ready: std::sync::mpsc::Sender<Result<(), String>>,
    volume: f32,
) {
    if let Err(error) = initialize_mta().ok() {
        let _ = ready.send(Err(format!("COM init failed: {error}")));
        return;
    }
    if let Err(error) = run_playback(&rx, &stop, &muted, volume, &ready) {
        let _ = ready.send(Err(error));
    }
}

fn run_playback(
    rx: &Receiver<Vec<f32>>,
    stop: &AtomicBool,
    muted: &AtomicBool,
    volume: f32,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
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
    let _ = ready.send(Ok(()));

    let mut queue: VecDeque<u8> = VecDeque::new();

    while !stop.load(Ordering::Relaxed) {
        let available = audio_client
            .get_available_space_in_frames()
            .map_err(|error| error.to_string())? as usize;
        let needed = available * BLOCK_ALIGN;
        while queue.len() < needed {
            match rx.try_recv() {
                Ok(frame) => {
                    let gain = if muted.load(Ordering::Relaxed) {
                        0.0
                    } else {
                        volume
                    };
                    for sample in frame {
                        queue.extend((sample * gain).to_le_bytes());
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    while queue.len() < needed {
                        queue.extend(0f32.to_le_bytes());
                    }
                }
            }
        }
        render_client
            .write_to_device_from_deque(available, &mut queue, None)
            .map_err(|error| error.to_string())?;
        if event.wait_for_event(1_000_000).is_err() {
            break;
        }
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::select_discord_root;

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
}
