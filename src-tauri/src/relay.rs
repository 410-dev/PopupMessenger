//! Relay client (docs/relay-protocol.md). Any number of relay connections, each a
//! (server, tenancy) pair with its own device credentials, contacts, inbox and a
//! control connection kept open, plus a short-lived transfer connection per message.
//!
//! Every device of a relay contact becomes a peer under its device id, the same id it
//! has on the LAN, so a device seen both ways is one peer. A device listed by several
//! connections (the same person in two tenancies) gets a route through each
//! (`Peer::routes`); sending picks the best one.

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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::timeout;

const REQ_TIMEOUT: Duration = Duration::from_secs(30);
const PING_EVERY: Duration = Duration::from_secs(30);
const CHUNK: usize = 256 * 1024;
const SEEN_CAP: usize = 500;
/// Where `hello` without a tenancy lands (protocol 8.3).
const UNMIGRATED: &str = "unmigrated";

/// One relay connection: a server and a tenancy on it.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// Local id of this connection.
    pub id: String,
    /// "host:port".
    pub url: String,
    pub tls: bool,
    /// Pinned SHA-256 (hex) of the server's leaf certificate.
    pub fingerprint: String,
    pub server_id: String,
    pub server_name: String,
    /// "<protocol>.<YYMMDD>.<NN>" as the server reported it.
    pub server_version: String,
    /// Server name in addresses (`hello` server.domain).
    pub domain: String,
    /// Tenancy ID sent in `hello`. Empty for a relay linked before tenancies (the
    /// unmigrated tenancy) until the first logon tells which tenancy it is.
    pub tenancy: String,
    pub tenancy_name: String,
    pub account: String,
    pub username: Option<String>,
    /// This device's address (protocol 9.1).
    pub address: String,
    pub policy: Value,
    pub contacts_since: i64,
    /// Account id -> contact entry (protocol 4.4).
    pub contacts: HashMap<String, Value>,
    /// The tenancy's departments: [{id, name, members}] (protocol 9.2).
    pub departments: Vec<Value>,
    /// Device id -> first public key seen (trust on first use).
    pub pins: HashMap<String, String>,
    /// Devices whose current key differs from the pin, until the user accepts it.
    pub key_changed: Vec<String>,
    /// Recently fetched server message ids, to drop redeliveries.
    pub seen: Vec<String>,
    /// Server features from its hello ("device_target", "hold", "tenancy", ...).
    pub features: Vec<String>,
    /// This device's "relay receive" setting: "allow", "device_off" or "all_off".
    pub hold: String,
}

impl RelayConfig {
    /// The tenancy as the server names it.
    fn tenancy_id(&self) -> &str {
        if self.tenancy.is_empty() { UNMIGRATED } else { &self.tenancy }
    }

    fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// Whether addresses ending in `server` mean this relay server.
    fn serves(&self, server: &str) -> bool {
        let ep = parse_endpoint(&self.url).ok();
        let names = [Some(self.domain.clone()), ep.as_ref().map(|e| e.host.clone()), ep.as_ref().map(|e| e.authority())];
        names.into_iter().flatten().any(|n| !n.is_empty() && n.eq_ignore_ascii_case(server))
    }

    /// "Tenancy name · server name" for lists.
    pub fn label(&self) -> String {
        let server = if self.server_name.is_empty() { self.url.clone() } else { self.server_name.clone() };
        let tn = if self.tenancy_name.is_empty() { self.tenancy.clone() } else { self.tenancy_name.clone() };
        if tn.is_empty() { server } else { format!("{tn} · {server}") }
    }
}

/// Device credentials per server and tenancy (see `cred_key`). Kept after
/// unlinking so the same device can log on again.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Creds {
    pub device_secret: String,
    pub secret_key: String,
    pub public_key: String,
}

