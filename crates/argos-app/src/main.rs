mod config;
mod ui;

use eframe::egui;

use ui::ArgosApp;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 520.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("Argos"),
        ..Default::default()
    };
    eframe::run_native(
        "Argos",
        options,
        Box::new(|cc| Ok(Box::new(ArgosApp::new(cc)))),
    )
}
