//! Relay client (docs/relay-protocol.md): one control connection kept open while
//! a relay is configured, plus a short-lived transfer connection per message.
//!
//! Every device of a relay contact becomes a peer under its device id, the same id it
//! has on the LAN, so a device seen both ways is one peer with `relay_account` set and
//! the relay as its fallback.

use crate::e2ee;
use crate::net::{self, human};
use crate::relay_io::*;
use crate::state::*;
use crate::tray;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::timeout;

pub use crate::relay_io::probe;

const REQ_TIMEOUT: Duration = Duration::from_secs(30);
const PING_EVERY: Duration = Duration::from_secs(30);
const CHUNK: usize = 256 * 1024;
const SEEN_CAP: usize = 500;

/// The relay this installation is linked to.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// "host:port".
    pub url: String,
    pub tls: bool,
    /// Pinned SHA-256 (hex) of the server's leaf certificate.
    pub fingerprint: String,
    pub server_id: String,
    pub server_name: String,
    /// "<protocol>.<YYMMDD>.<NN>" as the server reported it.
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
    /// Server features from its hello ("device_target", "hold").
    pub features: Vec<String>,
    /// This device's "relay receive" setting: "allow", "device_off" or "all_off".
    pub hold: String,
}

/// Per-server device credentials. Kept after unlinking so the same device can log on again.
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
    /// off | connecting | online | pending | error
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
}

impl Relay {
    pub fn load(dir: &Path) -> Self {
        let f: RelayFile = std::fs::read(dir.join("relay.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { cfg: Mutex::new(f.active), keyring: Mutex::new(f.keyring), ..Default::default() }
    }

    pub fn save(&self, dir: &Path) {
        let f = RelayFile { active: self.cfg.lock().unwrap().clone(), keyring: self.keyring.lock().unwrap().clone() };
        let p = dir.join("relay.json");
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_vec(&f).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    /// What this device announces on the LAN about its relay registration.
    pub fn identity(&self) -> Option<crate::state::RelayId> {
        self.cfg.lock().unwrap().as_ref().filter(|c| !c.account.is_empty()).map(|c| crate::state::RelayId {
            server: c.server_id.clone(),
            account: c.account.clone(),
        })
    }

    pub fn configured(&self) -> bool {
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
}

// ---------------------------------------------------------------- setup (wizard)

/// Registers this device (or logs it on to an account) and makes the relay active.
/// `mode`: "device" (no account), "login" or "register".
pub async fn join(app: &AppHandle, url: &str, tls: bool, fingerprint: &str, mode: &str, user: &str, pass: &str) -> Result<Value, String> {
    let st = app.state::<AppState>();
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
    st.relay.save(&st.dir);
    st.relay.wake.notify_one();
    changed(app);
    Ok(json!({ "state": if ok { "online" } else { "pending" }, "message": if ok { String::new() } else { err_text(&r) } }))
}

/// Logs off and forgets the relay (device credentials are kept for a later re-link).
pub async fn disconnect(app: &AppHandle) {
    let st = app.state::<AppState>();
    if st.relay.online() {
        let _ = request(app, json!({ "t": "logout" })).await;
    }
    *st.relay.cfg.lock().unwrap() = None;
    st.relay.save(&st.dir);
    st.relay.wake.notify_one();
    rebuild_peers(app);
    changed(app);
}

pub async fn logout(app: &AppHandle) {
    if app.state::<AppState>().relay.online() {
        let _ = timeout(Duration::from_secs(2), request(app, json!({ "t": "logout" }))).await;
    }
}

pub fn reconnect(app: &AppHandle) {
    app.state::<AppState>().relay.wake.notify_one();
}

/// Accepts every changed device key as the new pin.
pub fn accept_keys(app: &AppHandle) {
    let st = app.state::<AppState>();
    if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
        let changed: Vec<String> = std::mem::take(&mut c.key_changed);
        for d in changed {
            if let Some(k) = device_key(c, &d) {
                c.pins.insert(d, k);
            }
        }
    }
    st.relay.save(&st.dir);
    changed(app);
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
                return e["name"].as_str().unwrap_or(device).to_string();
            }
        }
    }
    device.to_string()
}

