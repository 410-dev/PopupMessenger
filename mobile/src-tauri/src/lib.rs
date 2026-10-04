//! PopupMessenger for Android and iOS. Unlike the desktop app there is no LAN:
//! the phone belongs to one relay server and talks to everyone through it, and it
//! cannot be used until it has joined one (see relay.rs for when it is connected).

#[path = "../../../src-tauri/src/e2ee.rs"]
mod e2ee;
mod relay;
#[path = "../../../src-tauri/src/relay_io.rs"]
mod relay_io;
mod store;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use store::*;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_pm_native::NativeExt;

pub struct App {
    pub store: Store,
    pub relay: relay::Relay,
}

type R<T> = Result<T, String>;

/// Everything the first screen needs.
#[tauri::command]
fn boot(app: AppHandle, st: State<App>) -> Value {
    json!({
        "settings": *st.store.settings.lock().unwrap(),
        "relay": relay::info(&app),
        "unread": st.store.unread(),
        "version": app.package_info().version.to_string(),
        "platform": relay::HOST,
    })
}

#[tauri::command]
fn foreground(app: AppHandle, on: bool) {
    relay::set_foreground(&app, on);
}

#[tauri::command]
async fn insets(app: AppHandle) {
    app.native().insets();
}

// ---------------------------------------------------------------- joining

