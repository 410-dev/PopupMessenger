//! OS integration: autostart and the Explorer "Send using PopupMessenger" verb.

use crate::state::AppState;
use crate::ui;
use std::path::Path;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

/// Explorer launches the verb once per selected file; arrivals closer than this are one batch.
const BATCH_GAP: Duration = Duration::from_millis(600);

pub fn apply(app: &AppHandle) {
    let (auto, ctx) = {
        let s = app.state::<AppState>();
        let s = s.settings.lock().unwrap();
        (s.autostart, s.context_menu)
    };
    // Registering a debug build for login would be surprising; release only.
    if !cfg!(debug_assertions) {
        use tauri_plugin_autostart::ManagerExt;
        let al = app.autolaunch();
        let on = al.is_enabled().unwrap_or(false);
        if auto && !on {
            let _ = al.enable();
        } else if !auto && on {
            let _ = al.disable();
        }
    }
    let _ = set_context_menu(ctx);
}

#[cfg(windows)]
fn set_context_menu(enable: bool) -> std::io::Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let base = r"Software\Classes\*\shell\PopupMessenger";
    if !enable {
        let _ = hkcu.delete_subkey_all(base);
        return Ok(());
    }
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let (k, _) = hkcu.create_subkey(base)?;
    k.set_value("", &"Send using PopupMessenger")?;
    k.set_value("MultiSelectModel", &"Player")?;
    k.set_value("Icon", &exe)?;
    let (c, _) = hkcu.create_subkey(format!(r"{base}\command"))?;
    c.set_value("", &format!("\"{exe}\" --send \"%1\""))?;
    Ok(())
}

#[cfg(not(windows))]
fn set_context_menu(_enable: bool) -> std::io::Result<()> {
    Ok(())
}

/// Files following `--send` in a command line.
pub fn files_from_args(args: &[String], cwd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut on = false;
    for a in args.iter().skip(1) {
        if a == "--send" {
            on = true;
        } else if a.starts_with("--") {
            on = false;
        } else if on {
            let p = Path::new(cwd).join(a);
            if p.is_file() {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}

/// Collects files from successive launches, then opens one recipient picker.
pub fn queue_files(app: &AppHandle, files: Vec<String>) {
    if files.is_empty() {
        return;
    }
    let first = {
        let st = app.state::<AppState>();
        let mut p = st.pending.lock().unwrap();
        let first = p.0.is_empty();
        p.0.extend(files);
        p.1 = Some(Instant::now());
        first
    };
    if !first {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let batch = {
                let st = app.state::<AppState>();
                let mut p = st.pending.lock().unwrap();
                if p.1.map_or(true, |t| t.elapsed() >= BATCH_GAP) {
                    p.1 = None;
                    Some(std::mem::take(&mut p.0))
                } else {
                    None
                }
            };
            if let Some(mut f) = batch {
                f.sort();
                f.dedup();
                ui::open_picker(&app, f);
                break;
            }
        }
    });
}
