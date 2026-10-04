// Desktop entry point, only for trying the mobile UI with `tauri dev`; phones start in lib.rs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    popup_messenger_mobile_lib::run()
}
