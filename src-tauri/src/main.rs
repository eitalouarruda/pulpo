// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// En Linux el mismo binario hace de proceso auxiliar de Chromium (renderer,
// GPU...): la macro despacha esos procesos antes de arrancar la app.
#[cfg_attr(
    any(target_os = "linux", target_os = "macos"),
    tauri_runtime_cef::cef_entry_point
)]
fn main() {
    pulpo_lib::run()
}