/// Everything the settings panel shows.
pub fn info(app: &AppHandle) -> Value {
    let st = app.state::<AppState>();
    let conn = st.relay.conn.lock().unwrap().clone();
    let cfg = st.relay.cfg.lock().unwrap().clone();
    let Some(c) = cfg else { return json!({ "configured": false }) };
    let my_key = st.relay.creds(&c.server_id).map(|k| e2ee::fingerprint(&k.public_key)).unwrap_or_default();
    let changed: Vec<Value> = c
        .key_changed
        .iter()
        .map(|d| json!({ "device": d, "name": device_name(&c, d), "fingerprint": device_key(&c, d).map(|k| e2ee::fingerprint(&k)).unwrap_or_default() }))
        .collect();
    json!({
        "configured": true, "state": conn.state, "message": conn.message,
        "url": c.url, "tls": c.tls, "fingerprint": c.fingerprint, "server_id": c.server_id, "server_name": c.server_name, "server_version": c.server_version,
        "username": c.username, "account": c.account, "device": st.my_id(), "policy": c.policy,
        "contacts": c.contacts.len().saturating_sub(1), "my_key": my_key, "key_changed": changed,
        "hold": if c.features.iter().any(|f| f == "hold") { json!(c.hold) } else { Value::Null },
    })
}

