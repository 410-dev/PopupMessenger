//! Every window loads the same page; the view object picks what it shows.
//!
//! Efficiency mode: windows are created on demand and destroyed on close, so an
//! idle app keeps no webview alive (a slow PC briefly shows a blank window).
//! Performance mode: one hidden "spare" window has the page loaded in advance.
//! Opening a window hands the view to the spare, which renders and then shows
//! itself, and a new spare is prepared in the background. Such windows keep
//! their spare label, so `aliases` maps logical labels to real ones.

use crate::state::AppState;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, LogicalSize, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// Delay before preparing the next spare, so it does not compete with the window being shown.
const REWARM_DELAY: Duration = Duration::from_millis(1500);

fn find(app: &AppHandle, label: &str) -> Option<WebviewWindow> {
    let real = app.state::<AppState>().aliases.lock().unwrap().get(label).cloned();
    app.get_webview_window(real.as_deref().unwrap_or(label))
}

fn open(app: &AppHandle, label: &str, view: Value, title: &str, size: (f64, f64), on_top: bool) {
    if let Some(w) = find(app, label) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    if let Some(w) = take_spare(app) {
        app.state::<AppState>().aliases.lock().unwrap().insert(label.to_string(), w.label().to_string());
        let _ = w.set_title(title);
        let _ = w.set_size(LogicalSize::new(size.0, size.1));
        let _ = w.set_always_on_top(on_top);
        #[cfg(target_os = "macos")]
        let _ = w.set_visible_on_all_workspaces(on_top);
        let _ = w.center();
        // The page renders the view, then shows and focuses its own window.
        let _ = w.eval(&format!("window.__boot({view});"));
        schedule_warm(app);
        return;
    }
    let b = WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))
        .title(title)
        .inner_size(size.0, size.1)
        .min_inner_size(260.0, 150.0)
        .center()
        .always_on_top(on_top)
        .focused(true)
        .initialization_script(&format!("window.__VIEW={view};"));
    // macOS: popups must also appear on the current Space / over full-screen apps.
    #[cfg(target_os = "macos")]
    let b = b.visible_on_all_workspaces(on_top);
    #[cfg(windows)]
    let b = b.disable_drag_drop_handler(); // see windrop.rs
    let b = profile_webview(app, b);
    let _w = b.build();
    #[cfg(windows)]
    if let Ok(w) = &_w {
        crate::windrop::attach(w);
    }
}

/// Test profiles (see `profile()`) keep their own WebView2 data folder, and
/// `POPUPMESSENGER_CDP_PORT` opens the remote debugging port so tests can drive the
/// windows. The other browser flags are wry's defaults, which this replaces.
fn profile_webview<'a>(app: &AppHandle, b: WebviewWindowBuilder<'a, tauri::Wry, AppHandle>) -> WebviewWindowBuilder<'a, tauri::Wry, AppHandle> {
    if crate::profile().is_none() {
        return b;
    }
    let b = b.data_directory(app.state::<AppState>().dir.join("webview"));
    #[cfg(windows)]
    if let Some(p) = std::env::var("POPUPMESSENGER_CDP_PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
        return b.additional_browser_args(&format!(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={p}"
        ));
    }
    b
}

fn take_spare(app: &AppHandle) -> Option<WebviewWindow> {
    let st = app.state::<AppState>();
    let mut spare = st.spare.lock().unwrap();
    match spare.as_ref() {
        Some((label, true)) => {
            let w = app.get_webview_window(label);
            *spare = None;
            w
        }
        _ => None,
    }
}

/// Creates the hidden spare window if performance mode is on and none exists.
pub fn warm(app: &AppHandle) {
    let st = app.state::<AppState>();
    if !st.settings.lock().unwrap().perf_mode {
        return;
    }
    let label = {
        let mut spare = st.spare.lock().unwrap();
        if spare.is_some() {
            return;
        }
        let label = format!("spare-{}", next(app));
        *spare = Some((label.clone(), false));
        label
    };
    let b = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
        .inner_size(400.0, 400.0)
        .min_inner_size(260.0, 150.0)
        .visible(false)
        .focused(false)
        .initialization_script("window.__SPARE=true;");
    #[cfg(windows)]
    let b = b.disable_drag_drop_handler(); // see windrop.rs
    let b = profile_webview(app, b);
    match b.build() {
        #[cfg(windows)]
        Ok(w) => crate::windrop::attach(&w),
        #[cfg(not(windows))]
        Ok(_) => {}
        Err(_) => *st.spare.lock().unwrap() = None,
    }
}

