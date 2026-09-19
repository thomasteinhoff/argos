use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use opus_rs::{Application, OpusDecoder, OpusEncoder};
use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_SAMPLES: usize = 960;
const BYTES_PER_SAMPLE: usize = 4;
const BLOCK_ALIGN: usize = CHANNELS * BYTES_PER_SAMPLE;

pub struct AudioCapture {
    rx: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let join = thread::Builder::new()
            .name("argos-audio-capture".to_string())
            .spawn(move || capture_loop(tx, thread_stop, ready_tx))
            .map_err(|error| error.to_string())?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx,
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
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    if let Err(error) = initialize_mta().ok() {
        let _ = ready.send(Err(format!("COM init failed: {error}")));
        return;
    }
    if let Err(error) = run_capture(&tx, &stop, &ready) {
        let _ = ready.send(Err(error));
    }
}

fn run_capture(
    tx: &SyncSender<Vec<f32>>,
    stop: &AtomicBool,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let enumerator = DeviceEnumerator::new().map_err(|error| error.to_string())?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|error| format!("no default output device: {error}"))?;
    let mut audio_client = device
        .get_iaudioclient()
        .map_err(|error| error.to_string())?;

    let format = WaveFormat::new(
        (BYTES_PER_SAMPLE * 8) as usize,
        (BYTES_PER_SAMPLE * 8) as usize,
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
    let _ = ready.send(Ok(()));

    let frame_bytes = FRAME_SAMPLES * BLOCK_ALIGN;
    let mut bytes: VecDeque<u8> = VecDeque::with_capacity(frame_bytes * 4);

    while !stop.load(Ordering::Relaxed) {
        capture_client
            .read_from_device_to_deque(&mut bytes)
            .map_err(|error| error.to_string())?;
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
            break;
        }
    }

    let _ = audio_client.stop_stream();
    Ok(())
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
    join: Option<JoinHandle<()>>,
}

impl AudioPlayback {
    pub fn start(volume: f32) -> Result<Self, String> {
        let (tx, rx) = sync_channel::<Vec<f32>>(32);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let join = thread::Builder::new()
            .name("argos-audio-playback".to_string())
            .spawn(move || playback_loop(rx, thread_stop, ready_tx, volume))
            .map_err(|error| error.to_string())?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                stop,
                join: Some(join),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("audio playback thread exited early".to_string()),
        }
    }

    pub fn push(&self, frame: Vec<f32>) {
        let _ = self.tx.try_send(frame);
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
    ready: std::sync::mpsc::Sender<Result<(), String>>,
    volume: f32,
) {
    if let Err(error) = initialize_mta().ok() {
        let _ = ready.send(Err(format!("COM init failed: {error}")));
        return;
    }
    if let Err(error) = run_playback(&rx, &stop, volume, &ready) {
        let _ = ready.send(Err(error));
    }
}

fn run_playback(
    rx: &Receiver<Vec<f32>>,
    stop: &AtomicBool,
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
        (BYTES_PER_SAMPLE * 8) as usize,
        (BYTES_PER_SAMPLE * 8) as usize,
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
                    for sample in frame {
                        queue.extend((sample * volume).to_le_bytes());
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
