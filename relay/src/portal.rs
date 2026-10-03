//! HTTP on the relay port: WebSocket upgrades go to the client protocol, every
//! other request to the user portal, where account holders manage their own
//! account (profile, password, devices, their own activity log).
//!
//! CSRF: the session cookie is SameSite=Strict and every non-GET request must
//! carry `X-Relay-User: 1`.

use crate::relay::{hash_password, verify_password, Io};
use crate::settings::Encryption;
use crate::store::{self, now_ms};
use crate::ws::{self, Forwarded, Rewind};
use crate::{janitor, Server};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::timeout;

const COOKIE: &str = "relay_user";
const SESSION_HOURS: i64 = 12;
const HEAD_LIMIT: usize = 16 * 1024;

pub struct UserSession {
    pub account: String,
    pub username: String,
    pub expires: i64,
}

/// Per-connection facts (per-request ones come from proxy headers).
#[derive(Clone)]
struct ConnInfo {
    tls: bool,
    ip: String,
}

/// Reads the request head; a WebSocket upgrade is returned as a protocol stream,
/// anything else is served as the portal on this connection.
pub async fn http<S: Io + 'static>(server: Arc<Server>, mut s: S, tls: bool, ip: String) -> Option<(DuplexStream, Forwarded)> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > HEAD_LIMIT {
            return None;
        }
        let n = match timeout(Duration::from_secs(15), s.read(&mut tmp)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return None,
        };
        buf.extend_from_slice(&tmp[..n]);
    }
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
    let upgrade = head.lines().any(|l| l.starts_with("upgrade:") && l.contains("websocket"));
    let st = server.settings();
    let mut stream = Rewind::new(buf, s);
    if upgrade {
        if !st.websocket {
            return None;
        }
        let chunk = st.ws_chunk_kb as usize * 1024;
        return match timeout(Duration::from_secs(15), ws::accept(stream, chunk)).await {
            Ok(Ok(x)) => Some(x),
            _ => None,
        };
    }
    if !st.user_portal {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return None;
    }
    let svc = hyper_util::service::TowerToHyperService::new(router(server).layer(Extension(ConnInfo { tls, ip })));
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(20))
        .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
        .await;
    None
}

fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/user.js", get(user_js))
        .route("/style.css", get(style_css))
        .route("/api/me/session", get(session_info))
        .route("/api/me/login", post(login))
        .route("/api/me/logout", post(logout))
        .route("/api/me", get(me).patch(patch_me))
        .route("/api/me/password", post(password))
        .route("/api/me/devices/{id}", delete(delete_device))
        .route("/api/me/devices/{id}/kick", post(kick_device))
        .route("/api/me/activity", get(activity))
        .with_state(server)
}

type S = State<Arc<Server>>;
type Res = Result<Json<Value>, ApiErr>;

