use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use argos_core::{h264, session, Packet};
use argos_media::audio::{
    AudioCapture, AudioPlayback, OpusAudioDecoder, OpusAudioEncoder, FRAME_SAMPLES,
};
use argos_media::capture::{self, CaptureSession, MonitorInfo};
use argos_media::decode::{DecodedFrame, H264Decoder};
use argos_media::encode::H264Encoder;

use crate::config::{self, AppConfig};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Share,
    View,
}

struct Preview {
    texture: egui::TextureHandle,
    width: u32,
    height: u32,
}

struct ShareSession {
    sharer: Arc<session::Sharer>,
    offer_code: Option<String>,
    answer_input: String,
    error: Option<String>,
    encoder: H264Encoder,
    frame_timestamp: u32,
    frames_sent: u64,
    encode_errors: u64,
    audio_capture: Option<AudioCapture>,
    audio_encoder: Option<OpusAudioEncoder>,
    audio_error: Option<String>,
    audio_timestamp: u32,
    audio_frames_sent: u64,
}

struct ViewStats {
    packets: u64,
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
}

impl Default for ViewStats {
    fn default() -> Self {
        Self {
            packets: 0,
            aus: 0,
            decoded: 0,
            decode_errors: 0,
            decode_none: 0,
            first_width: 0,
            first_height: 0,
            first_error: None,
            audio_packets: 0,
            audio_decoded: 0,
            audio_errors: 0,
            audio_error: None,
        }
    }
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
}

pub struct ArgosApp {
    config: AppConfig,
    mode: Mode,
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
    view: Option<ViewSession>,
    view_error: Option<String>,
}

