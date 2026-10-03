#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod e2ee;
mod net;
mod relay;
mod shell;
mod state;
mod tray;
mod ui;
#[cfg(windows)]
mod windrop;

use serde_json::{json, Value};
use state::{AppState, Group, GroupView, Message, PeerView, Sent, Settings};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};

// Commands that open windows are async: creating a window from a sync
// command deadlocks on Windows.

#[tauri::command]
fn me(st: State<AppState>) -> Value {
    json!({ "meta": st.my_meta(), "port": st.port, "os": std::env::consts::OS, "version": env!("CARGO_PKG_VERSION") })
}

#[tauri::command]
fn peers(st: State<AppState>) -> Vec<PeerView> {
    st.peers(false)
}

#[tauri::command]
fn get_settings(st: State<AppState>) -> Settings {
    st.settings.lock().unwrap().clone()
}

/// For the settings form: another user on the LAN already has this name.
#[tauri::command]
fn name_taken(st: State<AppState>, name: String) -> bool {
    let name = name.trim();
    !name.is_empty() && name != st.settings.lock().unwrap().name && st.name_in_use(name)
}

#[tauri::command]
async fn save_settings(app: AppHandle, s: Settings) -> Result<(), String> {
    let st = app.state::<AppState>();
    let name = s.name.trim().to_string();
    let renamed = !name.is_empty() && name != st.settings.lock().unwrap().name;
    if renamed && st.name_in_use(&name) {
        return Err(format!("같은 네트워크에 '{name}' 사용자가 이미 있습니다. 다른 이름을 써 주세요."));
    }
    {
        let mut cur = st.settings.lock().unwrap();
        let dir = s.download_dir.trim();
        *cur = Settings {
            id: cur.id.clone(),
            status: cur.status, // changed from the tray only; the form may be stale
            name_custom: cur.name_custom || renamed,
            name: if name.is_empty() { cur.name.clone() } else { name },
            download_dir: if dir.is_empty() { cur.download_dir.clone() } else { dir.to_string() },
            max_files: s.max_files.clamp(0, 10_000),
            max_file_gb: if s.max_file_gb.is_finite() { s.max_file_gb.clamp(0.0, 1.0e6) } else { cur.max_file_gb },
            scan_span: s.scan_span.clamp(1, 1000),
            ..s
        };
        std::fs::create_dir_all(&cur.download_dir).map_err(|e| format!("다운로드 폴더: {e}"))?;
    }
    st.save_settings();
    if st.settings.lock().unwrap().perf_mode {
        ui::warm(&app);
    } else {
        ui::cool(&app);
    }
    shell::apply(&app);
    tray::refresh(&app);
    let _ = app.emit("peers", ()); // display names depend on show_host
    st.scan.notify_one();
    relay::announce(&app);
    Ok(())
}

/// Kind and size of attachment paths for the compose window (folders are zipped when sent).
#[tauri::command]
fn stat_paths(paths: Vec<String>) -> Vec<Value> {
    paths
        .into_iter()
        .map(|p| match std::fs::metadata(&p) {
            Ok(m) => json!({ "path": p, "dir": m.is_dir(), "size": m.len(), "ok": true }),
            Err(_) => json!({ "path": p, "dir": false, "size": 0, "ok": false }),
        })
        .collect()
}

#[tauri::command]
async fn send_message(
    app: AppHandle,
    job: String,
    to: Vec<String>,
    text: String,
    files: Vec<String>,
    group: Option<String>,
    reveal: bool,
) -> Vec<net::SendResult> {
    net::send(app, job, to, text, files, group, reveal).await
}

#[tauri::command]
async fn open_compose(app: AppHandle, to: Vec<String>, files: Vec<String>, reply: Option<String>) {
    ui::open_compose(&app, to, files, reply);
}

#[tauri::command]
async fn open_group_compose(app: AppHandle, id: String) {
    ui::open_group_compose(&app, &id);
}

#[tauri::command]
fn groups(st: State<AppState>) -> Vec<GroupView> {
    st.groups()
}

/// Creates (empty id) or replaces a group.
#[tauri::command]
fn save_group(app: AppHandle, mut g: Group) -> Result<String, String> {
    g.name = g.name.trim().to_string();
    if g.name.is_empty() {
        return Err("그룹 이름을 입력하세요".into());
    }
    let st = app.state::<AppState>();
    let me = st.my_id();
    let mut seen = std::collections::HashSet::new();
    g.members.retain(|m| *m != me && seen.insert(m.clone()));
    if g.members.is_empty() {
        return Err("받는 사람을 한 명 이상 고르세요".into());
    }
    {
        let mut d = st.data.lock().unwrap();
        match d.groups.iter_mut().find(|x| !g.id.is_empty() && x.id == g.id) {
            Some(x) => *x = g.clone(),
            None => {
                g.id = state::new_id();
                d.groups.push(g.clone());
            }
        }
    }
    st.save_data();
    groups_changed(&app);
    Ok(g.id)
}

