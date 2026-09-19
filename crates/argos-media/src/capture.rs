use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use xcap::Monitor;

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
    join: Option<JoinHandle<()>>,
}

impl CaptureSession {
    pub fn new() -> Self {
        let (_tx, rx) = channel();
        Self {
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            join: None,
        }
    }

    pub fn start(&mut self, source: &MonitorInfo) -> Result<(), String> {
        if self.join.is_some() {
            return Err("capture already running".to_string());
        }
        let wanted = source.name.clone();
        let (tx, rx) = sync_channel::<Frame>(1);
        let stop = self.stop.clone();
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
            pump(recorder, frames, tx, stop);
        }));
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

fn pump(
    recorder: xcap::VideoRecorder,
    frames: Receiver<xcap::Frame>,
    tx: SyncSender<Frame>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let Ok(frame) = frames.recv() else {
            break;
        };
        let outgoing = Frame {
            width: frame.width,
            height: frame.height,
            rgba: frame.raw,
        };
        match tx.try_send(outgoing) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {}
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
        }
    }
    let _ = recorder.stop();
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