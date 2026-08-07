#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Headless commands (`--build-info`, `meeting process …`) are handled before
    // the GUI starts; `None` means launch the desktop app as usual.
    if let Some(code) = lumen_asr_desktop::maybe_run_cli() {
        std::process::exit(code);
    }
    lumen_asr_desktop::run();
}
