//! Relay client for the phone (docs/relay-protocol.md). The relay is the only network:
//! there is no LAN discovery and nothing runs in the background. The control
//! connection is open only while the app is on screen (plus a few seconds, so the
//! file picker or a quick app switch does not drop it). While the phone is away the
//! relay keeps its messages ("relay receive", 4.10), and they are fetched as soon as
//! the app comes back.

use crate::e2ee;
use crate::relay_io::*;
use crate::store::*;
use crate::App;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::timeout;

const REQ_TIMEOUT: Duration = Duration::from_secs(30);
const PING_EVERY: Duration = Duration::from_secs(30);
const CHUNK: usize = 256 * 1024;
const SEEN_CAP: usize = 500;
/// How long the connection outlives the app leaving the screen. iOS suspends apps
/// within seconds, so it logs off almost at once there.
#[cfg(target_os = "ios")]
const GRACE: Duration = Duration::from_secs(2);
#[cfg(not(target_os = "ios"))]
const GRACE: Duration = Duration::from_secs(8);

/// What other people's apps show as this device's computer name.
pub const HOST: &str = if cfg!(target_os = "android") {
    "Android"
} else if cfg!(target_os = "ios") {
    "iOS"
} else {
    "Mobile"
};

/// The relay this phone belongs to. Same shape as the desktop's relay.json.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    pub url: String,
    pub tls: bool,
    /// Pinned SHA-256 (hex) of the server's leaf certificate.
    pub fingerprint: String,
    pub server_id: String,
    pub server_name: String,
    pub server_version: String,
    pub account: String,
    pub username: Option<String>,
    pub policy: Value,
    pub contacts_since: i64,
    /// Account id -> contact entry (protocol 4.4).
    pub contacts: HashMap<String, Value>,
    /// Device id -> first public key seen (trust on first use).
    pub pins: HashMap<String, String>,
    /// Devices whose current key differs from the pin, until the user accepts it.
    pub key_changed: Vec<String>,
    /// Recently fetched server message ids, to drop redeliveries.
    pub seen: Vec<String>,
    pub features: Vec<String>,
    /// "allow", "device_off" or "all_off".
    pub hold: String,
}

/// Per-server device credentials, kept after unlinking so the device can log on again.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Creds {
    pub device_secret: String,
    pub secret_key: String,
    pub public_key: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct RelayFile {
    active: Option<RelayConfig>,
    keyring: HashMap<String, Creds>,
}

#[derive(Clone, Default, Serialize)]
pub struct ConnState {
    /// off | paused | connecting | online | pending | error | fatal
    pub state: &'static str,
    pub message: String,
}

#[derive(Default)]
pub struct Relay {
    pub cfg: Mutex<Option<RelayConfig>>,
    keyring: Mutex<HashMap<String, Creds>>,
    conn: Mutex<ConnState>,
    session: Mutex<Option<String>>,
    out: Mutex<Option<mpsc::UnboundedSender<Value>>>,
    waiting: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    rid: AtomicU64,
    /// Config changed or reconnect requested.
    wake: Notify,
    /// Name, status or limits changed.
    profile: Notify,
    inbox: tokio::sync::Mutex<()>,
    /// The app is on screen.
    foreground: AtomicBool,
    /// When it last left the screen.
    away_at: Mutex<Option<SystemTime>>,
    fg: Notify,
    /// Transfers in flight; the connection stays up for them.
    busy: AtomicUsize,
    /// Whether the app should keep running off screen until it has logged off.
    keep_running: AtomicBool,
}