pub struct ApiErr(StatusCode, String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(m: impl Into<String>) -> ApiErr {
    ApiErr(StatusCode::BAD_REQUEST, m.into())
}

fn db_err(e: rusqlite::Error) -> ApiErr {
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn header_val(h: &HeaderMap, k: &str) -> Option<String> {
    h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// Encrypted end to end between browser and this server (or a trusted TLS proxy)?
fn secure(server: &Server, c: &ConnInfo, h: &HeaderMap) -> bool {
    let https = header_val(h, "x-forwarded-proto").is_some_and(|p| p.eq_ignore_ascii_case("https"))
        || header_val(h, "cf-visitor").is_some_and(|v| v.contains("\"https\""));
    c.tls || (server.settings().proxy_tls && https)
}

fn client_ip(server: &Server, c: &ConnInfo, h: &HeaderMap) -> String {
    if server.settings().proxy_tls {
        if let Some(ip) = header_val(h, "cf-connecting-ip")
            .or_else(|| header_val(h, "x-forwarded-for").and_then(|f| f.split(',').next().map(|s| s.trim().to_string())))
        {
            return ip.chars().take(64).collect();
        }
    }
    c.ip.clone()
}

fn cookie_token(h: &HeaderMap) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
}

/// The logged-in account holder.
pub struct Me {
    account: String,
    username: String,
    token: String,
}

impl FromRequestParts<Arc<Server>> for Me {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> Result<Self, Self::Rejection> {
        if parts.method != Method::GET && header_val(&parts.headers, "x-relay-user").as_deref() != Some("1") {
            return Err(ApiErr(StatusCode::FORBIDDEN, "missing X-Relay-User header".into()));
        }
        let unauth = || ApiErr(StatusCode::UNAUTHORIZED, "로그인이 필요합니다".into());
        let token = cookie_token(&parts.headers).ok_or_else(unauth)?;
        let (account, username) = {
            let mut sessions = server.user_sessions.lock().unwrap();
            let s = sessions.get_mut(&token).filter(|s| s.expires > now_ms()).ok_or_else(unauth)?;
            s.expires = now_ms() + SESSION_HOURS * 3_600_000;
            (s.account.clone(), s.username.clone())
        };
        let state: Option<String> = server
            .db()
            .query_row("SELECT state FROM accounts WHERE id = ?1", [&account], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        if state.as_deref() != Some("active") {
            server.user_sessions.lock().unwrap().remove(&token);
            return Err(unauth());
        }
        Ok(Me { account, username, token })
    }
}

// ---------------------------------------------------------------- static

fn asset(ct: &'static str, body: impl Into<axum::body::Body>, cache: &'static str) -> Response {
    let body: axum::body::Body = body.into();
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, cache),
            (header::X_FRAME_OPTIONS, "DENY"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; style-src 'self'; script-src 'self'; img-src 'self' data:",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    let v = crate::version();
    let html = include_str!("../web/user.html")
        .replace("\"user.js\"", &format!("\"user.js?v={v}\""))
        .replace("\"style.css\"", &format!("\"style.css?v={v}\""));
    asset("text/html; charset=utf-8", html, "no-store")
}

async fn user_js() -> Response {
    asset("text/javascript; charset=utf-8", include_str!("../web/user.js"), "no-cache")
}

async fn style_css() -> Response {
    asset("text/css; charset=utf-8", include_str!("../web/style.css"), "no-cache")
}

// ---------------------------------------------------------------- session

async fn session_info(State(server): S, Extension(c): Extension<ConnInfo>, h: HeaderMap) -> Json<Value> {
    let s = server.settings();
    let user = cookie_token(&h).and_then(|t| {
        server.user_sessions.lock().unwrap().get(&t).filter(|x| x.expires > now_ms()).map(|x| x.username.clone())
    });
    Json(json!({
        "server_name": s.server_name,
        "version": crate::version(),
        "auth": s.policy.auth,
        "secure": secure(&server, &c, &h) || s.policy.encryption == Encryption::Plain,
        "public_url": s.public_url,
        "user": user,
    }))
}

#[derive(Deserialize)]
struct Cred {
    username: String,
    password: String,
}

async fn login(
    State(server): S,
    Extension(c): Extension<ConnInfo>,
    h: HeaderMap,
    Json(cred): Json<Cred>,
) -> Result<Response, ApiErr> {
    if header_val(&h, "x-relay-user").as_deref() != Some("1") {
        return Err(ApiErr(StatusCode::FORBIDDEN, "missing X-Relay-User header".into()));
    }
    let sec = secure(&server, &c, &h);
    if !sec && server.settings().policy.encryption != Encryption::Plain {
        return Err(bad("암호화되지 않은 연결로는 로그인할 수 없습니다. https:// 주소로 접속하세요"));
    }
    let ip = client_ip(&server, &c, &h);
    let user = cred.username.trim().to_string();
    let key = format!("user:{}", user.to_lowercase());
    if let Err(s) = server.locked(&key) {
        return Err(ApiErr(StatusCode::TOO_MANY_REQUESTS, format!("로그인 실패가 많아 {}분 동안 잠겼습니다", (s + 59) / 60)));
    }
    let row: Option<(String, String, Option<String>, String)> = server
        .db()
        .query_row("SELECT id, username, pass, state FROM accounts WHERE username = ?1", [&user], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()
        .map_err(db_err)?;
    let pass = cred.password.clone();
    let hash = row.as_ref().and_then(|r| r.2.clone());
    let ok = tokio::task::spawn_blocking(move || hash.is_some_and(|h| verify_password(&pass, &h))).await.unwrap_or(false);
    let Some((account, username, _, state)) = row.filter(|_| ok) else {
        server.login_failed(&key);
        store::audit(&server.db(), &format!("user:{user}"), "user_login_failed", &format!("web from {ip}"));
        return Err(ApiErr(StatusCode::UNAUTHORIZED, "사용자 이름 또는 비밀번호가 틀렸습니다".into()));
    };
    match state.as_str() {
        "pending" => return Err(bad("가입 승인을 기다리는 계정입니다")),
        "disabled" => return Err(bad("관리자가 정지한 계정입니다")),
        _ => {}
    }
    server.login_ok(&key);
    store::audit_for(&server.db(), &format!("user:{username}"), "user_login", &format!("web from {ip}"), &account);
    let token = store::token();
    server.user_sessions.lock().unwrap().insert(
        token.clone(),
        UserSession { account, username: username.clone(), expires: now_ms() + SESSION_HOURS * 3_600_000 },
    );
    let cookie = format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
        SESSION_HOURS * 3600,
        if sec { "; Secure" } else { "" }
    );
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({ "user": username }))).into_response())
}

