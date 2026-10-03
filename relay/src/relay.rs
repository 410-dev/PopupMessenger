//! The client protocol (docs/relay-protocol.md).

use crate::hub::{Conn, Out};
use crate::settings::{Auth, Contacts, Encryption, Registration, MAX_DEVICES_CAP};
use crate::store::{self, now_ms};
use crate::{frame, Server};
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use base64::Engine;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

const HANDSHAKE: Duration = Duration::from_secs(15);
const TRANSFER_IDLE: Duration = Duration::from_secs(60);
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const CHUNK: usize = 256 * 1024;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub struct PErr {
    pub code: &'static str,
    pub reason: String,
    pub extra: Value,
}

fn perr(code: &'static str, reason: impl Into<String>) -> PErr {
    PErr {
        code,
        reason: reason.into(),
        extra: Value::Null,
    }
}

fn err_frame(rid: &Value, e: &PErr) -> Value {
    let mut v = json!({ "t": "err", "rid": rid, "code": e.code, "reason": e.reason });
    if let Value::Object(x) = &e.extra {
        for (k, val) in x {
            v[k] = val.clone();
        }
    }
    v
}

fn with_rid(mut v: Value, rid: &Value) -> Value {
    v["rid"] = rid.clone();
    v
}

pub async fn serve(server: Arc<Server>, l: TcpListener) {
    loop {
        let Ok((s, addr)) = l.accept().await else {
            continue;
        };
        let server = server.clone();
        tokio::spawn(async move { handle(server, s, addr).await });
    }
}

async fn handle(server: Arc<Server>, s: TcpStream, addr: SocketAddr) {
    let _ = s.set_nodelay(true);
    let mut ip = match addr.ip() {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(|v4| v4.to_string())
            .unwrap_or_else(|| v6.to_string()),
        v4 => v4.to_string(),
    };
    // The first byte tells the transport apart: 0x16 starts a TLS handshake, 'G' an
    // HTTP request (WebSocket upgrade), and our frames start with a length whose
    // first byte is 0 (frames are at most 8 MiB).
    let mut b = [0u8; 1];
    if !matches!(timeout(HANDSHAKE, s.peek(&mut b)).await, Ok(Ok(1))) {
        return;
    }
    let st = server.settings();
    // HTTP requests start with an upper-case method name (GET, POST, ...).
    let http = |c: u8| c.is_ascii_uppercase();
    let (io, secure, transport): (Box<dyn Io>, bool, &'static str) = if b[0] == 0x16 {
        let acc = tokio_rustls::TlsAcceptor::from(server.tls.config.clone());
        let Ok(Ok(mut t)) = timeout(HANDSHAKE, acc.accept(s)).await else { return };
        // HTTP inside TLS (https:// or wss:// straight to this port)?
        let mut first = [0u8; 1];
        if !matches!(timeout(HANDSHAKE, t.read_exact(&mut first)).await, Ok(Ok(_))) {
            return;
        }
        let t = crate::ws::Rewind::new(first.to_vec(), t);
        if http(first[0]) {
            match crate::portal::http(server.clone(), t, true, ip.clone()).await {
                Some((d, _)) => (Box::new(d), true, "wss"),
                None => return,
            }
        } else {
            (Box::new(t), true, "tls")
        }
    } else if http(b[0]) {
        match crate::portal::http(server.clone(), s, false, ip.clone()).await {
            Some((d, fwd)) => {
                // Behind a TLS-terminating proxy (e.g. Cloudflare Tunnel), trusted only when the admin says so.
                let proxied = st.proxy_tls && fwd.https;
                if st.proxy_tls {
                    if let Some(real) = fwd.client_ip {
                        ip = real;
                    }
                }
                (Box::new(d), proxied, if proxied { "ws-proxy" } else { "ws" })
            }
            None => return,
        }
    } else {
        (Box::new(s), false, "tcp")
    };
    let _ = session(server, io, ip, secure, transport).await;
}

