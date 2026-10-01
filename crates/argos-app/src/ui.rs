use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use argos_core::metrics::{AudioMetrics, Ema, ReceiverMetrics, SenderMetrics, StageTimer};
use argos_core::quality::{Controller as QualityController, Decision, Report};
use argos_core::{h264, lan, session, Packet};
use argos_media::audio::{
    AudioCapture, AudioPlayback, OpusAudioDecoder, OpusAudioEncoder, FRAME_SAMPLES,
};
use argos_media::capture::{self, CaptureSession, MonitorInfo};
use argos_media::decode::{DecodedFrame, H264Decoder};
use argos_media::encode::H264Encoder;

use crate::config::{self, AppConfig};

/// Fallback interval for periodic intra frames.
///
/// This used to be 2 s and was the sharer's *only* recovery mechanism, because
/// the rtc transport offers no RTCP feedback path (see `session.rs`). It is now
/// a backstop behind on-demand requests over the LAN channel: every keyframe is
/// a burst on a Radmin tunnel, and a burst is what causes loss in the first
/// place. `KEYFRAME_REQUEST_INTERVAL` is the same quantity seen from the other
/// side.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(4);

/// Floor between keyframes forced on a viewer's request.
///
/// A keyframe costs a full-frame burst. Honouring every request from a viewer
/// on a bad link would turn a recovery mechanism into a denial of service, so
/// this rate-limits a peer regardless of how often it asks.
const KEYFRAME_REQUEST_FLOOR: Duration = Duration::from_millis(500);

/// How often the viewer may ask for a keyframe.
///
/// Two seconds is comfortably inside the `KEYFRAME_INTERVAL` fallback, so a
/// viewer waiting on an intra frame gets one sooner rather than at the interval
/// anyway — which would make the whole mechanism pointless.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_secs(2);

/// How often the viewer reports its view of the link to the sharer. Matches the
/// resolution of the receiver's own fps/loss measurement.
const REPORT_INTERVAL: Duration = Duration::from_millis(500);

