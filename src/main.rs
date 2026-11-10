// Release builds open as a plain window, without a console behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod dsp;
mod gui;
mod tray;

fn main() -> eframe::Result {
    let start_hidden = std::env::args().any(|arg| arg == autostart::TRAY_FLAG);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([460.0, 450.0])
            .with_min_inner_size([420.0, 420.0])
            .with_visible(!start_hidden)
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                    .expect("the bundled icon is a valid PNG"),
            ),
        // A restored older, smaller size would push the Start button off the window.
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "hum-anc",
        options,
        Box::new(move |cc| Ok(Box::new(gui::App::new(cc, start_hidden)))),
    )
}
