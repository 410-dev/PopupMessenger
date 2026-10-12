//! Relay client for the phone (docs/relay-protocol.md). The relay is the only network:
//! there is no LAN discovery and nothing runs in the background. The phone may be
//! linked to several relays at once: each connection is a (server, tenancy) pair
//! (protocol 8.2) with its own device secret, session, contacts and inbox, and the
//! same device id on all of them. A control connection is open only while the app
//! is on screen (plus a few seconds, so the file picker or a quick app switch does
//! not drop it). While the phone is away the relay keeps its messages ("relay
//! receive", 4.10), and they are fetched as soon as the app comes back.

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
use std::sync::{Arc, Mutex};
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

/// One relay connection: a server and a tenancy on it. Same shape as the desktop's
/// relay.json entry, plus the tenancy fields.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// Local id of the connection; messages and recipients refer to it.
    pub id: String,
    pub url: String,
    pub tls: bool,
    /// Pinned SHA-256 (hex) of the server's leaf certificate.
    pub fingerprint: String,
    pub server_id: String,
    pub server_name: String,
    pub server_version: String,
    /// The server's name in addresses (`hello` server.domain).
    pub domain: String,
    /// Tenancy ID. Empty for a connection saved before tenancies: its `hello` names
    /// none, and the tenancy the server logs it on to is stored afterwards.
    pub tenancy: String,
    pub tenancy_name: String,
    /// This device's address (protocol 9).
    pub address: String,
    pub account: String,
    pub username: Option<String>,
    pub policy: Value,
    pub contacts_since: i64,
    /// Account id -> contact entry (protocol 4.4).
    pub contacts: HashMap<String, Value>,
    /// People of other tenancies, from `resolve` (their device keys): account id -> entry.
    pub external: HashMap<String, Value>,
    /// The tenancy's departments (9.2): {id, name, members}.
    pub departments: Vec<Value>,
    /// Device id (or "<tenancy>/<device>" for other tenancies) -> first public key seen.
    pub pins: HashMap<String, String>,
    /// Devices whose current key differs from the pin, until the user accepts it.
    pub key_changed: Vec<String>,
    /// Recently fetched server message ids, to drop redeliveries.
    pub seen: Vec<String>,
    pub features: Vec<String>,
    /// "allow", "device_off" or "all_off".
    pub hold: String,
}

impl RelayConfig {
    fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// What the UI calls this connection: the tenancy, else the server.
    fn label(&self) -> String {
        [&self.tenancy_name, &self.tenancy, &self.server_name, &self.url].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default()
    }

    /// Pin key of a device: its id in this tenancy, "<tenancy>/<id>" in another.
    fn pin_key(&self, tenancy: &str, device: &str) -> String {
        if tenancy.is_empty() || tenancy == self.tenancy { device.to_string() } else { format!("{tenancy}/{device}") }
    }

    /// The entry of an account, in the contacts or among other tenancies' people.
    fn entry(&self, account: &str) -> Option<&Value> {
        self.contacts.get(account).or_else(|| self.external.get(account))
    }
}

/// Per-server (and per-tenancy) device credentials, kept after unlinking so the
/// device can log on again.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Creds {
    pub device_secret: String,
    pub secret_key: String,
    pub public_key: String,
}

