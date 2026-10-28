//! SnowHush — neve interativa sobre o ambiente de trabalho do Windows.
//!
//! Executável de bandeja (sem janela principal). Ver `README.md`.

#![windows_subsystem = "windows"]

mod app;
mod autostart;
mod geometry;
mod overlay;
mod renderer;
mod settings;
mod sim;
mod surfaces;
mod tray;
mod winutil;

fn main() {
    std::panic::set_hook(Box::new(|info| {
        winutil::log(&format!("panic: {info}"));
    }));

    if std::env::args().any(|a| a == "--help" || a == "-h") {
        winutil::show_help();
        return;
    }

    if let Err(e) = app::run() {
        winutil::log(&format!("erro fatal: {e}"));
        winutil::error_box("SnowHush", &e);
    }
}