async fn logout(State(server): S, me: Me) -> Response {
    server.user_sessions.lock().unwrap().remove(&me.token);
    let cookie = format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Json(json!({}))).into_response()
}

// ---------------------------------------------------------------- account

async fn me(State(server): S, me: Me) -> Res {
    let s = server.settings();
    let online = server.hub.online();
    let db = server.db();
    type Row = (Option<String>, String, String, Option<i64>, Option<i64>, i64, Option<i64>);
    let (username, name, state, max_devices, quota_mb, created, last_seen): Row = db
        .query_row(
            "SELECT username, name, state, max_devices, quota_mb, created, last_seen FROM accounts WHERE id = ?1",
            [&me.account],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .map_err(db_err)?;
    let one = |sql: &str| -> i64 { db.query_row(sql, [&me.account], |r| r.get(0)).unwrap_or(0) };
    let stored = one("SELECT COALESCE(SUM(bytes), 0) FROM messages WHERE from_account = ?1 AND files_gone = 0");
    let pending = one("SELECT COUNT(*) FROM deliveries WHERE account = ?1 AND acked IS NULL");
    let mut st = db
        .prepare(
            "SELECT d.id, d.profile, d.pubkey, d.disabled, d.app, d.last_ip, d.created, d.last_seen,
               (SELECT COUNT(*) FROM deliveries x WHERE x.device = d.id AND x.acked IS NULL)
             FROM devices d WHERE d.account = ?1 ORDER BY d.last_seen DESC",
        )
        .map_err(db_err)?;
    let devices: Vec<Value> = st
        .query_map([&me.account], |r| {
            let id: String = r.get(0)?;
            let p: Value = serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or(json!({}));
            Ok(json!({
                "id": id, "name": p["name"], "host": p["host"], "status": p["status"],
                "key": store::key_fingerprint(&r.get::<_, String>(2)?), "disabled": r.get::<_, i64>(3)? != 0,
                "app": r.get::<_, String>(4)?, "last_ip": r.get::<_, String>(5)?, "created": r.get::<_, i64>(6)?,
                "last_seen": r.get::<_, Option<i64>>(7)?, "pending": r.get::<_, i64>(8)?, "online": online.contains(&id),
            }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    let p = &s.policy;
    Ok(Json(json!({
        "account": {
            "id": me.account, "username": username, "name": name, "state": state, "created": created, "last_seen": last_seen,
            "max_devices": max_devices.map(|m| m as u32).unwrap_or(p.max_devices),
            "quota_mb": quota_mb.map(|q| q as u64).unwrap_or(s.account_quota_mb),
            "stored_bytes": stored, "pending": pending,
        },
        "devices": devices,
        "policy": {
            "encryption": p.encryption, "relay_max_files": p.relay_max_files, "relay_max_file_size": p.relay_max_file_size,
            "retention_hours": p.retention_hours, "min_password_len": s.min_password_len,
        },
    })))
}

#[derive(Deserialize)]
struct NamePatch {
    name: String,
}

async fn patch_me(State(server): S, me: Me, Json(p): Json<NamePatch>) -> Res {
    let name: String = p.name.trim().chars().take(64).collect();
    if name.is_empty() {
        return Err(bad("이름을 입력하세요"));
    }
    {
        let db = server.db();
        db.execute("UPDATE accounts SET name = ?1 WHERE id = ?2", params![name, me.account]).map_err(db_err)?;
        store::audit_for(&db, &format!("user:{}", me.username), "user_name", &name, &me.account);
    }
    server.contact_changed(&me.account);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct PassChange {
    old: String,
    new: String,
}

async fn password(State(server): S, me: Me, Json(p): Json<PassChange>) -> Res {
    let hash: Option<String> = server
        .db()
        .query_row("SELECT pass FROM accounts WHERE id = ?1", [&me.account], |r| r.get(0))
        .map_err(db_err)?;
    let old = p.old.clone();
    if !tokio::task::spawn_blocking(move || hash.is_some_and(|h| verify_password(&old, &h))).await.unwrap_or(false) {
        return Err(bad("현재 비밀번호가 틀렸습니다"));
    }
    let min = server.settings().min_password_len;
    if (p.new.chars().count() as u32) < min {
        return Err(bad(format!("새 비밀번호는 {min}자 이상이어야 합니다")));
    }
    let new = p.new.clone();
    let hash = tokio::task::spawn_blocking(move || hash_password(&new)).await.map_err(|e| bad(e.to_string()))?.map_err(bad)?;
    {
        let db = server.db();
        db.execute("UPDATE accounts SET pass = ?1 WHERE id = ?2", params![hash, me.account]).map_err(db_err)?;
        store::audit_for(&db, &format!("user:{}", me.username), "user_password", "", &me.account);
    }
    // Other web sessions of this account end; this one stays.
    server.user_sessions.lock().unwrap().retain(|t, s| s.account != me.account || *t == me.token);
    Ok(Json(json!({ "ok": true })))
}

fn own_device(server: &Server, me: &Me, id: &str) -> Result<(), ApiErr> {
    let acc: Option<String> = server
        .db()
        .query_row("SELECT account FROM devices WHERE id = ?1", [id], |r| r.get(0))
        .optional()
        .map_err(db_err)?;
    match acc {
        Some(a) if a == me.account => Ok(()),
        _ => Err(ApiErr(StatusCode::NOT_FOUND, "기기를 찾을 수 없습니다".into())),
    }
}

async fn kick_device(State(server): S, me: Me, Path(id): Path<String>) -> Res {
    own_device(&server, &me, &id)?;
    let ok = server.hub.kick(&id, "계정 소유자가 웹에서 연결을 끊었습니다");
    store::audit_for(&server.db(), &format!("user:{}", me.username), "user_device_kick", &id, &me.account);
    Ok(Json(json!({ "ok": ok })))
}

/// Unregisters a device: it must log in with the password again to come back,
/// and messages waiting for it are dropped.
async fn delete_device(State(server): S, me: Me, Path(id): Path<String>) -> Res {
    own_device(&server, &me, &id)?;
    server.hub.kick(&id, "계정 소유자가 이 기기의 등록을 해제했습니다");
    {
        let db = server.db();
        let name: String = db
            .query_row("SELECT COALESCE(json_extract(profile, '$.name'), '') FROM devices WHERE id = ?1", [&id], |r| r.get(0))
            .unwrap_or_default();
        let _ = db.execute("DELETE FROM deliveries WHERE device = ?1 AND acked IS NULL", [&id]);
        let _ = db.execute("DELETE FROM expired WHERE device = ?1", [&id]);
        db.execute("DELETE FROM devices WHERE id = ?1", [&id]).map_err(db_err)?;
        store::audit_for(&db, &format!("user:{}", me.username), "user_device_delete", &format!("{id} {name}"), &me.account);
    }
    server.contact_changed(&me.account);
    janitor::sweep(&server, now_ms());
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Page {
    page: Option<i64>,
    per: Option<i64>,
}

/// This account's own activity; actions taken by administrators are not shown.
async fn activity(State(server): S, me: Me, Query(q): Query<Page>) -> Res {
    let per = q.per.unwrap_or(50).clamp(10, 200);
    let page = q.page.unwrap_or(1).max(1);
    let db = server.db();
    let filter = "account = ?1 AND actor NOT LIKE 'admin:%'";
    let total: i64 = db
        .query_row(&format!("SELECT COUNT(*) FROM audit WHERE {filter}"), [&me.account], |r| r.get(0))
        .map_err(db_err)?;
    let mut st = db
        .prepare(&format!("SELECT ts, actor, action, detail FROM audit WHERE {filter} ORDER BY id DESC LIMIT ?2 OFFSET ?3"))
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map(params![me.account, per, (page - 1) * per], |r| {
            Ok(json!({ "ts": r.get::<_, i64>(0)?, "actor": r.get::<_, String>(1)?, "action": r.get::<_, String>(2)?,
                       "detail": r.get::<_, String>(3)? }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "items": rows, "total": total, "page": page, "per": per })))
}