fn schedule_warm(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(REWARM_DELAY).await;
        warm(&app);
    });
}

/// Called by the spare page once its script is loaded.
pub fn spare_ready(app: &AppHandle, label: &str) {
    if let Some((l, ready)) = app.state::<AppState>().spare.lock().unwrap().as_mut() {
        if l == label {
            *ready = true;
        }
    }
}

/// Drops the spare when performance mode is turned off.
pub fn cool(app: &AppHandle) {
    let label = app.state::<AppState>().spare.lock().unwrap().take().map(|s| s.0);
    if let Some(w) = label.and_then(|l| app.get_webview_window(&l)) {
        let _ = w.destroy();
    }
}

/// Forgets aliases and spare state of a destroyed window.
pub fn forget(app: &AppHandle, real: &str) {
    let st = app.state::<AppState>();
    st.aliases.lock().unwrap().retain(|_, v| v != real);
    let mut spare = st.spare.lock().unwrap();
    if spare.as_ref().is_some_and(|s| s.0 == real) {
        *spare = None;
    }
}

fn next(app: &AppHandle) -> u32 {
    app.state::<AppState>().seq.fetch_add(1, Ordering::Relaxed)
}

pub fn close(app: &AppHandle, label: &str) {
    if let Some(w) = find(app, label) {
        let _ = w.close();
    }
}

pub fn open_popup(app: &AppHandle, id: &str) {
    let name = app
        .state::<AppState>()
        .data
        .lock()
        .unwrap()
        .inbox
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.from_name.clone())
        .unwrap_or_default();
    open(app, &format!("popup-{id}"), json!({ "v": "popup", "id": id }), &format!("쪽지 - {name}"), (380.0, 280.0), true);
}

pub fn open_sent(app: &AppHandle, id: &str) {
    open(app, &format!("sent-{id}"), json!({ "v": "sent", "id": id }), "보낸 쪽지", (380.0, 280.0), false);
}

/// `reply`: id of the inbox message being answered; it is quoted below the text.
pub fn open_compose(app: &AppHandle, to: Vec<String>, files: Vec<String>, reply: Option<String>) {
    let label = format!("compose-{}", next(app));
    let title = if reply.is_some() { "답장" } else { "쪽지 쓰기" };
    open(app, &label, json!({ "v": "compose", "to": to, "files": files, "reply": reply }), title, (420.0, 400.0), false);
}

pub fn open_group_compose(app: &AppHandle, group: &str) {
    let label = format!("compose-{}", next(app));
    open(app, &label, json!({ "v": "compose", "to": [], "files": [], "group": group }), "그룹 쪽지 쓰기", (420.0, 420.0), false);
}

pub fn open_search(app: &AppHandle) {
    open(app, "search", json!({ "v": "search" }), "연락처 검색", (380.0, 520.0), false);
}

pub fn open_groups(app: &AppHandle) {
    open(app, "groups", json!({ "v": "groups" }), "전송 그룹 관리", (380.0, 520.0), false);
}

pub fn open_picker(app: &AppHandle, files: Vec<String>) {
    let label = format!("picker-{}", next(app));
    let title = if files.is_empty() { "받는 사람".to_string() } else { format!("받는 사람 - 파일 {}개", files.len()) };
    open(app, &label, json!({ "v": "pick", "files": files }), &title, (300.0, 440.0), true);
}

pub fn open_inbox(app: &AppHandle) {
    open(app, "inbox", json!({ "v": "inbox" }), "쪽지함", (460.0, 500.0), false);
}

pub fn open_relay_setup(app: &AppHandle) {
    open(app, "relay-setup", json!({ "v": "relaysetup" }), "릴레이 서버 연결", (440.0, 560.0), false);
}

pub fn open_settings(app: &AppHandle) {
    open(app, "settings", json!({ "v": "settings" }), "설정", (400.0, 640.0), false);
}

pub fn open_receiving(app: &AppHandle, id: &str, from: &str, count: usize, total: u64) {
    open(
        app,
        &format!("recv-{id}"),
        json!({ "v": "recv", "id": id, "from": from, "count": count, "total": total }),
        "수신 중",
        (360.0, 170.0),
        true,
    );
}