#[tauri::command]
fn delete_group(app: AppHandle, id: String) {
    let st = app.state::<AppState>();
    st.data.lock().unwrap().groups.retain(|g| g.id != id);
    st.save_data();
    groups_changed(&app);
}

fn groups_changed(app: &AppHandle) {
    tray::refresh(app);
    let _ = app.emit("groups", ());
}

#[tauri::command]
async fn open_groups(app: AppHandle) {
    ui::open_groups(&app);
}

#[tauri::command]
async fn open_picker(app: AppHandle, files: Vec<String>) {
    ui::open_picker(&app, files);
}

#[tauri::command]
async fn open_message(app: AppHandle, id: String) {
    ui::open_popup(&app, &id);
}

#[tauri::command]
async fn open_settings(app: AppHandle) {
    ui::open_settings(&app);
}

/// Returns a message and marks it read.
#[tauri::command]
fn get_message(app: AppHandle, id: String) -> Option<Message> {
    let st = app.state::<AppState>();
    let (m, changed) = {
        let mut d = st.data.lock().unwrap();
        let m = d.inbox.iter_mut().find(|m| m.id == id)?;
        let changed = !m.read;
        m.read = true;
        (m.clone(), changed)
    };
    if changed {
        st.save_data();
        tray::refresh(&app);
        let _ = app.emit("inbox", ());
    }
    Some(m)
}

#[tauri::command]
fn inbox(st: State<AppState>) -> Vec<Message> {
    st.data.lock().unwrap().inbox.clone()
}

#[tauri::command]
fn delete_message(app: AppHandle, id: String) {
    let st = app.state::<AppState>();
    st.data.lock().unwrap().inbox.retain(|m| m.id != id);
    st.save_data();
    tray::refresh(&app);
    let _ = app.emit("inbox", ());
}

#[tauri::command]
fn mark_all_read(app: AppHandle) {
    let st = app.state::<AppState>();
    st.data.lock().unwrap().inbox.iter_mut().for_each(|m| m.read = true);
    st.save_data();
    tray::refresh(&app);
    let _ = app.emit("inbox", ());
}

#[tauri::command]
fn sent_list(st: State<AppState>) -> Vec<Sent> {
    st.data.lock().unwrap().sent.clone()
}

#[tauri::command]
fn get_sent(st: State<AppState>, id: String) -> Option<Sent> {
    st.data.lock().unwrap().sent.iter().find(|s| s.id == id).cloned()
}

#[tauri::command]
fn delete_sent(app: AppHandle, id: String) {
    let st = app.state::<AppState>();
    st.data.lock().unwrap().sent.retain(|s| s.id != id);
    st.save_data();
    let _ = app.emit("sent", ());
}

#[tauri::command]
async fn open_sent(app: AppHandle, id: String) {
    ui::open_sent(&app, &id);
}

#[tauri::command]
fn spare_ready(app: AppHandle, window: tauri::WebviewWindow) {
    ui::spare_ready(&app, window.label());
}

#[tauri::command]
fn cancel_recv(st: State<AppState>, id: String) {
    if let Some(c) = st.cancels.lock().unwrap().get(&id) {
        c.store(true, Ordering::Relaxed);
    }
}

#[tauri::command]
fn open_path(app: AppHandle, path: String, reveal: bool) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let o = app.opener();
    let r = if reveal { o.reveal_item_in_dir(&path) } else { o.open_path(&path, None::<&str>) };
    r.map_err(|e| e.to_string())
}