/// Reads, or with `mode` sets, this device's "relay receive" setting on the server:
/// whether the relay keeps messages for it while it is offline.
pub async fn hold(app: &AppHandle, mode: Option<&str>) -> Result<String, String> {
    let st = app.state::<AppState>();
    if !st.relay.cfg.lock().unwrap().as_ref().is_some_and(|c| c.features.iter().any(|f| f == "hold")) {
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
    st.relay.save(&st.dir);
    changed(app);
    Ok(mode)
}

fn changed(app: &AppHandle) {
    let _ = app.emit("relay", ());
}

fn set_conn(app: &AppHandle, state: &'static str, message: impl Into<String>) {
    *app.state::<AppState>().relay.conn.lock().unwrap() = ConnState { state, message: message.into() };
    changed(app);
}

// ---------------------------------------------------------------- control connection

enum Stop {
    /// Retry with backoff.
    Net(String),
    /// Waiting for admin approval.
    Pending(String),
    /// Will not succeed without the user or admin doing something.
    Fatal(String),
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(manager(app));
}

async fn manager(app: AppHandle) {
    let mut backoff = Duration::from_secs(2);
    loop {
        let st = app.state::<AppState>();
        let cfg = st.relay.cfg.lock().unwrap().clone();
        let Some(cfg) = cfg else {
            set_conn(&app, "off", "");
            st.relay.wake.notified().await;
            continue;
        };
        set_conn(&app, "connecting", "");
        let r = session(&app, &cfg).await;
        *st.relay.session.lock().unwrap() = None;
        *st.relay.out.lock().unwrap() = None;
        st.relay.waiting.lock().unwrap().clear();
        rebuild_peers(&app); // relay contacts now read as offline
        let wait = match r {
            Stop::Net(m) => {
                set_conn(&app, "error", m);
                let w = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                w
            }
            Stop::Pending(m) => {
                set_conn(&app, "pending", m);
                Duration::from_secs(60)
            }
            Stop::Fatal(m) => {
                set_conn(&app, "error", m);
                Duration::from_secs(600)
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = st.relay.wake.notified() => { backoff = Duration::from_secs(2); }
        }
    }
}

fn login_req(st: &AppState, creds: &Creds) -> Value {
    let me = st.my_meta();
    json!({
        "t": "login", "rid": "l", "id": me.id, "device_secret": creds.device_secret, "pubkey": creds.public_key,
        "profile": profile(&me),
    })
}

fn profile(me: &Meta) -> Value {
    let status = match me.status {
        Status::Online => "online",
        Status::Dnd => "dnd",
        Status::Offline => "offline",
    };
    json!({ "name": me.name, "host": me.host, "status": status, "notify": me.notify,
            "max_files": me.max_files, "max_file_size": me.max_file_size })
}

async fn session(app: &AppHandle, cfg: &RelayConfig) -> Stop {
    let st = app.state::<AppState>();
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
    st.relay.save(&st.dir);
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
    let stop = loop {
        tokio::select! {
            f = read_frame(&mut rd, PING_EVERY * 3) => {
                let f = match f { Ok(f) => f, Err(e) => break Stop::Net(e) };
                if f["t"] == "event" {
                    match f["e"].as_str().unwrap_or("") {
                        "inbox" => {
                            let a = app.clone();
                            tauri::async_runtime::spawn(async move { process_inbox(&a).await });
                        }
                        "contact" => {
                            apply_contacts(app, std::slice::from_ref(&f["contact"]), false);
                            rebuild_peers(app);
                        }
                        "policy" => {
                            if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
                                c.policy = f["policy"].clone();
                            }
                            st.relay.save(&st.dir);
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
                let _ = tx.send(json!({ "t": "profile", "rid": "pr", "profile": profile(&st.my_meta()) }));
            }
            _ = st.relay.wake.notified() => {
                st.relay.wake.notify_one(); // let the manager see it too
                break Stop::Net(String::new());
            }
        }
    };
    drop(tx);
    *st.relay.out.lock().unwrap() = None;
    writer.abort();
    stop
}

/// Sends a request on the control connection and waits for its reply.
async fn request(app: &AppHandle, mut v: Value) -> Result<Value, String> {
    let st = app.state::<AppState>();
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
    app.state::<AppState>().relay.profile.notify_one();
}

/// "다시 탐색": refresh contacts too.
pub fn refresh(app: &AppHandle) {
    if app.state::<AppState>().relay.online() {
        let a = app.clone();
        tauri::async_runtime::spawn(async move { sync_contacts(&a).await });
    }
}

// ---------------------------------------------------------------- contacts

/// Asks the server for an account by exact username or account id (needed on
/// "lookup" servers, where contacts only hold people you have talked to).
pub async fn lookup(app: &AppHandle, query: &str) -> Option<String> {
    let r = request(app, json!({ "t": "lookup", "query": query })).await.ok()?;
    let entry = r["contact"].clone();
    let acc = entry["account"].as_str()?.to_string();
    // Unchanged contacts are left alone: rebuilding emits "peers", which makes an
    // open search window search (and look up) again.
    let known = app.state::<AppState>().relay.cfg.lock().unwrap().as_ref().and_then(|c| c.contacts.get(&acc).cloned());
    if known.as_ref() != Some(&entry) {
        apply_contacts(app, std::slice::from_ref(&entry), false);
        rebuild_peers(app);
    }
    Some(acc)
}

async fn sync_contacts(app: &AppHandle) {
    let st = app.state::<AppState>();
    let since = st.relay.cfg.lock().unwrap().as_ref().map(|c| c.contacts_since).unwrap_or(0);
    let Ok(r) = request(app, json!({ "t": "contacts", "since": since })).await else { return };
    let list = r["list"].as_array().cloned().unwrap_or_default();
    apply_contacts(app, &list, r["full"] == true);
    if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
        c.contacts_since = r["now"].as_i64().unwrap_or(0);
    }
    st.relay.save(&st.dir);
    rebuild_peers(app);
}

fn apply_contacts(app: &AppHandle, list: &[Value], full: bool) {
    let st = app.state::<AppState>();
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
    st.relay.save(&st.dir);
    changed(app);
}

fn status_of(s: &Value) -> Status {
    match s.as_str() {
        Some("dnd") => Status::Dnd,
        Some("offline") => Status::Offline,
        _ => Status::Online,
    }
}

/// Mirrors the relay contacts' devices into the peer list, one peer per device.
pub fn rebuild_peers(app: &AppHandle) {
    let st = app.state::<AppState>();
    let online = st.relay.online();
    let cfg = st.relay.cfg.lock().unwrap().clone();
    let my_dev = st.my_id();
    {
        let mut d = st.data.lock().unwrap();
        let contacts = cfg.as_ref().map(|c| c.contacts.clone()).unwrap_or_default();
        let relay = cfg.is_some();
        // Before 0.1.8 a relay contact was one "r:<account>" peer; groups now get its devices.
        d.peers.retain(|id, _| !id.starts_with("r:"));
        let mut legacy: HashMap<String, Vec<String>> = HashMap::new();
        let mut owned: HashSet<String> = HashSet::new();
        // My own account is listed too: my other devices can be reached through the relay.
        for (acc, e) in &contacts {
            if e["removed"] == true {
                continue;
            }
            let username = e["username"].as_str().unwrap_or("").to_string();
            for dv in e["devices"].as_array().into_iter().flatten() {
                let Some(id) = dv["id"].as_str().filter(|id| *id != my_dev) else { continue };
                owned.insert(id.to_string());
                legacy.entry(format!("r:{acc}")).or_default().push(id.to_string());
                let p = d.peers.entry(id.to_string()).or_insert_with(|| Peer { id: id.to_string(), ..Default::default() });
                if !p.online {
                    // While it is on the LAN, its own announcements are fresher.
                    let name = dv["name"].as_str().filter(|n| !n.is_empty()).or(e["name"].as_str()).unwrap_or("");
                    if !name.is_empty() {
                        p.name = name.to_string();
                    }
                    if let Some(h) = dv["host"].as_str().filter(|h| !h.is_empty()) {
                        p.host = h.to_string();
                    }
                    p.notify = dv["notify"] != false;
                    p.max_files = dv["max_files"].as_u64().unwrap_or(0) as u32;
                    p.max_file_size = dv["max_file_size"].as_u64().unwrap_or(0);
                    if p.ver == 0 {
                        p.ver = PROTO_VER;
                    }
                }
                p.relay_account = acc.clone();
                p.username = username.clone();
                p.relay_hold = dv["hold"] != false;
                p.relay_online = online && dv["online"] == true;
                p.relay_status = if p.relay_online { status_of(&dv["status"]) } else { Status::Offline };
            }
        }
        // Devices the relay no longer lists: LAN ones stay, relay-only ones go.
        d.peers.retain(|id, p| owned.contains(id) || !p.ip.is_empty());
        for (id, p) in d.peers.iter_mut() {
            if owned.contains(id) {
                continue;
            }
            p.relay_online = false;
            p.relay_hold = false;
            p.username.clear();
            // Announced on the LAN: the relay can still be tried for it.
            p.relay_account = if relay { p.relay_self.clone() } else { String::new() };
        }
        for g in d.groups.iter_mut() {
            if g.members.iter().any(|m| m.starts_with("r:")) {
                let mut seen = HashSet::new();
                let old = std::mem::take(&mut g.members);
                for m in old {
                    let ids = legacy.get(&m).cloned().unwrap_or_else(|| vec![m]);
                    g.members.extend(ids.into_iter().filter(|x| seen.insert(x.clone())));
                }
            }
        }
    }
    st.save_data();
    net::peers_changed(app);
    // Joining or leaving a relay changes what we announce on the LAN.
    static ANNOUNCED: Mutex<Option<Option<crate::state::RelayId>>> = Mutex::new(None);
    let id = st.relay.identity();
    let mut last = ANNOUNCED.lock().unwrap();
    if last.as_ref() != Some(&id) {
        let first = last.is_none();
        *last = Some(id);
        if !first {
            st.scan.notify_one();
        }
    }
}

/// A LAN device announced relay account `acc` on our server: link it as a contact
/// (once per run) so the relay knows its devices and keys.
pub fn learn_account(app: &AppHandle, acc: &str) {
    static TRIED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let st = app.state::<AppState>();
    if !st.relay.online() || st.relay.cfg.lock().unwrap().as_ref().is_some_and(|c| c.contacts.contains_key(acc) || c.account == acc) {
        return;
    }
    if !TRIED.lock().unwrap().get_or_insert_with(HashSet::new).insert(acc.to_string()) {
        return;
    }
    let (app, acc) = (app.clone(), acc.to_string());
    tauri::async_runtime::spawn(async move {
        if lookup(&app, &acc).await.is_none() {
            rebuild_peers(&app);
        }
    });
}

// ---------------------------------------------------------------- transfers

async fn transfer_conn(app: &AppHandle) -> Result<Stream, String> {
    let st = app.state::<AppState>();
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

pub struct Target {
    pub peer: String,
    pub account: String,
    /// Only this device of the account (when the server supports it).
    pub device: Option<String>,
}

/// Per target peer: Ok("relay" | "stored") or Err(reason).
pub async fn send(
    app: &AppHandle,
    job: &str,
    targets: &[Target],
    text: &str,
    files: &[(String, String, u64)],
    group: Option<&GroupInfo>,
) -> Vec<(String, Result<String, String>)> {
    let fail_all = |e: String| targets.iter().map(|t| (t.peer.clone(), Err(e.clone()))).collect();
    let st = app.state::<AppState>();
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

    let mut out: Vec<(String, Result<String, String>)> = Vec::new();
    let mut accounts: Vec<String> = Vec::new(); // "to" entries
    let mut recipients: Vec<(String, String)> = Vec::new();
    let cfg = st.relay.cfg.lock().unwrap().clone().unwrap_or_default();
    let by_device = cfg.features.iter().any(|f| f == "device_target");
    let to_of = |t: &Target| match &t.device {
        Some(d) if by_device => format!("{}/{d}", t.account),
        _ => t.account.clone(),
    };
    {
        for t in targets {
            let to = to_of(t);
            if accounts.contains(&to) {
                continue;
            }
            if e2ee {
                let mut devs = cfg.contacts.get(&t.account).and_then(|e| e["devices"].as_array().cloned()).unwrap_or_default();
                if let (Some(d), true) = (&t.device, by_device) {
                    devs.retain(|x| x["id"] == json!(d));
                }
                if devs.iter().any(|d| cfg.key_changed.iter().any(|x| d["id"] == json!(x))) {
                    out.push((t.peer.clone(), Err("받는 사람의 보안 키가 바뀌었습니다. 설정 > 릴레이 서버에서 확인하세요".into())));
                    continue;
                }
                if !devs.iter().any(|d| d["pubkey"].as_str().is_some_and(|k| !k.is_empty())) {
                    out.push((t.peer.clone(), Err("받는 사람의 보안 키를 알 수 없습니다".into())));
                    continue;
                }
                for d in devs {
                    if let (Some(id), Some(k)) = (d["id"].as_str(), d["pubkey"].as_str()) {
                        if !k.is_empty() {
                            recipients.push((id.to_string(), k.to_string()));
                        }
                    }
                }
            }
            accounts.push(to);
        }
    }
    let blocked: Vec<String> = out.iter().map(|o| o.0.clone()).collect();
    if accounts.is_empty() {
        return out;
    }

    let msg = new_id();
    let names: Vec<&str> = files.iter().map(|f| f.1.as_str()).collect();
    let mut plain = json!({ "text": text, "names": names });
    if let Some(g) = group {
        plain["group"] = json!(g);
    }
    let (body, key) = if e2ee {
        let creds = st.relay.cfg.lock().unwrap().as_ref().and_then(|c| st.relay.creds(&c.server_id));
        let Some(creds) = creds else { return fail_all("기기 키가 없습니다".into()) };
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
        write_frame(&mut io, &json!({ "t": "send", "rid": "s", "msg": msg, "to": accounts, "files": sizes, "body": body })).await?;
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
            out.extend(targets.iter().filter(|t| !blocked.contains(&t.peer)).map(|t| (t.peer.clone(), Err(e.clone()))));
            return out;
        }
    };
    let results: Vec<Value> = r["result"].as_array().or(r["rejected"].as_array()).cloned().unwrap_or_default();
    for t in targets {
        if blocked.contains(&t.peer) {
            continue;
        }
        let res = results.iter().find(|x| x["to"] == json!(to_of(t)));
        let v = match res {
            Some(x) if x["state"] == "delivered" => Ok("relay".to_string()),
            Some(x) if x["state"] == "stored" => Ok("stored".to_string()),
            Some(x) => Err(err_text(x)),
            None if r["t"] == "err" => Err(err_text(&r)),
            None => Err("서버 응답에 받는 사람이 없습니다".into()),
        };
        out.push((t.peer.clone(), v));
    }
    out
}

async fn upload(app: &AppHandle, job: &str, io: &mut Stream, files: &[(String, String, u64)], key: Option<&e2ee::Key>) -> Result<(), String> {
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
                let _ = app.emit("send-progress", json!({ "job": job, "to": "릴레이 서버", "done": done, "total": total }));
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
    let st = app.state::<AppState>();
    let _one = st.relay.inbox.lock().await;
    let Ok(r) = request(app, json!({ "t": "inbox" })).await else { return };
    for item in r["list"].as_array().cloned().unwrap_or_default() {
        let mid = item["mid"].as_str().unwrap_or("").to_string();
        if let Err(e) = fetch(app, &mid).await {
            set_conn(app, "online", format!("쪽지를 받지 못했습니다: {e}"));
            break;
        }
    }
    let expired = r["expired"].as_array().cloned().unwrap_or_default();
    if !expired.is_empty() {
        for x in &expired {
            let name = x["from_name"].as_str().unwrap_or("");
            let n = x["count"].as_u64().unwrap_or(0);
            net::deliver(
                app,
                Message {
                    id: new_id(),
                    from_id: format!("r:{}", x["from"].as_str().unwrap_or("")),
                    from_name: name.to_string(),
                    from_ip: "릴레이".into(),
                    text: String::new(),
                    files: vec![],
                    ts: x["last"].as_u64().unwrap_or_else(now_ms),
                    read: false,
                    via: "relay".into(),
                    note: format!("{name} 님이 보낸 쪽지 {n}개가 릴레이 서버 보관 기간이 지나 삭제되었습니다"),
                    group: None,
                },
            );
        }
        let _ = request(app, json!({ "t": "expired_ack" })).await;
    }
}

async fn fetch(app: &AppHandle, mid: &str) -> Result<(), String> {
    let st = app.state::<AppState>();
    let mut io = transfer_conn(app).await?;
    let already = st.relay.cfg.lock().unwrap().as_ref().is_some_and(|c| c.seen.iter().any(|m| m == mid));
    if !already {
        write_frame(&mut io, &json!({ "t": "fetch", "rid": "f", "mid": mid })).await?;
        let h = read_frame(&mut io, IO_TIMEOUT).await?;
        if h["t"] != "fetch" {
            return if h["code"] == "not_found" { Ok(()) } else { Err(err_text(&h)) };
        }
        receive(app, &mut io, &h).await?;
        if let Some(c) = st.relay.cfg.lock().unwrap().as_mut() {
            c.seen.push(mid.to_string());
            let n = c.seen.len();
            if n > SEEN_CAP {
                c.seen.drain(..n - SEEN_CAP);
            }
        }
        st.relay.save(&st.dir);
    }
    write_frame(&mut io, &json!({ "t": "ack", "rid": "k", "mid": mid })).await?;
    let _ = read_frame(&mut io, IO_TIMEOUT).await?;
    Ok(())
}

/// Reads the attachments that follow a fetch header, saves them and delivers the message.
async fn receive(app: &AppHandle, io: &mut Stream, h: &Value) -> Result<(), String> {
    let st = app.state::<AppState>();
    let from = h["from"].as_str().unwrap_or("").to_string();
    let from_device = h["from_device"].as_str().unwrap_or("");
    let from_name = h["from_name"].as_str().unwrap_or("").to_string();
    let sizes: Vec<u64> = h["files"].as_array().into_iter().flatten().map(|f| f["size"].as_u64().unwrap_or(0)).collect();
    let mut note = String::new();
    let body = &h["body"];

    // Decrypt (or read) text, names and group.
    let mut key = None;
    let plain = if body["enc"] == "none" || body["enc"].is_null() {
        body.clone()
    } else {
        let lookup = |st: &AppState| st.relay.cfg.lock().unwrap().as_ref().and_then(|c| {
            let k = c.contacts.get(&from)?["devices"].as_array()?.iter().find(|d| d["id"] == json!(from_device))?["pubkey"].as_str()?.to_string();
            Some((k, c.key_changed.iter().any(|x| x == from_device), st.relay.creds(&c.server_id)?))
        });
        if lookup(&st).is_none() {
            sync_contacts(app).await;
        }
        match lookup(&st) {
            Some((sender, changed, creds)) => match e2ee::open(body, &st.my_id(), &creds.secret_key, &sender) {
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

    let (_, _, base, free) = st.limits();
    let total: u64 = sizes.iter().sum();
    if total > 0 && total.saturating_add(DISK_MARGIN) > free {
        return Err("디스크 공간이 부족합니다".into());
    }
    let split = st.settings.lock().unwrap().split_folders;
    let dir = if split && !sizes.is_empty() { net::message_dir(&base, &from_name) } else { base.clone() };
    let mut saved = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    for (i, &size) in sizes.iter().enumerate() {
        let name = names.get(i).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| format!("file{}", i + 1));
        // Undecryptable attachments are still read off the wire, then dropped.
        let target = if key.is_none() && body["enc"] != "none" && !body["enc"].is_null() { None } else { net::reserve_path(&dir, &net::sanitize(&name)) };
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
                    f.write_all(&data).await.map_err(|e| e.to_string())?;
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
                if saved.is_empty() && dir != base {
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
    net::deliver(
        app,
        Message {
            id: new_id(),
            // Replies go to the device it came from (LAN first, then the relay).
            from_id: if from_device.is_empty() { format!("r:{from}") } else { from_device.to_string() },
            from_name,
            from_ip: "릴레이".into(),
            text: plain["text"].as_str().unwrap_or("").to_string(),
            files: saved,
            ts: h["ts"].as_u64().unwrap_or_else(now_ms),
            read: false,
            via: "relay".into(),
            note,
            group,
        },
    );
    tray::refresh(app);
    Ok(())
}
