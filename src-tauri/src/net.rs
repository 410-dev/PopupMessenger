//! Discovery (UDP) and messaging (TCP) share one port number: 55000 + a,
//! where `a` is the first offset whose TCP and UDP ports are both free.

use crate::state::*;
use crate::{tray, ui};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::timeout;

pub const BASE_PORT: u16 = 55000;
/// Test profiles (see `profile()`) use their own range so they never meet real instances.
const PROFILE_BASE_PORT: u16 = 47000;

fn base_port() -> u16 {
    if crate::profile().is_some() { PROFILE_BASE_PORT } else { BASE_PORT }
}
pub const MAX_PORT: u16 = 65500;
const SCAN_EVERY: Duration = Duration::from_secs(30);
const CHUNK: usize = 256 * 1024;
const MAX_FRAME: u32 = 8 * 1024 * 1024;
/// Transfers at or above these show the "receiving" window.
const BIG_COUNT: usize = 3;
const BIG_BYTES: u64 = 20 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

static UDP: OnceLock<Arc<UdpSocket>> = OnceLock::new();

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

pub fn human(n: u64) -> String {
    let u = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < u.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", u[i]) }
}

pub fn bind() -> Option<(std::net::TcpListener, std::net::UdpSocket, u16)> {
    for p in base_port()..=MAX_PORT {
        if let Ok(t) = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, p)) {
            if let Ok(u) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, p)) {
                return Some((t, u, p));
            }
        }
    }
    None
}

pub fn start(app: AppHandle, tcp: std::net::TcpListener, udp: std::net::UdpSocket) {
    let _ = tcp.set_nonblocking(true);
    let _ = udp.set_nonblocking(true);
    let _ = udp.set_broadcast(true);

    let a = app.clone();
    tauri::async_runtime::spawn(async move {
        let Ok(udp) = UdpSocket::from_std(udp) else { return };
        let udp = Arc::new(udp);
        let _ = UDP.set(udp.clone());
        tauri::async_runtime::spawn(scan_loop(a.clone(), udp.clone()));
        udp_loop(a, udp).await;
    });

    tauri::async_runtime::spawn(async move {
        let Ok(l) = TcpListener::from_std(tcp) else { return };
        loop {
            if let Ok((s, addr)) = l.accept().await {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = handle_conn(app, s, addr).await;
                });
            }
        }
    });
}

// ---------------------------------------------------------------- discovery

async fn udp_loop(app: AppHandle, sock: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 4096];
    loop {
        // Windows reports ICMP port-unreachable as a recv error; just keep going.
        let Ok((n, src)) = sock.recv_from(&mut buf).await else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let Ok(p) = serde_json::from_slice::<Value>(&buf[..n]) else { continue };
        let st = app.state::<AppState>();
        match p["t"].as_str() {
            Some(t @ ("q" | "a")) => {
                let Ok(m) = serde_json::from_value::<Meta>(p["me"].clone()) else { continue };
                if m.id == st.my_id() {
                    continue;
                }
                if t == "q" {
                    let reply = json!({ "t": "a", "me": st.my_meta() }).to_string();
                    let _ = sock.send_to(reply.as_bytes(), src).await;
                }
                upsert_peer(&app, m, src.ip());
            }
            Some("bye") => {
                if let Some(id) = p["id"].as_str() {
                    set_offline(&app, id);
                }
            }
            _ => {}
        }
    }
}

async fn scan_loop(app: AppHandle, sock: Arc<UdpSocket>) {
    loop {
        let pkt = json!({ "t": "q", "me": app.state::<AppState>().my_meta() }).to_string();
        for t in targets(&app) {
            let _ = sock.send_to(pkt.as_bytes(), t).await;
        }
        expire(&app);
        let st = app.state::<AppState>();
        let _ = timeout(SCAN_EVERY, st.scan.notified()).await;
    }
}

