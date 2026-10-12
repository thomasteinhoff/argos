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
    // Both icons matter and they are different slots: this one is the window's
    // own icon (title bar, taskbar while running). Without it eframe substitutes
    // its own logo. The file icon Explorer reads is the resource embedded by
    // `build.rs` from the same artwork.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("assets/icon.png must be a valid PNG");
    let options = eframe::NativeOptions {
        // Undecorated: the OS caption bar is replaced by the one `ui` draws, so
        // the app has a single bar instead of two stacked ones. The window's icon
        // and title still matter — they are what the taskbar and Alt-Tab show.
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 520.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("Argos")
            .with_decorations(false)
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "Argos",
        options,
        Box::new(|cc| Ok(Box::new(ArgosApp::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    /// A bad icon would otherwise only surface as a panic at launch, on whatever
    /// machine happened to run the build. Decoding the embedded asset here keeps
    /// that failure in the test run.
    #[test]
    fn the_window_icon_is_a_valid_png() {
        eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
            .expect("assets/icon.png must be a valid PNG");
    }
}
