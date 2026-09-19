use eframe::egui::{self, Color32, RichText};

use crate::config::{self, AppConfig};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Share,
    View,
}

pub struct ArgosApp {
    config: AppConfig,
    mode: Mode,
    show_settings: bool,
    name_input: String,
    code_input: String,
}

impl ArgosApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config = config::load();
        Self {
            name_input: config.name.clone(),
            code_input: String::new(),
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
                    RichText::new("Idle")
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

    fn share_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Share your screen");
        ui.add_space(4.0);
        ui.label("One companion connects to you directly, peer to peer.");
        ui.label("No accounts. No servers. Nothing is routed through us.");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label("Monitor");
            ui.label(RichText::new("primary").weak());
        });
        ui.horizontal(|ui| {
            ui.label("Quality");
            ui.label(RichText::new("1080p - 30 fps").weak());
        });
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
        self.content(ctx);
        if self.show_settings {
            self.settings_window(ctx);
        }
    }
}