#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = nebula_desktop::gui::run() {
        eprintln!("NebulaDesk could not start: {error}");
        std::process::exit(1);
    }
}