async fn session(
    server: Arc<Server>,
    mut io: Box<dyn Io>,
    ip: String,
    tls: bool,
    transport: &'static str,
) -> Result<(), String> {
    let hello = frame::read(&mut io, HANDSHAKE).await?.ok_or("eof")?;
    let rid = hello["rid"].clone();
    if hello["t"] != "hello" {
        return frame::write(
            &mut io,
            &err_frame(&rid, &perr("bad_request", "hello가 필요합니다")),
        )
        .await;
    }
    if hello["proto"].as_u64() != Some(crate::PROTOCOL) {
        return frame::write(
            &mut io,
            &err_frame(&rid, &perr("version", "지원하지 않는 프로토콜 버전입니다")),
        )
        .await;
    }
    let app: String = hello["app"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(32)
        .collect();
    let s = server.settings();
    let reply = json!({
        "t": "hello", "rid": rid, "proto": crate::PROTOCOL,
        "server": { "id": server.id, "name": s.server_name, "version": crate::version() },
        "transport": { "kind": transport, "ws_chunk": s.ws_chunk_kb as usize * 1024 },
        "policy": s.policy,
    });
    frame::write(&mut io, &reply).await?;
    if !tls && s.policy.encryption != Encryption::Plain {
        let e = perr("tls_required", "이 서버는 암호화(TLS) 연결만 받습니다");
        return frame::write(&mut io, &err_frame(&Value::Null, &e)).await;
    }

    let req = frame::read(&mut io, HANDSHAKE).await?.ok_or("eof")?;
    match req["t"].as_str() {
        Some("login") | Some("register") => control(server, io, ip, tls, transport, app, req).await,
        Some("auth") => transfer(server, io, req).await,
        _ => {
            frame::write(
                &mut io,
                &err_frame(
                    &req["rid"],
                    &perr("bad_request", "login 또는 auth가 필요합니다"),
                ),
            )
            .await
        }
    }
}

// ---------------------------------------------------------------- login

struct LoggedIn {
    device: String,
    account: String,
    username: Option<String>,
}

fn s_field(v: &Value, k: &str, max: usize) -> String {
    v[k].as_str()
        .unwrap_or("")
        .trim()
        .chars()
        .take(max)
        .collect()
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_username(u: &str) -> bool {
    (2..=32).contains(&u.chars().count())
        && u.chars().all(|c| c.is_alphanumeric() || "._-@".contains(c))
}

fn profile(v: &Value) -> Value {
    let status = match v["status"].as_str() {
        Some(s @ ("online" | "dnd" | "offline")) => s,
        _ => "online",
    };
    json!({
        "name": s_field(v, "name", 64),
        "host": s_field(v, "host", 64),
        "status": status,
        "notify": v["notify"].as_bool().unwrap_or(true),
        "max_files": v["max_files"].as_u64().unwrap_or(0),
        "max_file_size": v["max_file_size"].as_u64().unwrap_or(0),
    })
}

pub fn hash_password(pass: &str) -> Result<String, String> {
    Argon2::default()
        .hash_password(pass.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

pub fn verify_password(pass: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| {
        Argon2::default()
            .verify_password(pass.as_bytes(), &h)
            .is_ok()
    })
}

async fn verify_password_async(pass: String, hash: Option<String>) -> bool {
    // Runs even for unknown users so timing does not reveal which usernames exist.
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    tokio::task::spawn_blocking(move || {
        let found = hash.is_some();
        let hash = hash.unwrap_or_else(|| {
            DUMMY
                .get_or_init(|| hash_password("dummy").unwrap_or_default())
                .clone()
        });
        verify_password(&pass, &hash) && found
    })
    .await
    .unwrap_or(false)
}

async fn login(server: &Server, req: &Value, ip: &str, app: &str) -> Result<LoggedIn, PErr> {
    let s = server.settings();
    let p = &s.policy;
    let id = s_field(req, "id", 64);
    if !valid_id(&id) {
        return Err(perr("bad_request", "기기 ID가 올바르지 않습니다"));
    }
    let secret = base64::engine::general_purpose::STANDARD
        .decode(req["device_secret"].as_str().unwrap_or(""))
        .ok()
        .filter(|b| (16..=128).contains(&b.len()))
        .ok_or_else(|| perr("bad_request", "기기 비밀값이 올바르지 않습니다"))?;
    let secret = store::sha256_hex(&secret);
    let pubkey = s_field(req, "pubkey", 128);
    if p.encryption == Encryption::E2ee && pubkey.is_empty() {
        return Err(perr(
            "policy",
            "이 서버는 종단간 암호화 공개키가 필요합니다",
        ));
    }
    let prof = profile(&req["profile"]);
    let register = req["t"] == "register";
    let dev_key = format!("dev:{id}");
    server.locked(&dev_key).map_err(|s| locked(s))?;

    let device: Option<(String, String, bool)> = server
        .db()
        .query_row(
            "SELECT account, secret, disabled FROM devices WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)),
        )
        .optional()
        .ok()
        .flatten();
    if let Some((_, sec, _)) = &device {
        if *sec != secret {
            server.login_failed(&dev_key);
            return Err(perr("auth_failed", "기기 인증에 실패했습니다"));
        }
    }

    let (account, username) = match p.auth {
        Auth::None => match &device {
            Some((acc, _, _)) => (acc.clone(), None),
            None => {
                let state = match p.registration {
                    Registration::Closed => {
                        return Err(perr(
                            "registration_closed",
                            "이 서버는 가입을 받지 않습니다",
                        ))
                    }
                    Registration::Approval => "pending",
                    Registration::Open => "active",
                };
                let acc = store::new_id();
                let now = now_ms();
                let c = server.db();
                c.execute(
                    "INSERT INTO accounts (id, name, state, created, updated) VALUES (?1, ?2, ?3, ?4, ?4)",
                    params![acc, prof["name"].as_str().unwrap_or(""), state, now],
                )
                .map_err(|e| perr("bad_request", e.to_string()))?;
                insert_device(&c, &id, &acc, &secret, now);
                store::audit(
                    &c,
                    &format!("device:{id}"),
                    "register",
                    &format!("account {acc} ({state}) from {ip}"),
                );
                (acc, None)
            }
        },
        Auth::Password => {
            let user = s_field(req, "user", 32);
            let pass = req["pass"].as_str().unwrap_or("").to_string();
            let known = device
                .as_ref()
                .filter(|_| !register && user.is_empty() && pass.is_empty());
            if let Some((acc, _, _)) = known {
                // A registered device logs on with its device secret alone (checked above);
                // the password is only needed to add a device to an account.
                let uname: Option<String> = server
                    .db()
                    .query_row("SELECT username FROM accounts WHERE id = ?1", [acc], |r| {
                        r.get(0)
                    })
                    .optional()
                    .ok()
                    .flatten()
                    .flatten();
                (acc.clone(), uname)
            } else {
                if user.is_empty() || pass.is_empty() {
                    return Err(perr("auth_required", "사용자 이름과 비밀번호가 필요합니다"));
                }
                let user_key = format!("user:{}", user.to_lowercase());
                server.locked(&user_key).map_err(|s| locked(s))?;
                if register {
                    if p.registration == Registration::Closed {
                        return Err(perr(
                            "registration_closed",
                            "이 서버는 가입을 받지 않습니다",
                        ));
                    }
                    if !valid_username(&user) {
                        return Err(perr(
                            "bad_request",
                            "사용자 이름은 2~32자의 글자, 숫자, . _ - @ 만 쓸 수 있습니다",
                        ));
                    }
                    if (pass.chars().count() as u32) < s.min_password_len {
                        return Err(perr(
                            "bad_request",
                            format!("비밀번호는 {}자 이상이어야 합니다", s.min_password_len),
                        ));
                    }
                    if device.is_some() {
                        return Err(perr("bad_request", "이미 등록된 기기입니다. 로그인하세요"));
                    }
                    let hash = tokio::task::spawn_blocking(move || hash_password(&pass))
                        .await
                        .map_err(|e| perr("bad_request", e.to_string()))?
                        .map_err(|e| perr("bad_request", e))?;
                    let state = if p.registration == Registration::Open {
                        "active"
                    } else {
                        "pending"
                    };
                    let acc = store::new_id();
                    let now = now_ms();
                    let c = server.db();
                    let r = c.execute(
                    "INSERT INTO accounts (id, username, pass, name, state, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![acc, user, hash, prof["name"].as_str().unwrap_or(""), state, now],
                );
                    if r.is_err() {
                        return Err(perr("username_taken", "이미 쓰이는 사용자 이름입니다"));
                    }
                    insert_device(&c, &id, &acc, &secret, now);
                    store::audit(
                        &c,
                        &format!("user:{user}"),
                        "register",
                        &format!("account {acc} ({state}) device {id} from {ip}"),
                    );
                    (acc, Some(user))
                } else {
                    let row: Option<(String, String, Option<String>)> = server
                        .db()
                        .query_row(
                            "SELECT id, username, pass FROM accounts WHERE username = ?1",
                            [&user],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()
                        .ok()
                        .flatten();
                    let ok =
                        verify_password_async(pass, row.as_ref().and_then(|r| r.2.clone())).await;
                    let Some((acc, uname, _)) = row.filter(|_| ok) else {
                        server.login_failed(&user_key);
                        return Err(perr(
                            "auth_failed",
                            "사용자 이름 또는 비밀번호가 틀렸습니다",
                        ));
                    };
                    server.login_ok(&user_key);
                    let c = server.db();
                    let same = device.as_ref().is_some_and(|d| d.0 == acc);
                    if !same {
                        let (count, limit): (i64, Option<i64>) = c
                        .query_row(
                            "SELECT (SELECT COUNT(*) FROM devices WHERE account = ?1), max_devices FROM accounts WHERE id = ?1",
                            [&acc],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .unwrap_or((0, None));
                        let limit = limit
                            .map(|l| l as u32)
                            .unwrap_or(p.max_devices)
                            .clamp(1, MAX_DEVICES_CAP);
                        if count >= limit as i64 {
                            return Err(perr(
                                "device_limit",
                                format!("이 계정에는 기기를 {limit}대까지 등록할 수 있습니다"),
                            ));
                        }
                        match &device {
                            Some((old, _, _)) => {
                                // The secret matched above: the same installation switched accounts.
                                let _ = c.execute(
                                    "UPDATE devices SET account = ?1 WHERE id = ?2",
                                    params![acc, id],
                                );
                                let _ = c.execute(
                                    "DELETE FROM deliveries WHERE device = ?1 AND acked IS NULL",
                                    [&id],
                                );
                                store::bump(&c, old);
                                store::audit(
                                    &c,
                                    &format!("user:{uname}"),
                                    "device_move",
                                    &format!("device {id} from account {old}"),
                                );
                            }
                            None => {
                                insert_device(&c, &id, &acc, &secret, now_ms());
                                store::audit(
                                    &c,
                                    &format!("user:{uname}"),
                                    "device_add",
                                    &format!("device {id} from {ip}"),
                                );
                            }
                        }
                    }
                    (acc, Some(uname))
                }
            }
        }
    };

    let c = server.db();
    let (state, dev_disabled): (String, bool) = c
        .query_row(
            "SELECT a.state, d.disabled FROM accounts a JOIN devices d ON d.account = a.id WHERE a.id = ?1 AND d.id = ?2",
            [&account, &id],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
        )
        .map_err(|_| perr("auth_failed", "계정을 찾을 수 없습니다"))?;
    match state.as_str() {
        "pending" => {
            return Err(perr(
                "pending",
                "가입 신청이 접수되었습니다. 관리자 승인을 기다리는 중입니다",
            ))
        }
        "disabled" => return Err(perr("disabled", "관리자가 정지한 계정입니다")),
        _ => {}
    }
    if dev_disabled {
        return Err(perr("disabled", "관리자가 정지한 기기입니다"));
    }
    let now = now_ms();
    let _ = c.execute(
        "UPDATE devices SET pubkey = ?1, profile = ?2, app = ?3, last_ip = ?4, last_seen = ?5 WHERE id = ?6",
        params![pubkey, prof.to_string(), app, ip, now, id],
    );
    let name = prof["name"].as_str().unwrap_or("");
    if !name.is_empty() {
        let _ = c.execute(
            "UPDATE accounts SET name = ?1 WHERE id = ?2",
            params![name, account],
        );
    }
    let _ = c.execute(
        "UPDATE accounts SET last_seen = ?1 WHERE id = ?2",
        params![now, account],
    );
    drop(c);
    server.login_ok(&dev_key);
    Ok(LoggedIn {
        device: id,
        account,
        username,
    })
}

fn locked(secs: i64) -> PErr {
    PErr {
        code: "locked",
        reason: format!("로그인 실패가 많아 {}분 동안 잠겼습니다", (secs + 59) / 60),
        extra: json!({ "retry_after": secs }),
    }
}

fn insert_device(c: &rusqlite::Connection, id: &str, account: &str, secret: &str, now: i64) {
    let _ = c.execute(
        "INSERT INTO devices (id, account, secret, created) VALUES (?1, ?2, ?3, ?4)",
        params![id, account, secret, now],
    );
}

// ---------------------------------------------------------------- control connection

async fn control(
    server: Arc<Server>,
    mut io: Box<dyn Io>,
    ip: String,
    tls: bool,
    transport: &'static str,
    app: String,
    req: Value,
) -> Result<(), String> {
    let rid = req["rid"].clone();
    let me = match login(&server, &req, &ip, &app).await {
        Ok(m) => m,
        Err(e) => return frame::write(&mut io, &err_frame(&rid, &e)).await,
    };
    let token = store::token();
    frame::write(
        &mut io,
        &json!({ "t": "login", "rid": rid, "session": token, "account": me.account, "username": me.username }),
    )
    .await?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let kill = Arc::new(Notify::new());
    server.hub.add(
        &me.device,
        Conn {
            tx: tx.clone(),
            kill: kill.clone(),
            token: token.clone(),
            account: me.account.clone(),
            ip,
            tls,
            transport,
            app,
            since: now_ms(),
        },
    );
    let (mut r, mut w) = tokio::io::split(io);
    let writer = tokio::spawn(async move {
        while let Some(o) = rx.recv().await {
            match o {
                Out::Frame(v) => {
                    if frame::write(&mut w, &v).await.is_err() {
                        break;
                    }
                }
                Out::Close => break,
            }
        }
        let _ = w.shutdown().await;
    });

    server.contact_changed(&me.account);
    let (pending, expired) = {
        let c = server.db();
        (
            store::pending_count(&c, &me.device),
            store::expired_count(&c, &me.device),
        )
    };
    if pending > 0 || expired > 0 {
        let _ = tx.send(Out::Frame(
            json!({ "t": "event", "e": "inbox", "count": pending, "expired": expired }),
        ));
    }

    loop {
        let idle = Duration::from_secs(server.settings().idle_timeout_secs as u64);
        let req = tokio::select! {
            r = frame::read(&mut r, idle) => match r { Ok(Some(v)) => v, _ => break },
            _ = kill.notified() => break,
        };
        let rid = req["rid"].clone();
        let t = req["t"].as_str().unwrap_or("");
        if t == "logout" {
            let _ = tx.send(Out::Frame(json!({ "t": "ok", "rid": rid })));
            break;
        }
        let resp = match control_req(&server, &me, t, &req) {
            Ok(v) => with_rid(v, &rid),
            Err(e) => err_frame(&rid, &e),
        };
        if tx.send(Out::Frame(resp)).is_err() {
            break;
        }
    }
    let _ = tx.send(Out::Close);
    if server.hub.remove(&me.device, &token) {
        let now = now_ms();
        {
            let c = server.db();
            let _ = c.execute(
                "UPDATE devices SET last_seen = ?1 WHERE id = ?2",
                params![now, me.device],
            );
            let _ = c.execute(
                "UPDATE accounts SET last_seen = ?1 WHERE id = ?2",
                params![now, me.account],
            );
        }
        server.contact_changed(&me.account);
    }
    let _ = timeout(Duration::from_secs(5), writer).await;
    Ok(())
}

fn control_req(server: &Server, me: &LoggedIn, t: &str, req: &Value) -> Result<Value, PErr> {
    match t {
        "ping" => Ok(json!({ "t": "pong" })),
        "profile" => {
            let p = profile(&req["profile"]);
            {
                let c = server.db();
                let _ = c.execute(
                    "UPDATE devices SET profile = ?1 WHERE id = ?2",
                    params![p.to_string(), me.device],
                );
                if let Some(n) = p["name"].as_str().filter(|n| !n.is_empty()) {
                    let _ = c.execute(
                        "UPDATE accounts SET name = ?1 WHERE id = ?2",
                        params![n, me.account],
                    );
                }
            }
            server.contact_changed(&me.account);
            Ok(json!({ "t": "ok" }))
        }
        "contacts" => Ok(contacts(
            server,
            &me.account,
            req["since"].as_i64().unwrap_or(0),
        )),
        "lookup" => {
            let q = s_field(req, "query", 64);
            let s = server.settings();
            let found: Option<String> = server
                .db()
                .query_row(
                    "SELECT id FROM accounts WHERE (username = ?1 OR id = ?1) AND state = 'active'",
                    [&q],
                    |r| r.get(0),
                )
                .optional()
                .ok()
                .flatten();
            let Some(acc) = found.filter(|_| !q.is_empty()) else {
                return Err(perr("not_found", "사용자를 찾을 수 없습니다"));
            };
            let new = store::link(&server.db(), &me.account, &acc);
            if new && s.policy.contacts == Contacts::Lookup {
                server.contact_changed(&me.account); // the other side now sees us
            }
            let online = server.hub.online();
            let entry = store::contact(&server.db(), &online, &acc, s.show_host);
            Ok(json!({ "t": "lookup", "contact": entry }))
        }
        "inbox" => {
            let c = server.db();
            let now = now_ms();
            let mut st = c
                .prepare_cached(
                    "SELECT m.mid, m.from_account, m.from_device, m.from_name, m.created, m.files, m.files_gone, m.files_expires
                     FROM deliveries d JOIN messages m ON m.mid = d.mid
                     WHERE d.device = ?1 AND d.acked IS NULL ORDER BY m.created",
                )
                .map_err(|e| perr("bad_request", e.to_string()))?;
            let list: Vec<Value> = st
                .query_map([&me.device], |r| {
                    let files: String = r.get(5)?;
                    let gone = r.get::<_, i64>(6)? != 0 || r.get::<_, i64>(7)? <= now;
                    Ok(json!({
                        "mid": r.get::<_, String>(0)?,
                        "from": r.get::<_, String>(1)?,
                        "from_device": r.get::<_, String>(2)?,
                        "from_name": r.get::<_, String>(3)?,
                        "ts": r.get::<_, i64>(4)?,
                        "files": if gone { json!([]) } else { files_json(&files) },
                        "files_expired": gone && files != "[]",
                    }))
                })
                .map_err(|e| perr("bad_request", e.to_string()))?
                .filter_map(Result::ok)
                .collect();
            let mut st = c
                .prepare_cached("SELECT from_account, from_name, count, last FROM expired WHERE device = ?1 ORDER BY last")
                .map_err(|e| perr("bad_request", e.to_string()))?;
            let expired: Vec<Value> = st
                .query_map([&me.device], |r| {
                    Ok(json!({
                        "from": r.get::<_, String>(0)?,
                        "from_name": r.get::<_, String>(1)?,
                        "count": r.get::<_, i64>(2)?,
                        "last": r.get::<_, i64>(3)?,
                    }))
                })
                .map_err(|e| perr("bad_request", e.to_string()))?
                .filter_map(Result::ok)
                .collect();
            Ok(json!({ "t": "inbox", "list": list, "expired": expired }))
        }
        "expired_ack" => {
            let _ = server
                .db()
                .execute("DELETE FROM expired WHERE device = ?1", [&me.device]);
            Ok(json!({ "t": "ok" }))
        }
        _ => Err(perr("bad_request", format!("알 수 없는 요청: {t}"))),
    }
}

/// `files` column ("[size, …]") as protocol file entries.
fn files_json(files: &str) -> Value {
    let sizes: Vec<u64> = serde_json::from_str(files).unwrap_or_default();
    Value::Array(sizes.into_iter().map(|s| json!({ "size": s })).collect())
}

fn contacts(server: &Server, viewer: &str, since: i64) -> Value {
    let s = server.settings();
    let online = server.hub.online();
    let now = now_ms();
    let c = server.db();
    let full = since <= 0;
    let ids: Vec<String> = {
        let (sql, args): (&str, Vec<Value>) = match (s.policy.contacts, full) {
            (Contacts::All, true) => ("SELECT id FROM accounts WHERE state = 'active'", vec![]),
            (Contacts::All, false) => (
                "SELECT id FROM accounts WHERE updated >= ?1 UNION SELECT account FROM removed WHERE ts >= ?1",
                vec![json!(since)],
            ),
            (Contacts::Lookup, true) => (
                "SELECT id FROM accounts WHERE state = 'active' AND (id = ?1 OR id IN (SELECT b FROM links WHERE a = ?1))",
                vec![json!(viewer)],
            ),
            (Contacts::Lookup, false) => (
                "SELECT id FROM accounts WHERE (id = ?1 OR id IN (SELECT b FROM links WHERE a = ?1)) AND updated >= ?2
                 UNION SELECT b FROM links WHERE a = ?1 AND ts >= ?2
                 UNION SELECT account FROM removed WHERE ts >= ?2",
                vec![json!(viewer), json!(since)],
            ),
        };
        let mut st = c.prepare(sql).expect("contacts query");
        let params: Vec<Box<dyn rusqlite::ToSql>> = args
            .iter()
            .map(|a| match a {
                Value::Number(n) => Box::new(n.as_i64().unwrap_or(0)) as Box<dyn rusqlite::ToSql>,
                v => Box::new(v.as_str().unwrap_or("").to_string()),
            })
            .collect();
        st.query_map(rusqlite::params_from_iter(params.iter()), |r| r.get(0))
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default()
    };
    let list: Vec<Value> = ids
        .iter()
        .filter_map(|id| store::contact(&c, &online, id, s.show_host))
        .collect();
    json!({ "t": "contacts", "now": now, "full": full, "list": list })
}

// ---------------------------------------------------------------- transfer connection

async fn transfer(server: Arc<Server>, mut io: Box<dyn Io>, req: Value) -> Result<(), String> {
    let token = req["session"].as_str().unwrap_or("").to_string();
    let Some((device, account)) = server.hub.session(&token) else {
        return frame::write(
            &mut io,
            &err_frame(&req["rid"], &perr("session", "세션이 만료되었습니다")),
        )
        .await;
    };
    frame::write(&mut io, &json!({ "t": "ok", "rid": req["rid"] })).await?;
    loop {
        let Some(req) = frame::read(&mut io, TRANSFER_IDLE).await? else {
            return Ok(());
        };
        let rid = req["rid"].clone();
        if server.hub.session(&token).is_none() {
            return frame::write(
                &mut io,
                &err_frame(&rid, &perr("session", "세션이 만료되었습니다")),
            )
            .await;
        }
        match req["t"].as_str().unwrap_or("") {
            "send" => send(&server, &mut io, &device, &account, &req).await?,
            "fetch" => fetch(&server, &mut io, &device, &req).await?,
            "ack" => {
                let mid = req["mid"].as_str().unwrap_or("");
                let v = match ack(&server, &device, mid) {
                    Ok(()) => json!({ "t": "ok", "rid": rid }),
                    Err(e) => err_frame(&rid, &e),
                };
                frame::write(&mut io, &v).await?;
            }
            t => {
                frame::write(
                    &mut io,
                    &err_frame(&rid, &perr("bad_request", format!("알 수 없는 요청: {t}"))),
                )
                .await?
            }
        }
    }
}

struct Target {
    account: String,
    devices: Vec<String>,
}

async fn send(
    server: &Server,
    io: &mut Box<dyn Io>,
    device: &str,
    account: &str,
    req: &Value,
) -> Result<(), String> {
    let rid = req["rid"].clone();
    let fail = |e: PErr| err_frame(&rid, &e);
    let s = server.settings();
    let p = &s.policy;
    let body = req["body"].clone();
    if !body.is_object() {
        return frame::write(io, &fail(perr("bad_request", "body가 필요합니다"))).await;
    }
    let enc = body["enc"].as_str().unwrap_or("none");
    if p.encryption == Encryption::E2ee && enc == "none" {
        return frame::write(
            io,
            &fail(perr("policy", "이 서버는 종단간 암호화된 쪽지만 받습니다")),
        )
        .await;
    }
    let body_s = body.to_string();
    if body_s.len() > s.max_body_kb as usize * 1024 {
        return frame::write(
            io,
            &fail(perr(
                "policy",
                format!("본문이 너무 큽니다 (최대 {}KB)", s.max_body_kb),
            )),
        )
        .await;
    }
    let sizes: Vec<u64> = req["files"]
        .as_array()
        .map(|a| a.iter().map(|f| f["size"].as_u64().unwrap_or(0)).collect())
        .unwrap_or_default();
    if sizes.len() > p.relay_max_files as usize {
        let r = if p.relay_max_files == 0 {
            "이 서버는 첨부파일을 중계하지 않습니다".into()
        } else {
            format!("첨부파일은 {}개까지 보낼 수 있습니다", p.relay_max_files)
        };
        return frame::write(io, &fail(perr("policy", r))).await;
    }
    if sizes.iter().any(|&n| n > p.relay_max_file_size) {
        return frame::write(
            io,
            &fail(perr(
                "policy",
                format!(
                    "파일 하나의 최대 크기는 {}입니다",
                    human(p.relay_max_file_size)
                ),
            )),
        )
        .await;
    }
    let total: u64 = sizes.iter().sum();

    // Resolve recipients.
    let mut targets: Vec<Target> = Vec::new();
    let mut rejected: Vec<Value> = Vec::new();
    let quota_err = {
        let c = server.db();
        let mut seen = HashSet::new();
        for to in req["to"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            if !seen.insert(to.to_string()) || to == account {
                continue;
            }
            let active: bool = c
                .query_row(
                    "SELECT state = 'active' FROM accounts WHERE id = ?1",
                    [to],
                    |r| r.get(0),
                )
                .optional()
                .ok()
                .flatten()
                .unwrap_or(false);
            let devs = if active {
                store::devices_of(&c, to)
            } else {
                vec![]
            };
            if devs.is_empty() {
                let reason = if active {
                    "받는 사람이 아직 이 서버에 기기를 등록하지 않았습니다"
                } else {
                    "받는 사람이 이 서버에 없습니다"
                };
                rejected.push(json!({ "to": to, "code": "unknown_user", "reason": reason }));
                continue;
            }
            let over = devs.iter().find_map(|(_, prof)| {
                let mf = prof["max_files"].as_u64().unwrap_or(0);
                let ms = prof["max_file_size"].as_u64().unwrap_or(0);
                if mf > 0 && sizes.len() as u64 > mf {
                    Some(format!("받는 사람은 첨부파일을 {mf}개까지 받습니다"))
                } else if ms > 0 && sizes.iter().any(|&n| n > ms) {
                    Some(format!(
                        "받는 사람의 파일 허용 크기({})를 넘습니다",
                        human(ms)
                    ))
                } else {
                    None
                }
            });
            if let Some(r) = over {
                rejected.push(json!({ "to": to, "code": "policy", "reason": r }));
                continue;
            }
            if enc != "none" {
                let keys = &body["keys"];
                if devs
                    .iter()
                    .any(|(d, _)| keys[d].as_str().map_or(true, str::is_empty))
                {
                    rejected.push(json!({ "to": to, "code": "keys", "reason": "받는 사람의 기기 키가 바뀌었습니다. 연락처를 새로 받은 뒤 다시 보내세요" }));
                    continue;
                }
            }
            targets.push(Target {
                account: to.to_string(),
                devices: devs.into_iter().map(|d| d.0).collect(),
            });
        }
        if total > 0 {
            let (all, mine): (i64, i64) = c
                .query_row(
                    "SELECT COALESCE(SUM(bytes), 0), COALESCE(SUM(CASE WHEN from_account = ?1 THEN bytes END), 0) FROM messages WHERE files_gone = 0",
                    [account],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap_or((0, 0));
            let acct_quota: Option<i64> = c
                .query_row(
                    "SELECT quota_mb FROM accounts WHERE id = ?1",
                    [account],
                    |r| r.get(0),
                )
                .optional()
                .ok()
                .flatten()
                .flatten();
            let mb = 1024 * 1024;
            let acct_quota = acct_quota.map(|q| q as u64).unwrap_or(s.account_quota_mb);
            if s.storage_quota_mb > 0 && all as u64 + total > s.storage_quota_mb * mb {
                Some(perr("quota", "서버 저장 공간이 부족합니다"))
            } else if acct_quota > 0 && mine as u64 + total > acct_quota * mb {
                Some(perr("quota", "내 계정의 서버 보관 용량이 부족합니다"))
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(e) = quota_err {
        return frame::write(io, &fail(e)).await;
    }
    if targets.is_empty() {
        let first = rejected
            .first()
            .cloned()
            .unwrap_or(json!({ "code": "bad_request", "reason": "받는 사람이 없습니다" }));
        let mut v =
            json!({ "t": "err", "rid": rid, "code": first["code"], "reason": first["reason"] });
        v["rejected"] = Value::Array(rejected);
        return frame::write(io, &v).await;
    }

    let mid = store::new_id();
    frame::write(
        io,
        &json!({ "t": "ready", "rid": rid, "mid": mid, "rejected": rejected }),
    )
    .await?;

    // Attachments go to <blobs>/<mid>/<n>.part and are renamed once complete.
    let dir = store::blob_dir(&server.dir, &mid);
    if !sizes.is_empty() {
        if let Err(e) = receive_files(io, &dir, &sizes).await {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(e);
        }
    }

    let now = now_ms();
    let hour = 3_600_000i64;
    let text_exp = now + p.retention_hours.text as i64 * hour;
    let files_exp = now + p.retention_hours.files.min(p.retention_hours.text) as i64 * hour;
    let from_name: String = {
        let c = server.db();
        c.query_row("SELECT name FROM accounts WHERE id = ?1", [account], |r| {
            r.get(0)
        })
        .unwrap_or_default()
    };
    let mut new_links = Vec::new();
    {
        let mut c = server.db();
        let tx = c.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO messages (mid, from_account, from_device, from_name, body, files, bytes, created, text_expires, files_expires)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![mid, account, device, from_name, body_s, serde_json::to_string(&sizes).unwrap_or_default(), total as i64, now, text_exp, files_exp],
        )
        .map_err(|e| e.to_string())?;
        for t in &targets {
            for d in &t.devices {
                tx.execute(
                    "INSERT INTO deliveries (mid, device, account) VALUES (?1, ?2, ?3)",
                    params![mid, d, t.account],
                )
                .map_err(|e| e.to_string())?;
            }
            if store::link(&tx, account, &t.account) {
                new_links.push(t.account.clone());
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        let n: usize = targets.iter().map(|t| t.devices.len()).sum();
        store::stat(&c, "msgs", 1);
        store::stat(&c, "bytes", total as i64);
        store::stat(&c, "deliveries", n as i64);
    }
    if s.archive && enc == "none" {
        archive(server, &mid, now, account, device, &from_name, &targets, &body, &sizes, s.archive_files);
    }
    if !new_links.is_empty() && p.contacts == Contacts::Lookup {
        server.contact_changed(account);
        for a in &new_links {
            server.contact_changed(a);
        }
    }

    let mut result = Vec::new();
    for t in &targets {
        let mut online = 0;
        for d in &t.devices {
            if server.hub.is_online(d) {
                online += 1;
                let (n, x) = {
                    let c = server.db();
                    (store::pending_count(&c, d), store::expired_count(&c, d))
                };
                server.hub.send(
                    d,
                    json!({ "t": "event", "e": "inbox", "count": n, "expired": x }),
                );
            }
        }
        let mut r = json!({
            "to": t.account,
            "state": if online > 0 { "delivered" } else { "stored" },
            "devices": t.devices.len(),
            "online": online,
        });
        if online == 0 {
            r["expires"] = json!(text_exp);
        }
        result.push(r);
    }
    for r in &rejected {
        let mut r = r.clone();
        r["state"] = json!("failed");
        result.push(r);
    }
    frame::write(
        io,
        &json!({ "t": "sent", "rid": rid, "mid": mid, "result": result }),
    )
    .await
}

/// Keeps a reviewable copy of a plaintext message (admin "쪽지 감사").
#[allow(clippy::too_many_arguments)]
fn archive(
    server: &Server,
    mid: &str,
    now: i64,
    account: &str,
    device: &str,
    from_name: &str,
    targets: &[Target],
    body: &Value,
    sizes: &[u64],
    with_files: bool,
) {
    let c = server.db();
    let recipients: Vec<Value> = targets
        .iter()
        .map(|t| {
            let (user, name): (Option<String>, String) = c
                .query_row("SELECT username, name FROM accounts WHERE id = ?1", [&t.account], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap_or((None, String::new()));
            json!({ "account": t.account, "username": user, "name": name, "devices": t.devices.len() })
        })
        .collect();
    let kept = with_files && !sizes.is_empty() && {
        let (src, dst) = (store::blob_dir(&server.dir, mid), store::archive_dir(&server.dir, mid));
        std::fs::create_dir_all(&dst).is_ok()
            && (0..sizes.len()).all(|i| {
                let (a, b) = (src.join(i.to_string()), dst.join(i.to_string()));
                std::fs::hard_link(&a, &b).is_ok() || std::fs::copy(&a, &b).is_ok()
            })
    };
    let group = body.get("group").filter(|g| g.is_object()).map(|g| g.to_string());
    let _ = c.execute(
        "INSERT OR REPLACE INTO archive (mid, created, from_account, from_device, from_name, recipients, text, names, sizes, grp, files_kept)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            mid,
            now,
            account,
            device,
            from_name,
            Value::Array(recipients).to_string(),
            body["text"].as_str().unwrap_or(""),
            body.get("names").cloned().unwrap_or(json!([])).to_string(),
            serde_json::to_string(sizes).unwrap_or_default(),
            group,
            kept as i64,
        ],
    );
}

async fn receive_files(
    io: &mut Box<dyn Io>,
    dir: &std::path::Path,
    sizes: &[u64],
) -> Result<(), String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; CHUNK];
    for (i, &size) in sizes.iter().enumerate() {
        let part = dir.join(format!("{i}.part"));
        let mut f = tokio::fs::File::create(&part)
            .await
            .map_err(|e| e.to_string())?;
        let mut remain = size;
        while remain > 0 {
            let want = remain.min(CHUNK as u64) as usize;
            let n = timeout(IO_TIMEOUT, io.read(&mut buf[..want]))
                .await
                .map_err(|_| "timeout".to_string())?
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("connection closed".into());
            }
            f.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
            remain -= n as u64;
        }
        f.flush().await.map_err(|e| e.to_string())?;
        drop(f);
        tokio::fs::rename(&part, dir.join(i.to_string()))
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn fetch(
    server: &Server,
    io: &mut Box<dyn Io>,
    device: &str,
    req: &Value,
) -> Result<(), String> {
    let rid = req["rid"].clone();
    let mid = req["mid"].as_str().unwrap_or("").to_string();
    let now = now_ms();
    let row: Option<(String, String, String, i64, String, String, bool)> = {
        let c = server.db();
        c.query_row(
            "SELECT m.from_account, m.from_device, m.from_name, m.created, m.files, m.body, (m.files_gone != 0 OR m.files_expires <= ?3)
             FROM deliveries d JOIN messages m ON m.mid = d.mid WHERE d.mid = ?1 AND d.device = ?2 AND d.acked IS NULL",
            params![mid, device, now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .optional()
        .ok()
        .flatten()
    };
    let Some((from, from_device, from_name, ts, files, body, gone)) = row else {
        return frame::write(
            io,
            &err_frame(&rid, &perr("not_found", "보관함에 없는 쪽지입니다")),
        )
        .await;
    };
    let mut body: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    // Each device only gets its own wrapped key.
    if let Some(keys) = body.get("keys").and_then(|k| k.as_object()) {
        let mine: Map<String, Value> = keys
            .iter()
            .filter(|(k, _)| *k == device)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        body["keys"] = Value::Object(mine);
    }
    let sizes: Vec<u64> = if gone {
        vec![]
    } else {
        serde_json::from_str(&files).unwrap_or_default()
    };
    let dir = store::blob_dir(&server.dir, &mid);
    // Open every file first so a concurrent expiry cannot cut the stream short.
    let mut handles = Vec::new();
    for i in 0..sizes.len() {
        match tokio::fs::File::open(dir.join(i.to_string())).await {
            Ok(f) => handles.push(f),
            Err(_) => {
                return frame::write(
                    io,
                    &err_frame(&rid, &perr("not_found", "첨부파일을 찾을 수 없습니다")),
                )
                .await
            }
        }
    }
    let hdr = json!({
        "t": "fetch", "rid": rid, "mid": mid, "from": from, "from_device": from_device, "from_name": from_name,
        "ts": ts, "files": sizes.iter().map(|s| json!({ "size": s })).collect::<Vec<_>>(),
        "files_expired": gone && files != "[]", "body": body,
    });
    frame::write(io, &hdr).await?;
    let mut buf = vec![0u8; CHUNK];
    for (mut f, size) in handles.into_iter().zip(sizes) {
        let mut remain = size;
        while remain > 0 {
            let want = remain.min(CHUNK as u64) as usize;
            let n = f.read(&mut buf[..want]).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("stored file is truncated".into());
            }
            timeout(IO_TIMEOUT, io.write_all(&buf[..n]))
                .await
                .map_err(|_| "timeout".to_string())?
                .map_err(|e| e.to_string())?;
            remain -= n as u64;
        }
    }
    io.flush().await.map_err(|e| e.to_string())
}

fn ack(server: &Server, device: &str, mid: &str) -> Result<(), PErr> {
    let c = server.db();
    let n = c
        .execute(
            "UPDATE deliveries SET acked = ?1 WHERE mid = ?2 AND device = ?3 AND acked IS NULL",
            params![now_ms(), mid, device],
        )
        .unwrap_or(0);
    if n == 0 {
        return Err(perr("not_found", "보관함에 없는 쪽지입니다"));
    }
    let left: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM deliveries WHERE mid = ?1 AND acked IS NULL",
            [mid],
            |r| r.get(0),
        )
        .unwrap_or(1);
    if left == 0 {
        store::drop_message(&c, &server.dir, mid);
    }
    Ok(())
}

pub fn human(n: u64) -> String {
    let u = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < u.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", u[i])
    }
}
