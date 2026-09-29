//! Windows are created on demand and destroyed on close, so an idle app keeps
//! no webview alive. Every window loads the same page; `window.__VIEW` picks the view.

use crate::state::AppState;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

fn open(app: &AppHandle, label: &str, view: Value, title: &str, size: (f64, f64), on_top: bool) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))
        .title(title)
        .inner_size(size.0, size.1)
        .min_inner_size(260.0, 150.0)
        .center()
        .always_on_top(on_top)
        .focused(true)
        .initialization_script(&format!("window.__VIEW={view};"))
        .build();
}

fn next(app: &AppHandle) -> u32 {
    app.state::<AppState>().seq.fetch_add(1, Ordering::Relaxed)
}

pub fn close(app: &AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
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

/// `reply`: id of the inbox message being answered; it is quoted below the text.
pub fn open_compose(app: &AppHandle, to: Vec<String>, files: Vec<String>, reply: Option<String>) {
    let label = format!("compose-{}", next(app));
    let title = if reply.is_some() { "답장" } else { "쪽지 쓰기" };
    open(app, &label, json!({ "v": "compose", "to": to, "files": files, "reply": reply }), title, (420.0, 400.0), false);
}

pub fn open_picker(app: &AppHandle, files: Vec<String>) {
    let label = format!("picker-{}", next(app));
    let title = if files.is_empty() { "받는 사람".to_string() } else { format!("받는 사람 - 파일 {}개", files.len()) };
    open(app, &label, json!({ "v": "pick", "files": files }), &title, (300.0, 440.0), true);
}

pub fn open_inbox(app: &AppHandle) {
    open(app, "inbox", json!({ "v": "inbox" }), "받은 쪽지", (440.0, 480.0), false);
}

pub fn open_settings(app: &AppHandle) {
    open(app, "settings", json!({ "v": "settings" }), "설정", (400.0, 560.0), false);
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
