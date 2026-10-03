// Link as a GUI subsystem binary rather than a console one, so launching Argos
// does not open a cmd window behind the app window. Nothing here writes to
// stdout or stderr — the only output the console ever carried was a panic
// message, and the app reports its failures in the UI instead.
//
// Excluded from the test build on purpose: the test harness reports through
// stdout, and a GUI subsystem would swallow every result `cargo test` prints.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod config;
mod radmin;
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