impl Relay {
    pub fn load(dir: &Path) -> Self {
        let f: RelayFile = std::fs::read(dir.join("relay.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { cfg: Mutex::new(f.active), keyring: Mutex::new(f.keyring), foreground: AtomicBool::new(true), ..Default::default() }
    }

    pub fn save(&self, dir: &Path) {
        let f = RelayFile { active: self.cfg.lock().unwrap().clone(), keyring: self.keyring.lock().unwrap().clone() };
        let p = dir.join("relay.json");
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_vec(&f).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    pub fn linked(&self) -> bool {
        self.cfg.lock().unwrap().is_some()
    }

    pub fn online(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    fn policy(&self) -> Value {
        self.cfg.lock().unwrap().as_ref().map(|c| c.policy.clone()).unwrap_or(Value::Null)
    }

    fn creds(&self, server: &str) -> Option<Creds> {
        self.keyring.lock().unwrap().get(server).cloned()
    }

    fn has(&self, feature: &str) -> bool {
        self.cfg.lock().unwrap().as_ref().is_some_and(|c| c.features.iter().any(|f| f == feature))
    }
}

/// Keeps the connection up while a transfer runs.
struct Busy<'a>(&'a AtomicUsize);

impl<'a> Busy<'a> {
    fn new(n: &'a AtomicUsize) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Self(n)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn changed(app: &AppHandle) {
    let _ = app.emit("relay", ());
}

fn contacts_changed(app: &AppHandle) {
    let _ = app.emit("contacts", ());
}

fn set_conn(app: &AppHandle, state: &'static str, message: impl Into<String>) {
    *app.state::<App>().relay.conn.lock().unwrap() = ConnState { state, message: message.into() };
    changed(app);
}

// ---------------------------------------------------------------- lifecycle

/// The webview reports whether the app is on screen.
pub fn set_foreground(app: &AppHandle, on: bool) {
    let st = app.state::<App>();
    let was = st.relay.foreground.swap(on, Ordering::SeqCst);
    if was == on {
        return;
    }
    let away = {
        let mut a = st.relay.away_at.lock().unwrap();
        if on {
            a.take().and_then(|t| t.elapsed().ok())
        } else {
            *a = Some(SystemTime::now());
            None
        }
    };
    keep_running(app, !on && st.relay.online());
    st.relay.fg.notify_one();
    // Back after the grace period: the OS may have frozen the app with the
    // connection still open, so start over and fetch what arrived meanwhile.
    if on && st.relay.online() && away.is_some_and(|d| d > GRACE + Duration::from_secs(2)) {
        st.relay.wake.notify_one();
    }
}

/// iOS freezes the app as soon as it leaves the screen; without some background
/// time the logoff below never runs and the device stays online with a dead
/// connection. Only the latest wish is applied, from a worker thread: `foreground`
/// runs on the main thread, where a blocking plugin call would deadlock.
fn keep_running(app: &AppHandle, on: bool) {
    let st = app.state::<App>();
    if st.relay.keep_running.swap(on, Ordering::SeqCst) == on {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_pm_native::NativeExt;
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _g = ONE_AT_A_TIME.lock().unwrap();
        app.native().keep_running(app.state::<App>().relay.keep_running.load(Ordering::SeqCst));
    });
}

// ---------------------------------------------------------------- setup

/// Registers this device (or logs it on to an account) and makes the relay active.
/// `mode`: "device" (no account), "login" or "register".
pub async fn join(app: &AppHandle, url: &str, tls: bool, fingerprint: &str, mode: &str, user: &str, pass: &str) -> Result<Value, String> {
    let st = app.state::<App>();
    let pin = (!fingerprint.is_empty()).then_some(fingerprint);
    let (mut io, seen) = connect(url, tls, pin).await?;
    let h = hello(&mut io).await?;
    let server_id = h["server"]["id"].as_str().unwrap_or("").to_string();
    if server_id.is_empty() {
        return Err("서버 정보를 받지 못했습니다".into());
    }
    let creds = st.relay.creds(&server_id).unwrap_or_else(|| {
        let (sk, pk) = e2ee::keypair();
        Creds { device_secret: B64.encode(rand::random::<[u8; 32]>()), secret_key: sk, public_key: pk }
    });
    let mut req = login_req(&st, &creds);
    req["t"] = json!(if mode == "register" { "register" } else { "login" });
    if mode != "device" {
        req["user"] = json!(user.trim());
        req["pass"] = json!(pass);
    }
    write_frame(&mut io, &req).await?;
    let r = read_frame(&mut io, IO_TIMEOUT).await?;
    drop(io);
    let ok = r["t"] == "login";
    let pending = r["code"] == "pending";
    if !ok && !pending {
        return Err(err_text(&r));
    }
    st.relay.keyring.lock().unwrap().insert(server_id.clone(), creds);
    let username = r["username"].as_str().map(str::to_string).or_else(|| (mode != "device").then(|| user.trim().to_string()));
    *st.relay.cfg.lock().unwrap() = Some(RelayConfig {
        url: url.to_string(),
        tls,
        fingerprint: if seen.fingerprint.is_empty() { fingerprint.to_string() } else { seen.fingerprint },
        server_id,
        server_name: h["server"]["name"].as_str().unwrap_or("").to_string(),
        server_version: h["server"]["version"].as_str().unwrap_or("").to_string(),
        account: r["account"].as_str().unwrap_or("").to_string(),
        username,
        policy: h["policy"].clone(),
        ..Default::default()
    });
    st.relay.save(&st.store.dir);
    if !pending {
        set_conn(app, "connecting", "");
    } else {
        set_conn(app, "pending", err_text(&r));
    }
    st.relay.wake.notify_one();
    contacts_changed(app);
    Ok(json!({ "state": if ok { "online" } else { "pending" }, "message": if ok { String::new() } else { err_text(&r) } }))
}

/// Logs off and leaves the relay; the app goes back to its setup screen.
/// Device credentials are kept so the same device can be linked again.
pub async fn unlink(app: &AppHandle) {
    let st = app.state::<App>();
    if st.relay.online() {
        let _ = timeout(Duration::from_secs(3), request(app, json!({ "t": "logout" }))).await;
    }
    *st.relay.cfg.lock().unwrap() = None;
    st.relay.save(&st.store.dir);
    st.relay.wake.notify_one();
    changed(app);
    contacts_changed(app);
}

pub fn reconnect(app: &AppHandle) {
    app.state::<App>().relay.wake.notify_one();
}

/// Accepts every changed device key as the new pin.
pub fn accept_keys(app: &AppHandle) {
    let st = app.state::<App>();
    if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
        for d in std::mem::take(&mut c.key_changed) {
            if let Some(k) = device_key(c, &d) {
                c.pins.insert(d, k);
            }
        }
    }
    st.relay.save(&st.store.dir);
    changed(app);
    contacts_changed(app);
}

fn device_key(c: &RelayConfig, device: &str) -> Option<String> {
    c.contacts.values().flat_map(|e| e["devices"].as_array().cloned().unwrap_or_default()).find_map(|d| {
        (d["id"] == device).then(|| d["pubkey"].as_str().unwrap_or("").to_string()).filter(|k| !k.is_empty())
    })
}

fn device_name(c: &RelayConfig, device: &str) -> String {
    for e in c.contacts.values() {
        for d in e["devices"].as_array().into_iter().flatten() {
            if d["id"] == device {
                let n = d["name"].as_str().filter(|n| !n.is_empty()).or(e["name"].as_str()).unwrap_or(device);
                return n.to_string();
            }
        }
    }
    device.to_string()
}

/// Everything the settings screen shows.
pub fn info(app: &AppHandle) -> Value {
    let st = app.state::<App>();
    let conn = st.relay.conn.lock().unwrap().clone();
    let cfg = st.relay.cfg.lock().unwrap().clone();
    let Some(c) = cfg else { return json!({ "linked": false, "state": conn.state, "message": conn.message }) };
    let my_key = st.relay.creds(&c.server_id).map(|k| e2ee::fingerprint(&k.public_key)).unwrap_or_default();
    let changed: Vec<Value> = c
        .key_changed
        .iter()
        .map(|d| json!({ "device": d, "name": device_name(&c, d), "fingerprint": device_key(&c, d).map(|k| e2ee::fingerprint(&k)).unwrap_or_default() }))
        .collect();
    json!({
        "linked": true, "state": conn.state, "message": conn.message,
        "url": c.url, "tls": c.tls, "fingerprint": c.fingerprint, "server_id": c.server_id, "server_name": c.server_name,
        "server_version": c.server_version, "username": c.username, "account": c.account, "device": st.store.my_id(),
        "policy": c.policy, "my_key": my_key, "key_changed": changed, "features": c.features,
        "hold": if c.features.iter().any(|f| f == "hold") { json!(c.hold) } else { Value::Null },
    })
}

/// Reads, or with `mode` sets, whether the relay keeps messages for this device while
/// it is offline. On a phone that is how messages reach a closed app.
pub async fn hold(app: &AppHandle, mode: Option<&str>) -> Result<String, String> {
    let st = app.state::<App>();
    if !st.relay.has("hold") {
        return Err("이 릴레이 서버는 이 설정을 지원하지 않습니다. 서버를 업데이트하세요".into());
    }
    let mut req = json!({ "t": "hold" });
    if let Some(m) = mode {
        req["mode"] = json!(m);
    }
    let r = request(app, req).await?;
    if r["t"] != "hold" {
        return Err(err_text(&r));
    }
    let mode = r["mode"].as_str().unwrap_or("allow").to_string();
    if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
        c.hold = mode.clone();
    }
    st.relay.save(&st.store.dir);
    changed(app);
    Ok(mode)
}

// ---------------------------------------------------------------- control connection

enum Stop {
    /// Retry with backoff.
    Net(String),
    /// Waiting for admin approval.
    Pending(String),
    /// Will not succeed without the user or admin doing something.
    Fatal(String),
    /// The app left the screen.
    Away,
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(manager(app));
}

async fn manager(app: AppHandle) {
    let mut backoff = Duration::from_secs(2);
    loop {
        let st = app.state::<App>();
        let cfg = st.relay.cfg.lock().unwrap().clone();
        let Some(cfg) = cfg else {
            set_conn(&app, "off", "");
            st.relay.wake.notified().await;
            continue;
        };
        if !st.relay.foreground.load(Ordering::SeqCst) {
            set_conn(&app, "paused", "");
            tokio::select! {
                _ = st.relay.fg.notified() => {}
                _ = st.relay.wake.notified() => {}
            }
            continue;
        }
        set_conn(&app, "connecting", "");
        let r = session(&app, &cfg).await;
        keep_running(&app, false);
        *st.relay.session.lock().unwrap() = None;
        *st.relay.out.lock().unwrap() = None;
        st.relay.waiting.lock().unwrap().clear();
        contacts_changed(&app); // everyone now reads as unknown
        let wait = match r {
            Stop::Away => {
                backoff = Duration::from_secs(2);
                continue;
            }
            Stop::Net(m) => {
                set_conn(&app, "error", m);
                let w = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                w
            }
            Stop::Pending(m) => {
                set_conn(&app, "pending", m);
                Duration::from_secs(30)
            }
            Stop::Fatal(m) => {
                set_conn(&app, "fatal", m);
                Duration::from_secs(600)
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = st.relay.wake.notified() => { backoff = Duration::from_secs(2); }
            _ = st.relay.fg.notified() => { backoff = Duration::from_secs(2); }
        }
    }
}

fn login_req(st: &App, creds: &Creds) -> Value {
    json!({
        "t": "login", "rid": "l", "id": st.store.my_id(), "device_secret": creds.device_secret, "pubkey": creds.public_key,
        "profile": profile(st),
    })
}

fn profile(st: &App) -> Value {
    let s = st.store.settings.lock().unwrap();
    json!({ "name": s.name, "host": HOST, "status": s.status, "notify": s.popup,
            "max_files": s.max_files, "max_file_size": s.max_file_mb.saturating_mul(1024 * 1024) })
}

async fn session(app: &AppHandle, cfg: &RelayConfig) -> Stop {
    let st = app.state::<App>();
    let Some(creds) = st.relay.creds(&cfg.server_id) else {
        return Stop::Fatal("기기 인증 정보가 없습니다. 서버를 다시 연결하세요".into());
    };
    let (mut io, seen) = match connect(&cfg.url, cfg.tls, Some(&cfg.fingerprint)).await {
        Ok(x) => x,
        Err(e) if e.contains("인증서가 바뀌었습니다") => return Stop::Fatal(e),
        Err(e) => return Stop::Net(e),
    };
    let h = match hello(&mut io).await {
        Ok(h) => h,
        Err(e) => return Stop::Net(e),
    };
    if h["server"]["id"] != json!(cfg.server_id) {
        return Stop::Fatal("같은 주소에 다른 릴레이 서버가 있습니다. 서버를 다시 연결하세요".into());
    }
    if let Err(e) = write_frame(&mut io, &login_req(&st, &creds)).await {
        return Stop::Net(e);
    }
    let r = match read_frame(&mut io, IO_TIMEOUT).await {
        Ok(r) => r,
        Err(e) => return Stop::Net(e),
    };
    if r["t"] != "login" {
        let m = err_text(&r);
        return match r["code"].as_str().unwrap_or("") {
            "pending" => Stop::Pending(m),
            "locked" | "bad_request" => Stop::Net(m),
            _ => Stop::Fatal(m),
        };
    }
    {
        let mut c = st.relay.cfg.lock().unwrap();
        if let Some(c) = c.as_mut() {
            c.policy = h["policy"].clone();
            c.server_name = h["server"]["name"].as_str().unwrap_or(&c.server_name).to_string();
            c.server_version = h["server"]["version"].as_str().unwrap_or(&c.server_version).to_string();
            c.features = h["features"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(str::to_string)).collect();
            c.account = r["account"].as_str().unwrap_or(&c.account).to_string();
            if r["username"].is_string() {
                c.username = r["username"].as_str().map(str::to_string);
            }
            if seen.trusted && !seen.fingerprint.is_empty() {
                c.fingerprint = seen.fingerprint.clone(); // a CA-valid certificate may rotate
            }
        }
    }
    st.relay.save(&st.store.dir);
    *st.relay.session.lock().unwrap() = r["session"].as_str().map(str::to_string);

    let (mut rd, mut wr) = tokio::io::split(io);
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    *st.relay.out.lock().unwrap() = Some(tx.clone());
    let writer = tauri::async_runtime::spawn(async move {
        while let Some(v) = rx.recv().await {
            if write_frame(&mut wr, &v).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    set_conn(app, "online", "");
    {
        let a = app.clone();
        tauri::async_runtime::spawn(async move {
            sync_contacts(&a).await;
            let _ = hold(&a, None).await;
            process_inbox(&a).await;
        });
    }

    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut away: Option<Instant> = (!st.relay.foreground.load(Ordering::SeqCst)).then(Instant::now);
    let stop = loop {
        let deadline = away.map(|t| t + GRACE);
        tokio::select! {
            f = read_frame(&mut rd, PING_EVERY * 3) => {
                let f = match f { Ok(f) => f, Err(e) => break Stop::Net(e) };
                if f["t"] == "event" {
                    match f["e"].as_str().unwrap_or("") {
                        "inbox" => {
                            let a = app.clone();
                            tauri::async_runtime::spawn(async move { process_inbox(&a).await });
                        }
                        "contact" => apply_contacts(app, std::slice::from_ref(&f["contact"]), false),
                        "policy" => {
                            if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
                                c.policy = f["policy"].clone();
                            }
                            st.relay.save(&st.store.dir);
                            changed(app);
                        }
                        "kicked" => break Stop::Net(f["reason"].as_str().unwrap_or("서버가 연결을 끊었습니다").to_string()),
                        _ => {}
                    }
                } else if let Some(rid) = f["rid"].as_str() {
                    if let Some(w) = st.relay.waiting.lock().unwrap().remove(rid) {
                        let _ = w.send(f);
                    }
                }
            }
            _ = ping.tick() => {
                let _ = tx.send(json!({ "t": "ping", "rid": "p" }));
            }
            _ = st.relay.profile.notified() => {
                let _ = tx.send(json!({ "t": "profile", "rid": "pr", "profile": profile(&st) }));
            }
            _ = st.relay.wake.notified() => {
                st.relay.wake.notify_one(); // let the manager see it too
                break Stop::Net(String::new());
            }
            _ = st.relay.fg.notified() => {
                away = (!st.relay.foreground.load(Ordering::SeqCst)).then(Instant::now);
            }
            _ = async { match deadline { Some(d) => tokio::time::sleep_until(d.into()).await, None => std::future::pending().await } } => {
                if st.relay.busy.load(Ordering::SeqCst) > 0 {
                    away = Some(Instant::now() - GRACE + Duration::from_secs(5)); // look again shortly
                    continue;
                }
                // Signed off: senders now see this device as away, and the relay keeps its messages.
                let _ = tx.send(json!({ "t": "logout", "rid": "bye" }));
                break Stop::Away;
            }
        }
    };
    drop(tx);
    *st.relay.out.lock().unwrap() = None;
    *st.relay.session.lock().unwrap() = None;
    // Let queued frames (the logout) go out before the connection closes.
    let _ = timeout(Duration::from_secs(2), writer).await;
    stop
}

/// Sends a request on the control connection and waits for its reply.
async fn request(app: &AppHandle, mut v: Value) -> Result<Value, String> {
    let st = app.state::<App>();
    let rid = format!("c{}", st.relay.rid.fetch_add(1, Ordering::Relaxed));
    v["rid"] = json!(rid);
    let (tx, rx) = oneshot::channel();
    st.relay.waiting.lock().unwrap().insert(rid.clone(), tx);
    let sent = st.relay.out.lock().unwrap().as_ref().map(|o| o.send(v).is_ok()).unwrap_or(false);
    if !sent {
        st.relay.waiting.lock().unwrap().remove(&rid);
        return Err("릴레이 서버에 연결되어 있지 않습니다".into());
    }
    let r = timeout(REQ_TIMEOUT, rx).await.map_err(|_| "서버 응답 시간 초과".to_string())?.map_err(|_| "서버 연결이 끊어졌습니다".to_string())?;
    if r["t"] == "err" { Err(err_text(&r)) } else { Ok(r) }
}

/// Tells the relay about a changed name, status or limit.
pub fn announce(app: &AppHandle) {
    app.state::<App>().relay.profile.notify_one();
}

/// Pull to refresh.
pub fn refresh(app: &AppHandle) {
    if app.state::<App>().relay.online() {
        let a = app.clone();
        tauri::async_runtime::spawn(async move {
            sync_contacts(&a).await;
            process_inbox(&a).await;
        });
    } else {
        reconnect(app);
    }
}

// ---------------------------------------------------------------- contacts

/// Asks the server for an account by exact username or account id (needed on
/// "lookup" servers, where contacts only hold people you have talked to).
pub async fn lookup(app: &AppHandle, query: &str) -> Result<Value, String> {
    let r = request(app, json!({ "t": "lookup", "query": query.trim().trim_start_matches('@') })).await?;
    let entry = r["contact"].clone();
    let acc = entry["account"].as_str().ok_or("찾을 수 없습니다")?.to_string();
    apply_contacts(app, std::slice::from_ref(&entry), false);
    contacts(app).into_iter().find(|c| c["account"] == json!(acc)).ok_or_else(|| "찾을 수 없습니다".into())
}

async fn sync_contacts(app: &AppHandle) {
    let st = app.state::<App>();
    let since = st.relay.cfg.lock().unwrap().as_ref().map(|c| c.contacts_since).unwrap_or(0);
    let Ok(r) = request(app, json!({ "t": "contacts", "since": since })).await else { return };
    let list = r["list"].as_array().cloned().unwrap_or_default();
    apply_contacts(app, &list, r["full"] == true);
    if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
        c.contacts_since = r["now"].as_i64().unwrap_or(0);
    }
    st.relay.save(&st.store.dir);
}

fn apply_contacts(app: &AppHandle, list: &[Value], full: bool) {
    let st = app.state::<App>();
    {
        let mut g = st.relay.cfg.lock().unwrap();
        let Some(c) = g.as_mut() else { return };
        let old = if full { std::mem::take(&mut c.contacts) } else { c.contacts.clone() };
        for e in list {
            let Some(acc) = e["account"].as_str() else { continue };
            // A reply computed before a "contact" event we already applied is older.
            let newer = |o: &Value| o["updated"].as_i64().zip(e["updated"].as_i64()).is_some_and(|(a, b)| a > b);
            if let Some(o) = old.get(acc).filter(|o| newer(o)) {
                c.contacts.insert(acc.to_string(), o.clone());
                continue;
            }
            if e["removed"] == true {
                c.contacts.remove(acc);
                continue;
            }
            for d in e["devices"].as_array().into_iter().flatten() {
                let (Some(id), Some(k)) = (d["id"].as_str(), d["pubkey"].as_str()) else { continue };
                if k.is_empty() {
                    continue;
                }
                match c.pins.get(id) {
                    None => {
                        c.pins.insert(id.to_string(), k.to_string());
                    }
                    Some(p) if p != k && !c.key_changed.iter().any(|x| x == id) => c.key_changed.push(id.to_string()),
                    _ => {}
                }
            }
            c.contacts.insert(acc.to_string(), e.clone());
        }
    }
    st.relay.save(&st.store.dir);
    contacts_changed(app);
    changed(app); // key changes show in settings
}

/// "online", "dnd", "hold" (away, the relay keeps messages for it) or "off".
fn device_state(d: &Value, connected: bool) -> &'static str {
    let online = connected && d["online"] == true;
    if online && d["status"] == "dnd" {
        "dnd"
    } else if online {
        "online"
    } else if d["hold"] != false {
        "hold"
    } else {
        "off"
    }
}

fn rank(s: &str) -> u8 {
    match s {
        "online" => 3,
        "dnd" => 2,
        "hold" => 1,
        _ => 0,
    }
}

/// People on the relay, most reachable and most written to first. My own account is
/// listed too when I have other devices (they can be sent to one by one).
pub fn contacts(app: &AppHandle) -> Vec<Value> {
    let st = app.state::<App>();
    let connected = st.relay.online();
    let me = st.store.my_id();
    let Some(cfg) = st.relay.cfg.lock().unwrap().clone() else { return vec![] };
    let counts = st.store.data.lock().unwrap().counts.clone();
    let mut out: Vec<Value> = Vec::new();
    for (acc, e) in &cfg.contacts {
        if e["removed"] == true {
            continue;
        }
        let mine = *acc == cfg.account;
        let devices: Vec<Value> = e["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["id"] != json!(me))
            .map(|d| {
                let id = d["id"].as_str().unwrap_or("");
                json!({
                    "id": id,
                    "name": d["name"].as_str().filter(|n| !n.is_empty()).or(e["name"].as_str()).unwrap_or(""),
                    "host": d["host"].as_str().unwrap_or(""),
                    "state": device_state(d, connected),
                    "key_changed": cfg.key_changed.iter().any(|x| x == id),
                    "last_seen": d["last_seen"],
                })
            })
            .collect();
        if mine && devices.is_empty() {
            continue;
        }
        let state = devices.iter().map(|d| d["state"].as_str().unwrap_or("off")).max_by_key(|s| rank(s)).unwrap_or("off");
        out.push(json!({
            "account": acc,
            "username": e["username"].as_str().unwrap_or(""),
            "name": if mine { "내 다른 기기" } else { e["name"].as_str().unwrap_or("") },
            "me": mine,
            "state": state,
            "devices": devices,
            "count": counts.get(acc).copied().unwrap_or(0),
            "last_seen": e["last_seen"],
        }));
    }
    out.sort_by(|a, b| {
        let r = |v: &Value| rank(v["state"].as_str().unwrap_or(""));
        a["me"]
            .as_bool()
            .cmp(&b["me"].as_bool())
            .then(r(b).cmp(&r(a)))
            .then(b["count"].as_u64().cmp(&a["count"].as_u64()))
            .then_with(|| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()))
    });
    out
}

/// Display name of a recipient ("<account>" or "<account>/<device>").
pub fn target_name(app: &AppHandle, to: &str) -> String {
    let st = app.state::<App>();
    let cfg = st.relay.cfg.lock().unwrap();
    let Some(c) = cfg.as_ref() else { return to.to_string() };
    let (acc, dev) = match to.split_once('/') {
        Some((a, d)) => (a, Some(d)),
        None => (to, None),
    };
    match dev {
        Some(d) => device_name(c, d),
        None => c.contacts.get(acc).and_then(|e| e["name"].as_str()).unwrap_or(acc).to_string(),
    }
}

/// The account a device belongs to.
pub fn account_of_device(app: &AppHandle, device: &str) -> Option<String> {
    let st = app.state::<App>();
    let cfg = st.relay.cfg.lock().unwrap();
    let c = cfg.as_ref()?;
    c.contacts
        .iter()
        .find(|(_, e)| e["devices"].as_array().is_some_and(|ds| ds.iter().any(|d| d["id"] == json!(device))))
        .map(|(a, _)| a.clone())
}

pub fn my_account(app: &AppHandle) -> String {
    app.state::<App>().relay.cfg.lock().unwrap().as_ref().map(|c| c.account.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------- transfers

async fn transfer_conn(app: &AppHandle) -> Result<Stream, String> {
    let st = app.state::<App>();
    let cfg = st.relay.cfg.lock().unwrap().clone().ok_or("릴레이 서버가 설정되지 않았습니다")?;
    let session = st.relay.session.lock().unwrap().clone().ok_or("릴레이 서버에 연결되어 있지 않습니다")?;
    let (mut io, _) = connect(&cfg.url, cfg.tls, Some(&cfg.fingerprint)).await?;
    hello(&mut io).await?;
    write_frame(&mut io, &json!({ "t": "auth", "rid": "a", "session": session })).await?;
    let r = read_frame(&mut io, IO_TIMEOUT).await?;
    if r["t"] != "ok" {
        return Err(err_text(&r));
    }
    Ok(io)
}

pub fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Sends to every target ("<account>" or "<account>/<device>").
/// Per target: Ok("relay" | "stored") or Err(reason).
pub async fn send(
    app: &AppHandle,
    job: &str,
    targets: &[String],
    text: &str,
    files: &[(PathBuf, String, u64)],
    group: Option<&GroupInfo>,
) -> Vec<(String, Result<String, String>)> {
    let fail_all = |e: String| targets.iter().map(|t| (t.clone(), Err(e.clone()))).collect();
    let st = app.state::<App>();
    let _busy = Busy::new(&st.relay.busy);
    if !st.relay.online() {
        return fail_all("릴레이 서버에 연결되어 있지 않습니다".into());
    }
    let pol = st.relay.policy();
    let max_files = pol["relay_max_files"].as_u64().unwrap_or(0);
    let max_size = pol["relay_max_file_size"].as_u64().unwrap_or(0);
    if !files.is_empty() && max_files == 0 {
        return fail_all("릴레이 서버가 첨부파일을 중계하지 않습니다".into());
    }
    if files.len() as u64 > max_files {
        return fail_all(format!("릴레이로는 첨부파일을 {max_files}개까지 보낼 수 있습니다"));
    }
    let e2ee = pol["encryption"] == "e2ee";
    if let Some((_, n, _)) = files.iter().find(|f| (if e2ee { e2ee::cipher_len(f.2) } else { f.2 }) > max_size) {
        return fail_all(format!("'{n}' 파일이 릴레이 허용 크기({})를 넘습니다", human(max_size)));
    }
    if e2ee {
        sync_contacts(app).await; // latest device keys
    }

    let cfg = st.relay.cfg.lock().unwrap().clone().unwrap_or_default();
    let by_device = cfg.features.iter().any(|f| f == "device_target");
    let mut out: Vec<(String, Result<String, String>)> = Vec::new();
    // Target -> what goes in "to" (servers without device_target get the whole account).
    let mut wire: Vec<(String, String)> = Vec::new();
    let mut recipients: Vec<(String, String)> = Vec::new();
    for t in targets {
        let (acc, dev) = match t.split_once('/') {
            Some((a, d)) => (a.to_string(), Some(d.to_string())),
            None => (t.clone(), None),
        };
        if acc == cfg.account && (dev.is_none() || !by_device) {
            out.push((t.clone(), Err("이 서버에서는 내 다른 기기로 보낼 수 없습니다".into())));
            continue;
        }
        let to = match &dev {
            Some(d) if by_device => format!("{acc}/{d}"),
            _ => acc.clone(),
        };
        if e2ee {
            let mut devs = cfg.contacts.get(&acc).and_then(|e| e["devices"].as_array().cloned()).unwrap_or_default();
            if let (Some(d), true) = (&dev, by_device) {
                devs.retain(|x| x["id"] == json!(d));
            }
            if devs.iter().any(|d| cfg.key_changed.iter().any(|x| d["id"] == json!(x))) {
                out.push((t.clone(), Err("받는 사람의 보안 키가 바뀌었습니다. 설정에서 확인하세요".into())));
                continue;
            }
            if !devs.iter().any(|d| d["pubkey"].as_str().is_some_and(|k| !k.is_empty())) {
                out.push((t.clone(), Err("받는 사람의 보안 키를 알 수 없습니다".into())));
                continue;
            }
            for d in devs {
                if let (Some(id), Some(k)) = (d["id"].as_str(), d["pubkey"].as_str()) {
                    if !k.is_empty() && !recipients.iter().any(|r| r.0 == id) {
                        recipients.push((id.to_string(), k.to_string()));
                    }
                }
            }
        }
        wire.push((t.clone(), to));
    }
    if wire.is_empty() {
        return out;
    }
    let mut to_list: Vec<String> = Vec::new();
    for (_, to) in &wire {
        if !to_list.contains(to) {
            to_list.push(to.clone());
        }
    }

    let msg = new_id();
    let names: Vec<&str> = files.iter().map(|f| f.1.as_str()).collect();
    let mut plain = json!({ "text": text, "names": names });
    if let Some(g) = group {
        plain["group"] = json!(g);
    }
    let (body, key) = if e2ee {
        let Some(creds) = st.relay.creds(&cfg.server_id) else { return fail_all("기기 키가 없습니다".into()) };
        match e2ee::seal(&msg, &plain, &recipients, &creds.secret_key) {
            Ok((b, k)) => (b, Some(k)),
            Err(e) => return fail_all(e),
        }
    } else {
        plain["enc"] = json!("none");
        (plain, None)
    };
    let sizes: Vec<Value> = files.iter().map(|f| json!({ "size": if e2ee { e2ee::cipher_len(f.2) } else { f.2 } })).collect();

    let r: Result<Value, String> = async {
        let mut io = transfer_conn(app).await?;
        write_frame(&mut io, &json!({ "t": "send", "rid": "s", "msg": msg, "to": to_list, "files": sizes, "body": body })).await?;
        let ready = read_frame(&mut io, IO_TIMEOUT).await?;
        if ready["t"] != "ready" {
            return Ok(ready); // err with optional `rejected`
        }
        upload(app, job, &mut io, files, key.as_ref()).await?;
        read_frame(&mut io, IO_TIMEOUT * 4).await
    }
    .await;
    let r = match r {
        Ok(r) => r,
        Err(e) => {
            out.extend(wire.iter().map(|(t, _)| (t.clone(), Err(e.clone()))));
            return out;
        }
    };
    let results: Vec<Value> = r["result"].as_array().or(r["rejected"].as_array()).cloned().unwrap_or_default();
    for (t, to) in &wire {
        let res = results.iter().find(|x| x["to"] == json!(to));
        let v = match res {
            Some(x) if x["state"] == "delivered" => Ok("relay".to_string()),
            Some(x) if x["state"] == "stored" => Ok("stored".to_string()),
            Some(x) => Err(err_text(x)),
            None if r["t"] == "err" => Err(err_text(&r)),
            None => Err("서버 응답에 받는 사람이 없습니다".into()),
        };
        out.push((t.clone(), v));
    }
    out
}

async fn upload(app: &AppHandle, job: &str, io: &mut Stream, files: &[(PathBuf, String, u64)], key: Option<&e2ee::Key>) -> Result<(), String> {
    let total: u64 = files.iter().map(|f| f.2).sum();
    let mut done = 0u64;
    let mut last = Instant::now();
    let step = if key.is_some() { e2ee::CHUNK } else { CHUNK };
    let mut buf = vec![0u8; step];
    for (i, (path, _, size)) in files.iter().enumerate() {
        let mut f = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
        let mut sealer = key.map(|k| e2ee::FileSealer::new(k, i));
        let mut remain = *size;
        loop {
            let want = remain.min(step as u64) as usize;
            f.read_exact(&mut buf[..want]).await.map_err(|_| "전송 중 파일이 변경되었습니다".to_string())?;
            remain -= want as u64;
            let chunk = match sealer.as_mut() {
                Some(s) if remain == 0 => s.last(&buf[..want])?,
                Some(s) => s.next(&buf[..want])?,
                None => buf[..want].to_vec(),
            };
            timeout(IO_TIMEOUT, io.write_all(&chunk)).await.map_err(|_| "시간 초과".to_string())?.map_err(|e| e.to_string())?;
            done += want as u64;
            if last.elapsed() >= Duration::from_millis(200) {
                last = Instant::now();
                let _ = app.emit("progress", json!({ "job": job, "done": done, "total": total }));
            }
            if remain == 0 {
                break;
            }
        }
    }
    io.flush().await.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- receiving

async fn process_inbox(app: &AppHandle) {
    let st = app.state::<App>();
    let _one = st.relay.inbox.lock().await;
    let _busy = Busy::new(&st.relay.busy);
    let Ok(r) = request(app, json!({ "t": "inbox" })).await else { return };
    for item in r["list"].as_array().cloned().unwrap_or_default() {
        let mid = item["mid"].as_str().unwrap_or("").to_string();
        if let Err(e) = fetch(app, &mid).await {
            if st.relay.online() {
                set_conn(app, "online", format!("쪽지를 받지 못했습니다: {e}"));
            }
            break;
        }
    }
    let expired = r["expired"].as_array().cloned().unwrap_or_default();
    if !expired.is_empty() {
        for x in &expired {
            let name = x["from_name"].as_str().unwrap_or("");
            let n = x["count"].as_u64().unwrap_or(0);
            deliver(
                app,
                Message {
                    from: x["from"].as_str().unwrap_or("").to_string(),
                    from_name: name.to_string(),
                    ts: x["last"].as_u64().unwrap_or_else(now_ms),
                    note: format!("{name} 님이 보낸 쪽지 {n}개가 릴레이 서버 보관 기간이 지나 삭제되었습니다"),
                    ..Default::default()
                },
            );
        }
        let _ = request(app, json!({ "t": "expired_ack" })).await;
    }
}

async fn fetch(app: &AppHandle, mid: &str) -> Result<(), String> {
    let st = app.state::<App>();
    let mut io = transfer_conn(app).await?;
    let already = st.relay.cfg.lock().unwrap().as_ref().is_some_and(|c| c.seen.iter().any(|m| m == mid));
    if !already {
        write_frame(&mut io, &json!({ "t": "fetch", "rid": "f", "mid": mid })).await?;
        let h = read_frame(&mut io, IO_TIMEOUT).await?;
        if h["t"] != "fetch" {
            return if h["code"] == "not_found" { Ok(()) } else { Err(err_text(&h)) };
        }
        receive(app, &mut io, mid, &h).await?;
        if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
            c.seen.push(mid.to_string());
            let n = c.seen.len();
            if n > SEEN_CAP {
                c.seen.drain(..n - SEEN_CAP);
            }
        }
        st.relay.save(&st.store.dir);
    }
    write_frame(&mut io, &json!({ "t": "ack", "rid": "k", "mid": mid })).await?;
    let _ = read_frame(&mut io, IO_TIMEOUT).await?;
    Ok(())
}

/// Reads the attachments that follow a fetch header, saves them and delivers the message.
async fn receive(app: &AppHandle, io: &mut Stream, mid: &str, h: &Value) -> Result<(), String> {
    let st = app.state::<App>();
    let from = h["from"].as_str().unwrap_or("").to_string();
    let from_device = h["from_device"].as_str().unwrap_or("").to_string();
    let from_name = h["from_name"].as_str().unwrap_or("").to_string();
    let sizes: Vec<u64> = h["files"].as_array().into_iter().flatten().map(|f| f["size"].as_u64().unwrap_or(0)).collect();
    let mut note = String::new();
    let body = &h["body"];
    let encrypted = !(body["enc"] == "none" || body["enc"].is_null());

    // Decrypt (or read) text, names and group.
    let mut key = None;
    let plain = if !encrypted {
        body.clone()
    } else {
        let lookup = |st: &App| {
            st.relay.cfg.lock().unwrap().as_ref().and_then(|c| {
                let k = c.contacts.get(&from)?["devices"].as_array()?.iter().find(|d| d["id"] == json!(from_device))?["pubkey"].as_str()?.to_string();
                Some((k, c.key_changed.iter().any(|x| *x == from_device), st.relay.creds(&c.server_id)?))
            })
        };
        if lookup(&st).is_none() {
            sync_contacts(app).await;
        }
        match lookup(&st) {
            Some((sender, changed, creds)) => match e2ee::open(body, &st.store.my_id(), &creds.secret_key, &sender) {
                Ok((p, k)) => {
                    key = Some(k);
                    if changed {
                        note = "⚠ 보낸 사람의 보안 키가 바뀌었습니다".into();
                    }
                    p
                }
                Err(e) => {
                    note = e;
                    json!({})
                }
            },
            None => {
                note = "보낸 사람의 보안 키를 확인할 수 없어 내용을 열 수 없습니다".into();
                json!({})
            }
        }
    };
    let names: Vec<String> = plain["names"].as_array().map(|a| a.iter().map(|n| n.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();

    let base = st.store.files.clone();
    let dir = if sizes.is_empty() { base.clone() } else { message_dir(&base, &from_name) };
    let mut saved = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    for (i, &size) in sizes.iter().enumerate() {
        let name = names.get(i).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| format!("file{}", i + 1));
        // Undecryptable attachments are still read off the wire, then dropped.
        let target = if encrypted && key.is_none() { None } else { reserve_path(&dir, &sanitize(&name)) };
        let mut file = match &target {
            Some((tmp, _)) => Some(tokio::fs::OpenOptions::new().write(true).truncate(true).open(tmp).await.map_err(|e| e.to_string())?),
            None => None,
        };
        let mut opener = key.as_ref().map(|k| e2ee::FileOpener::new(k, i));
        let mut remain = size;
        let mut written = 0u64;
        let r: Result<(), String> = async {
            while remain > 0 {
                let want = match opener {
                    Some(_) => remain.min((e2ee::CHUNK + 16) as u64) as usize,
                    None => remain.min(CHUNK as u64) as usize,
                };
                if buf.len() < want {
                    buf.resize(want, 0);
                }
                timeout(IO_TIMEOUT, io.read_exact(&mut buf[..want])).await.map_err(|_| "시간 초과".to_string())?.map_err(|_| "연결이 끊어졌습니다".to_string())?;
                remain -= want as u64;
                let data = match opener.as_mut() {
                    Some(o) if remain == 0 => o.last(&buf[..want])?,
                    Some(o) => o.next(&buf[..want])?,
                    None => buf[..want].to_vec(),
                };
                if let Some(f) = file.as_mut() {
                    f.write_all(&data).await.map_err(|e| if e.raw_os_error() == Some(28) { "저장 공간이 부족합니다".into() } else { e.to_string() })?;
                }
                written += data.len() as u64;
            }
            if let Some(f) = file.as_mut() {
                f.flush().await.map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        .await;
        drop(file);
        match (r, target) {
            (Ok(()), Some((tmp, fin))) => {
                if tokio::fs::rename(&tmp, &fin).await.is_ok() {
                    saved.push(FileEntry {
                        name: fin.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        size: written,
                        path: fin.to_string_lossy().into_owned(),
                    });
                } else {
                    let _ = tokio::fs::remove_file(&tmp).await;
                }
            }
            (Ok(()), None) => {}
            (Err(e), t) => {
                if let Some((tmp, _)) = t {
                    let _ = tokio::fs::remove_file(&tmp).await;
                }
                remove_files(&saved);
                if dir != base {
                    let _ = std::fs::remove_dir(&dir);
                }
                return Err(e); // not acked; fetched again next time
            }
        }
    }
    if saved.is_empty() && dir != base {
        let _ = std::fs::remove_dir(&dir);
    }
    if h["files_expired"] == true {
        note = "첨부파일은 릴레이 서버 보관 기간이 지나 삭제되었습니다".into();
    }
    let group = serde_json::from_value::<GroupInfo>(plain["group"].clone()).ok().map(|mut g| {
        g.members.truncate(MAX_GROUP_MEMBERS);
        g
    });
    deliver(
        app,
        Message {
            mid: mid.to_string(),
            from,
            from_device,
            from_name,
            text: plain["text"].as_str().unwrap_or("").to_string(),
            files: saved,
            ts: h["ts"].as_u64().unwrap_or_else(now_ms),
            note,
            group,
            ..Default::default()
        },
    );
    Ok(())
}

fn deliver(app: &AppHandle, m: Message) {
    let st = app.state::<App>();
    st.store.push_message(m.clone());
    let quiet = {
        let s = st.store.settings.lock().unwrap();
        !s.popup || s.status == "dnd"
    };
    if !quiet && st.relay.foreground.load(Ordering::SeqCst) {
        use tauri_plugin_pm_native::NativeExt;
        app.native().haptic();
    }
    let _ = app.emit("message", json!({ "message": m, "popup": !quiet }));
}