/// The unmigrated tenancy keeps the per-server key of versions before tenancies.
fn cred_key(server: &str, tenancy: &str) -> String {
    if tenancy.is_empty() || tenancy == UNMIGRATED { server.to_string() } else { format!("{server}/{tenancy}") }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct RelayFile {
    conns: Vec<RelayConfig>,
    /// The one relay of versions before tenancies; moved into `conns` on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<RelayConfig>,
    keyring: HashMap<String, Creds>,
}

#[derive(Clone, Default, Serialize)]
pub struct ConnState {
    /// off | connecting | online | pending | error
    pub state: &'static str,
    pub message: String,
}

/// A relay connection and its live state.
#[derive(Default)]
pub struct Link {
    pub cfg: Mutex<RelayConfig>,
    conn: Mutex<ConnState>,
    session: Mutex<Option<String>>,
    out: Mutex<Option<mpsc::UnboundedSender<Value>>>,
    waiting: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    rid: AtomicU64,
    /// Reconnect requested or the connection was removed.
    wake: Notify,
    /// Name, status or limits changed.
    profile: Notify,
    inbox: tokio::sync::Mutex<()>,
    removed: AtomicBool,
}

impl Link {
    fn new(cfg: RelayConfig) -> Arc<Self> {
        Arc::new(Self { cfg: Mutex::new(cfg), ..Default::default() })
    }

    pub fn id(&self) -> String {
        self.cfg.lock().unwrap().id.clone()
    }

    pub fn config(&self) -> RelayConfig {
        self.cfg.lock().unwrap().clone()
    }

    pub fn online(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    fn policy(&self) -> Value {
        self.cfg.lock().unwrap().policy.clone()
    }
}

#[derive(Default)]
pub struct Relay {
    links: Mutex<Vec<Arc<Link>>>,
    keyring: Mutex<HashMap<String, Creds>>,
}

impl Relay {
    pub fn load(dir: &Path) -> Self {
        let mut f: RelayFile = std::fs::read(dir.join("relay.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        // A single relay saved before tenancies becomes the first connection; its
        // tenancy stays unset until the server tells which one it is.
        if let Some(a) = f.active.take() {
            if f.conns.is_empty() {
                f.conns.push(a);
            }
        }
        for c in f.conns.iter_mut().filter(|c| c.id.is_empty()) {
            c.id = new_id();
        }
        let links = f.conns.into_iter().map(Link::new).collect();
        Self { links: Mutex::new(links), keyring: Mutex::new(f.keyring) }
    }

    pub fn save(&self, dir: &Path) {
        let conns = self.links().iter().map(|l| l.config()).collect();
        let f = RelayFile { conns, active: None, keyring: self.keyring.lock().unwrap().clone() };
        let p = dir.join("relay.json");
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_vec(&f).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    pub fn links(&self) -> Vec<Arc<Link>> {
        self.links.lock().unwrap().clone()
    }

    pub fn link(&self, id: &str) -> Option<Arc<Link>> {
        self.links().into_iter().find(|l| l.id() == id)
    }

    /// What this device announces on the LAN about its relay registration: the
    /// first connection's (protocol 6).
    pub fn identity(&self) -> Option<RelayId> {
        self.links().iter().map(|l| l.config()).find(|c| !c.account.is_empty()).map(|c| RelayId {
            server: c.server_id.clone(),
            tenancy: c.tenancy_id().to_string(),
            account: c.account.clone(),
        })
    }

    pub fn configured(&self) -> bool {
        !self.links.lock().unwrap().is_empty()
    }

    /// Our connection to the relay a LAN peer announced, if we have one.
    pub fn conn_for(&self, r: &RelayId) -> Option<String> {
        let tn = if r.tenancy.is_empty() { UNMIGRATED } else { r.tenancy.as_str() };
        self.links().iter().map(|l| l.config()).find(|c| c.server_id == r.server && c.tenancy_id() == tn).map(|c| c.id)
    }

    /// (connection id, force_relay, block_direct_outsiders) of every connection.
    pub fn rules(&self) -> Vec<(String, bool, bool)> {
        self.links()
            .iter()
            .map(|l| {
                let c = l.cfg.lock().unwrap();
                let force = c.policy["force_relay"] == true;
                (c.id.clone(), force, force && c.policy["block_direct_outsiders"] == true)
            })
            .collect()
    }

    /// The tenancy's own credentials, else the server's (an account moved out of the
    /// unmigrated tenancy keeps its device secret).
    fn creds(&self, server: &str, tenancy: &str) -> Option<Creds> {
        let k = self.keyring.lock().unwrap();
        k.get(&cred_key(server, tenancy)).or_else(|| k.get(server)).cloned()
    }

    /// Another connection to the same server and tenancy.
    fn duplicate(&self, server: &str, tenancy: &str, except: &str) -> bool {
        let tn = if tenancy.is_empty() { UNMIGRATED } else { tenancy };
        self.links().iter().map(|l| l.config()).any(|c| c.id != except && c.server_id == server && c.tenancy_id() == tn)
    }
}

/// `<id>[:<device>]@[<departments>:]<tenancy>:<server>` -> (tenancy, server), lowercase.
pub fn address_parts(addr: &str) -> Option<(String, String)> {
    let (left, right) = addr.trim().rsplit_once('@')?;
    if left.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = right.split(':').map(str::to_string).collect();
    // "<host>:<port>" at the end.
    if parts.len() >= 3 && parts.last().is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        let port = parts.pop().unwrap_or_default();
        let host = parts.pop().unwrap_or_default();
        parts.push(format!("{host}:{port}"));
    }
    let (tenancy, server) = match parts.as_slice() {
        [t, s] | [_, t, s] => (t.to_lowercase(), s.to_lowercase()),
        _ => return None,
    };
    (!tenancy.is_empty() && !server.is_empty() && tenancy != "*").then_some((tenancy, server))
}

/// The connection to send to an address through: the one for its tenancy, else one to
/// the same server (the server decides whether its tenancies may exchange), else the
/// first (the server answers with its policy).
pub fn link_for_address(app: &AppHandle, addr: &str) -> Option<Arc<Link>> {
    let links = app.state::<AppState>().relay.links();
    let (tenancy, server) = address_parts(addr)?;
    let best = |f: &dyn Fn(&RelayConfig) -> bool| {
        let hits: Vec<&Arc<Link>> = links.iter().filter(|l| f(&l.cfg.lock().unwrap())).collect();
        hits.iter().find(|l| l.online()).or(hits.first()).map(|l| (*l).clone())
    };
    best(&|c| c.serves(&server) && c.tenancy_id() == tenancy)
        .or_else(|| best(&|c| c.serves(&server)))
        .or_else(|| best(&|_| true))
}

// ---------------------------------------------------------------- setup (wizard)

/// Tenancy IDs: 2-32 of a-z, 0-9 and '-'.
fn valid_tenancy(t: &str) -> bool {
    (2..=32).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Contacts a server for a new connection; refuses one we already have.
pub async fn probe(app: &AppHandle, url: &str, tenancy: &str) -> Result<Value, String> {
    let tenancy = tenancy.trim().to_lowercase();
    if !valid_tenancy(&tenancy) {
        return Err("Tenancy ID는 영문 소문자, 숫자, '-'로 된 2~32자입니다".into());
    }
    let r = probe_in(url, Some(&tenancy)).await?;
    let tn = r["tenancy"]["id"].as_str().unwrap_or("");
    if app.state::<AppState>().relay.duplicate(r["server"]["id"].as_str().unwrap_or(""), tn, "") {
        return Err("이 서버의 이 Tenancy에는 이미 연결되어 있습니다".into());
    }
    Ok(r)
}

/// Registers this device (or logs it on to an account) and adds the connection.
/// `mode`: "device" (no account), "login" or "register".
#[allow(clippy::too_many_arguments)]
pub async fn join(
    app: &AppHandle,
    url: &str,
    tls: bool,
    fingerprint: &str,
    tenancy: &str,
    mode: &str,
    user: &str,
    pass: &str,
) -> Result<Value, String> {
    let st = app.state::<AppState>();
    let tenancy = tenancy.trim().to_lowercase();
    if !valid_tenancy(&tenancy) {
        return Err("Tenancy ID는 영문 소문자, 숫자, '-'로 된 2~32자입니다".into());
    }
    let pin = (!fingerprint.is_empty()).then_some(fingerprint);
    let (mut io, seen) = connect(url, tls, pin).await?;
    let h = hello_in(&mut io, Some(&tenancy)).await?;
    let server_id = h["server"]["id"].as_str().unwrap_or("").to_string();
    if server_id.is_empty() {
        return Err("서버 정보를 받지 못했습니다".into());
    }
    // A server without tenancies ignores the ID; such a connection keeps none.
    let tenancy = h["tenancy"]["id"].as_str().unwrap_or("").to_string();
    if st.relay.duplicate(&server_id, &tenancy, "") {
        return Err("이 서버의 이 Tenancy에는 이미 연결되어 있습니다".into());
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
    let username = r["username"].as_str().map(str::to_string).or_else(|| (mode != "device").then(|| user.trim().to_string()));
    let mut cfg = RelayConfig {
        id: new_id(),
        url: url.to_string(),
        tls,
        fingerprint: if seen.fingerprint.is_empty() { fingerprint.to_string() } else { seen.fingerprint },
        server_id: server_id.clone(),
        server_name: h["server"]["name"].as_str().unwrap_or("").to_string(),
        server_version: h["server"]["version"].as_str().unwrap_or("").to_string(),
        domain: h["server"]["domain"].as_str().unwrap_or("").to_string(),
        tenancy,
        tenancy_name: h["tenancy"]["name"].as_str().unwrap_or("").to_string(),
        account: r["account"].as_str().unwrap_or("").to_string(),
        username,
        policy: h["policy"].clone(),
        features: features(&h),
        ..Default::default()
    };
    take_login(&mut cfg, &r);
    if st.relay.duplicate(&server_id, &cfg.tenancy, "") {
        return Err("이 계정은 이미 연결된 다른 Tenancy로 옮겨졌습니다".into());
    }
    st.relay.keyring.lock().unwrap().insert(cred_key(&server_id, &cfg.tenancy), creds);
    let link = Link::new(cfg);
    st.relay.links.lock().unwrap().push(link.clone());
    st.relay.save(&st.dir);
    tauri::async_runtime::spawn(manager(app.clone(), link));
    changed(app);
    Ok(json!({ "state": if ok { "online" } else { "pending" }, "message": if ok { String::new() } else { err_text(&r) } }))
}

fn features(h: &Value) -> Vec<String> {
    h["features"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(str::to_string)).collect()
}

/// What a `login` reply says about the account: where it is and its address.
/// Returns the tenancy it had before when the account turned out to be elsewhere
/// (a relay linked before tenancies, or an account moved by the server's admin).
fn take_login(c: &mut RelayConfig, r: &Value) -> Option<String> {
    c.account = r["account"].as_str().unwrap_or(&c.account).to_string();
    if r["username"].is_string() {
        c.username = r["username"].as_str().map(str::to_string);
    }
    if let Some(a) = r["address"].as_str() {
        c.address = a.to_string();
    }
    let tn = &r["tenancy"];
    if let Some(name) = tn["name"].as_str() {
        c.tenancy_name = name.to_string();
    }
    match tn["id"].as_str() {
        Some(id) if !id.is_empty() && id != c.tenancy => Some(std::mem::replace(&mut c.tenancy, id.to_string())),
        _ => None,
    }
}

/// Logs off and removes a connection (device credentials are kept for a later re-link).
pub async fn disconnect(app: &AppHandle, id: &str) {
    let st = app.state::<AppState>();
    let Some(link) = st.relay.link(id) else { return };
    if link.online() {
        let _ = timeout(Duration::from_secs(3), request(&link, json!({ "t": "logout" }))).await;
    }
    remove(app, &link);
}

fn remove(app: &AppHandle, link: &Arc<Link>) {
    let st = app.state::<AppState>();
    link.removed.store(true, Ordering::Relaxed);
    st.relay.links.lock().unwrap().retain(|l| !Arc::ptr_eq(l, link));
    st.relay.save(&st.dir);
    link.wake.notify_one();
    rebuild_peers(app);
    changed(app);
}

/// App exit: log off everywhere.
pub async fn logout(app: &AppHandle) {
    let links = app.state::<AppState>().relay.links();
    let _ = timeout(Duration::from_secs(2), async {
        for l in links.iter().filter(|l| l.online()) {
            let _ = request(l, json!({ "t": "logout" })).await;
        }
    })
    .await;
}

pub fn reconnect(app: &AppHandle, id: &str) {
    if let Some(l) = app.state::<AppState>().relay.link(id) {
        l.wake.notify_one();
    }
}

/// Accepts every changed device key of a connection as the new pin.
pub fn accept_keys(app: &AppHandle, id: &str) {
    let st = app.state::<AppState>();
    let Some(link) = st.relay.link(id) else { return };
    {
        let mut c = link.cfg.lock().unwrap();
        let changed: Vec<String> = std::mem::take(&mut c.key_changed);
        for d in changed {
            if let Some(k) = device_key(&c, &d) {
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

/// Everything the settings panel shows, per connection.
pub fn info(app: &AppHandle) -> Value {
    let st = app.state::<AppState>();
    let conns: Vec<Value> = st
        .relay
        .links()
        .iter()
        .map(|l| {
            let conn = l.conn.lock().unwrap().clone();
            let c = l.config();
            let my_key = st.relay.creds(&c.server_id, &c.tenancy).map(|k| e2ee::fingerprint(&k.public_key)).unwrap_or_default();
            let changed: Vec<Value> = c
                .key_changed
                .iter()
                .map(|d| json!({ "device": d, "name": device_name(&c, d), "fingerprint": device_key(&c, d).map(|k| e2ee::fingerprint(&k)).unwrap_or_default() }))
                .collect();
            json!({
                "id": c.id, "state": conn.state, "message": conn.message,
                "url": c.url, "tls": c.tls, "fingerprint": c.fingerprint, "server_id": c.server_id, "server_name": c.server_name,
                "server_version": c.server_version, "domain": c.domain, "tenancy": c.tenancy, "tenancy_name": c.tenancy_name,
                "username": c.username, "account": c.account, "address": c.address, "policy": c.policy,
                "contacts": c.contacts.len().saturating_sub(1), "departments": c.departments.len(), "my_key": my_key, "key_changed": changed,
                "hold": if c.has("hold") { json!(c.hold) } else { Value::Null },
            })
        })
        .collect();
    json!({ "device": st.my_id(), "conns": conns })
}

/// Reads, or with `mode` sets, this device's "relay receive" setting on one
/// connection: whether the relay keeps messages for it while it is offline.
pub async fn hold(app: &AppHandle, id: &str, mode: Option<&str>) -> Result<String, String> {
    let st = app.state::<AppState>();
    let link = st.relay.link(id).ok_or("없는 연결입니다")?;
    hold_on(app, &link, mode).await
}

async fn hold_on(app: &AppHandle, link: &Arc<Link>, mode: Option<&str>) -> Result<String, String> {
    let st = app.state::<AppState>();
    if !link.cfg.lock().unwrap().has("hold") {
        return Err("이 릴레이 서버는 이 설정을 지원하지 않습니다. 서버를 업데이트하세요".into());
    }
    let mut req = json!({ "t": "hold" });
    if let Some(m) = mode {
        req["mode"] = json!(m);
    }
    let r = request(link, req).await?;
    if r["t"] != "hold" {
        return Err(err_text(&r));
    }
    let mode = r["mode"].as_str().unwrap_or("allow").to_string();
    link.cfg.lock().unwrap().hold = mode.clone();
    st.relay.save(&st.dir);
    changed(app);
    Ok(mode)
}

fn changed(app: &AppHandle) {
    let _ = app.emit("relay", ());
}

fn set_conn(app: &AppHandle, link: &Link, state: &'static str, message: impl Into<String>) {
    *link.conn.lock().unwrap() = ConnState { state, message: message.into() };
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
    for link in app.state::<AppState>().relay.links() {
        tauri::async_runtime::spawn(manager(app.clone(), link));
    }
}

/// Keeps one connection's control connection up until the connection is removed.
async fn manager(app: AppHandle, link: Arc<Link>) {
    let mut backoff = Duration::from_secs(2);
    while !link.removed.load(Ordering::Relaxed) {
        set_conn(&app, &link, "connecting", "");
        let r = session(&app, &link).await;
        *link.session.lock().unwrap() = None;
        *link.out.lock().unwrap() = None;
        link.waiting.lock().unwrap().clear();
        rebuild_peers(&app); // its contacts now read as offline
        if link.removed.load(Ordering::Relaxed) {
            break;
        }
        let wait = match r {
            Stop::Net(m) => {
                set_conn(&app, &link, "error", m);
                let w = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                w
            }
            Stop::Pending(m) => {
                set_conn(&app, &link, "pending", m);
                Duration::from_secs(60)
            }
            Stop::Fatal(m) => {
                set_conn(&app, &link, "error", m);
                Duration::from_secs(600)
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = link.wake.notified() => { backoff = Duration::from_secs(2); }
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

async fn session(app: &AppHandle, link: &Arc<Link>) -> Stop {
    let st = app.state::<AppState>();
    let cfg = link.config();
    let Some(creds) = st.relay.creds(&cfg.server_id, &cfg.tenancy) else {
        return Stop::Fatal("기기 인증 정보가 없습니다. 서버를 다시 연결하세요".into());
    };
    let (mut io, seen) = match connect(&cfg.url, cfg.tls, Some(&cfg.fingerprint)).await {
        Ok(x) => x,
        Err(e) if e.contains("인증서가 바뀌었습니다") => return Stop::Fatal(e),
        Err(e) => return Stop::Net(e),
    };
    let h = match hello_in(&mut io, Some(&cfg.tenancy)).await {
        Ok(h) => h,
        Err(e) if e.starts_with(UNKNOWN_TENANCY) => return Stop::Fatal(e),
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
    let moved = {
        let mut c = link.cfg.lock().unwrap();
        c.policy = h["policy"].clone();
        c.server_name = h["server"]["name"].as_str().unwrap_or(&c.server_name).to_string();
        c.server_version = h["server"]["version"].as_str().unwrap_or(&c.server_version).to_string();
        c.domain = h["server"]["domain"].as_str().unwrap_or(&c.domain).to_string();
        c.features = features(&h);
        if seen.trusted && !seen.fingerprint.is_empty() {
            c.fingerprint = seen.fingerprint.clone(); // a CA-valid certificate may rotate
        }
        take_login(&mut c, &r).map(|_| c.tenancy.clone())
    };
    if let Some(tn) = moved {
        // From now on log on to that tenancy directly, with the same credentials.
        st.relay.keyring.lock().unwrap().entry(cred_key(&cfg.server_id, &tn)).or_insert(creds);
        if st.relay.duplicate(&cfg.server_id, &tn, &cfg.id) {
            // Another connection already is that tenancy: this one is redundant.
            let _ = write_frame(&mut io, &json!({ "t": "logout", "rid": "o" })).await;
            remove(app, link);
            return Stop::Net(String::new());
        }
    }
    st.relay.save(&st.dir);
    *link.session.lock().unwrap() = r["session"].as_str().map(str::to_string);

    let (mut rd, mut wr) = tokio::io::split(io);
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    *link.out.lock().unwrap() = Some(tx.clone());
    let writer = tauri::async_runtime::spawn(async move {
        while let Some(v) = rx.recv().await {
            if write_frame(&mut wr, &v).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    set_conn(app, link, "online", "");
    {
        let (a, l) = (app.clone(), link.clone());
        tauri::async_runtime::spawn(async move {
            sync_contacts(&a, &l).await;
            sync_departments(&a, &l).await;
            let _ = hold_on(&a, &l, None).await;
            process_inbox(&a, &l).await;
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
                            let (a, l) = (app.clone(), link.clone());
                            tauri::async_runtime::spawn(async move { process_inbox(&a, &l).await });
                        }
                        "contact" => {
                            apply_contacts(app, link, std::slice::from_ref(&f["contact"]), false);
                            rebuild_peers(app);
                        }
                        "policy" => {
                            link.cfg.lock().unwrap().policy = f["policy"].clone();
                            st.relay.save(&st.dir);
                            changed(app);
                        }
                        "kicked" => break Stop::Net(f["reason"].as_str().unwrap_or("서버가 연결을 끊었습니다").to_string()),
                        _ => {}
                    }
                } else if let Some(rid) = f["rid"].as_str() {
                    if let Some(w) = link.waiting.lock().unwrap().remove(rid) {
                        let _ = w.send(f);
                    }
                }
            }
            _ = ping.tick() => {
                let _ = tx.send(json!({ "t": "ping", "rid": "p" }));
            }
            _ = link.profile.notified() => {
                let _ = tx.send(json!({ "t": "profile", "rid": "pr", "profile": profile(&st.my_meta()) }));
            }
            _ = link.wake.notified() => {
                link.wake.notify_one(); // let the manager see it too
                break Stop::Net(String::new());
            }
        }
    };
    drop(tx);
    *link.out.lock().unwrap() = None;
    writer.abort();
    stop
}

/// Sends a request on a control connection and waits for its reply.
async fn request(link: &Link, mut v: Value) -> Result<Value, String> {
    let rid = format!("c{}", link.rid.fetch_add(1, Ordering::Relaxed));
    v["rid"] = json!(rid);
    let (tx, rx) = oneshot::channel();
    link.waiting.lock().unwrap().insert(rid.clone(), tx);
    let sent = link.out.lock().unwrap().as_ref().map(|o| o.send(v).is_ok()).unwrap_or(false);
    if !sent {
        link.waiting.lock().unwrap().remove(&rid);
        return Err("릴레이 서버에 연결되어 있지 않습니다".into());
    }
    let r = timeout(REQ_TIMEOUT, rx).await.map_err(|_| "서버 응답 시간 초과".to_string())?.map_err(|_| "서버 연결이 끊어졌습니다".to_string())?;
    if r["t"] == "err" { Err(err_text(&r)) } else { Ok(r) }
}

/// Tells every relay about a changed name, status or limit.
pub fn announce(app: &AppHandle) {
    for l in app.state::<AppState>().relay.links() {
        l.profile.notify_one();
    }
}

/// "다시 탐색": refresh contacts and departments too.
pub fn refresh(app: &AppHandle) {
    for l in app.state::<AppState>().relay.links().into_iter().filter(|l| l.online()) {
        let a = app.clone();
        tauri::async_runtime::spawn(async move {
            sync_contacts(&a, &l).await;
            sync_departments(&a, &l).await;
        });
    }
}

// ---------------------------------------------------------------- contacts

/// Asks the server for an account by exact username or account id (needed on
/// "lookup" servers, where contacts only hold people you have talked to).
pub async fn lookup(app: &AppHandle, link: &Arc<Link>, query: &str) -> Option<String> {
    let r = request(link, json!({ "t": "lookup", "query": query })).await.ok()?;
    let entry = r["contact"].clone();
    let acc = entry["account"].as_str()?.to_string();
    // Unchanged contacts are left alone: rebuilding emits "peers", which makes an
    // open search window search (and look up) again.
    let known = link.cfg.lock().unwrap().contacts.get(&acc).cloned();
    if known.as_ref() != Some(&entry) {
        apply_contacts(app, link, std::slice::from_ref(&entry), false);
        rebuild_peers(app);
    }
    Some(acc)
}

async fn sync_contacts(app: &AppHandle, link: &Arc<Link>) {
    let st = app.state::<AppState>();
    let since = link.cfg.lock().unwrap().contacts_since;
    let Ok(r) = request(link, json!({ "t": "contacts", "since": since })).await else { return };
    let list = r["list"].as_array().cloned().unwrap_or_default();
    apply_contacts(app, link, &list, r["full"] == true);
    link.cfg.lock().unwrap().contacts_since = r["now"].as_i64().unwrap_or(0);
    st.relay.save(&st.dir);
    rebuild_peers(app);
}

async fn sync_departments(app: &AppHandle, link: &Arc<Link>) {
    if !link.cfg.lock().unwrap().has("departments") {
        return;
    }
    let Ok(r) = request(link, json!({ "t": "departments" })).await else { return };
    let list = r["list"].as_array().cloned().unwrap_or_default();
    let st = app.state::<AppState>();
    link.cfg.lock().unwrap().departments = list;
    st.relay.save(&st.dir);
    changed(app);
}

/// Trust on first use for the device keys in contact entries: new keys are pinned,
/// changed ones listed until the user accepts them.
fn pin_keys(c: &mut RelayConfig, entries: &[Value]) {
    for d in entries.iter().flat_map(|e| e["devices"].as_array().cloned().unwrap_or_default()) {
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
}

fn apply_contacts(app: &AppHandle, link: &Link, list: &[Value], full: bool) {
    let st = app.state::<AppState>();
    {
        let mut c = link.cfg.lock().unwrap();
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
            pin_keys(&mut c, std::slice::from_ref(e));
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

/// Mirrors every connection's relay contacts into the peer list, one peer per device
/// with a route through each connection that lists it.
pub fn rebuild_peers(app: &AppHandle) {
    let st = app.state::<AppState>();
    let links: Vec<(RelayConfig, bool)> = st.relay.links().iter().map(|l| (l.config(), l.online())).collect();
    let my_dev = st.my_id();
    {
        let mut d = st.data.lock().unwrap();
        // Before 0.1.8 a relay contact was one "r:<account>" peer; groups now get its devices.
        d.peers.retain(|id, _| !id.starts_with("r:"));
        let mut legacy: HashMap<String, Vec<String>> = HashMap::new();
        let mut routes: HashMap<String, Vec<Route>> = HashMap::new();
        // My own account is listed too: my other devices can be reached through the relay.
        for (c, online) in &links {
            for (acc, e) in &c.contacts {
                if e["removed"] == true {
                    continue;
                }
                let username = e["username"].as_str().unwrap_or("").to_string();
                for dv in e["devices"].as_array().into_iter().flatten() {
                    let Some(id) = dv["id"].as_str().filter(|id| *id != my_dev) else { continue };
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
                    let up = *online && dv["online"] == true;
                    routes.entry(id.to_string()).or_default().push(Route {
                        conn: c.id.clone(),
                        account: acc.clone(),
                        username: username.clone(),
                        hold: dv["hold"] != false,
                        online: up,
                        status: if up { status_of(&dv["status"]) } else { Status::Offline },
                    });
                }
            }
        }
        // Devices no relay lists any more: LAN ones stay, relay-only ones go.
        d.peers.retain(|id, p| routes.contains_key(id) || !p.ip.is_empty());
        let conns: HashSet<&str> = links.iter().map(|(c, _)| c.id.as_str()).collect();
        for (id, p) in d.peers.iter_mut() {
            let mut r = routes.remove(id).unwrap_or_default();
            // Announced on the LAN for one of our relays: that relay can still be tried.
            if !p.relay_self.is_empty() && conns.contains(p.relay_self_conn.as_str()) && !r.iter().any(|x| x.conn == p.relay_self_conn) {
                r.push(Route { conn: p.relay_self_conn.clone(), account: p.relay_self.clone(), ..Default::default() });
            }
            // Best first: connected there, then stored for while offline.
            r.sort_by_key(|x| (!x.online, !x.hold));
            p.relay_online = r.iter().any(|x| x.online);
            p.relay_hold = r.iter().any(|x| x.hold);
            p.relay_status = if r.iter().any(|x| x.online && x.status == Status::Online) {
                Status::Online
            } else if p.relay_online {
                Status::Dnd
            } else {
                Status::Offline
            };
            p.relay_account = r.first().map(|x| x.account.clone()).unwrap_or_default();
            p.relay_conn = r.first().map(|x| x.conn.clone()).unwrap_or_default();
            p.username = r.iter().map(|x| x.username.clone()).find(|u| !u.is_empty()).unwrap_or_default();
            p.routes = r;
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
    static ANNOUNCED: Mutex<Option<Option<RelayId>>> = Mutex::new(None);
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

/// A LAN device announced relay account `acc` on connection `conn`'s relay: link it
/// as a contact (once per run) so the relay knows its devices and keys.
pub fn learn_account(app: &AppHandle, conn: &str, acc: &str) {
    static TRIED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let st = app.state::<AppState>();
    let Some(link) = st.relay.link(conn) else { return };
    let known = {
        let c = link.cfg.lock().unwrap();
        c.contacts.contains_key(acc) || c.account == acc
    };
    if !link.online() || known {
        return;
    }
    if !TRIED.lock().unwrap().get_or_insert_with(HashSet::new).insert(format!("{conn}/{acc}")) {
        return;
    }
    let (app, acc) = (app.clone(), acc.to_string());
    tauri::async_runtime::spawn(async move {
        if lookup(&app, &link, &acc).await.is_none() {
            rebuild_peers(&app);
        }
    });
}

/// Departments of every connection with the devices of their members, for picking a
/// whole department as recipients (protocol 9.2).
pub fn departments(app: &AppHandle) -> Vec<Value> {
    let st = app.state::<AppState>();
    let me = st.my_id();
    let mut out = Vec::new();
    for c in st.relay.links().iter().map(|l| l.config()) {
        let mut ids: Vec<(String, String)> = c
            .departments
            .iter()
            .filter_map(|d| Some((d["id"].as_str()?.to_string(), d["name"].as_str().unwrap_or("").to_string())))
            .collect();
        // Departments only seen in contact entries (listed before the request answered).
        for e in c.contacts.values() {
            for dep in e["departments"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
                if !ids.iter().any(|x| x.0 == dep) {
                    ids.push((dep.to_string(), String::new()));
                }
            }
        }
        for (id, name) in ids {
            let accounts: Vec<&Value> = c
                .contacts
                .values()
                .filter(|e| e["removed"] != true && e["departments"].as_array().is_some_and(|a| a.iter().any(|x| x == id.as_str())))
                .collect();
            let members: Vec<String> = accounts
                .iter()
                .flat_map(|e| e["devices"].as_array().cloned().unwrap_or_default())
                .filter_map(|d| d["id"].as_str().map(str::to_string))
                .filter(|d| *d != me)
                .collect();
            out.push(json!({
                "conn": c.id, "label": c.label(), "id": id, "name": if name.is_empty() { id.clone() } else { name },
                "people": accounts.len(), "members": members,
                "address": format!("*@{id}:{}:{}", c.tenancy_id(), c.domain),
            }));
        }
    }
    out
}

/// Who an address reaches (protocol 9.3), with device keys pinned like contacts'.
pub async fn resolve(app: &AppHandle, link: &Arc<Link>, address: &str) -> Result<Vec<Value>, String> {
    let r = request(link, json!({ "t": "resolve", "address": address.trim() })).await?;
    let list = r["list"].as_array().cloned().unwrap_or_default();
    pin_keys(&mut link.cfg.lock().unwrap(), &list);
    let st = app.state::<AppState>();
    st.relay.save(&st.dir);
    Ok(list)
}

/// For the search window: checks an address on the connection that would send to it.
pub async fn check_address(app: &AppHandle, address: &str) -> Result<Value, String> {
    if address_parts(address).is_none() {
        return Err("주소 형식이 올바르지 않습니다 (예: kim@example:relay.example.com)".into());
    }
    let link = link_for_address(app, address).ok_or("연결된 릴레이 서버가 없습니다")?;
    let list = resolve(app, &link, address).await?;
    if list.is_empty() {
        return Err("이 주소로 받을 사람이 없습니다".into());
    }
    let people: Vec<Value> = list
        .iter()
        .map(|e| json!({ "name": e["name"], "username": e["username"], "address": e["address"], "tenancy": e["tenancy"], "online": e["online"] }))
        .collect();
    Ok(json!({ "address": address.trim(), "conn": link.id(), "label": link.cfg.lock().unwrap().label(), "people": people }))
}

// ---------------------------------------------------------------- transfers

async fn transfer_conn(link: &Link) -> Result<Stream, String> {
    let cfg = link.config();
    let session = link.session.lock().unwrap().clone().ok_or("릴레이 서버에 연결되어 있지 않습니다")?;
    let (mut io, _) = connect(&cfg.url, cfg.tls, Some(&cfg.fingerprint)).await?;
    hello(&mut io).await?;
    write_frame(&mut io, &json!({ "t": "auth", "rid": "a", "session": session })).await?;
    let r = read_frame(&mut io, IO_TIMEOUT).await?;
    if r["t"] != "ok" {
        return Err(err_text(&r));
    }
    Ok(io)
}

#[derive(Clone)]
pub struct Target {
    pub peer: String,
    pub account: String,
    /// Only this device of the account (when the server supports it).
    pub device: Option<String>,
    /// An address (protocol 9.1) instead of an account of this tenancy.
    pub address: Option<String>,
}

/// Through one connection. Per target peer: Ok("relay" | "stored") or Err(reason).
pub async fn send(
    app: &AppHandle,
    link: &Arc<Link>,
    job: &str,
    targets: &[Target],
    text: &str,
    files: &[(String, String, u64)],
    group: Option<&GroupInfo>,
) -> Vec<(String, Result<String, String>)> {
    let fail_all = |e: String| targets.iter().map(|t| (t.peer.clone(), Err(e.clone()))).collect();
    let st = app.state::<AppState>();
    if !link.online() {
        return fail_all(format!("릴레이 서버({})에 연결되어 있지 않습니다", link.cfg.lock().unwrap().label()));
    }
    let pol = link.policy();
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
        sync_contacts(app, link).await; // latest device keys
    }

    let mut out: Vec<(String, Result<String, String>)> = Vec::new();
    let mut accounts: Vec<String> = Vec::new(); // "to" entries
    let mut recipients: Vec<(String, String)> = Vec::new();
    let by_device = link.cfg.lock().unwrap().has("device_target");
    let to_of = |t: &Target| match (&t.address, &t.device) {
        (Some(a), _) => a.trim().to_string(),
        (None, Some(d)) if by_device => format!("{}/{d}", t.account),
        _ => t.account.clone(),
    };
    for t in targets {
        let to = to_of(t);
        if accounts.contains(&to) {
            continue;
        }
        if e2ee {
            // Someone in another tenancy is not in the contacts: ask who the address reaches.
            let entries = match &t.address {
                Some(a) => match resolve(app, link, a).await {
                    Ok(list) => list,
                    Err(e) => {
                        out.push((t.peer.clone(), Err(e)));
                        continue;
                    }
                },
                None => {
                    let c = link.config();
                    c.contacts.get(&t.account).cloned().into_iter().collect()
                }
            };
            let mut devs: Vec<Value> = entries.iter().flat_map(|e| e["devices"].as_array().cloned().unwrap_or_default()).collect();
            if let (Some(d), true, None) = (&t.device, by_device, &t.address) {
                devs.retain(|x| x["id"] == json!(d));
            }
            let key_changed = link.cfg.lock().unwrap().key_changed.clone();
            if devs.iter().any(|d| key_changed.iter().any(|x| d["id"] == json!(x))) {
                out.push((t.peer.clone(), Err("받는 사람의 보안 키가 바뀌었습니다. 설정 > 릴레이 서버에서 확인하세요".into())));
                continue;
            }
            if !devs.iter().any(|d| d["pubkey"].as_str().is_some_and(|k| !k.is_empty())) {
                out.push((t.peer.clone(), Err("받는 사람의 보안 키를 알 수 없습니다".into())));
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
        accounts.push(to);
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
        let creds = {
            let c = link.cfg.lock().unwrap();
            st.relay.creds(&c.server_id, &c.tenancy)
        };
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
        let mut io = transfer_conn(link).await?;
        write_frame(&mut io, &json!({ "t": "send", "rid": "s", "msg": msg, "to": accounts, "files": sizes, "body": body })).await?;
        let ready = read_frame(&mut io, IO_TIMEOUT).await?;
        if ready["t"] != "ready" {
            return Ok(ready); // err with optional `rejected`
        }
        let label = link.cfg.lock().unwrap().label();
        upload(app, job, &label, &mut io, files, key.as_ref()).await?;
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
    let mut results: Vec<Value> = r["result"].as_array().cloned().unwrap_or_default();
    results.extend(r["rejected"].as_array().cloned().unwrap_or_default());
    for t in targets {
        if blocked.contains(&t.peer) {
            continue;
        }
        // An address may reach several accounts: one line each, all with the same `to`.
        let to = to_of(t);
        let lines: Vec<&Value> = results.iter().filter(|x| x["to"] == json!(to)).collect();
        let fails: Vec<String> = lines.iter().filter(|x| x["state"] != "delivered" && x["state"] != "stored").map(|x| err_text(x)).collect();
        let v = if lines.iter().any(|x| x["state"] == "delivered") {
            Ok("relay".to_string())
        } else if lines.iter().any(|x| x["state"] == "stored") {
            Ok("stored".to_string())
        } else if let Some(e) = fails.first() {
            Err(e.clone())
        } else if r["t"] == "err" {
            Err(err_text(&r))
        } else {
            Err("서버 응답에 받는 사람이 없습니다".into())
        };
        out.push((t.peer.clone(), v));
    }
    out
}

async fn upload(app: &AppHandle, job: &str, label: &str, io: &mut Stream, files: &[(String, String, u64)], key: Option<&e2ee::Key>) -> Result<(), String> {
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
                let _ = app.emit("send-progress", json!({ "job": job, "to": format!("릴레이 서버 ({label})"), "done": done, "total": total }));
            }
            if remain == 0 {
                break;
            }
        }
    }
    io.flush().await.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- receiving

async fn process_inbox(app: &AppHandle, link: &Arc<Link>) {
    let _one = link.inbox.lock().await;
    let Ok(r) = request(link, json!({ "t": "inbox" })).await else { return };
    for item in r["list"].as_array().cloned().unwrap_or_default() {
        let mid = item["mid"].as_str().unwrap_or("").to_string();
        if let Err(e) = fetch(app, link, &mid).await {
            set_conn(app, link, "online", format!("쪽지를 받지 못했습니다: {e}"));
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
                    from_address: String::new(),
                    note: format!("{name} 님이 보낸 쪽지 {n}개가 릴레이 서버 보관 기간이 지나 삭제되었습니다"),
                    group: None,
                },
            );
        }
        let _ = request(link, json!({ "t": "expired_ack" })).await;
    }
}

async fn fetch(app: &AppHandle, link: &Arc<Link>, mid: &str) -> Result<(), String> {
    let st = app.state::<AppState>();
    let mut io = transfer_conn(link).await?;
    let already = link.cfg.lock().unwrap().seen.iter().any(|m| m == mid);
    if !already {
        write_frame(&mut io, &json!({ "t": "fetch", "rid": "f", "mid": mid })).await?;
        let h = read_frame(&mut io, IO_TIMEOUT).await?;
        if h["t"] != "fetch" {
            return if h["code"] == "not_found" { Ok(()) } else { Err(err_text(&h)) };
        }
        receive(app, link, &mut io, &h).await?;
        {
            let mut c = link.cfg.lock().unwrap();
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

/// The sending device's public key: from the contacts, or for a sender in another
/// tenancy (not a contact) from resolving its address. With whether it changed.
async fn sender_key(app: &AppHandle, link: &Arc<Link>, from: &str, device: &str, address: &str) -> Option<(String, bool)> {
    let known = |c: &RelayConfig| -> Option<(String, bool)> {
        let k = c.contacts.get(from)?["devices"].as_array()?.iter().find(|d| d["id"] == json!(device))?["pubkey"].as_str()?.to_string();
        Some((k, c.key_changed.iter().any(|x| x == device)))
    };
    let k = known(&link.cfg.lock().unwrap());
    if k.is_some() {
        return k;
    }
    sync_contacts(app, link).await;
    let k = known(&link.cfg.lock().unwrap());
    if k.is_some() {
        return k;
    }
    if address.is_empty() {
        return None;
    }
    let list = resolve(app, link, address).await.ok()?;
    let k = list
        .iter()
        .flat_map(|e| e["devices"].as_array().cloned().unwrap_or_default())
        .find(|d| d["id"] == json!(device))?["pubkey"]
        .as_str()?
        .to_string();
    let c = link.cfg.lock().unwrap();
    let changed = c.pins.get(device).is_some_and(|p| *p != k);
    Some((k, changed))
}

/// Reads the attachments that follow a fetch header, saves them and delivers the message.
async fn receive(app: &AppHandle, link: &Arc<Link>, io: &mut Stream, h: &Value) -> Result<(), String> {
    let st = app.state::<AppState>();
    let from = h["from"].as_str().unwrap_or("").to_string();
    let from_device = h["from_device"].as_str().unwrap_or("");
    let from_name = h["from_name"].as_str().unwrap_or("").to_string();
    let from_address = h["from_address"].as_str().unwrap_or("").to_string();
    let sizes: Vec<u64> = h["files"].as_array().into_iter().flatten().map(|f| f["size"].as_u64().unwrap_or(0)).collect();
    let mut note = String::new();
    let body = &h["body"];
    let (my_tenancy, creds) = {
        let c = link.cfg.lock().unwrap();
        (c.tenancy_id().to_string(), st.relay.creds(&c.server_id, &c.tenancy))
    };
    // From another tenancy: not a contact here, so answered through its address.
    let foreign = address_parts(&from_address).is_some_and(|(t, _)| t != my_tenancy);

    // Decrypt (or read) text, names and group.
    let mut key = None;
    let plain = if body["enc"] == "none" || body["enc"].is_null() {
        body.clone()
    } else {
        match (sender_key(app, link, &from, from_device, &from_address).await, creds) {
            (Some((sender, changed)), Some(creds)) => match e2ee::open(body, &st.my_id(), &creds.secret_key, &sender) {
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
            _ => {
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
    // Replies go to the device it came from (LAN first, then a relay); to its address
    // when it is in another tenancy and no connection of ours lists that device.
    let known = !from_device.is_empty() && st.data.lock().unwrap().peers.get(from_device).is_some_and(|p| !p.routes.is_empty() || p.online);
    let from_id = if foreign && !known {
        from_address.clone()
    } else if from_device.is_empty() {
        format!("r:{from}")
    } else {
        from_device.to_string()
    };
    net::deliver(
        app,
        Message {
            id: new_id(),
            from_id,
            from_name,
            from_ip: "릴레이".into(),
            text: plain["text"].as_str().unwrap_or("").to_string(),
            files: saved,
            ts: h["ts"].as_u64().unwrap_or_else(now_ms),
            read: false,
            via: "relay".into(),
            from_address,
            note,
            group,
        },
    );
    tray::refresh(app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        let p = |s: &str| address_parts(s).map(|(t, s)| format!("{t}|{s}"));
        assert_eq!(p("kim@example:relay.example.com").as_deref(), Some("example|relay.example.com"));
        assert_eq!(p("john:948a@dev,ops:Example:relay.example.com:45443").as_deref(), Some("example|relay.example.com:45443"));
        assert_eq!(p("*@dev:beta:10.0.0.5").as_deref(), Some("beta|10.0.0.5"));
        assert_eq!(p("a@b@beta:host").as_deref(), Some("beta|host"));
        assert_eq!(p("kim@relay.example.com"), None);
        assert_eq!(p("@beta:host"), None);
        assert_eq!(p("kim@*:host"), None);
    }

    #[test]
    fn legacy_config() {
        let dir = std::env::temp_dir().join(format!("pm-relay-test-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = json!({ "active": { "url": "relay.example.com:45443", "server_id": "s1", "account": "a1" },
                          "keyring": { "s1": { "device_secret": "x", "secret_key": "y", "public_key": "z" } } });
        std::fs::write(dir.join("relay.json"), old.to_string()).unwrap();
        let r = Relay::load(&dir);
        let links = r.links();
        assert_eq!(links.len(), 1);
        let c = links[0].config();
        assert!(!c.id.is_empty() && c.tenancy.is_empty() && c.tenancy_id() == UNMIGRATED);
        assert_eq!(r.creds("s1", "").map(|k| k.device_secret).as_deref(), Some("x"));
        // An account moved to another tenancy keeps its device secret.
        assert_eq!(r.creds("s1", "acme").map(|k| k.device_secret).as_deref(), Some("x"));
        assert_eq!(r.identity().map(|i| i.tenancy).as_deref(), Some(UNMIGRATED));
        r.save(&dir);
        let saved: Value = serde_json::from_slice(&std::fs::read(dir.join("relay.json")).unwrap()).unwrap();
        assert!(saved["active"].is_null() && saved["conns"][0]["id"] == json!(c.id));
        assert!(r.duplicate("s1", "unmigrated", "") && !r.duplicate("s1", "acme", ""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