#[tauri::command]
async fn probe(url: String) -> R<Value> {
    relay_io::probe(&url).await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn join(app: AppHandle, url: String, tls: bool, fingerprint: String, mode: String, user: String, pass: String, name: String) -> R<Value> {
    let st = app.state::<App>();
    let name = name.trim();
    if name.is_empty() {
        return Err("이름을 입력하세요".into());
    }
    st.store.settings.lock().unwrap().name = name.chars().take(40).collect();
    st.store.save_settings();
    relay::join(&app, &url, tls, &fingerprint, &mode, &user, &pass).await
}

#[tauri::command]
async fn unlink(app: AppHandle) {
    relay::unlink(&app).await;
}

#[tauri::command]
fn relay_info(app: AppHandle) -> Value {
    relay::info(&app)
}

#[tauri::command]
fn reconnect(app: AppHandle) {
    relay::reconnect(&app);
}

#[tauri::command]
fn refresh(app: AppHandle) {
    relay::refresh(&app);
}

#[tauri::command]
async fn hold(app: AppHandle, mode: Option<String>) -> R<String> {
    relay::hold(&app, mode.as_deref()).await
}

#[tauri::command]
fn accept_keys(app: AppHandle) {
    relay::accept_keys(&app);
}

// ---------------------------------------------------------------- people

#[tauri::command]
fn contacts(app: AppHandle) -> Vec<Value> {
    relay::contacts(&app)
}

#[tauri::command]
async fn lookup(app: AppHandle, query: String) -> R<Value> {
    if query.trim().is_empty() {
        return Err("사용자 이름을 입력하세요".into());
    }
    relay::lookup(&app, &query).await
}

/// Where a reply goes: the sender's account (all of their devices), or for "reply
/// all" every other account in the group too.
#[tauri::command]
fn reply_targets(app: AppHandle, st: State<App>, id: String, all: bool) -> R<Vec<Value>> {
    let m = st.store.data.lock().unwrap().inbox.iter().find(|m| m.id == id).cloned().ok_or("쪽지가 없습니다")?;
    let me = st.store.my_id();
    let my_acc = relay::my_account(&app);
    let mut out: Vec<String> = Vec::new();
    let sender = if !m.from.is_empty() { Some(m.from.clone()) } else { relay::account_of_device(&app, &m.from_device) };
    match sender {
        // A message from another of my devices: back to that device.
        Some(a) if a == my_acc && !m.from_device.is_empty() => out.push(format!("{a}/{}", m.from_device)),
        Some(a) if !a.is_empty() => out.push(a),
        _ => {}
    }
    if all {
        for mem in m.group.iter().flat_map(|g| g.members.iter()) {
            if mem.id == me {
                continue;
            }
            let acc = match mem.id.strip_prefix("r:") {
                Some(a) => Some(a.to_string()),
                None => relay::account_of_device(&app, &mem.id),
            };
            if let Some(a) = acc.filter(|a| !a.is_empty() && *a != my_acc) {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
    }
    Ok(out.into_iter().map(|t| json!({ "to": t, "name": relay::target_name(&app, &t) })).collect())
}

// ---------------------------------------------------------------- messages

#[tauri::command]
fn messages(st: State<App>) -> Value {
    let d = st.store.data.lock().unwrap();
    json!({ "inbox": d.inbox, "sent": d.sent })
}

#[tauri::command]
fn mark_read(app: AppHandle, st: State<App>, id: String) {
    let mut hit = false;
    if let Some(m) = st.store.data.lock().unwrap().inbox.iter_mut().find(|m| m.id == id && !m.read) {
        m.read = true;
        hit = true;
    }
    if hit {
        st.store.save_data();
        let _ = app.emit("messages", ());
    }
}

#[tauri::command]
fn mark_all_read(app: AppHandle, st: State<App>) {
    for m in st.store.data.lock().unwrap().inbox.iter_mut() {
        m.read = true;
    }
    st.store.save_data();
    let _ = app.emit("messages", ());
}

/// `kind`: "inbox" (also deletes its saved attachments) or "sent".
#[tauri::command]
fn delete_message(app: AppHandle, st: State<App>, kind: String, id: String) {
    let removed = {
        let mut d = st.store.data.lock().unwrap();
        if kind == "sent" {
            d.sent.retain(|s| s.id != id);
            None
        } else {
            let i = d.inbox.iter().position(|m| m.id == id);
            i.map(|i| d.inbox.remove(i))
        }
    };
    if let Some(m) = removed {
        remove_files(&m.files);
    }
    st.store.save_data();
    let _ = app.emit("messages", ());
}

// ---------------------------------------------------------------- attachments

/// Starts copying a picked file in from the webview (which only has it as a Blob).
#[tauri::command]
fn stage_open(st: State<App>, name: String) -> R<String> {
    let token = new_id();
    let dir = st.store.stage.join(&token);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = sanitize(&name);
    let path = dir.join(&name);
    std::fs::File::create(&path).map_err(|e| e.to_string())?;
    st.store.staged.lock().unwrap().insert(token.clone(), Staged { name, path, size: 0 });
    Ok(token)
}

/// Appends a base64 piece (the IPC on Android has no binary bodies).
#[tauri::command]
async fn stage_write(st: State<'_, App>, token: String, data: String) -> R<u64> {
    let bytes = B64.decode(data).map_err(|_| "잘못된 데이터".to_string())?;
    let mut g = st.store.staged.lock().unwrap();
    let s = g.get_mut(&token).ok_or("첨부파일이 없습니다")?;
    let mut f = std::fs::OpenOptions::new().append(true).open(&s.path).map_err(|e| e.to_string())?;
    f.write_all(&bytes).map_err(|e| if e.raw_os_error() == Some(28) { "저장 공간이 부족합니다".into() } else { e.to_string() })?;
    s.size += bytes.len() as u64;
    Ok(s.size)
}

#[tauri::command]
fn stage_drop(st: State<App>, token: String) {
    if let Some(s) = st.store.staged.lock().unwrap().remove(&token) {
        if let Some(dir) = s.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Opens, shares or saves a received attachment.
#[tauri::command]
async fn file_action(app: AppHandle, path: String, action: String) -> R<String> {
    let st = app.state::<App>();
    // Only files this app saved.
    let p = PathBuf::from(&path);
    if !p.starts_with(&st.store.files) {
        return Err("열 수 없는 파일입니다".into());
    }
    let n = app.native();
    match action.as_str() {
        "share" => n.share(&path).map(|_| String::new()),
        "save" => n.save(&path),
        _ => n.open(&path).map(|_| String::new()),
    }
}

// ---------------------------------------------------------------- sending

#[derive(Deserialize)]
struct Outgoing {
    job: String,
    /// "<account>" or "<account>/<device>".
    to: Vec<String>,
    text: String,
    /// Staged attachment tokens.
    files: Vec<String>,
    /// Show every recipient the others (their app offers "reply all").
    reveal: bool,
}

#[tauri::command]
async fn send(app: AppHandle, msg: Outgoing) -> R<Value> {
    let st = app.state::<App>();
    if msg.to.is_empty() {
        return Err("받는 사람을 고르세요".into());
    }
    if msg.text.trim().is_empty() && msg.files.is_empty() {
        return Err("내용을 입력하세요".into());
    }
    let staged: Vec<(PathBuf, String, u64)> = {
        let g = st.store.staged.lock().unwrap();
        let mut v = Vec::new();
        for t in &msg.files {
            let s = g.get(t).ok_or("첨부파일을 다시 고르세요")?;
            v.push((s.path.clone(), s.name.clone(), s.size));
        }
        v
    };
    let names: Vec<(String, String)> = msg.to.iter().map(|t| (t.clone(), relay::target_name(&app, t))).collect();
    let group = (msg.reveal && msg.to.len() > 1).then(|| GroupInfo {
        name: String::new(),
        members: names
            .iter()
            .map(|(t, n)| Member {
                // Device ids as the desktop app keeps them; "r:<account>" for a whole account.
                id: match t.split_once('/') {
                    Some((_, d)) => d.to_string(),
                    None => format!("r:{t}"),
                },
                name: n.clone(),
            })
            .collect(),
    });
    let res = relay::send(&app, &msg.job, &msg.to, &msg.text, &staged, group.as_ref()).await;
    // Kept after a failure so the same sheet can try again; dropped when it closes.
    if res.iter().all(|r| r.1.is_ok()) {
        for t in &msg.files {
            stage_drop(st.clone(), t.clone());
        }
    }
    let to: Vec<Recipient> = names
        .iter()
        .map(|(t, n)| {
            let r = res.iter().find(|r| &r.0 == t).map(|r| r.1.clone()).unwrap_or_else(|| Err("보내지 못했습니다".into()));
            Recipient { to: t.clone(), name: n.clone(), ok: r.is_ok(), via: r.clone().unwrap_or_default(), error: r.err().unwrap_or_default() }
        })
        .collect();
    let any_ok = to.iter().any(|r| r.ok);
    let sent = Sent {
        id: new_id(),
        to: to.clone(),
        text: msg.text.clone(),
        files: staged.iter().map(|(_, n, s)| FileEntry { name: n.clone(), size: *s, path: String::new() }).collect(),
        ts: now_ms(),
    };
    if any_ok {
        st.store.push_sent(sent);
        let _ = app.emit("messages", ());
    }
    Ok(json!({ "ok": any_ok, "to": to }))
}

// ---------------------------------------------------------------- settings

#[derive(Deserialize)]
struct SettingsIn {
    name: String,
    status: String,
    popup: bool,
    max_files: u32,
    max_file_mb: u64,
    signature: String,
}

#[tauri::command]
fn set_settings(app: AppHandle, st: State<App>, s: SettingsIn) -> R<Value> {
    let name = s.name.trim();
    if name.is_empty() {
        return Err("이름을 입력하세요".into());
    }
    {
        let mut cur = st.store.settings.lock().unwrap();
        cur.name = name.chars().take(40).collect();
        cur.status = match s.status.as_str() {
            "dnd" | "offline" => s.status.clone(),
            _ => "online".into(),
        };
        cur.popup = s.popup;
        cur.max_files = s.max_files.min(1000);
        cur.max_file_mb = s.max_file_mb.clamp(1, 100 * 1024);
        cur.signature = s.signature.chars().take(500).collect();
    }
    st.store.save_settings();
    relay::announce(&app);
    Ok(json!(*st.store.settings.lock().unwrap()))
}

/// Desktop builds exist to try the app with `tauri dev`. `POPUPMESSENGER_PROFILE=<name>`
/// runs another instance with its own data (two people on one PC), and on Windows
/// `POPUPMESSENGER_CDP_PORT` opens WebView2's remote debugging port so tests can drive it.
#[cfg(desktop)]
fn profile() -> Option<String> {
    std::env::var("POPUPMESSENGER_PROFILE")
        .ok()
        .filter(|p| !p.is_empty() && p.len() <= 32 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

#[cfg(desktop)]
fn desktop_window<'a>(
    w: tauri::WebviewWindowBuilder<'a, tauri::Wry, tauri::App>,
    dir: &std::path::Path,
) -> tauri::WebviewWindowBuilder<'a, tauri::Wry, tauri::App> {
    let w = w.title("PopupMessenger").inner_size(400.0, 800.0);
    if profile().is_none() {
        return w;
    }
    let w = w.title(format!("PopupMessenger ({})", profile().unwrap_or_default())).data_directory(dir.join("webview"));
    #[cfg(windows)]
    if let Some(p) = std::env::var("POPUPMESSENGER_CDP_PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
        return w.additional_browser_args(&format!("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={p}"));
    }
    w
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_pm_native::init())
        .setup(|app| {
            let path = app.path();
            let dir = path.app_data_dir()?;
            #[cfg(desktop)]
            let dir = match profile() {
                Some(p) => dir.with_file_name(format!("{}.profile-{p}", dir.file_name().unwrap_or_default().to_string_lossy())),
                None => dir,
            };
            // Received files go where the user can reach them: the Files app on iOS
            // (Documents), the app's own folder in shared storage on Android.
            #[cfg(mobile)]
            let files = path.document_dir().map(|d| d.join("PopupMessenger")).unwrap_or_else(|_| dir.join("received"));
            #[cfg(not(mobile))]
            let files = dir.join("received");
            #[cfg(mobile)]
            let stage = path.app_cache_dir().unwrap_or_else(|_| dir.join("cache")).join("stage");
            #[cfg(not(mobile))]
            let stage = dir.join("stage"); // per test profile
            let store = Store::open(dir.clone(), files, stage);
            let relay = relay::Relay::load(&dir);
            app.manage(App { store, relay });
            relay::start(app.handle().clone());
            let w = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default());
            #[cfg(desktop)]
            let w = desktop_window(w, &dir);
            w.build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            boot,
            foreground,
            insets,
            probe,
            join,
            unlink,
            relay_info,
            reconnect,
            refresh,
            hold,
            accept_keys,
            contacts,
            lookup,
            reply_targets,
            messages,
            mark_read,
            mark_all_read,
            delete_message,
            stage_open,
            stage_write,
            stage_drop,
            file_action,
            send,
            set_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running PopupMessenger");
}