/// Broadcast addresses of every IPv4 interface, extra hosts from settings and
/// the last known address of every remembered peer.
fn targets(app: &AppHandle) -> Vec<SocketAddr> {
    let st = app.state::<AppState>();
    let (span, extra) = {
        let s = st.settings.lock().unwrap();
        (s.scan_span.clamp(1, 1000), s.extra_hosts.clone())
    };
    let mut hosts: Vec<Ipv4Addr> = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if i.is_loopback() {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = i.addr {
                let mask = u32::from(v4.netmask);
                if mask != u32::MAX {
                    hosts.push(Ipv4Addr::from(u32::from(v4.ip) | !mask));
                }
            }
        }
    }
    let mut out: Vec<SocketAddr> = Vec::new();
    for tok in extra.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()) {
        if let Ok(sa) = tok.parse::<SocketAddr>() {
            out.push(sa);
        } else if let Ok(ip) = tok.parse::<Ipv4Addr>() {
            hosts.push(ip);
        }
    }
    let base = base_port();
    let end = base.saturating_add(span).min(MAX_PORT + 1);
    for h in hosts {
        for p in base..end {
            out.push(SocketAddr::new(IpAddr::V4(h), p));
        }
    }
    for p in st.data.lock().unwrap().peers.values() {
        if let Ok(ip) = p.ip.parse::<IpAddr>() {
            out.push(SocketAddr::new(ip, p.port));
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn upsert_peer(app: &AppHandle, m: Meta, ip: IpAddr) {
    let st = app.state::<AppState>();
    let ip = ip.to_string();
    let other_name = m.name.clone();
    let mut learn: Option<String> = None;
    let (changed, persist) = {
        let mut d = st.data.lock().unwrap();
        let e = d.peers.entry(m.id.clone()).or_insert_with(|| Peer { id: m.id.clone(), ..Default::default() });
        let persist = e.name != m.name || e.ip != ip || e.port != m.port || e.host != m.host;
        let online = m.status != Status::Offline;
        let changed = persist || e.online != online || e.status != m.status || e.notify != m.notify;
        e.name = m.name;
        e.host = m.host;
        e.ip = ip;
        e.port = m.port;
        e.notify = m.notify;
        e.max_files = m.max_files;
        e.max_file_size = m.max_file_size;
        e.status = m.status;
        e.ver = m.ver;
        // Same relay server as ours: remember the account so the relay can be the fallback.
        let mine = st.relay.cfg.lock().unwrap().as_ref().map(|c| c.server_id.clone());
        let acc = m.relay.filter(|r| Some(&r.server) == mine.as_ref()).map(|r| r.account).unwrap_or_default();
        if e.relay_self != acc {
            e.relay_self = acc.clone();
            if !acc.is_empty() && e.relay_account.is_empty() {
                e.relay_account = acc.clone();
            }
            learn = Some(acc);
        }
        e.online = online;
        e.seen = Some(Instant::now());
        (changed || learn.is_some(), persist || learn.is_some())
    };
    if persist {
        st.save_data();
    }
    match learn {
        Some(acc) if !acc.is_empty() => {
            crate::relay::rebuild_peers(app);
            crate::relay::learn_account(app, &acc);
        }
        Some(_) => crate::relay::rebuild_peers(app),
        None if changed => peers_changed(app),
        None => {}
    }
    avoid_name_clash(app, &other_name);
}

/// A default (never user-chosen) name that someone else on the LAN also uses
/// becomes "<name>-<hash of this computer's name>" and stays the new default.
fn avoid_name_clash(app: &AppHandle, other: &str) {
    let st = app.state::<AppState>();
    let clash = {
        let s = st.settings.lock().unwrap();
        !s.name_custom && s.name.trim().eq_ignore_ascii_case(other.trim())
    };
    if !clash {
        return;
    }
    let name = tagged(&default_name(), &st.host, |c| st.name_in_use(c));
    st.settings.lock().unwrap().name = name;
    st.save_settings();
    st.scan.notify_one(); // announce the new name right away
    crate::relay::announce(app);
    tray::refresh(app);
}

fn set_offline(app: &AppHandle, id: &str) {
    let st = app.state::<AppState>();
    let changed = match st.data.lock().unwrap().peers.get_mut(id) {
        Some(p) if p.online => {
            p.online = false;
            true
        }
        _ => false,
    };
    if changed {
        peers_changed(app);
    }
}

fn expire(app: &AppHandle) {
    let st = app.state::<AppState>();
    let mut changed = false;
    for p in st.data.lock().unwrap().peers.values_mut() {
        if p.id.starts_with("r:") {
            continue; // relay contacts follow the relay, not LAN announcements
        }
        if p.online && p.seen.map_or(true, |t| t.elapsed() > RECENT) {
            p.online = false;
            changed = true;
        }
    }
    if changed {
        peers_changed(app);
    }
}

pub fn peers_changed(app: &AppHandle) {
    tray::refresh(app);
    let _ = app.emit("peers", ());
}

pub async fn send_bye(app: &AppHandle) {
    let Some(sock) = UDP.get() else { return };
    let pkt = json!({ "t": "bye", "id": app.state::<AppState>().my_id() }).to_string();
    for t in targets(app) {
        let _ = sock.send_to(pkt.as_bytes(), t).await;
    }
}

// ---------------------------------------------------------------- framing

async fn write_frame(s: &mut TcpStream, v: &Value) -> Result<(), String> {
    let b = serde_json::to_vec(v).map_err(es)?;
    timeout(IO_TIMEOUT, async {
        s.write_u32(b.len() as u32).await?;
        s.write_all(&b).await
    })
    .await
    .map_err(|_| "시간 초과".to_string())?
    .map_err(es)
}

async fn read_frame(s: &mut TcpStream, wait: Duration) -> Result<Value, String> {
    timeout(wait, async {
        let n = s.read_u32().await.map_err(es)?;
        if n > MAX_FRAME {
            return Err("잘못된 데이터".to_string());
        }
        let mut b = vec![0u8; n as usize];
        s.read_exact(&mut b).await.map_err(es)?;
        serde_json::from_slice(&b).map_err(es)
    })
    .await
    .map_err(|_| "응답 시간 초과".to_string())?
}

// ---------------------------------------------------------------- receiving

#[derive(Deserialize)]
struct FileHdr {
    name: String,
    size: u64,
}

#[derive(Deserialize)]
struct MsgHdr {
    from: Meta,
    #[serde(default)]
    text: String,
    #[serde(default)]
    files: Vec<FileHdr>,
    /// v3+: set only when the sender chose to reveal the group.
    #[serde(default)]
    group: Option<GroupInfo>,
}

async fn handle_conn(app: AppHandle, mut s: TcpStream, addr: SocketAddr) -> Result<(), String> {
    let _ = s.set_nodelay(true);
    let hdr = read_frame(&mut s, Duration::from_secs(10)).await?;
    match hdr["t"].as_str() {
        Some("meta") => {
            let me = app.state::<AppState>().my_meta();
            write_frame(&mut s, &json!({ "t": "meta", "me": me })).await
        }
        Some("msg") => receive(app, s, addr, hdr).await,
        _ => Err("unknown request".into()),
    }
}

async fn receive(app: AppHandle, mut s: TcpStream, addr: SocketAddr, hdr: Value) -> Result<(), String> {
    let h: MsgHdr = serde_json::from_value(hdr).map_err(es)?;
    upsert_peer(&app, h.from.clone(), addr.ip());
    let (max_files, max_size, dir, free) = app.state::<AppState>().limits();
    let total: u64 = h.files.iter().map(|f| f.size).sum();

    let reject = if h.files.len() > max_files as usize {
        Some(format!("첨부파일은 최대 {max_files}개까지 받을 수 있습니다"))
    } else if let Some(f) = h.files.iter().find(|f| f.size > max_size) {
        Some(format!("'{}' 파일이 허용 크기({})를 초과합니다", f.name, human(max_size)))
    } else if total > 0 && total.saturating_add(DISK_MARGIN) > free {
        Some("받는 사람의 디스크 공간이 부족합니다".to_string())
    } else {
        None
    };
    if let Some(r) = reject {
        let _ = write_frame(&mut s, &json!({ "t": "err", "reason": r })).await;
        return Err(r);
    }
    write_frame(&mut s, &json!({ "t": "ok" })).await?;

    // Local id: it ends up in window labels, so never trust the sender's.
    let id = new_id();
    let (dnd, split) = {
        let s = app.state::<AppState>();
        let s = s.settings.lock().unwrap();
        (s.status == Status::Dnd, s.split_folders)
    };
    let base = dir.clone();
    let dir = if split && !h.files.is_empty() { message_dir(&base, &h.from.name) } else { base.clone() };
    // Do-not-disturb: no progress window either; the message just lands in the inbox.
    let big = !dnd && (h.files.len() >= BIG_COUNT || total >= BIG_BYTES);
    let cancel = Arc::new(AtomicBool::new(false));
    if big {
        app.state::<AppState>().cancels.lock().unwrap().insert(id.clone(), cancel.clone());
        ui::open_receiving(&app, &id, &h.from.name, h.files.len(), total);
    }

    let mut saved = Vec::new();
    let mut note = String::new();
    let mut done: u64 = 0;
    let mut last = Instant::now();
    let mut buf = vec![0u8; CHUNK];
    for f in &h.files {
        let Some((tmp, fin)) = reserve_path(&dir, &sanitize(&f.name)) else {
            note = "파일을 저장할 수 없습니다".into();
            break;
        };
        let res: Result<(), String> = async {
            let mut file = tokio::fs::OpenOptions::new().write(true).truncate(true).open(&tmp).await.map_err(es)?;
            let mut remain = f.size;
            while remain > 0 {
                if cancel.load(Ordering::Relaxed) {
                    return Err("수신이 취소되었습니다".to_string());
                }
                let want = remain.min(CHUNK as u64) as usize;
                let n = timeout(IO_TIMEOUT, s.read(&mut buf[..want]))
                    .await
                    .map_err(|_| "시간 초과".to_string())?
                    .map_err(es)?;
                if n == 0 {
                    return Err("연결이 끊어졌습니다".to_string());
                }
                file.write_all(&buf[..n]).await.map_err(es)?;
                remain -= n as u64;
                done += n as u64;
                if big && last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    let _ = app.emit("recv-progress", json!({ "id": id, "done": done, "total": total, "file": f.name }));
                }
            }
            file.flush().await.map_err(es)?;
            drop(file.into_std().await);
            Ok(())
        }
        .await;
        match res {
            Ok(()) => {
                if tokio::fs::rename(&tmp, &fin).await.is_err() {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    note = format!("'{}' 저장 실패", f.name);
                    break;
                }
                saved.push(FileEntry {
                    name: fin.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    size: f.size,
                    path: fin.to_string_lossy().into_owned(),
                });
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                note = format!("첨부파일 수신 중단: {e}");
                break;
            }
        }
    }
    if note.is_empty() {
        let _ = write_frame(&mut s, &json!({ "t": "done" })).await;
    }
    drop(s);
    if saved.is_empty() && dir != base {
        let _ = std::fs::remove_dir(&dir); // nothing was saved into the per-message folder
    }

    if big {
        app.state::<AppState>().cancels.lock().unwrap().remove(&id);
        ui::close(&app, &format!("recv-{id}"));
    }
    deliver(
        &app,
        Message {
            id,
            from_id: h.from.id,
            from_name: h.from.name,
            from_ip: addr.ip().to_string(),
            text: h.text,
            files: saved,
            ts: now_ms(),
            read: false,
            note,
            group: h.group.map(|mut g| {
                g.members.truncate(MAX_GROUP_MEMBERS);
                g
            }),
        },
    );
    Ok(())
}

pub(crate) fn deliver(app: &AppHandle, m: Message) {
    let st = app.state::<AppState>();
    let id = m.id.clone();
    st.push_message(m);
    let popup = {
        let s = st.settings.lock().unwrap();
        s.notify && s.status != Status::Dnd
    };
    if popup {
        ui::open_popup(app, &id);
    }
    tray::refresh(app);
    let _ = app.emit("inbox", ());
}

/// Creates "<sender> <YYYY-MM-DD HHMMSS>" under `base` (with " (n)" if taken);
/// falls back to `base` itself if it cannot be created.
pub(crate) fn message_dir(base: &Path, sender: &str) -> PathBuf {
    let name = sanitize(&format!("{sender} {}", chrono::Local::now().format("%Y-%m-%d %H%M%S")));
    for n in 0..1000 {
        let d = if n == 0 { base.join(&name) } else { base.join(format!("{name} ({n})")) };
        if std::fs::create_dir(&d).is_ok() {
            return d;
        }
        if !d.exists() {
            break;
        }
    }
    base.to_path_buf()
}

pub(crate) fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    if s.is_empty() { "file".into() } else { s }
}