/// Opens a link from a received message in the default browser. Message text is
/// untrusted, so only plain http(s) URLs with a host and no embedded credentials pass.
#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let u = tauri::Url::parse(&url).map_err(|_| "올바른 주소가 아닙니다".to_string())?;
    let safe = url.len() <= 2048
        && matches!(u.scheme(), "http" | "https")
        && u.host_str().is_some_and(|h| !h.is_empty())
        && u.username().is_empty()
        && u.password().is_none();
    if !safe {
        return Err("열 수 없는 주소입니다".into());
    }
    app.opener().open_url(u.as_str(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
async fn pick_files(app: AppHandle) -> Vec<String> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .file()
        .blocking_pick_files()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| f.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

#[tauri::command]
async fn pick_dir(app: AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let p = app.dialog().file().blocking_pick_folder()?.into_path().ok()?;
    Some(p.to_string_lossy().into_owned())
}

#[tauri::command]
fn rescan(app: AppHandle) {
    app.state::<AppState>().scan.notify_one();
    relay::refresh(&app);
}

#[tauri::command]
fn relay_info(app: AppHandle) -> Value {
    relay::info(&app)
}

/// Contacts the server at `url` and returns its identity and policy for the user to confirm.
#[tauri::command]
async fn relay_probe(url: String) -> Result<Value, String> {
    relay::probe(&url).await
}

/// `mode`: "device" (servers without accounts), "login" or "register".
#[tauri::command]
async fn relay_join(
    app: AppHandle,
    url: String,
    tls: bool,
    fingerprint: String,
    mode: String,
    user: String,
    pass: String,
) -> Result<Value, String> {
    relay::join(&app, &url, tls, &fingerprint, &mode, &user, &pass).await
}

#[tauri::command]
async fn relay_disconnect(app: AppHandle) {
    relay::disconnect(&app).await;
}

#[tauri::command]
fn relay_reconnect(app: AppHandle) {
    relay::reconnect(&app);
}

#[tauri::command]
fn relay_accept_keys(app: AppHandle) {
    relay::accept_keys(&app);
}

#[tauri::command]
async fn open_relay_setup(app: AppHandle) {
    ui::open_relay_setup(&app);
}

/// `POPUPMESSENGER_PROFILE=<name>` runs a separate instance for testing: its own
/// data folder, no single-instance hand-off, and no autostart/context-menu registration.
pub fn profile() -> Option<String> {
    std::env::var("POPUPMESSENGER_PROFILE")
        .ok()
        .filter(|p| !p.is_empty() && p.len() <= 32 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();

    let mut builder = tauri::Builder::default();
    if profile().is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            let files = shell::files_from_args(&argv, &cwd);
            if files.is_empty() {
                ui::open_inbox(app);
            } else {
                shell::queue_files(app, files);
            }
        }));
    }
    builder
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            me,
            peers,
            get_settings,
            name_taken,
            save_settings,
            send_message,
            open_compose,
            open_group_compose,
            groups,
            save_group,
            delete_group,
            open_groups,
            open_picker,
            open_message,
            open_settings,
            get_message,
            inbox,
            delete_message,
            mark_all_read,
            sent_list,
            get_sent,
            delete_sent,
            open_sent,
            spare_ready,
            cancel_recv,
            open_path,
            stat_paths,
            open_url,
            pick_files,
            pick_dir,
            rescan,
            relay_info,
            relay_probe,
            relay_join,
            relay_disconnect,
            relay_reconnect,
            relay_accept_keys,
            open_relay_setup
        ])
        .on_window_event(|w, e| {
            if let tauri::WindowEvent::Destroyed = e {
                ui::forget(w.app_handle(), w.label());
            }
        })
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let mut dir = app.path().app_data_dir()?;
            if let Some(p) = profile() {
                let name = format!("{}.profile-{p}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                dir = dir.with_file_name(name);
            }
            std::fs::create_dir_all(&dir)?;
            let mut settings = AppState::load_settings(&dir);
            if settings.auto_online {
                settings.status = state::Status::Online;
            }
            let dl = match profile() {
                Some(_) => dir.join("received"),
                None => app.path().download_dir().map(|d| d.join("PopupMessenger")).unwrap_or_else(|_| dir.join("received")),
            };
            let (tcp, udp, port) = net::bind().ok_or("55000~65500 범위에 사용 가능한 포트가 없습니다")?;
            app.manage(AppState::new(dir, port, settings, dl));
            app.state::<AppState>().save_settings();

            let h = app.handle().clone();
            shell::apply(&h);
            tray::create(&h)?;
            net::start(h.clone(), tcp, udp);
            relay::rebuild_peers(&h);
            relay::start(h.clone());
            shell::queue_files(&h, shell::files_from_args(&args, &cwd));
            ui::warm(&h);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start PopupMessenger")
        .run(|_app, e| match e {
            // Tray-only app: closing the last window must not quit.
            RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
            // macOS: files/folders handed over by LaunchServices ("Open With", drop on the app).
            #[cfg(target_os = "macos")]
            RunEvent::Opened { urls } => {
                let files = urls
                    .iter()
                    .filter_map(|u| u.to_file_path().ok())
                    .filter(|p| p.exists())
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                shell::queue_files(_app, files);
            }
            _ => {}
        });
}