/// Loss above this, in a single measurement window, makes the viewer ask for a
/// keyframe.
///
/// Deliberately well below the adaptive controller's 3% down-threshold: this is
/// a different job. A keyframe is cheap insurance and recovers in one frame,
/// whereas stepping resolution down costs quality and takes seconds to undo.
const LOSS_BURST: f32 = 2.0;

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
    metrics: Arc<SenderMetrics>,
    force_keyframe: Arc<AtomicBool>,
) {
    // RTP timestamps come from elapsed wall time, not from the frame rate.
    // The counter form (`timestamp += 90_000 / fps`) claims a fixed interval
    // per frame, which is false for any frame that is late, coalesced or
    // dropped — so the sender's clock outruns real time and the receiver has to
    // discard good frames to stay in sync. The frame rate now lives only in the
    // encoder's own configuration, set before this thread starts.
    let clock = h264::Clock::new();
    let mut last_keyframe = Instant::now();
    // Set when an intra frame has been asked for but not yet encoded. The flag
    // outlives the request, because the frame that carries the intra arrives
    // later — and that frame's size is the number worth measuring, not the
    // request's.
    let mut pending_keyframe = false;
    while let Ok(msg) = rx.recv() {
        match msg {
            EncodeMsg::Quality { height } => {
                encoder.set_target_height(height);
                // A resolution change is unviewable until the next intra frame,
                // so this one is never optional.
                encoder.force_keyframe();
                last_keyframe = Instant::now();
                pending_keyframe = true;
            }
            EncodeMsg::Frame {
                rgba,
                width,
                height,
            } => {
                // A viewer asking for recovery. An atomic flag rather than a
                // message: the frame queue below is small and fills up exactly
                // when the machine is struggling, and a dropped request would
                // strand a viewer waiting for an intra frame it was promised.
                // Requests that arrive faster than the floor are coalesced — one
                // intra frame serves every request in the interval.
                let requested = force_keyframe.swap(false, Ordering::Relaxed);
                // Backstop for a request that never arrived: the LAN channel is
                // UDP, and a manual-code session has no channel at all.
                let overdue = last_keyframe.elapsed() >= KEYFRAME_INTERVAL;
                if (requested && last_keyframe.elapsed() >= KEYFRAME_REQUEST_FLOOR) || overdue {
                    encoder.force_keyframe();
                    last_keyframe = Instant::now();
                    pending_keyframe = true;
                }
                // Conversion and encoding are timed separately: they have very
                // different costs and very different fixes. The conversion is
                // our own scalar code and, when it scales, does five integer
                // divisions per output pixel; the encode is openh264.
                let converted = {
                    let _convert = StageTimer::new(&metrics.convert);
                    encoder.convert(&rgba, width, height)
                };
                let bitstream = match converted {
                    Ok(dims) => {
                        let _encode = StageTimer::new(&metrics.encode);
                        encoder.encode_planes(dims.0, dims.1)
                    }
                    Err(error) => Err(error),
                };
                match bitstream {
                    Ok(bitstream) => {
                        // Sampled at send time, not at receipt: the clock is
                        // measuring how long this frame waited, which is exactly
                        // the latency a receiver has to absorb.
                        let ts = clock.ticks(Instant::now());
                        let size = bitstream.len() as u64;
                        metrics.encoded_bytes.add(size);
                        if pending_keyframe {
                            // The frame following a forced intra is an IDR, so
                            // this is the size of the burst that goes out on the
                            // wire. If this number is large, it is a plausible
                            // cause of the loss it is meant to help recover from.
                            metrics.keyframes.incr();
                            metrics.last_keyframe_bytes.set(size);
                            pending_keyframe = false;
                        }
                        match session::block_on(sharer.send_frame(&bitstream, ts, &metrics)) {
                            Ok(()) => {
                                metrics.encoded.record();
                                if let Ok(mut stats) = stats.lock() {
                                    stats.frames_sent += 1;
                                    stats.encode_ms = metrics.encode.mean_ms();
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
                        metrics.encode_errors.incr();
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
    /// Stage timings and drop counts shared with the capture thread.
    metrics: Arc<SenderMetrics>,
    last_encode: Instant,
    audio_capture: Option<AudioCapture>,
    audio_encoder: Option<OpusAudioEncoder>,
    audio_error: Option<String>,
    audio_timestamp: u32,
    audio_frames_sent: u64,
    lan_peer: Option<String>,
    /// Adaptive resolution, driven by the viewer's reports over the LAN channel.
    /// Pure state machine: see `argos_core::quality`.
    quality: QualityController,
    /// Last height this controller asked for, so a repeat decision is not
    /// re-sent to the encoder.
    applied_height: Option<u32>,
    /// Why the height last changed, for the UI.
    quality_note: Option<String>,
    /// When the last keyframe went out, so the UI can show recovery activity.
    last_keyframe_request: Instant,
    /// Set by a viewer's keyframe request, consumed by the encode worker.
    force_keyframe: Arc<AtomicBool>,
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
    /// When the last report went to the sharer, rate-limited so a healthy link
    /// costs nothing and a broken one cannot flood the channel.
    last_report: Instant,
    /// When this viewer last asked the sharer for a keyframe. Rate limiting
    /// belongs on the requester: the sharer cannot tell a stream of genuine
    /// requests from one viewer stuck in a retry loop.
    last_keyframe_request: Instant,
    /// Loss measured in the window that triggered the last request, for display.
    requested_at_loss: f32,
    /// How many keyframes this session has asked for. Non-zero is what makes the
    /// "recovered" line meaningful; without it, a session that never lost
    /// anything would claim to have recovered from a request it never sent.
    recoveries: u64,
    /// Packets arrived in the last window but none of them decoded.
    ///
    /// Distinct from loss, which is measured by sequence gaps and so reports a
    /// perfectly intact stream here. This is the decoder waiting on an intra
    /// frame it will not be given, which is the one failure loss cannot see.
    starved: bool,
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
            // Both start "long ago" so the first measurement is not rate limited.
            last_report: Instant::now() - REPORT_INTERVAL - Duration::from_millis(1),
            last_keyframe_request: Instant::now()
                - KEYFRAME_REQUEST_INTERVAL
                - Duration::from_millis(1),
            requested_at_loss: 0.0,
            recoveries: 0,
            starved: false,
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
    /// Lock-free counters and stage timings for this receive session, shared
    /// with the transport's packet callback.
    metrics: Arc<ReceiverMetrics>,
    texture: Option<Preview>,
    audio_playback: Option<Arc<AudioPlayback>>,
    audio_error: Option<String>,
    quality: Quality,
    was_connected: bool,
    reconnect: Option<Reconnect>,
    lan_peer: Option<String>,
    /// Peer this viewer watches. Keyframe requests and link reports go only to
    /// the sharer actually being watched, never to whoever else is on the
    /// network. `None` for a manual-code session, which is why those sessions
    /// get no recovery: there is no LAN channel to ask over.
    sharer_id: Option<String>,
}

pub struct ArgosApp {
    config: AppConfig,
    screen: Screen,
    show_settings: bool,
    /// Always-on-top pipeline readout, toggled with Ctrl+D. Exists because the
    /// interesting numbers (stage means, peak stalls, drop rates) are the ones
    /// that explain a freeze, and they have to be readable *while* it freezes.
    show_metrics: bool,
    /// Start of the current measurement window. Reset with the overlay's
    /// "Reset" button so a mean covers a known span rather than the whole
    /// session, which is what makes an intermittent stall visible.
    metrics_since: Instant,
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
    /// Adaptive resolution toggle. Off means the sharer ignores viewer reports
    /// and only the user's choice applies, which is the escape hatch if the
    /// controller ever fights a link it should not be judging.
    auto_quality: bool,
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
            auto_quality: true,
            lan,
            lan_error,
            pending_view: None,
            radmin_exe: crate::radmin::find_exe(),
            radmin_error: None,
            config,
            screen: Screen::Home,
            show_settings: false,
            show_metrics: false,
            metrics_since: Instant::now(),
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
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let worker_force_keyframe = Arc::clone(&force_keyframe);
        let worker_metrics = self
            .capture
            .as_ref()
            .map(|capture| Arc::clone(capture.metrics()))
            .unwrap_or_default();
        let join = match thread::Builder::new()
            .name("argos-encode".to_string())
            .spawn(move || {
                encode_worker(
                    rx,
                    worker_sharer,
                    encoder,
                    worker_stats,
                    worker_metrics,
                    worker_force_keyframe,
                )
            }) {
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
            metrics: self
                .capture
                .as_ref()
                .map(|capture| Arc::clone(capture.metrics()))
                .unwrap_or_default(),
            last_encode: Instant::now(),
            audio_capture,
            audio_encoder,
            audio_error,
            audio_timestamp: 0,
            audio_frames_sent: 0,
            quality: QualityController::at_height(self.share_height),
            applied_height: self.share_height,
            quality_note: None,
            last_keyframe_request: Instant::now(),
            force_keyframe,
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
        let metrics = Arc::new(ReceiverMetrics::default());
        let (audio_playback, audio_error) = match AudioPlayback::start(1.0) {
            Ok(playback) => (Some(Arc::new(playback)), None),
            Err(error) => (None, Some(error)),
        };
        let callback: Arc<dyn Fn(&Packet) + Send + Sync> = Arc::new(Self::receive_callback(
            Arc::clone(&latest),
            Arc::clone(&stats),
            Arc::clone(&metrics),
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
        self.view = Some(ViewSession {
            viewer,
            answer_code,
            error: None,
            latest,
            stats,
            metrics,
            texture: None,
            audio_playback,
            audio_error,
            quality: Quality::default(),
            was_connected: false,
            reconnect: None,
            // `lan_peer` is the sharer's id, and is the only handle a
            // viewer has for asking that sharer for a keyframe or telling it
            // what the link looks like from here. `lan_peer` stays as the
            // reconnect target; the two differ only in intent, not in value.
            sharer_id: lan_peer.clone(),
            lan_peer,
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
                self.view
                    .as_ref()
                    .and_then(|view| view.sharer_id.as_deref())
                    == Some(id.as_str())
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
                lan::LanEvent::Keyframe { id } => {
                    // Only from the peer we are actually sharing to. Not every
                    // viewer on the network gets to spend this sharer's
                    // bandwidth on intra frames.
                    let Some(share) = self.share.as_mut() else {
                        continue;
                    };
                    if share.lan_peer.as_deref() != Some(id.as_str()) {
                        continue;
                    }
                    share.force_keyframe.store(true, Ordering::Relaxed);
                    share.last_keyframe_request = Instant::now();
                }
                lan::LanEvent::Report { id, loss, fps } => {
                    self.apply_report(&id, loss, fps);
                }
            }
        }
    }

    /// Feeds a viewer's link measurement into the adaptive quality controller.
    ///
    /// Runs on the UI thread, so it must not block. It only pushes to the
    /// encode worker's channel, and only when the decision is actually new.
    fn apply_report(&mut self, id: &str, loss: f32, fps: f32) {
        if !self.auto_quality {
            return;
        }
        let now = Instant::now();
        let Some(share) = self.share.as_mut() else {
            return;
        };
        if share.lan_peer.as_deref() != Some(id) {
            return;
        }
        let Decision::Step(_) = share.quality.update(Report { loss, fps }, now) else {
            return;
        };
        let height = share.quality.height();
        share.quality_note = share.quality.last_reason().map(str::to_string);
        if share.applied_height == height {
            return;
        }
        share.applied_height = height;
        let _ = share.tx.try_send(EncodeMsg::Quality { height });
    }

    fn receive_callback(
        latest: Arc<Mutex<Option<DecodedFrame>>>,
        stats: Arc<Mutex<ViewStats>>,
        metrics: Arc<ReceiverMetrics>,
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
            // Total time inside the callback. If this mean approaches the packet
            // arrival interval, the callback is the bottleneck and the receiver
            // cannot keep up no matter how much spare CPU the decoder has.
            let _receive = StageTimer::new(&metrics.receive);
            // Counters are atomics, not the `stats` mutex. At 500+ video packets
            // a second this callback was taking that lock three or four times
            // per packet, and the UI thread reads it every frame; the contention
            // was itself a source of jitter. `ViewStats` still records the
            // session-long numbers the existing panels show.
            metrics.packets.incr();
            metrics.bytes.add(packet.payload.len() as u64);
            if packet.header.payload_type == session::AUDIO_PT {
                metrics.audio_packets.incr();
                metrics.audio_bytes.add(packet.payload.len() as u64);
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
                        metrics.audio_decoded.record();
                        if let Ok(mut stats) = stats.lock() {
                            stats.audio_decoded += 1;
                        }
                    }
                    Err(error) => {
                        metrics.audio_errors.incr();
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
                stats.packets += 1;
                stats.bytes += packet.payload.len() as u64;
                let seq = packet.header.sequence_number;
                match stats.highest_seq {
                    None => {
                        stats.base_seq = Some(seq);
                        stats.highest_seq = Some(seq);
                    }
                    Some(highest) => {
                        if seq != highest && seq.wrapping_sub(highest) < 0x8000 {
                            let gap = seq.wrapping_sub(highest) as u64 - 1;
                            stats.lost += gap;
                            // A gap here means the packet never arrived, and with
                            // no retransmission buffer in the transport the frame
                            // it belonged to is unrecoverable. This is the number
                            // that explains a stalled viewer.
                            metrics.sequence_losses.add(gap);
                            stats.highest_seq = Some(seq);
                        }
                    }
                }
            }
            let Ok(mut pipeline) = pipeline.lock() else {
                return;
            };
            let nalus = {
                let _depacketize = StageTimer::new(&metrics.depacketize);
                pipeline.depacketizer.push(packet)
            };
            let Some(nalus) = nalus else {
                return;
            };
            metrics.access_units.incr();
            if let Ok(mut stats) = stats.lock() {
                stats.aus += 1;
            }
            let Some(decoder) = pipeline.decoder.as_mut() else {
                return;
            };
            let access_unit = h264::access_unit_to_annexb(&nalus);
            match decoder.decode(&access_unit, &metrics) {
                Ok(Some(frame)) => {
                    if let Ok(mut stats) = stats.lock() {
                        if stats.first_width == 0 {
                            stats.first_width = frame.width;
                            stats.first_height = frame.height;
                        }
                        stats.decoded += 1;
                    }
                    // Only the newest frame matters. If the UI has not consumed
                    // the previous one, it is stale by definition, so replacing
                    // it is the correct behaviour and the drop is counted so the
                    // present rate can be compared against the decode rate.
                    if let Ok(mut slot) = latest.lock() {
                        if slot.is_some() {
                            metrics.presented.drop_frame();
                        } else {
                            metrics.presented.record();
                        }
                        *slot = Some(frame);
                    }
                }
                Ok(None) => {
                    metrics.no_picture.incr();
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
                    metrics.decode_errors.incr();
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
        // The capture worker retries a dead device on its own and only reports the
        // failures it wants surfaced, so taking them here cannot hide a later
        // one behind an earlier.
        if let Some(notice) = share
            .audio_capture
            .as_ref()
            .and_then(AudioCapture::try_error)
        {
            share.audio_error = Some(notice);
        }
        // A worker that has been recovering on its own should stop saying so.
        if share
            .audio_capture
            .as_ref()
            .is_some_and(AudioCapture::is_alive)
        {
            share.audio_error = None;
        }
        if !share.sharer.is_connected() {
            return;
        }
        let metrics = Arc::clone(&share.metrics);
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
            if let Err(error) = session::block_on(sharer.send_audio(&packet, timestamp, &metrics)) {
                share.audio_error = Some(error);
                break;
            }
            share.audio_frames_sent += 1;
        }
    }

    fn poll_view(&mut self, ctx: &egui::Context, visible: bool) {
        if self.view.is_none() {
            return;
        }
        let snapshot = self.view.as_mut().and_then(|view| {
            view.stats
                .lock()
                .ok()
                .map(|s| (s.packets, s.decoded, s.bytes, s.lost, s.audio_packets))
        });
        {
            let Some(view) = self.view.as_mut() else {
                return;
            };
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
                    // Packets arrived but nothing decoded. Sequence gaps count
                    // this stream as intact, so loss-based detection cannot see
                    // it — the decoder is simply waiting for an intra frame.
                    view.quality.starved = d_received > 0 && d_frames == 0;
                    view.quality.last = now;
                    view.quality.last_decoded = decoded;
                    view.quality.last_bytes = bytes;
                    view.quality.last_lost = lost;
                    view.quality.last_received = received;
                }
            }
        }
        // Reporting happens before the `visible` early-return below. A viewer
        // whose window is not on screen is still watching, and its loss is the
        // whole input to the sharer's recovery decisions.
        self.report_link_state();
        if !visible {
            return;
        }
        let Some(view) = self.view.as_mut() else {
            return;
        };
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

    /// Tells the sharer what this end is seeing, and asks for a keyframe when
    /// the link has clearly lost one.
    ///
    /// Both messages are best-effort UDP and both are rate limited. The report
    /// matters whether or not anything has gone wrong — it is what lets the
    /// sharer's controller step back up when a link recovers, which it could
    /// never infer on its own.
    fn report_link_state(&mut self) {
        let now = Instant::now();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        // A manual-code session has no LAN peer, so there is nobody to tell. It
        // also gets no keyframe recovery, which is worth saying in the UI.
        let Some(peer) = view.sharer_id.clone() else {
            return;
        };
        let Some(lan) = self.lan.as_ref() else {
            return;
        };

        if now.duration_since(view.quality.last_report) >= REPORT_INTERVAL {
            view.quality.last_report = now;
            lan.send_report(&peer, view.quality.loss, view.quality.fps);
        }

        // Two independent reasons to ask, because they fail differently:
        // sequence gaps (loss), and an intact stream the decoder refuses to
        // decode because it is waiting for an intra frame (starved).
        let connected = view.viewer.is_connected();
        let needs_keyframe = view.quality.loss >= LOSS_BURST || view.quality.starved;
        if connected
            && needs_keyframe
            && now.duration_since(view.quality.last_keyframe_request) >= KEYFRAME_REQUEST_INTERVAL
        {
            view.quality.last_keyframe_request = now;
            view.quality.requested_at_loss = view.quality.loss;
            view.quality.recoveries += 1;
            lan.send_keyframe(&peer);
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
        // Recovery state, because "the fps is bad" and "the app is asking the
        // sharer to fix it" are different situations and only one of them is
        // worth waiting out.
        if let Some(peer) = &view.sharer_id {
            let since = view.quality.last_keyframe_request.elapsed();
            if quality.loss >= LOSS_BURST {
                ui.label(
                    RichText::new(format!(
                        "Requesting keyframes from {peer} — {:.1}% loss",
                        quality.loss
                    ))
                    .color(Color32::from_rgb(220, 200, 120)),
                );
            } else if view.quality.recoveries > 0 && since < Duration::from_secs(10) {
                ui.label(
                    RichText::new(format!("Recovered {:.0} s ago", since.as_secs_f32())).weak(),
                );
            }
        } else {
            ui.label(RichText::new("No LAN peer — no keyframe recovery for this session.").weak());
        }
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
        let mut auto_quality = self.auto_quality;
        if ui
            .checkbox(&mut auto_quality, "Adjust resolution to the viewer's link")
            .changed()
        {
            self.auto_quality = auto_quality;
            if let Some(share) = self.share.as_mut() {
                // Turning it off must restore whatever the user picked, since the
                // controller may have moved the encoder somewhere else.
                share.quality.reset(self.share_height);
                share.applied_height = self.share_height;
                share.quality_note = None;
                let _ = share.tx.try_send(EncodeMsg::Quality {
                    height: self.share_height,
                });
            }
        }
        // Show what the controller is actually doing. A silent resolution change is
        // indistinguishable from a bug to whoever is watching, and knowing it was
        // deliberate is the difference between trusting it and fighting it.
        if let Some(share) = self.share.as_ref() {
            if self.auto_quality && share.quality.is_auto() {
                let height = share
                    .quality
                    .height()
                    .map(|height| format!("{height}p"))
                    .unwrap_or_else(|| "native".to_string());
                ui.label(
                    RichText::new(format!(
                        "Sending {height} · smoothed loss {:.1}%",
                        share.quality.smoothed_loss()
                    ))
                    .color(Color32::from_rgb(220, 200, 120)),
                );
                if let Some(reason) = &share.quality_note {
                    ui.label(RichText::new(reason).weak());
                }
            }
        }
        if share_height != self.share_height {
            self.share_height = share_height;
            if let Some(share) = self.share.as_mut() {
                // A manual choice wins. Resetting the controller stops it from
                // undoing this on evidence gathered before the choice was made.
                share.quality.reset(share_height);
                share.applied_height = share_height;
                share.quality_note = None;
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
            // Presence of the objects only proves the thread started. The state
            // handle is what says whether a device is actually being serviced
            // right now, which is the question the panel is really asking.
            let audio_label = match (&share.audio_capture, &share.audio_encoder) {
                (Some(capture), Some(_)) if capture.is_alive() => "Audio: system sound shared",
                (Some(_), Some(_)) => "Audio: reconnecting to the sound device",
                _ => "Audio: not available",
            };
            ui.label(format!("Status: {}", share.sharer.status()));
            if share
                .audio_capture
                .as_ref()
                .is_some_and(AudioCapture::is_alive)
            {
                ui.label(RichText::new(audio_label).weak());
            } else {
                ui.label(RichText::new(audio_label).color(Color32::from_rgb(220, 200, 120)));
            }
            if let Some(error) = &share.audio_error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
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
                // Reported here as well as in the Watching panel: a viewer whose
                // speaker died during setup never reaches that panel, and an
                // unconnected viewer has no other way to find out.
                if let Some(view) = self.view.as_ref() {
                    if let Some(error) = &view.audio_error {
                        ui.label(
                            RichText::new(format!("Audio unavailable: {error}"))
                                .color(Color32::from_rgb(220, 120, 120)),
                        );
                    }
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

        let connected = self
            .view
            .as_ref()
            .and_then(|view| view.sharer_id.as_deref())
            == Some(id);
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
                // `Some(playback)` says the thread was started, not that the
                // speaker is still being driven. A worker stuck in its retry
                // backoff leaves the session silent with nothing else to show
                // for it.
                if !playback.is_alive() {
                    ui.label(
                        RichText::new("Audio: reconnecting to the speaker")
                            .color(Color32::from_rgb(220, 200, 120)),
                    );
                }
                if let Some(error) = playback.try_error() {
                    view.audio_error = Some(error);
                } else if playback.is_alive() {
                    view.audio_error = None;
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
                        // "on" must mean the speaker is actually being driven. Presence of the
                        // object only means the thread was started, which is how
                        // a dead audio thread used to look healthy.
                        let playing = view
                            .audio_playback
                            .as_ref()
                            .is_some_and(|playback| playback.is_alive());
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

    /// One line of the stage-timing table. The peak column is the important
    /// one: a mean over a long window stays flat through an intermittent stall,
    /// while a single 400 ms hitch is the whole story.
    fn stage_row(ui: &mut egui::Ui, name: &str, stage: &Ema) {
        ui.monospace(format!(
            "  {name:<11} {:>7.2} ms avg  {:>8.2} ms peak  {:>7} samples",
            stage.mean_ms(),
            stage.peak_ms(),
            stage.samples()
        ));
    }

    fn counter_row(ui: &mut egui::Ui, name: &str, value: String) {
        ui.monospace(format!("  {name:<11} {value}"));
    }

    /// Rows for one audio device worker.
    ///
    /// The drift figure is the one that matters over a long session: a buffer
    /// that is slowly emptying looks fine right up until every period is
    /// silence, and the level itself is what says whether that is happening.
    fn audio_rows(ui: &mut egui::Ui, name: &str, audio: &AudioMetrics, secs: f32) {
        ui.label(RichText::new(name).strong());
        Self::stage_row(ui, "  device io", &audio.device_io);
        Self::stage_row(ui, "  convert", &audio.convert);
        Self::stage_row(ui, "  codec", &audio.codec);
        Self::counter_row(
            ui,
            "  periods",
            format!(
                "{} ({:.1}/s)",
                audio.periods.get(),
                audio.periods.get() as f32 / secs
            ),
        );
        Self::counter_row(
            ui,
            "  underruns",
            format!(
                "{} ({} periods silent)",
                audio.underruns.dropped(),
                audio.underruns.dropped()
            ),
        );
        Self::counter_row(
            ui,
            "  overruns",
            format!(
                "{} ({} periods clipped)",
                audio.overruns.dropped(),
                audio.overruns.dropped()
            ),
        );
        Self::counter_row(
            ui,
            "  drift",
            format!(
                "{} samples ({:.1}/s), {} device errors, {} restarts",
                audio.drift_samples.get(),
                audio.drift_samples.get() as f32 / secs,
                audio.device_errors.get(),
                audio.restarts.get()
            ),
        );
    }

    /// The always-visible pipeline readout (Ctrl+D).
    ///
    /// Deliberately a floating window rather than a section in the sidebar: it
    /// has to stay readable while the video is stalling, which is exactly when
    /// the sidebar is not what anyone is looking at.
    fn metrics_window(&mut self, ctx: &egui::Context) {
        let mut reset = false;
        let mut close = false;
        let window = egui::Window::new("Pipeline")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 40.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!(
                        "window {:.0} s",
                        self.metrics_since.elapsed().as_secs_f32()
                    )));
                    if ui.small_button("Reset").clicked() {
                        reset = true;
                    }
                    if ui.small_button("Close").clicked() {
                        close = true;
                    }
                });
                ui.separator();

                if let Some(share) = self.share.as_ref() {
                    let m = &share.metrics;
                    let secs = self.metrics_since.elapsed().as_secs_f32().max(0.001);
                    if let Some(capture) = share.audio_capture.as_ref() {
                        Self::audio_rows(ui, "Capture", capture.state().metrics(), secs);
                    }
                    ui.label(RichText::new("Sending").strong());
                    Self::stage_row(ui, "acquire", &m.acquire);
                    Self::stage_row(ui, "convert", &m.convert);
                    Self::stage_row(ui, "encode", &m.encode);
                    Self::stage_row(ui, "packetize", &m.packetize);
                    Self::stage_row(ui, "write", &m.write);
                    ui.separator();
                    Self::counter_row(
                        ui,
                        "captured",
                        format!(
                            "{} ({:.1} fps, {} dropped)",
                            m.captured.frames(),
                            m.captured.frames() as f32 / secs,
                            m.captured.dropped()
                        ),
                    );
                    Self::counter_row(
                        ui,
                        "encoded",
                        format!(
                            "{} ({:.1} fps, {} dropped)",
                            m.encoded.frames(),
                            m.encoded.frames() as f32 / secs,
                            m.encoded.dropped()
                        ),
                    );
                    Self::counter_row(
                        ui,
                        "bitrate",
                        format!(
                            "{:.2} Mbps, {} keyframes, last IDR {} KiB",
                            m.encoded_bytes.get() as f32 * 8.0 / secs / 1_000_000.0,
                            m.keyframes.get(),
                            m.last_keyframe_bytes.get() / 1024
                        ),
                    );
                    Self::counter_row(
                        ui,
                        "errors",
                        format!(
                            "{} encode, {} video write, {} audio write",
                            m.encode_errors.get(),
                            m.write_errors.get(),
                            m.audio_errors.get()
                        ),
                    );
                    Self::counter_row(
                        ui,
                        "audio out",
                        format!(
                            "{} frames ({:.1}/s), {} KiB",
                            m.audio_frames.get(),
                            m.audio_frames.get() as f32 / secs,
                            m.audio_bytes.get() / 1024
                        ),
                    );
                    if let Some(quality) = share.quality.last_reason() {
                        let rung = share.quality.rung();
                        Self::counter_row(
                            ui,
                            "adaptive",
                            format!(
                                "rung {rung} of {}, {:.1}% smoothed loss — {quality}",
                                argos_core::quality::LADDER.len() - 1,
                                share.quality.smoothed_loss()
                            ),
                        );
                    }
                }

                if let Some(view) = self.view.as_ref() {
                    if self.share.is_some() {
                        ui.separator();
                    }
                    let m = &view.metrics;
                    let secs = self.metrics_since.elapsed().as_secs_f32().max(0.001);
                    if let Some(playback) = view.audio_playback.as_ref() {
                        Self::audio_rows(ui, "Speaker", playback.state().metrics(), secs);
                    }
                    ui.label(RichText::new("Receiving").strong());
                    Self::stage_row(ui, "callback", &m.receive);
                    Self::stage_row(ui, "depacketize", &m.depacketize);
                    Self::stage_row(ui, "decode", &m.decode);
                    Self::stage_row(ui, "present", &m.present);
                    ui.separator();
                    Self::counter_row(
                        ui,
                        "packets",
                        format!(
                            "{} ({:.0}/s), {} lost, {:.2} Mbps",
                            m.packets.get(),
                            m.packets.get() as f32 / secs,
                            m.sequence_losses.get(),
                            m.bytes.get() as f32 * 8.0 / secs / 1_000_000.0
                        ),
                    );
                    Self::counter_row(
                        ui,
                        "frames",
                        format!(
                            "{} decoded ({:.1} fps), {} replaced before display, {} no-picture",
                            m.decoded.frames(),
                            m.decoded.frames() as f32 / secs,
                            m.presented.dropped(),
                            m.no_picture.get()
                        ),
                    );
                    Self::counter_row(
                        ui,
                        "errors",
                        format!(
                            "{} decode, {} audio",
                            m.decode_errors.get(),
                            m.audio_errors.get()
                        ),
                    );
                    Self::counter_row(
                        ui,
                        "audio in",
                        format!(
                            "{} packets ({:.1}/s), {} decoded, {} dropped by device",
                            m.audio_packets.get(),
                            m.audio_packets.get() as f32 / secs,
                            m.audio_decoded.frames(),
                            m.audio_decoded.dropped()
                        ),
                    );
                    // What the sharer is being told, and how many times
                    // recovery has actually been asked for. A count, not a
                    // timestamp: "idle" and "never needed" look identical on a
                    // clock and mean completely different things.
                    if view.sharer_id.is_some() {
                        Self::counter_row(
                            ui,
                            "recovery",
                            match view.quality.recoveries {
                                0 => "no keyframes requested".to_string(),
                                count => format!(
                                    "{count} keyframes requested, last {:.1} s ago, \
                                     {:.1}% loss at the time",
                                    view.quality.last_keyframe_request.elapsed().as_secs_f32(),
                                    view.quality.requested_at_loss
                                ),
                            },
                        );
                    } else {
                        Self::counter_row(ui, "recovery", "no LAN peer — unavailable".to_string());
                    }
                }

                if self.share.is_none() && self.view.is_none() {
                    ui.label(RichText::new("Idle — nothing is streaming.").weak());
                }
            });
        if window.is_none() {
            self.show_metrics = false;
        }
        if close {
            self.show_metrics = false;
        }
        if reset {
            self.metrics_since = Instant::now();
            if let Some(metrics) = self.share.as_ref().map(|share| &share.metrics) {
                Self::reset_sender(metrics);
            }
            if let Some(metrics) = self.view.as_ref().map(|view| &view.metrics) {
                Self::reset_receiver(metrics);
            }
        }
    }

    fn reset_sender(metrics: &SenderMetrics) {
        for stage in [
            &metrics.acquire,
            &metrics.readback,
            &metrics.convert,
            &metrics.encode,
            &metrics.packetize,
            &metrics.write,
        ] {
            stage.reset();
        }
        for frames in [&metrics.captured, &metrics.queued, &metrics.encoded] {
            frames.reset();
        }
        for counter in [
            &metrics.encoded_bytes,
            &metrics.last_keyframe_bytes,
            &metrics.keyframes,
            &metrics.encode_errors,
            &metrics.write_errors,
            &metrics.audio_frames,
            &metrics.audio_bytes,
            &metrics.audio_errors,
        ] {
            counter.reset();
        }
    }

    fn reset_receiver(metrics: &ReceiverMetrics) {
        for stage in [
            &metrics.depacketize,
            &metrics.decode,
            &metrics.present,
            &metrics.receive,
        ] {
            stage.reset();
        }
        for frames in [&metrics.audio_decoded, &metrics.decoded, &metrics.presented] {
            frames.reset();
        }
        for counter in [
            &metrics.packets,
            &metrics.bytes,
            &metrics.audio_bytes,
            &metrics.audio_packets,
            &metrics.audio_errors,
            &metrics.sequence_losses,
            &metrics.access_units,
            &metrics.no_picture,
            &metrics.decode_errors,
        ] {
            counter.reset();
        }
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
        if ctx.input_mut(|input| input.key_pressed(egui::Key::D) && input.modifiers.ctrl) {
            self.show_metrics = !self.show_metrics;
        }
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
        if self.show_metrics {
            // The overlay is the only thing that redraws while nothing else
            // would, so a stall that has already stopped the pipeline still
            // refreshes the numbers that explain it.
            self.metrics_window(ctx);
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.lan.is_some() && self.capture.is_none() && self.view.is_none() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.show_settings {
            self.settings_window(ctx);
        }
    }
}