fn cred_key(server: &str, tenancy: &str) -> String {
    if tenancy.is_empty() { server.to_string() } else { format!("{server}/{tenancy}") }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct RelayFile {
    /// The single relay of versions before 0.2; moved into `connections` on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<RelayConfig>,
    connections: Vec<RelayConfig>,
    keyring: HashMap<String, Creds>,
}

#[derive(Clone, Default, Serialize)]
pub struct ConnState {
    /// off | paused | connecting | online | pending | error | fatal
    pub state: &'static str,
    pub message: String,
}

/// A connection and its live state.
pub struct Conn {
    pub id: String,
    pub cfg: Mutex<RelayConfig>,
    state: Mutex<ConnState>,
    session: Mutex<Option<String>>,
    out: Mutex<Option<mpsc::UnboundedSender<Value>>>,
    waiting: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// Config changed, reconnect requested or removed.
    wake: Notify,
    /// The app came on or left the screen.
    fg: Notify,
    /// Name, status or limits changed.
    profile: Notify,
    inbox: tokio::sync::Mutex<()>,
    /// Transfers in flight; the connection stays up for them.
    busy: AtomicUsize,
    removed: AtomicBool,
}

impl Conn {
    fn new(cfg: RelayConfig) -> Arc<Self> {
        Arc::new(Self {
            id: cfg.id.clone(),
            cfg: Mutex::new(cfg),
            state: Mutex::new(ConnState { state: "off", message: String::new() }),
            session: Mutex::new(None),
            out: Mutex::new(None),
            waiting: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            fg: Notify::new(),
            profile: Notify::new(),
            inbox: tokio::sync::Mutex::new(()),
            busy: AtomicUsize::new(0),
            removed: AtomicBool::new(false),
        })
    }

    pub fn online(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    fn cfg(&self) -> RelayConfig {
        self.cfg.lock().unwrap().clone()
    }

    fn has(&self, feature: &str) -> bool {
        self.cfg.lock().unwrap().has(feature)
    }
}

#[derive(Default)]
pub struct Relay {
    conns: Mutex<Vec<Arc<Conn>>>,
    keyring: Mutex<HashMap<String, Creds>>,
    rid: AtomicU64,
    /// The app is on screen.
    foreground: AtomicBool,
    /// When it last left the screen.
    away_at: Mutex<Option<SystemTime>>,
    /// Whether the app should keep running off screen until it has logged off.
    keep_running: AtomicBool,
}

impl Relay {
    pub fn load(dir: &Path) -> Self {
        let f: RelayFile = std::fs::read(dir.join("relay.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut list = f.connections;
        // The one relay of older versions becomes the first connection, without a
        // tenancy until the server says which one it logged on to.
        if let Some(c) = f.active {
            list.insert(0, c);
        }
        let conns = list
            .into_iter()
            .map(|mut c| {
                if c.id.is_empty() {
                    c.id = new_id();
                }
                Conn::new(c)
            })
            .collect();
        Self { conns: Mutex::new(conns), keyring: Mutex::new(f.keyring), foreground: AtomicBool::new(true), ..Default::default() }
    }

    pub fn save(&self, dir: &Path) {
        let connections: Vec<RelayConfig> = self.list().iter().map(|c| c.cfg()).collect();
        let f = RelayFile { active: None, connections, keyring: self.keyring.lock().unwrap().clone() };
        let p = dir.join("relay.json");
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_vec(&f).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    pub fn list(&self) -> Vec<Arc<Conn>> {
        self.conns.lock().unwrap().clone()
    }

    /// A connection by id; an empty id (messages saved before connections had ids)
    /// means the first one.
    pub fn get(&self, id: &str) -> Option<Arc<Conn>> {
        let g = self.conns.lock().unwrap();
        if id.is_empty() { g.first().cloned() } else { g.iter().find(|c| c.id == id).cloned() }
    }

    pub fn linked(&self) -> bool {
        !self.conns.lock().unwrap().is_empty()
    }

    fn any_online(&self) -> bool {
        self.list().iter().any(|c| c.online())
    }

    /// The connection to (server, tenancy), if there is one.
    fn find(&self, server: &str, tenancy: &str, except: &str) -> Option<Arc<Conn>> {
        self.list().into_iter().find(|c| {
            let g = c.cfg.lock().unwrap();
            c.id != except && g.server_id == server && g.tenancy == tenancy
        })
    }

    /// The device's credentials for a tenancy; the server-wide ones of a connection
    /// saved before tenancies go along when its account moves (protocol 8.3).
    fn creds(&self, server: &str, tenancy: &str) -> Option<Creds> {
        let k = self.keyring.lock().unwrap();
        k.get(&cred_key(server, tenancy)).or_else(|| k.get(server)).cloned()
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

fn set_conn(app: &AppHandle, conn: &Conn, state: &'static str, message: impl Into<String>) {
    *conn.state.lock().unwrap() = ConnState { state, message: message.into() };
    changed(app);
}

fn save(app: &AppHandle) {
    let st = app.state::<App>();
    st.relay.save(&st.store.dir);
}

fn not_connected() -> String {
    "릴레이 서버에 연결되어 있지 않습니다".into()
}

/// Tenancy IDs as the server makes them: 2-32 of a-z, 0-9 and '-'.
pub fn valid_tenancy(t: &str) -> bool {
    (2..=32).contains(&t.len())
        && t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !t.starts_with('-')
        && !t.ends_with('-')
}

/// (tenancy, server) of an address `<id>[:<device>]@[<departments>:]<tenancy>:<server>`;
/// the server may carry a port.
pub fn address_parts(a: &str) -> Option<(String, String)> {
    let (_, right) = a.trim().rsplit_once('@')?;
    let mut parts: Vec<String> = right.split(':').map(|s| s.to_lowercase()).collect();
    if parts.len() >= 3 && parts.last().is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
        let port = parts.pop().unwrap_or_default();
        let host = parts.pop().unwrap_or_default();
        parts.push(format!("{host}:{port}"));
    }
    match parts.as_slice() {
        [.., t, s] if !t.is_empty() && !s.is_empty() => Some((t.clone(), s.clone())),
        _ => None,
    }
}

fn host_of(server: &str) -> &str {
    match server.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => server,
    }
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
    keep_running(app, !on && st.relay.any_online());
    for c in st.relay.list() {
        c.fg.notify_one();
        // Back after the grace period: the OS may have frozen the app with the
        // connection still open, so start over and fetch what arrived meanwhile.
        if on && c.online() && away.is_some_and(|d| d > GRACE + Duration::from_secs(2)) {
            c.wake.notify_one();
        }
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

/// `hello` naming the tenancy (none for connections saved before tenancies).
async fn hello(io: &mut Stream, tenancy: &str) -> Result<Value, String> {
    let mut req = json!({ "t": "hello", "rid": "h", "proto": PROTO, "app": env!("CARGO_PKG_VERSION") });
    if !tenancy.is_empty() {
        req["tenancy"] = json!(tenancy);
    }
    write_frame(io, &req).await?;
    let h = read_frame(io, IO_TIMEOUT).await?;
    if h["t"] != "hello" {
        if h["code"] == "unknown_tenancy" {
            return Err(format!("이 서버에 '{tenancy}' Tenancy가 없습니다. 관리자에게 받은 Tenancy ID를 확인하세요"));
        }
        return Err(h["reason"].as_str().unwrap_or("릴레이 서버가 아닙니다").to_string());
    }
    if let Some(n) = h["transport"]["ws_chunk"].as_u64() {
        WS_CHUNK.store(n as usize, Ordering::Relaxed);
    }
    Ok(h)
}

/// Connects to a new relay without trusting anything yet and returns what the user
/// must confirm (server, tenancy, certificate).
pub async fn probe(app: &AppHandle, addr: &str, tenancy: &str) -> Result<Value, String> {
    let tenancy = tenancy.trim().to_lowercase();
    if tenancy.is_empty() {
        return Err("Tenancy ID를 입력하세요".into());
    }
    if !valid_tenancy(&tenancy) {
        return Err("Tenancy ID는 영문 소문자, 숫자, '-'로 된 2~32자입니다".into());
    }
    let ep = parse_endpoint(addr)?;
    let url = ep.to_url();
    let (tls, mut io, seen) = match ep.tls {
        Some(tls) => {
            let (io, seen) = connect(&url, tls, None).await?;
            (tls, io, seen)
        }
        None => match connect(&url, true, None).await {
            Ok((io, seen)) => (true, io, seen),
            Err(tls_err) if tls_err.contains("찾을 수 없습니다") => return Err(tls_err),
            Err(tls_err) => match connect(&url, false, None).await {
                Ok((io, seen)) => (false, io, seen),
                Err(_) => return Err(tls_err),
            },
        },
    };
    let h = hello(&mut io, &tenancy).await?;
    // A plain connection to a TLS-only server gets tls_required right after hello.
    if !tls {
        if let Ok(e) = read_frame(&mut io, Duration::from_millis(500)).await {
            if e["code"] == "tls_required" {
                return Err(if ep.ws {
                    "서버가 이 연결을 암호화된 연결로 인정하지 않습니다. https:// 주소를 쓰거나, 관리 화면에서 \"프록시가 TLS 처리\"를 켜세요".into()
                } else {
                    "서버가 암호화 연결만 받는데 암호화 연결에 실패했습니다".into()
                });
            }
        }
    }
    let server_id = h["server"]["id"].as_str().unwrap_or("");
    let supports = h["tenancy"]["id"].is_string();
    let tn = if supports { h["tenancy"]["id"].as_str().unwrap_or("") } else { "" };
    if app.state::<App>().relay.find(server_id, tn, "").is_some() {
        return Err("이미 연결된 서버와 Tenancy입니다".into());
    }
    Ok(json!({
        "url": url, "tls": tls, "ws": ep.ws, "fingerprint": seen.fingerprint, "trusted": seen.trusted,
        "server": h["server"], "policy": h["policy"], "tenancy": if supports { h["tenancy"].clone() } else { Value::Null },
    }))
}

/// Registers this device (or logs it on to an account) on a relay and adds the
/// connection. `mode`: "device" (no account), "login" or "register".
#[allow(clippy::too_many_arguments)]
pub async fn join(app: &AppHandle, url: &str, tls: bool, fingerprint: &str, tenancy: &str, mode: &str, user: &str, pass: &str) -> Result<Value, String> {
    let st = app.state::<App>();
    let tenancy = tenancy.trim().to_lowercase();
    if !valid_tenancy(&tenancy) {
        return Err("Tenancy ID는 영문 소문자, 숫자, '-'로 된 2~32자입니다".into());
    }
    let pin = (!fingerprint.is_empty()).then_some(fingerprint);
    let (mut io, seen) = connect(url, tls, pin).await?;
    let h = hello(&mut io, &tenancy).await?;
    let server_id = h["server"]["id"].as_str().unwrap_or("").to_string();
    if server_id.is_empty() {
        return Err("서버 정보를 받지 못했습니다".into());
    }
    // A server without tenancies ignores the ID; the connection then works like one
    // saved before tenancies.
    let tenancy = if h["tenancy"]["id"].is_string() { tenancy } else { String::new() };
    if st.relay.find(&server_id, &tenancy, "").is_some() {
        return Err("이미 연결된 서버와 Tenancy입니다".into());
    }
    let creds = st.relay.creds(&server_id, &tenancy).unwrap_or_else(|| {
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
    st.relay.keyring.lock().unwrap().insert(cred_key(&server_id, &tenancy), creds);
    let username = r["username"].as_str().map(str::to_string).or_else(|| (mode != "device").then(|| user.trim().to_string()));
    let cfg = RelayConfig {
        id: new_id(),
        url: url.to_string(),
        tls,
        fingerprint: if seen.fingerprint.is_empty() { fingerprint.to_string() } else { seen.fingerprint },
        server_id,
        server_name: h["server"]["name"].as_str().unwrap_or("").to_string(),
        server_version: h["server"]["version"].as_str().unwrap_or("").to_string(),
        domain: h["server"]["domain"].as_str().unwrap_or("").to_string(),
        tenancy_name: h["tenancy"]["name"].as_str().unwrap_or("").to_string(),
        tenancy,
        address: r["address"].as_str().unwrap_or("").to_string(),
        account: r["account"].as_str().unwrap_or("").to_string(),
        username,
        policy: h["policy"].clone(),
        ..Default::default()
    };
    let conn = Conn::new(cfg);
    st.relay.conns.lock().unwrap().push(conn.clone());
    save(app);
    if !pending {
        set_conn(app, &conn, "connecting", "");
    } else {
        set_conn(app, &conn, "pending", err_text(&r));
    }
    tauri::async_runtime::spawn(manager(app.clone(), conn.clone()));
    contacts_changed(app);
    Ok(json!({ "conn": conn.id, "state": if ok { "online" } else { "pending" }, "message": if ok { String::new() } else { err_text(&r) } }))
}

/// Logs off and removes a connection. Received messages stay on the phone, and the
/// device credentials are kept so the same device can be linked again.
pub async fn unlink(app: &AppHandle, id: &str) {
    let st = app.state::<App>();
    let Some(conn) = st.relay.get(id) else { return };
    if conn.online() {
        let _ = timeout(Duration::from_secs(3), request(app, &conn, json!({ "t": "logout" }))).await;
    }
    conn.removed.store(true, Ordering::SeqCst);
    conn.wake.notify_one();
    st.relay.conns.lock().unwrap().retain(|c| c.id != conn.id);
    save(app);
    changed(app);
    contacts_changed(app);
}

/// Reconnects one connection, or every one when `id` is empty.
pub fn reconnect(app: &AppHandle, id: &str) {
    for c in app.state::<App>().relay.list() {
        if id.is_empty() || c.id == id {
            c.wake.notify_one();
        }
    }
}

/// Accepts every changed device key as the new pin.
pub fn accept_keys(app: &AppHandle) {
    for conn in app.state::<App>().relay.list() {
        let mut c = conn.cfg.lock().unwrap();
        for d in std::mem::take(&mut c.key_changed) {
            if let Some(k) = device_key(&c, &d) {
                c.pins.insert(d, k);
            }
        }
    }
    save(app);
    changed(app);
    contacts_changed(app);
}

/// Entries of this tenancy and of others, with the pin key of their devices' tenancy.
fn all_entries(c: &RelayConfig) -> impl Iterator<Item = (&Value, String)> {
    let mine = c.contacts.values().map(|e| (e, String::new()));
    let other = c.external.values().map(|e| (e, e["tenancy"].as_str().unwrap_or("").to_string()));
    mine.chain(other)
}

/// The current key of a device by its pin key.
fn device_key(c: &RelayConfig, pin: &str) -> Option<String> {
    all_entries(c).find_map(|(e, tn)| {
        e["devices"].as_array()?.iter().find_map(|d| {
            let id = d["id"].as_str()?;
            (c.pin_key(&tn, id) == pin).then(|| d["pubkey"].as_str().unwrap_or("").to_string()).filter(|k| !k.is_empty())
        })
    })
}

fn device_name(c: &RelayConfig, pin: &str) -> String {
    for (e, tn) in all_entries(c) {
        for d in e["devices"].as_array().into_iter().flatten() {
            if d["id"].as_str().is_some_and(|id| c.pin_key(&tn, id) == pin) {
                let n = d["name"].as_str().filter(|n| !n.is_empty()).or(e["name"].as_str()).unwrap_or(pin);
                return n.to_string();
            }
        }
    }
    pin.to_string()
}

/// Everything the settings screen shows.
pub fn info(app: &AppHandle) -> Value {
    let st = app.state::<App>();
    let conns = st.relay.list();
    let list: Vec<Value> = conns
        .iter()
        .map(|conn| {
            let s = conn.state.lock().unwrap().clone();
            let c = conn.cfg();
            let my_key = st.relay.creds(&c.server_id, &c.tenancy).map(|k| e2ee::fingerprint(&k.public_key)).unwrap_or_default();
            let changed: Vec<Value> = c
                .key_changed
                .iter()
                .map(|d| json!({ "device": d, "name": device_name(&c, d), "fingerprint": device_key(&c, d).map(|k| e2ee::fingerprint(&k)).unwrap_or_default() }))
                .collect();
            json!({
                "id": c.id, "label": c.label(), "state": s.state, "message": s.message,
                "url": c.url, "tls": c.tls, "fingerprint": c.fingerprint, "server_id": c.server_id, "server_name": c.server_name,
                "server_version": c.server_version, "domain": c.domain, "tenancy": c.tenancy, "tenancy_name": c.tenancy_name,
                "address": c.address, "username": c.username, "account": c.account,
                "policy": c.policy, "my_key": my_key, "key_changed": changed, "features": c.features,
                "hold": if c.has("hold") { json!(c.hold) } else { Value::Null },
            })
        })
        .collect();
    // The app is usable while any connection is; it waits on the blocked screen only
    // when every one of them waits for an admin or was refused.
    let states: Vec<&str> = list.iter().map(|c| c["state"].as_str().unwrap_or("off")).collect();
    let state = if states.contains(&"online") {
        "online"
    } else if !states.is_empty() && states.iter().all(|s| *s == "pending" || *s == "fatal") {
        if states.contains(&"pending") { "pending" } else { "fatal" }
    } else if states.contains(&"error") && !states.contains(&"connecting") {
        "error"
    } else {
        "connecting"
    };
    json!({ "linked": !list.is_empty(), "state": state, "device": st.store.my_id(), "connections": list })
}

/// Reads, or with `mode` sets, whether the relay keeps messages for this device while
/// it is offline. On a phone that is how messages reach a closed app.
pub async fn hold(app: &AppHandle, id: &str, mode: Option<&str>) -> Result<String, String> {
    let conn = app.state::<App>().relay.get(id).ok_or_else(not_connected)?;
    if !conn.has("hold") {
        return Err("이 릴레이 서버는 이 설정을 지원하지 않습니다. 서버를 업데이트하세요".into());
    }
    let mut req = json!({ "t": "hold" });
    if let Some(m) = mode {
        req["mode"] = json!(m);
    }
    let r = request(app, &conn, req).await?;
    if r["t"] != "hold" {
        return Err(err_text(&r));
    }
    let mode = r["mode"].as_str().unwrap_or("allow").to_string();
    conn.cfg.lock().unwrap().hold = mode.clone();
    save(app);
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
    for c in app.state::<App>().relay.list() {
        tauri::async_runtime::spawn(manager(app.clone(), c));
    }
}

/// Keeps one connection's control connection up while the app is on screen.
async fn manager(app: AppHandle, conn: Arc<Conn>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        if conn.removed.load(Ordering::SeqCst) {
            return;
        }
        let st = app.state::<App>();
        if !st.relay.foreground.load(Ordering::SeqCst) {
            set_conn(&app, &conn, "paused", "");
            tokio::select! {
                _ = conn.fg.notified() => {}
                _ = conn.wake.notified() => {}
            }
            continue;
        }
        set_conn(&app, &conn, "connecting", "");
        let r = session(&app, &conn).await;
        *conn.session.lock().unwrap() = None;
        *conn.out.lock().unwrap() = None;
        conn.waiting.lock().unwrap().clear();
        keep_running(&app, !st.relay.foreground.load(Ordering::SeqCst) && st.relay.any_online());
        contacts_changed(&app); // everyone now reads as unknown
        if conn.removed.load(Ordering::SeqCst) {
            return;
        }
        let wait = match r {
            Stop::Away => {
                backoff = Duration::from_secs(2);
                continue;
            }
            Stop::Net(m) => {
                set_conn(&app, &conn, "error", m);
                let w = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                w
            }
            Stop::Pending(m) => {
                set_conn(&app, &conn, "pending", m);
                Duration::from_secs(30)
            }
            Stop::Fatal(m) => {
                set_conn(&app, &conn, "fatal", m);
                Duration::from_secs(600)
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = conn.wake.notified() => { backoff = Duration::from_secs(2); }
            _ = conn.fg.notified() => { backoff = Duration::from_secs(2); }
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

async fn session(app: &AppHandle, conn: &Arc<Conn>) -> Stop {
    let st = app.state::<App>();
    let cfg = conn.cfg();
    let Some(creds) = st.relay.creds(&cfg.server_id, &cfg.tenancy) else {
        return Stop::Fatal("기기 인증 정보가 없습니다. 이 연결을 지우고 다시 추가하세요".into());
    };
    let (mut io, seen) = match connect(&cfg.url, cfg.tls, Some(&cfg.fingerprint)).await {
        Ok(x) => x,
        Err(e) if e.contains("인증서가 바뀌었습니다") => return Stop::Fatal(e),
        Err(e) => return Stop::Net(e),
    };
    let h = match hello(&mut io, &cfg.tenancy).await {
        Ok(h) => h,
        Err(e) if e.contains("Tenancy") => return Stop::Fatal(e),
        Err(e) => return Stop::Net(e),
    };
    if h["server"]["id"] != json!(cfg.server_id) {
        return Stop::Fatal("같은 주소에 다른 릴레이 서버가 있습니다. 이 연결을 지우고 다시 추가하세요".into());
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
    // Where the server logged on: a connection saved before tenancies learns its
    // tenancy here, and one whose account was moved follows it (protocol 8.3).
    let tn = r["tenancy"]["id"].as_str().unwrap_or("").to_string();
    let follow = !tn.is_empty() && tn != cfg.tenancy && (cfg.tenancy.is_empty() || r["tenancy"]["moved"] == true);
    if follow && st.relay.find(&cfg.server_id, &tn, &conn.id).is_some() {
        let _ = write_frame(&mut io, &json!({ "t": "logout", "rid": "bye" })).await;
        return Stop::Fatal(format!("계정이 '{tn}' Tenancy로 옮겨졌는데, 그 Tenancy는 이미 다른 연결로 연결되어 있습니다. 이 연결을 지우세요"));
    }
    if follow {
        st.relay.keyring.lock().unwrap().insert(cred_key(&cfg.server_id, &tn), creds.clone());
    }
    {
        let mut c = conn.cfg.lock().unwrap();
        if follow {
            if !c.tenancy.is_empty() {
                // Another tenancy: its people, keys and departments are not these.
                c.contacts.clear();
                c.contacts_since = 0;
                c.external.clear();
                c.departments.clear();
            }
            c.tenancy = tn.clone();
        }
        if c.tenancy == tn {
            c.tenancy_name = r["tenancy"]["name"].as_str().or(h["tenancy"]["name"].as_str()).unwrap_or(&c.tenancy_name).to_string();
        }
        c.policy = h["policy"].clone();
        c.server_name = h["server"]["name"].as_str().unwrap_or(&c.server_name).to_string();
        c.server_version = h["server"]["version"].as_str().unwrap_or(&c.server_version).to_string();
        c.domain = h["server"]["domain"].as_str().unwrap_or(&c.domain).to_string();
        c.features = h["features"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(str::to_string)).collect();
        c.account = r["account"].as_str().unwrap_or(&c.account).to_string();
        c.address = r["address"].as_str().unwrap_or(&c.address).to_string();
        if r["username"].is_string() {
            c.username = r["username"].as_str().map(str::to_string);
        }
        if seen.trusted && !seen.fingerprint.is_empty() {
            c.fingerprint = seen.fingerprint.clone(); // a CA-valid certificate may rotate
        }
    }
    save(app);
    *conn.session.lock().unwrap() = r["session"].as_str().map(str::to_string);

    let (mut rd, mut wr) = tokio::io::split(io);
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    *conn.out.lock().unwrap() = Some(tx.clone());
    let writer = tauri::async_runtime::spawn(async move {
        while let Some(v) = rx.recv().await {
            if write_frame(&mut wr, &v).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    set_conn(app, conn, "online", "");
    {
        let a = app.clone();
        let c = conn.clone();
        tauri::async_runtime::spawn(async move {
            sync_contacts(&a, &c).await;
            sync_departments(&a, &c).await;
            let _ = hold(&a, &c.id, None).await;
            process_inbox(&a, &c).await;
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
                            let c = conn.clone();
                            tauri::async_runtime::spawn(async move { process_inbox(&a, &c).await });
                        }
                        "contact" => apply_contacts(app, conn, std::slice::from_ref(&f["contact"]), false),
                        "policy" => {
                            conn.cfg.lock().unwrap().policy = f["policy"].clone();
                            save(app);
                            changed(app);
                        }
                        "kicked" => break Stop::Net(f["reason"].as_str().unwrap_or("서버가 연결을 끊었습니다").to_string()),
                        _ => {}
                    }
                } else if let Some(rid) = f["rid"].as_str() {
                    if let Some(w) = conn.waiting.lock().unwrap().remove(rid) {
                        let _ = w.send(f);
                    }
                }
            }
            _ = ping.tick() => {
                let _ = tx.send(json!({ "t": "ping", "rid": "p" }));
            }
            _ = conn.profile.notified() => {
                let _ = tx.send(json!({ "t": "profile", "rid": "pr", "profile": profile(&st) }));
            }
            _ = conn.wake.notified() => {
                conn.wake.notify_one(); // let the manager see it too
                break Stop::Net(String::new());
            }
            _ = conn.fg.notified() => {
                away = (!st.relay.foreground.load(Ordering::SeqCst)).then(Instant::now);
            }
            _ = async { match deadline { Some(d) => tokio::time::sleep_until(d.into()).await, None => std::future::pending().await } } => {
                if conn.busy.load(Ordering::SeqCst) > 0 {
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
    *conn.out.lock().unwrap() = None;
    *conn.session.lock().unwrap() = None;
    // Let queued frames (the logout) go out before the connection closes.
    let _ = timeout(Duration::from_secs(2), writer).await;
    stop
}

/// Sends a request on a control connection and waits for its reply.
async fn request(app: &AppHandle, conn: &Conn, mut v: Value) -> Result<Value, String> {
    let st = app.state::<App>();
    let rid = format!("c{}", st.relay.rid.fetch_add(1, Ordering::Relaxed));
    v["rid"] = json!(rid);
    let (tx, rx) = oneshot::channel();
    conn.waiting.lock().unwrap().insert(rid.clone(), tx);
    let sent = conn.out.lock().unwrap().as_ref().map(|o| o.send(v).is_ok()).unwrap_or(false);
    if !sent {
        conn.waiting.lock().unwrap().remove(&rid);
        return Err(not_connected());
    }
    let r = timeout(REQ_TIMEOUT, rx).await.map_err(|_| "서버 응답 시간 초과".to_string())?.map_err(|_| "서버 연결이 끊어졌습니다".to_string())?;
    if r["t"] == "err" { Err(err_text(&r)) } else { Ok(r) }
}

/// Tells the relays about a changed name, status or limit.
pub fn announce(app: &AppHandle) {
    for c in app.state::<App>().relay.list() {
        c.profile.notify_one();
    }
}

/// Pull to refresh.
pub fn refresh(app: &AppHandle) {
    for c in app.state::<App>().relay.list() {
        if c.online() {
            let a = app.clone();
            tauri::async_runtime::spawn(async move {
                sync_contacts(&a, &c).await;
                sync_departments(&a, &c).await;
                process_inbox(&a, &c).await;
            });
        } else {
            c.wake.notify_one();
        }
    }
}

// ---------------------------------------------------------------- contacts

/// Asks the servers for an account by exact username or account id (needed on
/// "lookup" servers, where contacts only hold people you have talked to). The
/// connections are tried in order; the first that knows the name answers.
pub async fn lookup(app: &AppHandle, query: &str) -> Result<Value, String> {
    let q = query.trim().trim_start_matches('@');
    let mut last = not_connected();
    for conn in app.state::<App>().relay.list().into_iter().filter(|c| c.online()) {
        let r = match request(app, &conn, json!({ "t": "lookup", "query": q })).await {
            Ok(r) => r,
            Err(e) => {
                last = e;
                continue;
            }
        };
        let entry = r["contact"].clone();
        let Some(acc) = entry["account"].as_str().map(str::to_string) else { continue };
        apply_contacts(app, &conn, std::slice::from_ref(&entry), false);
        if let Some(c) = contacts(app).into_iter().find(|c| c["account"] == json!(acc) && c["conn"] == json!(conn.id)) {
            return Ok(c);
        }
    }
    Err(if last == not_connected() { last } else { "찾을 수 없습니다".into() })
}

async fn sync_contacts(app: &AppHandle, conn: &Conn) {
    let since = conn.cfg.lock().unwrap().contacts_since;
    let Ok(r) = request(app, conn, json!({ "t": "contacts", "since": since })).await else { return };
    let list = r["list"].as_array().cloned().unwrap_or_default();
    apply_contacts(app, conn, &list, r["full"] == true);
    conn.cfg.lock().unwrap().contacts_since = r["now"].as_i64().unwrap_or(0);
    save(app);
}

async fn sync_departments(app: &AppHandle, conn: &Conn) {
    if !conn.has("departments") {
        return;
    }
    let Ok(r) = request(app, conn, json!({ "t": "departments" })).await else { return };
    conn.cfg.lock().unwrap().departments = r["list"].as_array().cloned().unwrap_or_default();
    save(app);
    contacts_changed(app);
}

/// Pins (trust on first use) the keys of an entry's devices.
fn pin_devices(c: &mut RelayConfig, e: &Value, tenancy: &str) {
    for d in e["devices"].as_array().into_iter().flatten() {
        let (Some(id), Some(k)) = (d["id"].as_str(), d["pubkey"].as_str()) else { continue };
        if k.is_empty() {
            continue;
        }
        let pin = c.pin_key(tenancy, id);
        match c.pins.get(&pin) {
            None => {
                c.pins.insert(pin, k.to_string());
            }
            Some(p) if p != k && !c.key_changed.contains(&pin) => c.key_changed.push(pin),
            _ => {}
        }
    }
}

fn apply_contacts(app: &AppHandle, conn: &Conn, list: &[Value], full: bool) {
    {
        let mut g = conn.cfg.lock().unwrap();
        let c = &mut *g;
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
            pin_devices(c, e, "");
            c.contacts.insert(acc.to_string(), e.clone());
        }
    }
    save(app);
    contacts_changed(app);
    changed(app); // key changes show in settings
}

/// Who an address reaches (protocol 9.3). People of this tenancy join the contacts;
/// those of other tenancies are kept apart, for their keys.
async fn resolve_on(app: &AppHandle, conn: &Conn, address: &str) -> Result<Vec<Value>, String> {
    if !conn.has("address") {
        return Err("이 릴레이 서버는 주소로 보내기를 지원하지 않습니다. 서버를 업데이트하세요".into());
    }
    let r = request(app, conn, json!({ "t": "resolve", "address": address.trim() })).await?;
    let list = r["list"].as_array().cloned().unwrap_or_default();
    let tenancy = conn.cfg.lock().unwrap().tenancy.clone();
    let (mine, other): (Vec<Value>, Vec<Value>) = list.iter().cloned().partition(|e| e["tenancy"].as_str().is_none_or(|t| t == tenancy));
    if !mine.is_empty() {
        apply_contacts(app, conn, &mine, false);
    }
    if !other.is_empty() {
        {
            let mut g = conn.cfg.lock().unwrap();
            for e in &other {
                let Some(acc) = e["account"].as_str() else { continue };
                pin_devices(&mut g, e, e["tenancy"].as_str().unwrap_or(""));
                // An answer for one device must not drop the keys of the others.
                let mut e = e.clone();
                if let Some(old) = g.external.get(acc) {
                    let mut devs = old["devices"].as_array().cloned().unwrap_or_default();
                    for d in e["devices"].as_array().into_iter().flatten() {
                        devs.retain(|x| x["id"] != d["id"]);
                        devs.push(d.clone());
                    }
                    e["devices"] = json!(devs);
                }
                g.external.insert(acc.to_string(), e);
            }
        }
        save(app);
        changed(app);
    }
    Ok(list)
}

/// The connection to write to an address on: the one with the address's server and
/// tenancy, else one on the same server, else the first.
fn conn_for_address(app: &AppHandle, address: &str) -> Option<Arc<Conn>> {
    let list = app.state::<App>().relay.list();
    let (tn, server) = address_parts(address)?;
    let same_server = |c: &RelayConfig| {
        let d = c.domain.to_lowercase();
        let url_host = parse_endpoint(&c.url).map(|e| e.host.to_lowercase()).unwrap_or_default();
        d == server || (!d.is_empty() && host_of(&d) == host_of(&server)) || url_host == host_of(&server)
    };
    list.iter()
        .find(|c| {
            let g = c.cfg.lock().unwrap();
            g.tenancy == tn && same_server(&g)
        })
        .or_else(|| list.iter().find(|c| same_server(&c.cfg.lock().unwrap())))
        .or(list.first())
        .cloned()
}

/// Checks an address typed in the recipient search and says who it reaches.
pub async fn resolve(app: &AppHandle, address: &str) -> Result<Value, String> {
    let address = address.trim();
    if address_parts(address).is_none() {
        return Err("주소 형식은 사용자@Tenancy:서버 입니다 (예: kim@example:relay.example.com)".into());
    }
    let conn = conn_for_address(app, address).ok_or_else(not_connected)?;
    let list = resolve_on(app, &conn, address).await?;
    let me = app.state::<App>().store.my_id();
    let people: Vec<Value> = list
        .iter()
        .filter(|e| e["devices"].as_array().is_some_and(|d| d.iter().any(|d| d["id"] != json!(me))))
        .map(|e| json!({ "account": e["account"], "name": e["name"], "username": e["username"], "address": e["address"], "tenancy": e["tenancy"] }))
        .collect();
    if people.is_empty() {
        return Err("이 주소로 받을 사람이 없습니다".into());
    }
    let first = people[0]["name"].as_str().filter(|n| !n.is_empty()).or(people[0]["username"].as_str()).unwrap_or(address).to_string();
    let name = if people.len() == 1 { first } else { format!("{first} 외 {}명", people.len() - 1) };
    Ok(json!({ "conn": conn.id, "to": address, "name": name, "people": people }))
}

/// Departments of every connection, with how many members are known.
pub fn departments(app: &AppHandle) -> Vec<Value> {
    let mut out = Vec::new();
    for conn in app.state::<App>().relay.list() {
        let c = conn.cfg();
        let mut list = c.departments.clone();
        // Servers without the request: what the contacts say.
        for e in c.contacts.values() {
            for d in e["departments"].as_array().into_iter().flatten().filter_map(|d| d.as_str()) {
                if !list.iter().any(|x| x["id"] == json!(d)) {
                    list.push(json!({ "id": d, "name": d, "members": 0 }));
                }
            }
        }
        for d in list {
            let id = d["id"].as_str().unwrap_or("");
            let known = c.contacts.iter().filter(|(a, e)| **a != c.account && e["removed"] != true && in_dept(e, id)).count();
            out.push(json!({ "conn": c.id, "conn_label": c.label(), "id": id, "name": d["name"], "members": d["members"].as_u64().unwrap_or(0).max(known as u64) }));
        }
    }
    out
}

fn in_dept(e: &Value, dept: &str) -> bool {
    e["departments"].as_array().is_some_and(|ds| ds.iter().any(|d| d == dept))
}

/// Everyone in a department, as recipients. On servers with addresses the whole
/// department is asked for (contacts may hold only some of it).
pub async fn dept_members(app: &AppHandle, id: &str, dept: &str) -> Result<Vec<Value>, String> {
    let conn = app.state::<App>().relay.get(id).ok_or_else(not_connected)?;
    let c = conn.cfg();
    if conn.online() && c.has("address") && !c.domain.is_empty() && !c.tenancy.is_empty() {
        let _ = resolve_on(app, &conn, &format!("*@{dept}:{}:{}", c.tenancy, c.domain)).await;
    }
    let c = conn.cfg();
    let mut out: Vec<Value> = c
        .contacts
        .iter()
        .filter(|(a, e)| **a != c.account && e["removed"] != true && in_dept(e, dept))
        .map(|(a, e)| json!({ "conn": c.id, "to": a, "name": e["name"].as_str().filter(|n| !n.is_empty()).or(e["username"].as_str()).unwrap_or(a) }))
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    if out.is_empty() {
        return Err("이 부서에 보낼 사람이 없습니다".into());
    }
    Ok(out)
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

/// People on every relay connection, most reachable and most written to first. My
/// own account is listed too when I have other devices (they can be sent to one by
/// one). Each entry names its connection, which sends to it.
pub fn contacts(app: &AppHandle) -> Vec<Value> {
    let st = app.state::<App>();
    let me = st.store.my_id();
    let counts = st.store.data.lock().unwrap().counts.clone();
    let mut out: Vec<Value> = Vec::new();
    for conn in st.relay.list() {
        let connected = conn.online();
        let cfg = conn.cfg();
        let label = cfg.label();
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
                "conn": cfg.id,
                "conn_label": label,
                "account": acc,
                "username": e["username"].as_str().unwrap_or(""),
                "name": if mine { "내 다른 기기" } else { e["name"].as_str().unwrap_or("") },
                "me": mine,
                "state": state,
                "devices": devices,
                "departments": e["departments"].as_array().cloned().unwrap_or_default(),
                "count": counts.get(acc).copied().unwrap_or(0),
                "last_seen": e["last_seen"],
            }));
        }
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

/// Display name of a recipient ("<account>", "<account>/<device>" or an address).
pub fn target_name(app: &AppHandle, conn: &str, to: &str) -> String {
    let Some(conn) = app.state::<App>().relay.get(conn) else { return to.to_string() };
    if to.contains('@') {
        return to.to_string();
    }
    let c = conn.cfg.lock().unwrap();
    match to.split_once('/') {
        Some((_, d)) => device_name(&c, d),
        None => c.entry(to).and_then(|e| e["name"].as_str()).unwrap_or(to).to_string(),
    }
}

/// The account a device belongs to, on a connection.
pub fn account_of_device(app: &AppHandle, conn: &str, device: &str) -> Option<String> {
    let conn = app.state::<App>().relay.get(conn)?;
    let c = conn.cfg.lock().unwrap();
    c.contacts
        .iter()
        .find(|(_, e)| e["devices"].as_array().is_some_and(|ds| ds.iter().any(|d| d["id"] == json!(device))))
        .map(|(a, _)| a.clone())
}

/// (connection id, my account, my address) of a connection.
pub fn me_on(app: &AppHandle, conn: &str) -> Option<(String, String, String)> {
    let conn = app.state::<App>().relay.get(conn)?;
    let c = conn.cfg.lock().unwrap();
    Some((c.id.clone(), c.account.clone(), c.address.clone()))
}

/// Whether an address is in another tenancy (or on another server) than the connection.
pub fn is_external(app: &AppHandle, conn: &str, address: &str) -> bool {
    let Some(conn) = app.state::<App>().relay.get(conn) else { return false };
    let c = conn.cfg.lock().unwrap();
    match address_parts(address) {
        Some((tn, server)) => (!c.tenancy.is_empty() && tn != c.tenancy) || (!c.domain.is_empty() && host_of(&server) != host_of(&c.domain.to_lowercase())),
        None => false,
    }
}

/// How a member of a revealed group reads for recipients on `conn`: device ids and
/// "r:<account>" there, an address for people on other connections.
pub fn member_id(app: &AppHandle, member_conn: &str, to: &str, conn: &str) -> String {
    let st = app.state::<App>();
    let (Some(mc), Some(tc)) = (st.relay.get(member_conn), st.relay.get(conn)) else { return to.to_string() };
    let local = |to: &str| match to.split_once('/') {
        Some((_, d)) => d.to_string(),
        None => format!("r:{to}"),
    };
    if to.contains('@') || mc.id == tc.id {
        return if to.contains('@') { to.to_string() } else { local(to) };
    }
    let c = mc.cfg.lock().unwrap();
    let (acc, dev) = match to.split_once('/') {
        Some((a, d)) => (a, Some(d)),
        None => (to, None),
    };
    if c.tenancy.is_empty() || c.domain.is_empty() {
        return local(to);
    }
    let user = c.entry(acc).and_then(|e| e["username"].as_str()).filter(|u| !u.is_empty()).unwrap_or(acc);
    match dev {
        Some(d) => format!("{user}:{d}@{}:{}", c.tenancy, c.domain),
        None => format!("{user}@{}:{}", c.tenancy, c.domain),
    }
}

// ---------------------------------------------------------------- transfers

async fn transfer_conn(conn: &Conn) -> Result<Stream, String> {
    let cfg = conn.cfg();
    let session = conn.session.lock().unwrap().clone().ok_or_else(not_connected)?;
    let (mut io, _) = connect(&cfg.url, cfg.tls, Some(&cfg.fingerprint)).await?;
    hello(&mut io, &cfg.tenancy).await?;
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

/// What happened to one recipient: Ok("relay" | "stored", partial failure note) or Err(reason).
pub type Outcome = Result<(String, String), String>;

/// Sends to every target of one connection ("<account>", "<account>/<device>" or an
/// address).
pub async fn send(
    app: &AppHandle,
    conn_id: &str,
    job: &str,
    targets: &[String],
    text: &str,
    files: &[(PathBuf, String, u64)],
    group: Option<&GroupInfo>,
) -> Vec<(String, Outcome)> {
    let fail_all = |e: String| targets.iter().map(|t| (t.clone(), Err(e.clone()))).collect();
    let st = app.state::<App>();
    let Some(conn) = st.relay.get(conn_id) else { return fail_all("릴레이 연결이 없습니다. 받는 사람을 다시 고르세요".into()) };
    let _busy = Busy::new(&conn.busy);
    if !conn.online() {
        return fail_all(not_connected());
    }
    let pol = conn.cfg.lock().unwrap().policy.clone();
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
        sync_contacts(app, &conn).await; // latest device keys
    }

    let me = st.store.my_id();
    let mut out: Vec<(String, Outcome)> = Vec::new();
    // Address -> who it reaches, with keys (only needed for end-to-end encryption).
    let mut reached: HashMap<String, Vec<Value>> = HashMap::new();
    if e2ee {
        for t in targets.iter().filter(|t| t.contains('@')) {
            match resolve_on(app, &conn, t).await {
                Ok(list) => {
                    reached.insert(t.clone(), list);
                }
                Err(e) => out.push((t.clone(), Err(e))),
            }
        }
    }
    let cfg = conn.cfg();
    let by_device = cfg.has("device_target");
    // Target -> what goes in "to" (servers without device_target get the whole account).
    let mut wire: Vec<(String, String)> = Vec::new();
    let mut recipients: Vec<(String, String)> = Vec::new();
    for t in targets {
        if out.iter().any(|o| &o.0 == t) {
            continue;
        }
        // (pin tenancy, devices) this target reaches.
        let mut devs: Vec<(String, Value)> = Vec::new();
        let to = if t.contains('@') {
            if !cfg.has("address") {
                out.push((t.clone(), Err("이 릴레이 서버는 주소로 보내기를 지원하지 않습니다".into())));
                continue;
            }
            for e in reached.get(t).into_iter().flatten() {
                let tn = e["tenancy"].as_str().unwrap_or("").to_string();
                devs.extend(e["devices"].as_array().into_iter().flatten().filter(|d| d["id"] != json!(me)).map(|d| (tn.clone(), d.clone())));
            }
            t.clone()
        } else {
            let (acc, dev) = match t.split_once('/') {
                Some((a, d)) => (a.to_string(), Some(d.to_string())),
                None => (t.clone(), None),
            };
            if acc == cfg.account && (dev.is_none() || !by_device) {
                out.push((t.clone(), Err("이 서버에서는 내 다른 기기로 보낼 수 없습니다".into())));
                continue;
            }
            let mut ds = cfg.contacts.get(&acc).and_then(|e| e["devices"].as_array().cloned()).unwrap_or_default();
            if let (Some(d), true) = (&dev, by_device) {
                ds.retain(|x| x["id"] == json!(d));
            }
            devs.extend(ds.into_iter().map(|d| (String::new(), d)));
            match &dev {
                Some(d) if by_device => format!("{acc}/{d}"),
                _ => acc.clone(),
            }
        };
        if e2ee {
            if devs.iter().any(|(tn, d)| d["id"].as_str().is_some_and(|id| cfg.key_changed.contains(&cfg.pin_key(tn, id)))) {
                out.push((t.clone(), Err("받는 사람의 보안 키가 바뀌었습니다. 설정에서 확인하세요".into())));
                continue;
            }
            if !devs.iter().any(|(_, d)| d["pubkey"].as_str().is_some_and(|k| !k.is_empty())) {
                out.push((t.clone(), Err("받는 사람의 보안 키를 알 수 없습니다".into())));
                continue;
            }
            for (_, d) in devs {
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
        let Some(creds) = st.relay.creds(&cfg.server_id, &cfg.tenancy) else { return fail_all("기기 키가 없습니다".into()) };
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
        let mut io = transfer_conn(&conn).await?;
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
        // An address may reach several people: one line each.
        let lines: Vec<&Value> = results.iter().filter(|x| x["to"] == json!(to)).collect();
        let ok: Vec<&&Value> = lines.iter().filter(|x| x["state"] == "delivered" || x["state"] == "stored").collect();
        let failed: Vec<String> = lines.iter().filter(|x| x["state"] != "delivered" && x["state"] != "stored").map(|x| err_text(x)).collect();
        let v = if !ok.is_empty() {
            let via = if ok.iter().any(|x| x["state"] == "delivered") { "relay" } else { "stored" };
            let note = if failed.is_empty() { String::new() } else { format!("{}명은 받지 못함: {}", failed.len(), failed[0]) };
            Ok((via.to_string(), note))
        } else if let Some(e) = failed.first() {
            Err(e.clone())
        } else if r["t"] == "err" {
            Err(err_text(&r))
        } else {
            Err("서버 응답에 받는 사람이 없습니다".into())
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

async fn process_inbox(app: &AppHandle, conn: &Arc<Conn>) {
    let _one = conn.inbox.lock().await;
    let _busy = Busy::new(&conn.busy);
    let Ok(r) = request(app, conn, json!({ "t": "inbox" })).await else { return };
    for item in r["list"].as_array().cloned().unwrap_or_default() {
        let mid = item["mid"].as_str().unwrap_or("").to_string();
        if let Err(e) = fetch(app, conn, &mid).await {
            if conn.online() {
                set_conn(app, conn, "online", format!("쪽지를 받지 못했습니다: {e}"));
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
                    conn: conn.id.clone(),
                    from: x["from"].as_str().unwrap_or("").to_string(),
                    from_name: name.to_string(),
                    ts: x["last"].as_u64().unwrap_or_else(now_ms),
                    note: format!("{name} 님이 보낸 쪽지 {n}개가 릴레이 서버 보관 기간이 지나 삭제되었습니다"),
                    ..Default::default()
                },
            );
        }
        let _ = request(app, conn, json!({ "t": "expired_ack" })).await;
    }
}

async fn fetch(app: &AppHandle, conn: &Arc<Conn>, mid: &str) -> Result<(), String> {
    let mut io = transfer_conn(conn).await?;
    let already = conn.cfg.lock().unwrap().seen.iter().any(|m| m == mid);
    if !already {
        write_frame(&mut io, &json!({ "t": "fetch", "rid": "f", "mid": mid })).await?;
        let h = read_frame(&mut io, IO_TIMEOUT).await?;
        if h["t"] != "fetch" {
            return if h["code"] == "not_found" { Ok(()) } else { Err(err_text(&h)) };
        }
        receive(app, conn, &mut io, mid, &h).await?;
        {
            let mut c = conn.cfg.lock().unwrap();
            c.seen.push(mid.to_string());
            let n = c.seen.len();
            if n > SEEN_CAP {
                c.seen.drain(..n - SEEN_CAP);
            }
        }
        save(app);
    }
    write_frame(&mut io, &json!({ "t": "ack", "rid": "k", "mid": mid })).await?;
    let _ = read_frame(&mut io, IO_TIMEOUT).await?;
    Ok(())
}

/// The sender device's key, whether its key changed, and my credentials.
fn sender_key(st: &App, conn: &Conn, from: &str, device: &str) -> Option<(String, bool, Creds)> {
    let c = conn.cfg.lock().unwrap();
    let e = c.entry(from)?;
    let tn = if c.contacts.contains_key(from) { "" } else { e["tenancy"].as_str().unwrap_or("") };
    let k = e["devices"].as_array()?.iter().find(|d| d["id"] == json!(device))?["pubkey"].as_str()?.to_string();
    let pin = c.pin_key(tn, device);
    Some((k, c.key_changed.contains(&pin), st.relay.creds(&c.server_id, &c.tenancy)?))
}

/// Reads the attachments that follow a fetch header, saves them and delivers the message.
async fn receive(app: &AppHandle, conn: &Arc<Conn>, io: &mut Stream, mid: &str, h: &Value) -> Result<(), String> {
    let st = app.state::<App>();
    let from = h["from"].as_str().unwrap_or("").to_string();
    let from_device = h["from_device"].as_str().unwrap_or("").to_string();
    let from_name = h["from_name"].as_str().unwrap_or("").to_string();
    let from_address = h["from_address"].as_str().unwrap_or("").to_string();
    let sizes: Vec<u64> = h["files"].as_array().into_iter().flatten().map(|f| f["size"].as_u64().unwrap_or(0)).collect();
    let mut note = String::new();
    let body = &h["body"];
    let encrypted = !(body["enc"] == "none" || body["enc"].is_null());

    // Decrypt (or read) text, names and group.
    let mut key = None;
    let plain = if !encrypted {
        body.clone()
    } else {
        if sender_key(&st, conn, &from, &from_device).is_none() {
            // Someone of another tenancy is not in the contacts: ask for their keys.
            if !from_address.is_empty() && is_external(app, &conn.id, &from_address) {
                let _ = resolve_on(app, conn, &from_address).await;
            } else {
                sync_contacts(app, conn).await;
            }
        }
        match sender_key(&st, conn, &from, &from_device) {
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
            conn: conn.id.clone(),
            from,
            from_device,
            from_name,
            from_address,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(address_parts("kim@example:relay.example.com"), Some(("example".into(), "relay.example.com".into())));
        assert_eq!(address_parts("john:948a@dev,ops:Beta:relay.example.com:45443"), Some(("beta".into(), "relay.example.com:45443".into())));
        assert_eq!(address_parts("a@b@t:s"), Some(("t".into(), "s".into())));
        assert_eq!(address_parts("kim@example"), None);
        assert_eq!(address_parts("kim"), None);
        assert_eq!(host_of("relay.example.com:45443"), "relay.example.com");
        assert!(valid_tenancy("example-1") && !valid_tenancy("a") && !valid_tenancy("-ab") && !valid_tenancy("Ab"));
    }
}
