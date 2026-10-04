// The commands are called from Rust (`NativeExt`), not from the webview.
const COMMANDS: &[&str] = &[];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).android_path("android").ios_path("ios").build();

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    println!("cargo:rustc-check-cfg=cfg(mobile)");
    if os == "ios" || os == "android" {
        println!("cargo:rustc-cfg=mobile");
    }
}