/// Picks a free "name (n).ext" and creates its ".part" file so concurrent
/// transfers cannot claim the same name.
pub(crate) fn reserve_path(dir: &Path, name: &str) -> Option<(PathBuf, PathBuf)> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 0..10000 {
        let cand = if n == 0 { name.to_string() } else { format!("{stem} ({n}){ext}") };
        let fin = dir.join(&cand);
        let tmp = dir.join(format!("{cand}.part"));
        if fin.exists() {
            continue;
        }
        if std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).is_ok() {
            return Some((tmp, fin));
        }
    }
    None
}

// ---------------------------------------------------------------- sending

#[derive(Serialize)]
pub struct SendResult {
    pub to: String,
    pub name: String,
    pub ok: bool,
    pub error: String,
    /// "lan", "relay" or "stored".
    pub via: String,
}

/// `group`: id of the group being sent to; with `reveal`, v3+ recipients see
/// its name and every member.
pub async fn send(
    app: AppHandle,
    job: String,
    to: Vec<String>,
    text: String,
    files: Vec<String>,
    group: Option<String>,
    reveal: bool,
) -> Vec<SendResult> {
    let st = app.state::<AppState>();
    let group = group.and_then(|id| st.data.lock().unwrap().groups.iter().find(|g| g.id == id).cloned());
    let info = group.as_ref().filter(|_| reveal).map(|g| {
        let me = st.my_id();
        let d = st.data.lock().unwrap();
        GroupInfo {
            name: g.name.clone(),
            members: g
                .members
                .iter()
                .filter(|id| **id != me)
                .map(|id| Member { id: id.clone(), name: d.peers.get(id).map(|p| p.name.clone()).unwrap_or_default() })
                .collect(),
        }
    });
    // Folders are sent as "<folder>.zip", built once here for all recipients.
    let tmp = std::env::temp_dir().join("PopupMessenger").join(new_id());
    let mut list: Vec<(String, String, u64)> = Vec::new(); // (file to read, name on the wire, size)
    let mut sources: Vec<String> = Vec::new(); // what the user picked, for the sent history
    let mut bad = String::new();
    for (i, p) in files.iter().enumerate() {
        let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match std::fs::metadata(p) {
            Ok(m) if m.is_file() => list.push((p.clone(), name, m.len())),
            Ok(m) if m.is_dir() => {
                let _ = app.emit("send-progress", json!({ "job": job, "zip": name }));
                let zip_name = format!("{name}.zip");
                let (src, dst) = (PathBuf::from(p), tmp.join(i.to_string()).join(&zip_name));
                match tauri::async_runtime::spawn_blocking(move || zip_dir(&src, &dst).map(|n| (dst, n))).await {
                    Ok(Ok((dst, n))) => list.push((dst.to_string_lossy().into_owned(), zip_name, n)),
                    Ok(Err(e)) => bad = format!("'{name}' 폴더를 압축하지 못했습니다: {e}"),
                    Err(e) => bad = format!("'{name}' 폴더를 압축하지 못했습니다: {e}"),
                }
            }
            _ => bad = format!("파일을 읽을 수 없습니다: {p}"),
        }
        if !bad.is_empty() {
            break;
        }
        sources.push(p.clone());
    }

    // Route per recipient (docs/relay-protocol.md 5). The relay server's policy comes
    // first (forced relay, blocked outsiders); otherwise the LAN is tried and the relay
    // is the fallback for every recipient with a relay account: offline devices, LAN
    // failures, and users ("r:<account>") with no device reachable on the LAN.
    let relay_on = st.relay.configured();
    let pol = st.relay.cfg.lock().unwrap().as_ref().map(|c| c.policy.clone()).unwrap_or_default();
    let force = relay_on && pol["force_relay"] == true;
    let block = force && pol["block_direct_outsiders"] == true;
    let mut out: Vec<SendResult> = Vec::new();
    let mut deferred: Vec<(usize, crate::relay::Target)> = Vec::new();
    for id in to {
        let peer = st.data.lock().unwrap().peers.get(&id).cloned();
        let name = peer.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| id.clone());
        let mut relay_target = None;
        let r: Result<String, String> = if !bad.is_empty() {
            Err(bad.clone())
        } else if let Some(p) = peer {
            let acc = Some(p.relay_account.clone()).filter(|a| relay_on && !a.is_empty());
            if id.starts_with("r:") {
                // A user: every device of theirs online on the LAN, else the relay.
                let devs: Vec<Peer> = if force {
                    Vec::new()
                } else {
                    let d = st.data.lock().unwrap();
                    crate::state::devices_of(&d, &p.relay_account).filter(|x| x.online).cloned().collect()
                };
                let mut lan = Err(String::new());
                for dev in &devs {
                    let info = info.as_ref().filter(|_| dev.ver >= 3);
                    match send_one(&app, &job, dev, &text, &list, info).await {
                        Ok(()) => lan = Ok("lan".to_string()),
                        Err(e) if lan.is_err() => lan = Err(e.1),
                        Err(_) => {}
                    }
                }
                if lan.is_err() {
                    if acc.is_some() {
                        relay_target = acc;
                    } else {
                        lan = Err("릴레이 서버가 설정되어 있지 않습니다".into());
                    }
                }
                lan
            } else if acc.is_some() && (force || !p.online) {
                relay_target = acc;
                Err(String::new())
            } else if block && acc.is_none() {
                Err("서버 정책상 릴레이 서버를 쓰지 않는 사용자에게는 보낼 수 없습니다".into())
            } else {
                let info = info.as_ref().filter(|_| p.ver >= 3);
                match send_one(&app, &job, &p, &text, &list, info).await {
                    Ok(()) => Ok("lan".into()),
                    // The LAN failed (not a refusal by the recipient): try the relay.
                    Err((true, e)) if acc.is_some() => {
                        relay_target = acc;
                        Err(e)
                    }
                    Err((_, e)) => Err(e),
                }
            }
        } else {
            Err("알 수 없는 사용자".into())
        };
        if let Some(account) = relay_target {
            deferred.push((out.len(), crate::relay::Target { peer: id.clone(), account }));
        }
        let (ok, via, error) = match r {
            Ok(v) => (true, v, String::new()),
            Err(e) => (false, String::new(), e),
        };
        out.push(SendResult { to: id, name, ok, error, via });
    }
    if !deferred.is_empty() {
        let targets: Vec<crate::relay::Target> = deferred.iter().map(|d| crate::relay::Target { peer: d.1.peer.clone(), account: d.1.account.clone() }).collect();
        let res = crate::relay::send(&app, &job, &targets, &text, &list, info.as_ref()).await;
        for (i, t) in &deferred {
            if let Some((_, r)) = res.iter().find(|r| r.0 == t.peer) {
                let o = &mut out[*i];
                match r {
                    Ok(v) => {
                        o.ok = true;
                        o.via = v.clone();
                        o.error.clear();
                    }
                    Err(e) if o.error.is_empty() => o.error = e.clone(),
                    Err(e) => o.error = format!("{} / 릴레이: {e}", o.error),
                }
            }
        }
    }
    for r in out.iter().filter(|r| r.ok) {
        *st.data.lock().unwrap().counts.entry(r.to.clone()).or_insert(0) += 1;
    }
    st.save_data();
    tray::refresh(&app);

    st.push_sent(Sent {
        id: new_id(),
        to: out
            .iter()
            .map(|r| Recipient { id: r.to.clone(), name: r.name.clone(), ok: r.ok, error: r.error.clone(), via: r.via.clone() })
            .collect(),
        text,
        files: list.into_iter().zip(sources).map(|((_, name, size), path)| FileEntry { name, size, path }).collect(),
        ts: now_ms(),
        group: group.map(|g| g.name),
    });
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = app.emit("sent", ());
    out
}

