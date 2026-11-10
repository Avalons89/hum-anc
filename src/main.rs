// Release builds open as a plain window, without a console behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod dsp;
mod gui;
mod tray;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([440.0, 300.0])
            .with_min_inner_size([400.0, 260.0]),
        ..Default::default()
    };
    eframe::run_native(
        "hum-anc",
        options,
        Box::new(|cc| Ok(Box::new(gui::App::new(cc)))),
    )
}