impl ArgosApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config = config::load();
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
            view: None,
            view_error: None,
            config,
            mode: Mode::View,
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
                ui.label(RichText::new("ARGOS").strong().size(18.0));
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
        egui::SidePanel::left("mode_panel")
            .resizable(false)
            .default_width(150.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                let share = ui.selectable_label(self.mode == Mode::Share, "Share");
                let view = ui.selectable_label(self.mode == Mode::View, "View");
                if share.clicked() {
                    self.mode = Mode::Share;
                }
                if view.clicked() {
                    self.mode = Mode::View;
                }
                ui.separator();
                ui.label(RichText::new("Status").weak());
                ui.label(RichText::new(self.status_text()).color(Color32::from_rgb(150, 220, 150)));
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
                        self.config.name = name;
                        config::save(&self.config);
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

    fn start_capture(&mut self) -> Result<Option<MonitorInfo>, String> {
        let Some(source) = self.monitors.get(self.selected_monitor) else {
            return Ok(None);
        };
        let mut session = CaptureSession::new();
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

    fn start_share(&mut self) {
        self.share_error = None;
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
        let offer_code = match session::block_on(sharer.create_offer()) {
            Ok(code) => Some(code),
            Err(error) => {
                self.share_error = Some(error);
                return;
            }
        };
        let encoder = match H264Encoder::new() {
            Ok(encoder) => encoder,
            Err(error) => {
                self.share_error = Some(error);
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
        self.share = Some(ShareSession {
            sharer,
            offer_code,
            answer_input: String::new(),
            error: None,
            encoder,
            frame_timestamp: 0,
            frames_sent: 0,
            encode_errors: 0,
            audio_capture,
            audio_encoder,
            audio_error,
            audio_timestamp: 0,
            audio_frames_sent: 0,
        });
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

    fn stop_share(&mut self) {
        if let Some(share) = self.share.take() {
            let _ = session::block_on(share.sharer.close());
        }
        self.share_error = None;
    }

    fn start_view(&mut self) {
        self.view_error = None;
        let offer = self.code_input.trim().to_string();
        if offer.is_empty() {
            self.view_error = Some("paste a connection code from the sharer first".to_string());
            return;
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
        let answer_code = match session::block_on(viewer.answer_offer(&offer)) {
            Ok(code) => Some(code),
            Err(error) => {
                self.view_error = Some(error);
                return;
            }
        };
        self.view = Some(ViewSession {
            viewer,
            answer_code,
            error: None,
            latest,
            stats,
            texture: None,
            audio_playback,
            audio_error,
        });
    }

    fn stop_view(&mut self) {
        if let Some(view) = self.view.take() {
            let _ = session::block_on(view.viewer.close());
        }
        self.view_error = None;
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
        let Some(frame) = self.capture.as_ref().and_then(CaptureSession::latest) else {
            return;
        };
        let want_image = self.preview_active || self.share.is_some();
        if self.preview_active {
            self.fps_frames += 1;
            let now = Instant::now();
            let elapsed = now.duration_since(self.fps_last);
            if elapsed >= Duration::from_secs(1) {
                self.fps = self.fps_frames as f32 / elapsed.as_secs_f32();
                self.fps_frames = 0;
                self.fps_last = now;
            }
        }
        if want_image {
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
            if share.sharer.is_connected() {
                match share.encoder.encode(&frame.rgba, frame.width, frame.height) {
                    Ok(bitstream) => {
                        share.frames_sent += 1;
                        let timestamp = share.frame_timestamp;
                        share.frame_timestamp = timestamp.wrapping_add(3000);
                        let sharer = Arc::clone(&share.sharer);
                        if let Err(error) =
                            session::block_on(sharer.send_frame(&bitstream, timestamp))
                        {
                            share.error = Some(error);
                        }
                    }
                    Err(error) => {
                        share.encode_errors += 1;
                        share.error = Some(error);
                    }
                }
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
        loop {
            let Some(frame) = share
                .audio_capture
                .as_ref()
                .and_then(AudioCapture::try_frame)
            else {
                break;
            };
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

    fn poll_view(&mut self, ctx: &egui::Context) {
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

    fn share_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Share your screen");
        ui.add_space(4.0);
        ui.label("One companion connects to you directly, peer to peer.");
        ui.label("No accounts. No servers. Nothing is routed through us.");
        ui.add_space(12.0);

        if self.monitors.is_empty() {
            self.refresh_monitors();
        }
        if self.monitors.is_empty() {
            ui.label(RichText::new("No monitor found").color(Color32::from_rgb(220, 120, 120)));
            return;
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

        ui.add_space(10.0);
        let preview_label = if self.preview_active {
            "Stop preview"
        } else {
            "Preview"
        };
        if ui.button(preview_label).clicked() {
            self.toggle_preview();
        }
        if let Some(error) = &self.preview_error {
            ui.label(RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)));
        }

        if self.preview_active && self.capture.is_some() {
            if let Some(preview) = &self.preview {
                ui.label(format!(
                    "Preview {}x{} @ {:.0} fps",
                    preview.width, preview.height, self.fps
                ));
            }
            ui.add_space(8.0);
            if let Some(preview) = &self.preview {
                Self::render_image(ui, preview);
            }
        }

        ui.add_space(12.0);
        let mut request_stop = false;
        if let Some(share) = self.share.as_mut() {
            let audio_label = match (&share.audio_capture, &share.audio_encoder) {
                (Some(_), Some(_)) => format!(
                    "Audio: system sound shared ({} frames sent)",
                    share.audio_frames_sent
                ),
                _ => "Audio: not available on this device".to_string(),
            };
            ui.label(audio_label);
            if let Some(error) = &share.audio_error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
            ui.label(format!("Status: {}", share.sharer.status()));
            ui.label(format!(
                "Diagnostics: {} frames encoded & sent, {} encode errors",
                share.frames_sent, share.encode_errors
            ));
            if let Some(code) = &share.offer_code {
                ui.add_space(8.0);
                ui.label(RichText::new("Connection code").strong());
                ui.label("Give this to the viewer. They answer, then paste their reply below.");
                Self::code_widget(ui, code, 3);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut share.answer_input)
                        .hint_text("Paste the viewer's answer code")
                        .desired_width(300.0),
                );
                if ui.button("Connect").clicked() {
                    Self::accept_answer(share);
                }
            });
            if let Some(error) = &share.error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
            ui.add_space(8.0);
            if ui.button("Stop sharing").clicked() {
                request_stop = true;
            }
        } else {
            if ui.button("Start sharing").clicked() {
                self.start_share();
            }
            if let Some(error) = &self.share_error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
        }
        if request_stop {
            self.stop_share();
        }
    }

    fn view_panel(&mut self, ui: &mut egui::Ui) {
        let mut request_connect = false;
        let mut request_stop = false;
        if let Some(view) = self.view.as_mut() {
            ui.heading("Watching");
            ui.label(format!("Status: {}", view.viewer.status()));
            {
                let stats = view.stats.lock().ok();
                if let Some(stats) = stats {
                    ui.label(format!(
                        "Diagnostics: {} packets, {} frames assembled, {} decoded (first {}x{}), {} no-picture, {} decode errors",
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
                }
            }
            if let Some(error) = &view.audio_error {
                ui.label(
                    RichText::new(format!("Audio unavailable: {error}"))
                        .color(Color32::from_rgb(220, 120, 120)),
                );
            }
            {
                let playing = view.audio_playback.is_some();
                let stats = view.stats.lock().ok();
                if let Some(stats) = stats {
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
            }
            if let Some(code) = &view.answer_code {
                ui.add_space(8.0);
                ui.label(RichText::new("Your reply code").strong());
                ui.label("Send this back to the sharer to complete the connection.");
                Self::code_widget(ui, code, 2);
            }
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
        } else {
            ui.heading("Watch a share");
            ui.add_space(4.0);
            ui.label("Paste a connection code from someone you trust.");
            ui.add_space(8.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.code_input)
                    .hint_text("Connection code")
                    .desired_width(320.0),
            );
            ui.add_space(6.0);
            if ui.button("Connect").clicked() {
                request_connect = true;
            }
            if let Some(error) = &self.view_error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
            ui.add_space(16.0);
            ui.separator();
            ui.label(RichText::new("Recent").weak());
            ui.add_space(4.0);
            ui.label(RichText::new("No contacts yet").weak());
        }
        if request_connect {
            self.start_view();
        }
        if request_stop {
            self.stop_view();
        }
    }

    fn content(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.mode {
                    Mode::Share => self.share_panel(ui),
                    Mode::View => self.view_panel(ui),
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
        self.top_bar(ctx);
        self.side_bar(ctx);
        if self.capture.is_some() {
            self.poll_capture(ctx);
            ctx.request_repaint();
        }
        if self.share.is_some() {
            self.poll_share_audio();
            ctx.request_repaint();
        }
        if self.view.is_some() {
            self.poll_view(ctx);
            ctx.request_repaint();
        }
        self.content(ctx);
        if self.show_settings {
            self.settings_window(ctx);
        }
    }
}