/// Zips `src` (recursively, keeping its own name as the top folder) into `dst`.
/// Symlinks and junctions are skipped so a link cannot pull in files outside the folder.
fn zip_dir(src: &Path, dst: &Path) -> Result<u64, String> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(es)?;
    }
    let root = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "folder".into());
    let mut z = zip::ZipWriter::new(std::io::BufWriter::new(std::fs::File::create(dst).map_err(es)?));
    // Fast level: attachments are often already compressed (media, archives).
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(1));
    let mut stack = vec![(src.to_path_buf(), root)];
    while let Some((dir, prefix)) = stack.pop() {
        z.add_directory(format!("{prefix}/"), opts).map_err(es)?;
        for e in std::fs::read_dir(&dir).map_err(es)? {
            let e = e.map_err(es)?;
            let ft = e.file_type().map_err(es)?; // does not follow links
            let name = e.file_name().to_string_lossy().into_owned();
            if ft.is_symlink() || name == ".DS_Store" {
                continue;
            }
            let entry = format!("{prefix}/{name}");
            if ft.is_dir() {
                stack.push((e.path(), entry));
            } else if ft.is_file() {
                let size = e.metadata().map_err(es)?.len();
                z.start_file(entry, opts.large_file(size >= u32::MAX as u64)).map_err(es)?;
                std::io::copy(&mut std::fs::File::open(e.path()).map_err(es)?, &mut z).map_err(es)?;
            }
        }
    }
    z.finish().map_err(es)?.flush().map_err(es)?;
    Ok(std::fs::metadata(dst).map_err(es)?.len())
}

