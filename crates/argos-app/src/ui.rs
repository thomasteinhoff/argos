use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use argos_core::{h264, lan, session, Packet};
use argos_media::audio::{
    AudioCapture, AudioPlayback, OpusAudioDecoder, OpusAudioEncoder, FRAME_SAMPLES,
};
use argos_media::capture::{self, CaptureSession, MonitorInfo};
use argos_media::decode::{DecodedFrame, H264Decoder};
use argos_media::encode::H264Encoder;

use crate::config::{self, AppConfig};

#[derive(Clone, PartialEq, Eq)]
enum Screen {
    Home,
    Share,
    Peer(String),
    View,
}

struct Preview {
    texture: egui::TextureHandle,
    width: u32,
    height: u32,
}

enum EncodeMsg {
    Frame {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    },
    Quality {
        height: Option<u32>,
    },
}

#[derive(Default)]
struct EncodeStats {
    frames_sent: u64,
    encode_ms: f32,
    encode_errors: u64,
    error: Option<String>,
}

fn encode_worker(
    rx: Receiver<EncodeMsg>,
    sharer: Arc<session::Sharer>,
    mut encoder: H264Encoder,
    stats: Arc<Mutex<EncodeStats>>,
    frame_rate: u32,
) {
    // Video RTP uses a 90 kHz clock: each frame advances the RTP timestamp by
    // 90_000 / fps (3000 at 30 FPS, 1500 at 60 FPS).
    let timestamp_interval = h264::timestamp_interval(frame_rate);
    let mut timestamp = 0u32;
    let mut last_keyframe = Instant::now();
    while let Ok(msg) = rx.recv() {
        match msg {
            EncodeMsg::Quality { height } => {
                encoder.set_target_height(height);
                encoder.force_keyframe();
            }
            EncodeMsg::Frame {
                rgba,
                width,
                height,
            } => {
                // Force a keyframe periodically so receivers can re-sync after
                // packet loss without relying on RTCP keyframe requests.
                if last_keyframe.elapsed() >= Duration::from_secs(2) {
                    encoder.force_keyframe();
                    last_keyframe = Instant::now();
                }
                let started = Instant::now();
                match encoder.encode(&rgba, width, height) {
                    Ok(bitstream) => {
                        let ms = started.elapsed().as_secs_f32() * 1000.0;
                        let ts = timestamp;
                        timestamp = ts.wrapping_add(timestamp_interval);
                        if let Ok(mut stats) = stats.lock() {
                            stats.encode_ms = if stats.frames_sent == 0 {
                                ms
                            } else {
                                stats.encode_ms * 0.8 + ms * 0.2
                            };
                        }
                        match session::block_on(sharer.send_frame(&bitstream, ts)) {
                            Ok(()) => {
                                if let Ok(mut stats) = stats.lock() {
                                    stats.frames_sent += 1;
                                }
                            }
                            Err(error) => {
                                if let Ok(mut stats) = stats.lock() {
                                    if stats.error.is_none() {
                                        stats.error = Some(error);
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if let Ok(mut stats) = stats.lock() {
                            stats.encode_errors += 1;
                            if stats.error.is_none() {
                                stats.error = Some(error);
                            }
                        }
                    }
                }
            }
        }
    }
}

struct ShareSession {
    sharer: Arc<session::Sharer>,
    offer_code: Option<String>,
    answer_input: String,
    error: Option<String>,
    tx: SyncSender<EncodeMsg>,
    join: Option<JoinHandle<()>>,
    stats: Arc<Mutex<EncodeStats>>,
    last_encode: Instant,
    audio_capture: Option<AudioCapture>,
    audio_encoder: Option<OpusAudioEncoder>,
    audio_error: Option<String>,
    audio_timestamp: u32,
    audio_frames_sent: u64,
    lan_peer: Option<String>,
}

#[derive(Default)]
struct ViewStats {
    packets: u64,
    bytes: u64,
    aus: u64,
    decoded: u64,
    decode_errors: u64,
    decode_none: u64,
    first_width: u32,
    first_height: u32,
    first_error: Option<String>,
    audio_packets: u64,
    audio_decoded: u64,
    audio_errors: u64,
    audio_error: Option<String>,
    base_seq: Option<u16>,
    highest_seq: Option<u16>,
    lost: u64,
}

struct Quality {
    last: Instant,
    last_decoded: u64,
    last_bytes: u64,
    last_lost: u64,
    last_received: u64,
    fps: f32,
    mbps: f32,
    loss: f32,
}

impl Default for Quality {
    fn default() -> Self {
        Self {
            last: Instant::now(),
            last_decoded: 0,
            last_bytes: 0,
            last_lost: 0,
            last_received: 0,
            fps: 0.0,
            mbps: 0.0,
            loss: 0.0,
        }
    }
}

struct Reconnect {
    attempts: u32,
    last_request: Instant,
}

struct ViewSession {
    viewer: Arc<session::Viewer>,
    answer_code: Option<String>,
    error: Option<String>,
    latest: Arc<Mutex<Option<DecodedFrame>>>,
    stats: Arc<Mutex<ViewStats>>,
    texture: Option<Preview>,
    audio_playback: Option<Arc<AudioPlayback>>,
    audio_error: Option<String>,
    quality: Quality,
    was_connected: bool,
    reconnect: Option<Reconnect>,
    lan_peer: Option<String>,
    peer_id: Option<String>,
}

pub struct ArgosApp {
    config: AppConfig,
    screen: Screen,
    show_settings: bool,
    name_input: String,
    code_input: String,
    monitors: Vec<MonitorInfo>,
    selected_monitor: usize,
    capture: Option<CaptureSession>,
    preview: Option<Preview>,
    preview_active: bool,
    fps_frames: u64,
    fps_last: Instant,
    fps: f32,
    preview_error: Option<String>,
    share: Option<ShareSession>,
    share_error: Option<String>,
    share_height: Option<u32>,
    frame_rate: u32,
    live: bool,
    view: Option<ViewSession>,
    view_error: Option<String>,
    lan: Option<lan::Lan>,
    lan_error: Option<String>,
    pending_view: Option<String>,
    radmin_exe: Option<std::path::PathBuf>,
    radmin_error: Option<String>,
}

impl ArgosApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config = config::load();
        let (lan, lan_error) = match lan::Lan::start(config.name.clone()) {
            Ok(lan) => (Some(lan), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            name_input: config.name.clone(),
            code_input: String::new(),
            monitors: Vec::new(),
            selected_monitor: 0,
            capture: None,
            preview: None,
            preview_active: false,
            fps_frames: 0,
            fps_last: Instant::now(),
            fps: 0.0,
            preview_error: None,
            share: None,
            share_error: None,
            share_height: Some(720),
            frame_rate: 30,
            live: false,
            view: None,
            view_error: None,
            lan,
            lan_error,
            pending_view: None,
            radmin_exe: crate::radmin::find_exe(),
            radmin_error: None,
            config,
            screen: Screen::Home,
            show_settings: false,
        }
    }

    fn profile_label(&self) -> String {
        if self.config.name.is_empty() {
            "Set your name".to_owned()
        } else {
            self.config.name.clone()
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("ARGOS").strong().size(18.0))
                    .on_hover_text("Home")
                    .clicked()
                {
                    self.screen = Screen::Home;
                }
                ui.separator();
                if ui
                    .button(
                        RichText::new(self.profile_label()).color(Color32::from_rgb(160, 190, 255)),
                    )
                    .on_hover_text("Your name, as friends will see it")
                    .clicked()
                {
                    self.name_input.clone_from(&self.config.name);
                    self.show_settings = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(self.status_text()).color(Color32::GRAY));
                });
            });
        });
    }

    fn side_bar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("side_panel")
            .resizable(false)
            .default_width(240.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                let selected = self.screen == Screen::Share;
                if ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::Button::selectable(selected, RichText::new("Share").strong()),
                    )
                    .clicked()
                {
                    self.screen = Screen::Share;
                }
                ui.separator();
                self.sidebar(ui);
            });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Profile")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(300.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("How should friends see you?");
                ui.add(
                    egui::TextEdit::singleline(&mut self.name_input)
                        .hint_text("Your name")
                        .desired_width(260.0),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        let name = self.name_input.trim().to_string();
                        self.config.name = name.clone();
                        config::save(&self.config);
                        if let Some(lan) = &self.lan {
                            lan.set_name(name);
                        }
                        self.show_settings = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_settings = false;
                    }
                });
            });
        self.show_settings = open;
    }

    fn refresh_monitors(&mut self) {
        self.monitors = capture::list_monitors();
        if !self.monitors.is_empty() {
            self.selected_monitor = self.selected_monitor.min(self.monitors.len() - 1);
        }
    }

    fn monitor_label(info: &MonitorInfo) -> String {
        let primary = if info.is_primary { " [primary]" } else { "" };
        format!("{} ({}x{}){}", info.name, info.width, info.height, primary)
    }

    fn quality_label(height: Option<u32>) -> &'static str {
        match height {
            Some(720) => "720p",
            Some(540) => "540p",
            Some(360) => "360p",
            _ => "Native",
        }
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.frame_rate.max(1) as u64)
    }

    fn start_capture(&mut self) -> Result<Option<MonitorInfo>, String> {
        let Some(source) = self.monitors.get(self.selected_monitor) else {
            return Ok(None);
        };
        let mut session = CaptureSession::new();
        session.set_interval(self.frame_interval());
        session.start(source)?;
        self.fps_last = Instant::now();
        self.fps_frames = 0;
        self.fps = 0.0;
        self.capture = Some(session);
        Ok(Some(source.clone()))
    }

    fn toggle_preview(&mut self) {
        self.preview_error = None;
        if self.preview_active {
            self.preview_active = false;
            self.preview = None;
            return;
        }
        if self.capture.is_none() {
            if let Err(error) = self.start_capture() {
                self.preview_error = Some(error);
                return;
            }
        }
        self.preview_active = true;
    }

    fn code_widget(ui: &mut egui::Ui, code: &str, rows: usize) {
        let mut display = code.to_owned();
        ui.add(
            egui::TextEdit::multiline(&mut display)
                .code_editor()
                .desired_width(380.0)
                .desired_rows(rows),
        );
    }

    fn attach_sharer(&mut self, lan_peer: Option<String>) {
        self.share_error = None;
        if let Some(mut existing) = self.share.take() {
            let sharer = Arc::clone(&existing.sharer);
            let join = existing.join.take();
            drop(existing);
            if let Some(join) = join {
                let _ = join.join();
            }
            session::block_on(sharer.close());
        }
        if self.capture.is_none() {
            if let Err(error) = self.start_capture() {
                self.share_error = Some(error);
                return;
            }
        }
        let udp = vec!["0.0.0.0:0".to_string()];
        let sharer = match session::block_on(session::Sharer::new(udp)) {
            Ok(sharer) => Arc::new(sharer),
            Err(error) => {
                self.share_error = Some(error);
                return;
            }
        };
        let offer = match session::block_on(sharer.create_offer()) {
            Ok(code) => code,
            Err(error) => {
                self.share_error = Some(error);
                return;
            }
        };
        let offer_code = match &lan_peer {
            Some(peer) => {
                if let Some(lan) = &self.lan {
                    lan.send_offer(peer, offer);
                }
                None
            }
            None => Some(offer),
        };
        let encoder_result = H264Encoder::new_at(self.frame_rate as f32).map(|mut encoder| {
            encoder.set_target_height(self.share_height);
            encoder
        });
        let encoder = match encoder_result {
            Ok(encoder) => encoder,
            Err(error) => {
                session::block_on(sharer.close());
                self.share_error = Some(error);
                return;
            }
        };
        let (tx, rx) = sync_channel::<EncodeMsg>(4);
        let stats = Arc::new(Mutex::new(EncodeStats::default()));
        let worker_stats = Arc::clone(&stats);
        let worker_sharer = Arc::clone(&sharer);
        let worker_fps = self.frame_rate;
        let join = match thread::Builder::new()
            .name("argos-encode".to_string())
            .spawn(move || encode_worker(rx, worker_sharer, encoder, worker_stats, worker_fps))
        {
            Ok(join) => join,
            Err(error) => {
                session::block_on(sharer.close());
                self.share_error = Some(error.to_string());
                return;
            }
        };
        let (audio_capture, audio_encoder, audio_error) =
            match (AudioCapture::start(), OpusAudioEncoder::new()) {
                (Ok(capture), Ok(encoder)) => (Some(capture), Some(encoder), None),
                (capture, encoder) => {
                    let error = capture.err().or_else(|| encoder.err());
                    (None, None, error)
                }
            };
        if lan_peer.is_some() {
            if let Some(lan) = &self.lan {
                lan.set_sharing(true);
            }
        }
        self.share = Some(ShareSession {
            sharer,
            offer_code,
            answer_input: String::new(),
            error: None,
            tx,
            join: Some(join),
            stats,
            last_encode: Instant::now(),
            audio_capture,
            audio_encoder,
            audio_error,
            audio_timestamp: 0,
            audio_frames_sent: 0,
            lan_peer,
        });
    }

    fn go_live(&mut self) {
        self.share_error = None;
        if self.capture.is_none() {
            if let Err(error) = self.start_capture() {
                self.share_error = Some(error);
                return;
            }
        }
        self.live = true;
        if let Some(lan) = &self.lan {
            lan.set_sharing(true);
        }
    }

    fn stop_live(&mut self) {
        if let Some(mut share) = self.share.take() {
            let sharer = Arc::clone(&share.sharer);
            let join = share.join.take();
            drop(share);
            if let Some(join) = join {
                let _ = join.join();
            }
            session::block_on(sharer.close());
        }
        self.live = false;
        if let Some(lan) = &self.lan {
            lan.set_sharing(false);
        }
        self.share_error = None;
    }

    fn create_manual_offer(&mut self) {
        self.attach_sharer(None);
    }

    fn update_capture_active(&mut self) -> bool {
        let active = self.preview_active
            || self
                .share
                .as_ref()
                .is_some_and(|share| share.sharer.is_connected());
        if let Some(capture) = &self.capture {
            capture.set_active(active);
        }
        if !active {
            self.preview = None;
            self.fps = 0.0;
        }
        active
    }

    fn accept_answer(share: &mut ShareSession) {
        let code = share.answer_input.trim().to_string();
        if code.is_empty() {
            share.error = Some("paste the viewer's answer code first".to_string());
            return;
        }
        match session::block_on(share.sharer.set_answer(&code)) {
            Ok(()) => share.error = None,
            Err(error) => share.error = Some(error),
        }
    }

    fn start_view(&mut self, offer: String, lan_peer: Option<String>) {
        self.view_error = None;
        if offer.is_empty() {
            self.view_error = Some("paste a connection code from the sharer first".to_string());
            return;
        }
        if let Some(existing) = self.view.take() {
            session::block_on(existing.viewer.close());
        }
        let latest = Arc::new(Mutex::new(None));
        let stats = Arc::new(Mutex::new(ViewStats::default()));
        let (audio_playback, audio_error) = match AudioPlayback::start(1.0) {
            Ok(playback) => (Some(Arc::new(playback)), None),
            Err(error) => (None, Some(error)),
        };
        let callback: Arc<dyn Fn(&Packet) + Send + Sync> = Arc::new(Self::receive_callback(
            Arc::clone(&latest),
            Arc::clone(&stats),
            audio_playback.clone(),
        ));
        let udp = vec!["0.0.0.0:0".to_string()];
        let viewer = match session::block_on(session::Viewer::new(udp, callback)) {
            Ok(viewer) => Arc::new(viewer),
            Err(error) => {
                self.view_error = Some(error);
                return;
            }
        };
        let answer = match session::block_on(viewer.answer_offer(&offer)) {
            Ok(code) => code,
            Err(error) => {
                self.view_error = Some(error);
                return;
            }
        };
        let answer_code = match &lan_peer {
            Some(peer) => {
                if let Some(lan) = &self.lan {
                    lan.send_answer(peer, answer);
                }
                None
            }
            None => Some(answer),
        };
        let peer_id = lan_peer.clone();
        self.view = Some(ViewSession {
            viewer,
            answer_code,
            error: None,
            latest,
            stats,
            texture: None,
            audio_playback,
            audio_error,
            quality: Quality::default(),
            was_connected: false,
            reconnect: None,
            lan_peer,
            peer_id,
        });
    }

    fn request_view(&mut self, id: &str) {
        self.view_error = None;
        if let Some(lan) = &self.lan {
            lan.send_request(id, &self.config.name);
        }
        self.pending_view = Some(id.to_string());
    }

    fn stop_view(&mut self) {
        if let Some(view) = self.view.take() {
            session::block_on(view.viewer.close());
        }
        self.pending_view = None;
        self.view_error = None;
        self.screen = Screen::Home;
    }

    fn view_visible(&self) -> bool {
        match &self.screen {
            Screen::Peer(id) => {
                self.view.as_ref().and_then(|view| view.peer_id.as_deref()) == Some(id.as_str())
            }
            Screen::View => self.view.is_some(),
            _ => false,
        }
    }

    fn poll_lan_events(&mut self) {
        let Some(lan) = self.lan.as_ref() else {
            return;
        };
        let mut events = Vec::new();
        while let Some(event) = lan.try_event() {
            events.push(event);
        }
        for event in events {
            match event {
                lan::LanEvent::Request { id, .. } => {
                    if !self.live {
                        continue;
                    }
                    let busy = self
                        .share
                        .as_ref()
                        .map(|share| share.sharer.is_connected())
                        .unwrap_or(false);
                    if !busy {
                        self.attach_sharer(Some(id));
                    }
                }
                lan::LanEvent::Offer { id, sdp } => {
                    if self.pending_view.as_deref() == Some(id.as_str()) {
                        self.pending_view = None;
                        self.start_view(sdp, Some(id));
                    }
                }
                lan::LanEvent::Answer { id, sdp } => {
                    if let Some(share) = self.share.as_mut() {
                        if share.lan_peer.as_deref() == Some(id.as_str()) {
                            match session::block_on(share.sharer.set_answer(&sdp)) {
                                Ok(()) => share.error = None,
                                Err(error) => share.error = Some(error),
                            }
                        }
                    }
                }
            }
        }
    }

    fn receive_callback(
        latest: Arc<Mutex<Option<DecodedFrame>>>,
        stats: Arc<Mutex<ViewStats>>,
        audio_playback: Option<Arc<AudioPlayback>>,
    ) -> impl Fn(&Packet) + Send + Sync {
        struct Pipeline {
            depacketizer: h264::Depacketizer,
            decoder: Option<H264Decoder>,
            audio_decoder: Option<OpusAudioDecoder>,
        }
        let pipeline = Arc::new(Mutex::new(Pipeline {
            depacketizer: h264::Depacketizer::new(),
            decoder: H264Decoder::new().ok(),
            audio_decoder: OpusAudioDecoder::new().ok(),
        }));
        move |packet: &Packet| {
            {
                let Ok(mut stats) = stats.lock() else {
                    return;
                };
                stats.packets += 1;
            }
            if packet.header.payload_type == session::AUDIO_PT {
                if let Ok(mut stats) = stats.lock() {
                    stats.audio_packets += 1;
                }
                let Some(playback) = &audio_playback else {
                    return;
                };
                let Ok(mut pipeline) = pipeline.lock() else {
                    return;
                };
                let Some(decoder) = pipeline.audio_decoder.as_mut() else {
                    return;
                };
                match decoder.decode(&packet.payload) {
                    Ok(samples) => {
                        playback.push(samples);
                        if let Ok(mut stats) = stats.lock() {
                            stats.audio_decoded += 1;
                        }
                    }
                    Err(error) => {
                        if let Ok(mut stats) = stats.lock() {
                            if stats.audio_error.is_none() {
                                stats.audio_error = Some(error);
                            }
                            stats.audio_errors += 1;
                        }
                    }
                }
                return;
            }
            {
                let Ok(mut stats) = stats.lock() else {
                    return;
                };
                stats.bytes += packet.payload.len() as u64;
                let seq = packet.header.sequence_number;
                match stats.highest_seq {
                    None => {
                        stats.base_seq = Some(seq);
                        stats.highest_seq = Some(seq);
                    }
                    Some(highest) => {
                        if seq != highest && seq.wrapping_sub(highest) < 0x8000 {
                            stats.lost += seq.wrapping_sub(highest) as u64 - 1;
                            stats.highest_seq = Some(seq);
                        }
                    }
                }
            }
            let Ok(mut pipeline) = pipeline.lock() else {
                return;
            };
            let Some(nalus) = pipeline.depacketizer.push(packet) else {
                return;
            };
            {
                let Ok(mut stats) = stats.lock() else {
                    return;
                };
                stats.aus += 1;
            }
            let Some(decoder) = pipeline.decoder.as_mut() else {
                return;
            };
            let access_unit = h264::access_unit_to_annexb(&nalus);
            match decoder.decode(&access_unit) {
                Ok(Some(frame)) => {
                    if let Ok(mut stats) = stats.lock() {
                        if stats.first_width == 0 {
                            stats.first_width = frame.width;
                            stats.first_height = frame.height;
                        }
                        stats.decoded += 1;
                    }
                    if let Ok(mut slot) = latest.lock() {
                        *slot = Some(frame);
                    }
                }
                Ok(None) => {
                    if let Ok(mut stats) = stats.lock() {
                        stats.decode_none += 1;
                    }
                }
                Err(error) => {
                    // The decoder lost sync (e.g. a keyframe was lost): drop
                    // everything until the next keyframe rather than feeding
                    // it error-prone frames; the depacketizer's sync gate
                    // handles that once reset.
                    pipeline.depacketizer.reset();
                    if let Ok(mut stats) = stats.lock() {
                        if stats.first_error.is_none() {
                            stats.first_error = Some(error);
                        }
                        stats.decode_errors += 1;
                    }
                }
            }
        }
    }

    fn poll_capture(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let interval = self.frame_interval();
        let encode_due = self.share.as_ref().is_some_and(|share| {
            share.sharer.is_connected() && now.duration_since(share.last_encode) >= interval
        });
        if !self.preview_active && !encode_due {
            return;
        }
        // Surface capture-side failures (recoverable retries are kept internal
        // to the capture thread) even when no frame is pending right now.
        let capture_error = self.capture.as_ref().and_then(CaptureSession::error);
        let frame = self.capture.as_ref().and_then(CaptureSession::latest);
        if let Some(error) = capture_error {
            if self.preview_active && self.preview_error.is_none() {
                self.preview_error = Some(error.clone());
            }
            if let Some(share) = self.share.as_mut() {
                if share.error.is_none() {
                    share.error = Some(error);
                }
            }
        }
        let Some(frame) = frame else {
            return;
        };
        if self.preview_active {
            self.fps_frames += 1;
            let elapsed = now.duration_since(self.fps_last);
            if elapsed >= Duration::from_secs(1) {
                self.fps = self.fps_frames as f32 / elapsed.as_secs_f32();
                self.fps_frames = 0;
                self.fps_last = now;
            }
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            match self.preview.as_mut() {
                Some(preview) if preview.width == frame.width && preview.height == frame.height => {
                    preview.texture.set(image, egui::TextureOptions::LINEAR);
                }
                _ => {
                    self.preview = Some(Preview {
                        texture: ctx.load_texture("preview", image, egui::TextureOptions::LINEAR),
                        width: frame.width,
                        height: frame.height,
                    });
                }
            }
        }
        if let Some(share) = self.share.as_mut() {
            if share.sharer.is_connected() && now.duration_since(share.last_encode) >= interval {
                share.last_encode = now;
                if let Ok(mut stats) = share.stats.lock() {
                    if let Some(error) = stats.error.take() {
                        share.error = Some(error);
                    }
                }
                let msg = EncodeMsg::Frame {
                    rgba: frame.rgba,
                    width: frame.width,
                    height: frame.height,
                };
                let _ = share.tx.try_send(msg);
            }
        }
    }

    fn poll_share_audio(&mut self) {
        let Some(share) = self.share.as_mut() else {
            return;
        };
        if !share.sharer.is_connected() {
            return;
        }
        while let Some(frame) = share
            .audio_capture
            .as_ref()
            .and_then(AudioCapture::try_frame)
        {
            let Some(encoder) = share.audio_encoder.as_mut() else {
                break;
            };
            let packet = match encoder.encode(&frame) {
                Ok(packet) => packet,
                Err(error) => {
                    share.audio_error = Some(error);
                    break;
                }
            };
            let timestamp = share.audio_timestamp;
            share.audio_timestamp = timestamp.wrapping_add(FRAME_SAMPLES as u32);
            let sharer = Arc::clone(&share.sharer);
            if let Err(error) = session::block_on(sharer.send_audio(&packet, timestamp)) {
                share.audio_error = Some(error);
                break;
            }
            share.audio_frames_sent += 1;
        }
    }

    fn poll_view(&mut self, ctx: &egui::Context, visible: bool) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let snapshot = view
            .stats
            .lock()
            .ok()
            .map(|s| (s.packets, s.decoded, s.bytes, s.lost, s.audio_packets));
        if view.viewer.is_connected() {
            view.was_connected = true;
            view.reconnect = None;
        }
        if let Some((packets, decoded, bytes, lost, audio_packets)) = snapshot {
            let now = Instant::now();
            let elapsed = now.duration_since(view.quality.last).as_secs_f32();
            if elapsed >= 0.5 {
                let received = packets.saturating_sub(audio_packets);
                let d_frames = decoded.saturating_sub(view.quality.last_decoded);
                let d_bytes = bytes.saturating_sub(view.quality.last_bytes);
                let d_lost = lost.saturating_sub(view.quality.last_lost);
                let d_received = received.saturating_sub(view.quality.last_received);
                view.quality.fps = d_frames as f32 / elapsed;
                view.quality.mbps = d_bytes as f32 * 8.0 / elapsed / 1_000_000.0;
                let total = d_received + d_lost;
                view.quality.loss = if total > 0 {
                    d_lost as f32 / total as f32 * 100.0
                } else {
                    0.0
                };
                view.quality.last = now;
                view.quality.last_decoded = decoded;
                view.quality.last_bytes = bytes;
                view.quality.last_lost = lost;
                view.quality.last_received = received;
            }
        }
        if !visible {
            return;
        }
        let Some(frame) = view.latest.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match &mut view.texture {
            Some(preview) if preview.width == frame.width && preview.height == frame.height => {
                preview.texture.set(image, egui::TextureOptions::LINEAR);
            }
            _ => {
                view.texture = Some(Preview {
                    texture: ctx.load_texture("remote", image, egui::TextureOptions::LINEAR),
                    width: frame.width,
                    height: frame.height,
                });
            }
        }
    }

    fn poll_reconnect(&mut self) {
        let now = Instant::now();
        let action = {
            let Some(view) = self.view.as_mut() else {
                return;
            };
            if !view.was_connected || view.viewer.is_connected() {
                return;
            }
            let Some(peer) = view.lan_peer.clone() else {
                return;
            };
            if view.reconnect.is_none() {
                view.reconnect = Some(Reconnect {
                    attempts: 0,
                    last_request: now,
                });
            }
            let state = view.reconnect.as_mut().expect("reconnect state");
            if state.attempts >= 5 {
                if view.error.is_none() {
                    view.error = Some("Connection lost — reconnect attempts exhausted".to_string());
                }
                None
            } else if now.duration_since(state.last_request) >= Duration::from_secs(3) {
                state.last_request = now;
                state.attempts += 1;
                Some(peer)
            } else {
                None
            }
        };
        if let Some(peer) = action {
            if let Some(lan) = &self.lan {
                lan.send_request(&peer, &self.config.name);
            }
            self.pending_view = Some(peer);
        }
    }

    fn render_image(ui: &mut egui::Ui, preview: &Preview) {
        let target = egui::vec2(preview.width as f32, preview.height as f32);
        if target.x <= 0.0 || target.y <= 0.0 {
            return;
        }
        let max = egui::vec2(1024.0, 576.0);
        let cap = (max.x / target.x).min(max.y / target.y).min(1.0);
        let mut scale = cap;
        let available = ui.available_size();
        if available.x > 1.0 && available.y > 1.0 {
            scale = scale
                .min(available.x / target.x)
                .min(available.y / target.y);
        }
        scale = scale.max((360.0 / target.y).min(1.0).min(cap));
        let size = egui::vec2(target.x * scale, target.y * scale);
        ui.image((preview.texture.id(), size));
    }

    fn network_status(&mut self, ui: &mut egui::Ui) {
        match self.lan.as_ref().and_then(|lan| lan.local_address()) {
            Some(ip) => {
                ui.label(RichText::new(format!("This PC on your network: {ip}")).weak());
            }
            None => {
                ui.label(
                    RichText::new("Radmin VPN not detected — friends can't find this PC yet.")
                        .color(Color32::from_rgb(220, 200, 120)),
                );
                if self.radmin_exe.is_some() {
                    if ui.button("Launch Radmin VPN").clicked() {
                        self.launch_radmin();
                    }
                } else {
                    ui.label(
                        RichText::new("Install Radmin VPN to share with friends online.").weak(),
                    );
                }
                if let Some(error) = &self.radmin_error {
                    ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
                }
            }
        }
        if let Some(error) = &self.lan_error {
            ui.label(RichText::new(format!("Network discovery off: {error}")).weak());
        }
    }

    fn launch_radmin(&mut self) {
        match crate::radmin::launch() {
            Ok(()) => self.radmin_error = None,
            Err(error) => self.radmin_error = Some(error),
        }
    }

    fn quality_line(ui: &mut egui::Ui, view: &ViewSession) {
        let quality = &view.quality;
        let color = if quality.mbps <= 0.0 {
            Color32::GRAY
        } else if quality.loss > 5.0 || quality.fps < 15.0 {
            Color32::from_rgb(220, 120, 120)
        } else if quality.loss > 1.0 || quality.fps < 24.0 {
            Color32::from_rgb(220, 200, 120)
        } else {
            Color32::from_rgb(150, 220, 150)
        };
        let resolution = view
            .texture
            .as_ref()
            .map(|preview| format!("{}x{} · ", preview.width, preview.height))
            .unwrap_or_default();
        ui.label(
            RichText::new(format!(
                "{resolution}{:.0} fps · {:.1} Mbps · {:.1}% loss",
                quality.fps, quality.mbps, quality.loss
            ))
            .color(color)
            .size(16.0),
        );
    }

    fn share_view(&mut self, ui: &mut egui::Ui) {
        let mut do_go_live = false;
        let mut do_stop_live = false;
        let mut do_preview = false;

        if self.monitors.is_empty() {
            self.refresh_monitors();
        }
        if self.monitors.is_empty() {
            ui.heading("Share");
            ui.label(RichText::new("No monitor found").color(Color32::from_rgb(220, 120, 120)));
            return;
        }

        ui.heading(if self.live {
            "You're live"
        } else {
            "Share your screen"
        });
        ui.add_space(4.0);
        if self.live || self.share.is_some() {
            if ui.button("Stop streaming").clicked() {
                do_stop_live = true;
            }
        } else if ui.button("Go Live").clicked() {
            do_go_live = true;
        }

        let selected = self.selected_monitor.min(self.monitors.len() - 1);
        egui::ComboBox::from_label("Monitor")
            .selected_text(Self::monitor_label(&self.monitors[selected]))
            .show_ui(ui, |ui| {
                for (index, monitor) in self.monitors.iter().enumerate() {
                    if ui
                        .selectable_label(
                            self.selected_monitor == index,
                            Self::monitor_label(monitor),
                        )
                        .clicked()
                    {
                        self.selected_monitor = index;
                    }
                }
            });

        let mut share_height = self.share_height;
        egui::ComboBox::from_label("Stream quality")
            .selected_text(Self::quality_label(share_height))
            .show_ui(ui, |ui| {
                for (label, height) in [
                    ("Native", None),
                    ("720p", Some(720)),
                    ("540p", Some(540)),
                    ("360p", Some(360)),
                ] {
                    if ui.selectable_label(share_height == height, label).clicked() {
                        share_height = height;
                    }
                }
            });
        if share_height != self.share_height {
            self.share_height = share_height;
            if let Some(share) = self.share.as_mut() {
                let _ = share.tx.try_send(EncodeMsg::Quality {
                    height: share_height,
                });
            }
        }

        let streaming = self.live || self.share.is_some();
        let mut frame_rate = self.frame_rate;
        ui.add_enabled_ui(!streaming, |ui| {
            egui::ComboBox::from_label("Frame rate")
                .selected_text(format!("{frame_rate} fps"))
                .show_ui(ui, |ui| {
                    for fps in [30u32, 60u32] {
                        if ui
                            .selectable_label(frame_rate == fps, format!("{fps} fps"))
                            .clicked()
                        {
                            frame_rate = fps;
                        }
                    }
                });
        });
        if streaming {
            ui.label(RichText::new("Stop streaming to change the frame rate.").weak());
        }
        if frame_rate != self.frame_rate {
            self.frame_rate = frame_rate;
            if let Some(capture) = &self.capture {
                capture.set_interval(self.frame_interval());
            }
        }

        ui.add_space(6.0);
        let preview_label = if self.preview_active {
            "Stop preview"
        } else {
            "Preview"
        };
        if ui.button(preview_label).clicked() {
            do_preview = true;
        }
        if let Some(error) = &self.preview_error {
            ui.label(RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)));
        }

        ui.add_space(6.0);
        if let Some(share) = self.share.as_mut() {
            let audio_label = match (&share.audio_capture, &share.audio_encoder) {
                (Some(_), Some(_)) => "Audio: system sound shared",
                _ => "Audio: not available",
            };
            ui.label(format!("Status: {}", share.sharer.status()));
            ui.label(RichText::new(audio_label).weak());
            if let Some(error) = &share.error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
        } else if self.live {
            ui.label(
                RichText::new("Waiting for someone to join…")
                    .color(Color32::from_rgb(150, 220, 150)),
            );
        }
        if let Some(error) = &self.share_error {
            ui.label(RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)));
        }

        ui.add_space(8.0);
        if self.preview_active {
            if let Some(preview) = &self.preview {
                ui.label(format!(
                    "{}x{} @ {:.0} fps",
                    preview.width, preview.height, self.fps
                ));
                ui.add_space(8.0);
                Self::render_image(ui, preview);
            }
        } else if self.live {
            ui.label(RichText::new("Press Preview to see your screen here.").weak());
        } else {
            ui.label(
                RichText::new(
                    "Press Go Live to start sharing, or Preview to check your screen first.",
                )
                .weak(),
            );
        }

        if do_preview {
            self.toggle_preview();
        }
        if do_go_live {
            self.go_live();
        }
        if do_stop_live {
            self.stop_live();
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let mut do_connect = false;
        let mut do_manual = false;
        let mut do_accept = false;

        let mut peers = self.lan.as_ref().map(|lan| lan.peers()).unwrap_or_default();
        peers.sort_by(|a, b| {
            b.sharing
                .cmp(&a.sharing)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.lan.is_none() {
                    ui.label(RichText::new("Peer discovery is unavailable").weak());
                } else if peers.is_empty() {
                    ui.label(RichText::new("Looking for people…").weak());
                }

                for peer in &peers {
                    ui.horizontal(|ui| {
                        if peer.sharing {
                            let selected =
                                matches!(&self.screen, Screen::Peer(id) if id == &peer.id);
                            if ui
                                .add(egui::Button::selectable(
                                    selected,
                                    RichText::new(&peer.name),
                                ))
                                .clicked()
                            {
                                self.screen = Screen::Peer(peer.id.clone());
                            }
                            ui.label(RichText::new("Live").color(Color32::from_rgb(150, 220, 150)));
                        } else {
                            ui.label(RichText::new(&peer.name).color(Color32::GRAY));
                        }
                    });
                }

                if self.pending_view.is_some() {
                    ui.label(RichText::new("Connecting…").color(Color32::from_rgb(220, 200, 120)));
                }
                if let Some(error) = &self.view_error {
                    ui.label(
                        RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)),
                    );
                }

                ui.add_space(8.0);
                self.network_status(ui);
                ui.add_space(6.0);
                egui::CollapsingHeader::new("Advanced")
                    .default_open(false)
                    .show(ui, |ui| {
                        if !self.live
                            && self.share.is_none()
                            && ui.button("Create manual connection code").clicked()
                        {
                            do_manual = true;
                        }
                        if let Some(share) = self.share.as_mut() {
                            if let Some(code) = &share.offer_code {
                                ui.label(RichText::new("Share code").strong());
                                Self::code_widget(ui, code, 3);
                            }
                            if share.lan_peer.is_none() {
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut share.answer_input)
                                            .hint_text("Paste answer code")
                                            .desired_width(140.0),
                                    );
                                    if ui.button("Connect").clicked() {
                                        do_accept = true;
                                    }
                                });
                            }
                            let (frames_sent, encode_ms, encode_errors) = share
                                .stats
                                .lock()
                                .map(|stats| {
                                    (stats.frames_sent, stats.encode_ms, stats.encode_errors)
                                })
                                .unwrap_or((0, 0.0, 0));
                            ui.label(format!(
                                "{} frames sent, {:.1} ms/frame, {} encode errors, {} audio frames",
                                frames_sent, encode_ms, encode_errors, share.audio_frames_sent
                            ));
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.code_input)
                                .hint_text("Connection code")
                                .desired_width(140.0),
                        );
                        if ui.button("Connect").clicked() {
                            do_connect = true;
                        }
                        if let Some(code) = self
                            .view
                            .as_ref()
                            .and_then(|view| view.answer_code.as_ref())
                        {
                            ui.label(RichText::new("Your reply code").strong());
                            Self::code_widget(ui, code, 2);
                        }
                    });
            });

        if do_connect {
            self.start_view(self.code_input.trim().to_string(), None);
            if self.view.is_some() {
                self.screen = Screen::View;
            }
        }
        if do_manual {
            self.create_manual_offer();
        }
        if do_accept {
            if let Some(share) = self.share.as_mut() {
                Self::accept_answer(share);
            }
        }
    }

    fn peer_view(&mut self, ui: &mut egui::Ui, id: &str) {
        let name = self
            .lan
            .as_ref()
            .map(|lan| lan.peers())
            .unwrap_or_default()
            .into_iter()
            .find(|peer| peer.id == id)
            .map(|peer| peer.name)
            .unwrap_or_else(|| "Stream".to_string());

        let connected = self.view.as_ref().and_then(|view| view.peer_id.as_deref()) == Some(id);
        if connected {
            self.watch_view(ui);
            return;
        }

        ui.heading(&name);
        let pending = self.pending_view.as_deref() == Some(id);
        if pending {
            ui.label(RichText::new("Connecting…").color(Color32::from_rgb(220, 200, 120)));
        } else {
            ui.label(RichText::new("Not watching yet.").weak());
        }
        if let Some(error) = &self.view_error {
            ui.label(RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)));
        }
        ui.add_space(8.0);

        let available = ui.available_size();
        let width = available.x.clamp(320.0, 1024.0);
        let height = (width * 9.0 / 16.0).min(available.y.max(200.0));
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(6), Color32::from_gray(28));
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::same(6),
            egui::Stroke::new(1.0_f32, Color32::from_gray(80)),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if pending {
                "Connecting…"
            } else {
                "Click to watch"
            },
            egui::FontId::proportional(20.0),
            Color32::from_gray(200),
        );
        if response.clicked() && !pending {
            self.request_view(id);
        }
    }

    fn watch_view(&mut self, ui: &mut egui::Ui) {
        let mut request_stop = false;
        if let Some(view) = self.view.as_mut() {
            ui.heading("Watching");
            ui.label(format!("Status: {}", view.viewer.status()));
            Self::quality_line(ui, view);
            let muted = view
                .audio_playback
                .as_ref()
                .map(|playback| playback.is_muted())
                .unwrap_or(false);
            if let Some(playback) = &view.audio_playback {
                let label = if muted { "Unmute" } else { "Mute" };
                if ui.button(label).clicked() {
                    playback.set_muted(!muted);
                }
            }
            if view.reconnect.is_some() {
                ui.label(
                    RichText::new("Connection lost — reconnecting…")
                        .color(Color32::from_rgb(220, 200, 120)),
                );
            }
            egui::CollapsingHeader::new("Diagnostics")
                .default_open(false)
                .show(ui, |ui| {
                    if let Ok(stats) = view.stats.lock() {
                        ui.label(format!(
                            "Video: {} packets, {} frames assembled, {} decoded (first {}x{}), {} no-picture, {} decode errors",
                            stats.packets,
                            stats.aus,
                            stats.decoded,
                            stats.first_width,
                            stats.first_height,
                            stats.decode_none,
                            stats.decode_errors
                        ));
                        if let Some(first_error) = &stats.first_error {
                            ui.label(
                                RichText::new(format!("First decode error: {first_error}"))
                                    .color(Color32::from_rgb(220, 120, 120)),
                            );
                        }
                        let playing = view.audio_playback.is_some();
                        ui.label(format!(
                            "Audio ({}): {} packets, {} decoded, {} errors",
                            if playing { "on" } else { "off" },
                            stats.audio_packets,
                            stats.audio_decoded,
                            stats.audio_errors
                        ));
                        if let Some(error) = &stats.audio_error {
                            ui.label(
                                RichText::new(format!("First audio error: {error}"))
                                    .color(Color32::from_rgb(220, 120, 120)),
                            );
                        }
                    }
                    if let Some(error) = &view.audio_error {
                        ui.label(
                            RichText::new(format!("Audio unavailable: {error}"))
                                .color(Color32::from_rgb(220, 120, 120)),
                        );
                    }
                });
            if let Some(error) = &view.error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
            ui.add_space(8.0);
            if let Some(preview) = &view.texture {
                ui.label(format!("Stream {}x{}", preview.width, preview.height));
                Self::render_image(ui, preview);
            } else if view.stats.lock().map(|s| s.packets).unwrap_or(0) > 0 {
                ui.label(
                    RichText::new("Receiving stream data but no decoded picture yet…")
                        .color(Color32::from_rgb(220, 200, 120)),
                );
            } else {
                ui.label(RichText::new("Waiting for stream data…").color(Color32::GRAY));
            }
            ui.add_space(8.0);
            if ui.button("Stop watching").clicked() {
                request_stop = true;
            }
        }
        if request_stop {
            self.stop_view();
        }
    }

    fn welcome(&self, ui: &mut egui::Ui) {
        ui.heading("Argos");
        ui.add_space(6.0);
        ui.label("Pick someone to watch, or press Share to stream your screen.");
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Everyone on your Radmin network sees each other. No accounts, no servers.",
            )
            .weak(),
        );
    }

    fn content(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.screen.clone() {
                    Screen::Home => self.welcome(ui),
                    Screen::Share => self.share_view(ui),
                    Screen::Peer(id) => self.peer_view(ui, &id),
                    Screen::View => self.watch_view(ui),
                });
        });
    }

    fn status_text(&self) -> &str {
        if self
            .share
            .as_ref()
            .map(|share| share.sharer.is_connected())
            .unwrap_or(false)
        {
            "sharing"
        } else if self.view.is_some() {
            "watching"
        } else if self.live {
            "live"
        } else if self.capture.is_some() {
            "previewing"
        } else {
            "offline"
        }
    }
}

impl eframe::App for ArgosApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_visuals(egui::Visuals::dark());
        self.poll_lan_events();
        self.top_bar(ctx);
        self.side_bar(ctx);
        if self.capture.is_some() {
            let active = self.update_capture_active();
            self.poll_capture(ctx);
            ctx.request_repaint_after(if active {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(250)
            });
        }
        if self.share.is_some() {
            self.poll_share_audio();
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if self.view.is_some() {
            let visible = self.view_visible();
            self.poll_view(ctx, visible);
            self.poll_reconnect();
            ctx.request_repaint_after(if visible {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(250)
            });
        }
        self.content(ctx);
        if self.lan.is_some() && self.capture.is_none() && self.view.is_none() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.show_settings {
            self.settings_window(ctx);
        }
    }
}
