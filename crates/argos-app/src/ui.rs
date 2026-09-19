use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use argos_media::capture::{self, CaptureSession, MonitorInfo};

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
    fps_frames: u64,
    fps_last: Instant,
    fps: f32,
    preview_error: Option<String>,
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
            fps_frames: 0,
            fps_last: Instant::now(),
            fps: 0.0,
            preview_error: None,
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
                    .button(RichText::new(self.profile_label()).color(Color32::from_rgb(160, 190, 255)))
                    .on_hover_text("Your name, as friends will see it")
                    .clicked()
                {
                    self.name_input.clone_from(&self.config.name);
                    self.show_settings = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("offline").color(Color32::GRAY));
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
                ui.label(
                    RichText::new(if self.capture.is_some() { "Previewing" } else { "Idle" })
                        .color(Color32::from_rgb(150, 220, 150)),
                );
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

    fn toggle_preview(&mut self) {
        self.preview_error = None;
        if self.capture.is_some() {
            self.capture = None;
            self.preview = None;
            self.fps = 0.0;
            self.fps_frames = 0;
            return;
        }
        let Some(source) = self.monitors.get(self.selected_monitor) else {
            return;
        };
        let mut session = CaptureSession::new();
        match session.start(source) {
            Ok(()) => {
                self.fps_last = Instant::now();
                self.fps_frames = 0;
                self.fps = 0.0;
                self.capture = Some(session);
            }
            Err(error) => {
                self.preview_error = Some(error);
            }
        }
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.capture.as_ref().and_then(CaptureSession::latest) else {
            return;
        };
        self.fps_frames += 1;
        let now = Instant::now();
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
            Some(preview)
                if preview.width == frame.width && preview.height == frame.height =>
            {
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
            ui.label(
                RichText::new("No monitor found")
                    .color(Color32::from_rgb(220, 120, 120)),
            );
            return;
        }

        let selected = self.selected_monitor.min(self.monitors.len() - 1);
        egui::ComboBox::from_label("Monitor")
            .selected_text(Self::monitor_label(&self.monitors[selected]))
            .show_ui(ui, |ui| {
                for (index, monitor) in self.monitors.iter().enumerate() {
                    if ui
                        .selectable_label(self.selected_monitor == index, Self::monitor_label(monitor))
                        .clicked()
                    {
                        self.selected_monitor = index;
                    }
                }
            });

        ui.add_space(10.0);
        let preview_label = if self.capture.is_some() {
            "Stop preview"
        } else {
            "Preview"
        };
        if ui.button(preview_label).clicked() {
            self.toggle_preview();
        }
        if let Some(error) = &self.preview_error {
            ui.label(
                RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)),
            );
        }

        if self.capture.is_some() {
            if let Some(preview) = &self.preview {
                ui.label(format!(
                    "Preview {}x{} @ {:.0} fps",
                    preview.width, preview.height, self.fps
                ));
            }
            ui.add_space(8.0);
            if let Some(preview) = &self.preview {
                let available = ui.available_size();
                if available.x > 0.0 && available.y > 0.0 {
                    let scale = (available.x / preview.width as f32)
                        .min(available.y / preview.height as f32);
                    let size = egui::vec2(
                        preview.width as f32 * scale,
                        preview.height as f32 * scale,
                    );
                    ui.image((preview.texture.id(), size));
                }
            }
        }

        ui.add_space(12.0);
        if ui
            .add_enabled(false, egui::Button::new("Start sharing"))
            .on_hover_text("Streaming arrives in a later milestone")
            .clicked()
        {}
    }

    fn view_panel(&mut self, ui: &mut egui::Ui) {
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
        if ui
            .add_enabled(false, egui::Button::new("Connect"))
            .on_hover_text("Connections arrive in a later milestone")
            .clicked()
        {}
        ui.add_space(16.0);
        ui.separator();
        ui.label(RichText::new("Recent").weak());
        ui.add_space(4.0);
        ui.label(RichText::new("No contacts yet").weak());
    }

    fn content(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| match self.mode {
            Mode::Share => self.share_panel(ui),
            Mode::View => self.view_panel(ui),
        });
    }
}

impl eframe::App for ArgosApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_visuals(egui::Visuals::dark());
        self.top_bar(ctx);
        self.side_bar(ctx);
        if self.capture.is_some() {
            self.poll_preview(ctx);
            ctx.request_repaint();
        }
        self.content(ctx);
        if self.show_settings {
            self.settings_window(ctx);
        }
    }
}