async fn send_one(
    app: &AppHandle,
    job: &str,
    p: &Peer,
    text: &str,
    files: &[(String, String, u64)],
    group: Option<&GroupInfo>,
) -> Result<(), (bool, String)> {
    // (true, _) is a network failure the relay may get past; (false, _) a refusal.
    let refuse = |e: String| (false, e);
    let fail = |e: String| (true, e);
    send_lan(app, job, p, text, files, group).await.map_err(|e| match e {
        LanErr::Refused(e) => refuse(e),
        LanErr::Net(e) => fail(e),
    })
}

enum LanErr {
    Refused(String),
    Net(String),
}

impl From<String> for LanErr {
    fn from(e: String) -> Self {
        LanErr::Net(e)
    }
}

async fn send_lan(
    app: &AppHandle,
    job: &str,
    p: &Peer,
    text: &str,
    files: &[(String, String, u64)],
    group: Option<&GroupInfo>,
) -> Result<(), LanErr> {
    let refused = |e: String| LanErr::Refused(e);
    if p.max_files > 0 && files.len() > p.max_files as usize {
        return Err(refused(format!("받는 사람은 첨부파일을 {}개까지 받습니다", p.max_files)));
    }
    if p.max_file_size > 0 {
        if let Some((_, n, _)) = files.iter().find(|f| f.2 > p.max_file_size) {
            return Err(refused(format!("'{n}' 파일이 받는 사람의 허용 크기({})를 초과합니다", human(p.max_file_size))));
        }
    }
    let ip: IpAddr = p.ip.parse().map_err(|_| "주소 없음".to_string())?;
    let mut s = timeout(Duration::from_secs(5), TcpStream::connect(SocketAddr::new(ip, p.port)))
        .await
        .map_err(|_| "연결 시간 초과 (오프라인?)".to_string())?
        .map_err(|e| format!("연결 실패: {e}"))?;
    let _ = s.set_nodelay(true);

    let me = app.state::<AppState>().my_meta();
    let fl: Vec<Value> = files.iter().map(|(_, n, sz)| json!({ "name": n, "size": sz })).collect();
    let mut hdr = json!({ "t": "msg", "id": new_id(), "from": me, "text": text, "files": fl });
    if let Some(g) = group {
        hdr["group"] = json!(g);
    }
    write_frame(&mut s, &hdr).await?;
    let r = read_frame(&mut s, IO_TIMEOUT).await?;
    if r["t"] != "ok" {
        return Err(refused(r["reason"].as_str().unwrap_or("거부되었습니다").to_string()));
    }

    let total: u64 = files.iter().map(|f| f.2).sum();
    let mut done: u64 = 0;
    let mut last = Instant::now();
    let mut buf = vec![0u8; CHUNK];
    for (path, _, size) in files {
        let mut f = tokio::fs::File::open(path).await.map_err(es)?;
        let mut remain = *size;
        while remain > 0 {
            let want = remain.min(CHUNK as u64) as usize;
            let n = f.read(&mut buf[..want]).await.map_err(es)?;
            if n == 0 {
                return Err(refused("전송 중 파일이 변경되었습니다".into()));
            }
            timeout(IO_TIMEOUT, s.write_all(&buf[..n]))
                .await
                .map_err(|_| "시간 초과".to_string())?
                .map_err(|_| "받는 쪽에서 연결을 끊었습니다".to_string())?;
            remain -= n as u64;
            done += n as u64;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                let _ = app.emit("send-progress", json!({ "job": job, "to": p.name, "done": done, "total": total }));
            }
        }
    }
    let _ = s.flush().await;
    let r = read_frame(&mut s, IO_TIMEOUT * 2).await?;
    if r["t"] == "done" { Ok(()) } else { Err("전송이 완료되지 않았습니다".to_string().into()) }
}
