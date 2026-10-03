//! Admin web UI and its JSON API. The first person to open the UI creates the
//! first admin account; after that every API call needs an admin session.
//!
//! CSRF: the session cookie is SameSite=Strict and every non-GET request must
//! carry `X-Relay-Admin: 1`, which a cross-site form cannot set.

use crate::relay::{hash_password, verify_password};
use crate::settings::{Settings, MAX_DEVICES_CAP};
use crate::store::{self, now_ms};
use crate::{janitor, AdminSession, Server};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

type S = State<Arc<Server>>;
type Res = Result<Json<Value>, ApiErr>;

const COOKIE: &str = "relay_admin";

pub struct ApiErr(StatusCode, String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiErr {
    ApiErr(StatusCode::BAD_REQUEST, msg.into())
}

fn db_err(e: rusqlite::Error) -> ApiErr {
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub struct Admin {
    pub id: i64,
    pub username: String,
}

impl Admin {
    fn actor(&self) -> String {
        format!("admin:{}", self.username)
    }
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

impl FromRequestParts<Arc<Server>> for Admin {
    type Rejection = ApiErr;

    async fn from_request_parts(
        parts: &mut Parts,
        server: &Arc<Server>,
    ) -> Result<Self, Self::Rejection> {
        if parts.method != Method::GET
            && parts
                .headers
                .get("x-relay-admin")
                .and_then(|v| v.to_str().ok())
                != Some("1")
        {
            return Err(ApiErr(
                StatusCode::FORBIDDEN,
                "missing X-Relay-Admin header".into(),
            ));
        }
        let token = cookie_token(&parts.headers).ok_or(ApiErr(
            StatusCode::UNAUTHORIZED,
            "로그인이 필요합니다".into(),
        ))?;
        let hours = server.settings().admin_session_hours as i64;
        let mut sessions = server.admin_sessions.lock().unwrap();
        let s = sessions.get_mut(&token).filter(|s| s.expires > now_ms());
        match s {
            Some(s) => {
                s.expires = now_ms() + hours * 3_600_000; // sliding expiry
                Ok(Admin {
                    id: s.admin,
                    username: s.username.clone(),
                })
            }
            None => Err(ApiErr(
                StatusCode::UNAUTHORIZED,
                "로그인이 필요합니다".into(),
            )),
        }
    }
}

pub async fn serve(server: Arc<Server>, l: TcpListener) {
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/session", get(session_info))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/overview", get(overview))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/accounts", get(accounts).post(create_account))
        .route("/api/accounts/bulk", post(bulk_accounts))
        .route(
            "/api/accounts/{id}",
            patch(patch_account).delete(delete_account),
        )
        .route("/api/devices", get(devices))
        .route(
            "/api/devices/{id}",
            patch(patch_device).delete(delete_device),
        )
        .route("/api/devices/{id}/kick", post(kick_device))
        .route("/api/connections", get(connections))
        .route("/api/messages", get(messages))
        .route("/api/messages/{mid}", get(message).delete(delete_message))
        .route("/api/messages/{mid}/files/{idx}", get(message_file))
        .route("/api/archive", get(archive_list))
        .route("/api/archive/{mid}", get(archive_get).delete(archive_delete))
        .route("/api/archive/{mid}/files/{idx}", get(archive_file))
        .route("/api/sweep", post(sweep))
        .route("/api/audit", get(audit))
        .route("/api/admins", get(admins).post(create_admin))
        .route("/api/admins/{id}", delete(delete_admin))
        .route("/api/password", post(change_password))
        .with_state(server);
    let _ = axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>()).await;
}

// ---------------------------------------------------------------- static

fn asset(ct: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-cache"),
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
    asset(
        "text/html; charset=utf-8",
        include_str!("../web/index.html"),
    )
}

async fn app_js() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../web/app.js"),
    )
}

async fn style_css() -> Response {
    asset("text/css; charset=utf-8", include_str!("../web/style.css"))
}

// ---------------------------------------------------------------- session

fn admin_count(server: &Server) -> i64 {
    server
        .db()
        .query_row("SELECT COUNT(*) FROM admins", [], |r| r.get(0))
        .unwrap_or(0)
}

async fn session_info(State(server): S, h: HeaderMap) -> Json<Value> {
    let user = cookie_token(&h).and_then(|t| {
        server
            .admin_sessions
            .lock()
            .unwrap()
            .get(&t)
            .filter(|s| s.expires > now_ms())
            .map(|s| s.username.clone())
    });
    Json(json!({
        "setup": admin_count(&server) == 0,
        "user": user,
        "server_name": server.settings().server_name,
        "min_password_len": server.settings().min_password_len,
    }))
}

fn start_session(server: &Server, id: i64, username: &str, body: Value) -> Response {
    let token = store::token();
    let hours = server.settings().admin_session_hours as i64;
    server.admin_sessions.lock().unwrap().insert(
        token.clone(),
        AdminSession {
            admin: id,
            username: username.to_string(),
            expires: now_ms() + hours * 3_600_000,
        },
    );
    let cookie = format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        hours * 3600
    );
    ([(header::SET_COOKIE, cookie)], Json(body)).into_response()
}

#[derive(Deserialize)]
struct Cred {
    username: String,
    password: String,
}

fn check_admin_cred(server: &Server, c: &Cred) -> Result<(), ApiErr> {
    let u = c.username.trim();
    if !(2..=32).contains(&u.chars().count())
        || !u.chars().all(|c| c.is_alphanumeric() || "._-@".contains(c))
    {
        return Err(bad(
            "관리자 이름은 2~32자의 글자, 숫자, . _ - @ 만 쓸 수 있습니다",
        ));
    }
    let min = server.settings().min_password_len.max(8);
    if (c.password.chars().count() as u32) < min {
        return Err(bad(format!("비밀번호는 {min}자 이상이어야 합니다")));
    }
    Ok(())
}

async fn setup(
    State(server): S,
    h: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(c): Json<Cred>,
) -> Result<Response, ApiErr> {
    if h.get("x-relay-admin").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(ApiErr(
            StatusCode::FORBIDDEN,
            "missing X-Relay-Admin header".into(),
        ));
    }
    check_admin_cred(&server, &c)?;
    let pass = c.password.clone();
    let hash = tokio::task::spawn_blocking(move || hash_password(&pass))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    let user = c.username.trim().to_string();
    let id = {
        let db = server.db();
        // Only succeeds while there is no admin, so two people racing cannot both win.
        let n = db
            .execute(
                "INSERT INTO admins (username, pass, created) SELECT ?1, ?2, ?3 WHERE NOT EXISTS (SELECT 1 FROM admins)",
                params![user, hash, now_ms()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(ApiErr(
                StatusCode::CONFLICT,
                "이미 관리자 계정이 있습니다".into(),
            ));
        }
        store::audit(
            &db,
            &format!("admin:{user}"),
            "admin_setup",
            &format!("first admin from {}", addr.ip()),
        );
        db.last_insert_rowid()
    };
    Ok(start_session(&server, id, &user, json!({ "user": user })))
}

async fn login(
    State(server): S,
    h: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(c): Json<Cred>,
) -> Result<Response, ApiErr> {
    if h.get("x-relay-admin").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(ApiErr(
            StatusCode::FORBIDDEN,
            "missing X-Relay-Admin header".into(),
        ));
    }
    let key = format!("admin:{}", c.username.trim().to_lowercase());
    if let Err(s) = server.locked(&key) {
        return Err(ApiErr(
            StatusCode::TOO_MANY_REQUESTS,
            format!("로그인 실패가 많아 {}분 동안 잠겼습니다", (s + 59) / 60),
        ));
    }
    let row: Option<(i64, String, String)> = server
        .db()
        .query_row(
            "SELECT id, username, pass FROM admins WHERE username = ?1",
            [c.username.trim()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(db_err)?;
    let pass = c.password.clone();
    let hash = row.as_ref().map(|r| r.2.clone());
    let ok = tokio::task::spawn_blocking(move || hash.is_some_and(|h| verify_password(&pass, &h)))
        .await
        .unwrap_or(false);
    let Some((id, user, _)) = row.filter(|_| ok) else {
        server.login_failed(&key);
        store::audit(
            &server.db(),
            &format!("admin:{}", c.username.trim()),
            "admin_login_failed",
            &addr.ip().to_string(),
        );
        return Err(ApiErr(
            StatusCode::UNAUTHORIZED,
            "이름 또는 비밀번호가 틀렸습니다".into(),
        ));
    };
    server.login_ok(&key);
    {
        let db = server.db();
        let _ = db.execute(
            "UPDATE admins SET last_login = ?1 WHERE id = ?2",
            params![now_ms(), id],
        );
        store::audit(
            &db,
            &format!("admin:{user}"),
            "admin_login",
            &addr.ip().to_string(),
        );
    }
    Ok(start_session(&server, id, &user, json!({ "user": user })))
}

async fn logout(State(server): S, _a: Admin, h: HeaderMap) -> Response {
    if let Some(t) = cookie_token(&h) {
        server.admin_sessions.lock().unwrap().remove(&t);
    }
    let cookie = format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Json(json!({}))).into_response()
}

// ---------------------------------------------------------------- overview

async fn overview(State(server): S, _a: Admin) -> Res {
    let s = server.settings();
    let conns = server.hub.snapshot();
    let db = server.db();
    let one = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    let counts = json!({
        "accounts": one("SELECT COUNT(*) FROM accounts"),
        "active": one("SELECT COUNT(*) FROM accounts WHERE state = 'active'"),
        "pending": one("SELECT COUNT(*) FROM accounts WHERE state = 'pending'"),
        "disabled": one("SELECT COUNT(*) FROM accounts WHERE state = 'disabled'"),
        "devices": one("SELECT COUNT(*) FROM devices"),
        "devices_disabled": one("SELECT COUNT(*) FROM devices WHERE disabled != 0"),
        "online": conns.len(),
        "online_plain": conns.iter().filter(|c| !c.tls).count(),
        "messages": one("SELECT COUNT(*) FROM messages"),
        "stored_bytes": one("SELECT COALESCE(SUM(bytes), 0) FROM messages WHERE files_gone = 0"),
        "pending_deliveries": one("SELECT COUNT(*) FROM deliveries WHERE acked IS NULL"),
        "expired_notices": one("SELECT COALESCE(SUM(count), 0) FROM expired"),
        "archived": one("SELECT COUNT(*) FROM archive"),
        "oldest_message": db.query_row("SELECT MIN(created) FROM messages", [], |r| r.get::<_, Option<i64>>(0)).unwrap_or(None),
    });
    let stats: Vec<Value> = db
        .prepare("SELECT day, msgs, bytes, deliveries, expired FROM stats ORDER BY day DESC LIMIT 30")
        .and_then(|mut st| {
            st.query_map([], |r| {
                Ok(json!({ "day": r.get::<_, String>(0)?, "msgs": r.get::<_, i64>(1)?, "bytes": r.get::<_, i64>(2)?,
                           "deliveries": r.get::<_, i64>(3)?, "expired": r.get::<_, i64>(4)? }))
            })?
            .collect()
        })
        .unwrap_or_default();
    let db_size = std::fs::metadata(server.dir.join("relay.db"))
        .map(|m| m.len())
        .unwrap_or(0)
        + std::fs::metadata(server.dir.join("relay.db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
    Ok(Json(json!({
        "server": {
            "id": server.id, "name": s.server_name, "version": crate::VERSION, "started": server.started, "now": now_ms(),
            "relay_port": server.relay_port, "tls_fingerprint": server.tls.fingerprint, "tls_self_signed": server.tls.self_signed,
            "data_dir": server.dir.display().to_string(), "db_bytes": db_size,
        },
        "policy": s.policy,
        "counts": counts,
        "stats": stats,
    })))
}

// ---------------------------------------------------------------- settings

async fn get_settings(State(server): S, _a: Admin) -> Res {
    Ok(Json(
        json!({ "settings": server.settings(), "max_devices_cap": MAX_DEVICES_CAP }),
    ))
}

/// "policy.encryption: tls → e2ee, show_host: false → true".
fn diff(old: &Value, new: &Value, prefix: &str, out: &mut Vec<String>) {
    if let (Value::Object(a), Value::Object(b)) = (old, new) {
        for (k, v) in b {
            let p = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            diff(a.get(k).unwrap_or(&Value::Null), v, &p, out);
        }
    } else if old != new {
        out.push(format!("{prefix}: {old} → {new}"));
    }
}

async fn put_settings(State(server): S, a: Admin, Json(s): Json<Settings>) -> Res {
    let old = serde_json::to_value(server.settings()).unwrap_or_default();
    let new = server.set_settings(s);
    let mut changes = Vec::new();
    diff(
        &old,
        &serde_json::to_value(&new).unwrap_or_default(),
        "",
        &mut changes,
    );
    if !changes.is_empty() {
        store::audit(&server.db(), &a.actor(), "settings", &changes.join(", "));
    }
    Ok(Json(json!({ "settings": new, "changes": changes })))
}

// ---------------------------------------------------------------- accounts

fn online_by_account(server: &Server) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for c in server.hub.snapshot() {
        *m.entry(c.account).or_insert(0) += 1;
    }
    m
}

async fn accounts(State(server): S, _a: Admin) -> Res {
    let online = online_by_account(&server);
    let db = server.db();
    let mut st = db
        .prepare(
            "SELECT a.id, a.username, a.name, a.state, a.max_devices, a.quota_mb, a.note, a.created, a.updated, a.last_seen,
               (SELECT COUNT(*) FROM devices d WHERE d.account = a.id),
               (SELECT COUNT(*) FROM deliveries x WHERE x.account = a.id AND x.acked IS NULL),
               (SELECT COALESCE(SUM(bytes), 0) FROM messages m WHERE m.from_account = a.id AND m.files_gone = 0),
               a.pass IS NOT NULL
             FROM accounts a ORDER BY a.created DESC",
        )
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map([], |r| {
            let id: String = r.get(0)?;
            Ok(json!({
                "id": id, "username": r.get::<_, Option<String>>(1)?, "name": r.get::<_, String>(2)?,
                "state": r.get::<_, String>(3)?, "max_devices": r.get::<_, Option<i64>>(4)?,
                "quota_mb": r.get::<_, Option<i64>>(5)?, "note": r.get::<_, String>(6)?,
                "created": r.get::<_, i64>(7)?, "updated": r.get::<_, i64>(8)?, "last_seen": r.get::<_, Option<i64>>(9)?,
                "devices": r.get::<_, i64>(10)?, "pending": r.get::<_, i64>(11)?, "stored_bytes": r.get::<_, i64>(12)?,
                "has_password": r.get::<_, bool>(13)?,
                "online": online.get(&id).copied().unwrap_or(0),
            }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "accounts": rows })))
}

#[derive(Deserialize)]
struct NewAccount {
    username: String,
    password: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    max_devices: Option<i64>,
    #[serde(default)]
    quota_mb: Option<i64>,
    #[serde(default)]
    note: String,
}

fn valid_username(u: &str) -> bool {
    (2..=32).contains(&u.chars().count())
        && u.chars().all(|c| c.is_alphanumeric() || "._-@".contains(c))
}

async fn hash_checked(server: &Server, pass: String) -> Result<String, ApiErr> {
    let min = server.settings().min_password_len;
    if (pass.chars().count() as u32) < min {
        return Err(bad(format!("비밀번호는 {min}자 이상이어야 합니다")));
    }
    tokio::task::spawn_blocking(move || hash_password(&pass))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)
}

fn check_limits(max_devices: Option<i64>, quota_mb: Option<i64>) -> Result<(), ApiErr> {
    if max_devices.is_some_and(|m| !(1..=MAX_DEVICES_CAP as i64).contains(&m)) {
        return Err(bad(format!("기기 수는 1~{MAX_DEVICES_CAP}이어야 합니다")));
    }
    if quota_mb.is_some_and(|q| q < 0) {
        return Err(bad("용량은 0 이상이어야 합니다"));
    }
    Ok(())
}

async fn create_account(State(server): S, a: Admin, Json(n): Json<NewAccount>) -> Res {
    let user = n.username.trim().to_string();
    if !valid_username(&user) {
        return Err(bad(
            "사용자 이름은 2~32자의 글자, 숫자, . _ - @ 만 쓸 수 있습니다",
        ));
    }
    check_limits(n.max_devices, n.quota_mb)?;
    let hash = hash_checked(&server, n.password).await?;
    let id = store::new_id();
    let now = now_ms();
    let name = if n.name.trim().is_empty() {
        user.clone()
    } else {
        n.name.trim().to_string()
    };
    {
        let db = server.db();
        db.execute(
            "INSERT INTO accounts (id, username, pass, name, state, max_devices, quota_mb, note, created, updated)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?8, ?8)",
            params![id, user, hash, name, n.max_devices, n.quota_mb, n.note, now],
        )
        .map_err(|_| bad("이미 쓰이는 사용자 이름입니다"))?;
        store::audit(&db, &a.actor(), "account_create", &format!("{user} ({id})"));
    }
    server.contact_changed(&id);
    Ok(Json(json!({ "id": id })))
}

fn account_label(server: &Server, id: &str) -> String {
    server
        .db()
        .query_row(
            "SELECT COALESCE(username, name) FROM accounts WHERE id = ?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .map(|n| format!("{n} ({id})"))
        .unwrap_or_else(|_| id.to_string())
}

fn kick_account(server: &Server, id: &str, reason: &str) -> usize {
    server.hub.kick_where(reason, |_, c| c.account == id)
}

/// Applies one field change from a PATCH body. Returns an audit fragment.
async fn apply_account(server: &Server, id: &str, k: &str, v: &Value) -> Result<String, ApiErr> {
    let db_set = |sql: &str, val: rusqlite::types::Value| -> Result<(), ApiErr> {
        let n = server
            .db()
            .execute(sql, params![val, id])
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    bad("이미 쓰이는 사용자 이름입니다")
                }
                e => db_err(e),
            })?;
        if n == 0 {
            return Err(ApiErr(StatusCode::NOT_FOUND, "계정이 없습니다".into()));
        }
        Ok(())
    };
    use rusqlite::types::Value as SV;
    match k {
        "name" => {
            let n = v
                .as_str()
                .unwrap_or("")
                .trim()
                .chars()
                .take(64)
                .collect::<String>();
            db_set(
                "UPDATE accounts SET name = ?1 WHERE id = ?2",
                SV::Text(n.clone()),
            )?;
            Ok(format!("name={n}"))
        }
        "note" => {
            let n = v
                .as_str()
                .unwrap_or("")
                .chars()
                .take(500)
                .collect::<String>();
            db_set("UPDATE accounts SET note = ?1 WHERE id = ?2", SV::Text(n))?;
            Ok("note".into())
        }
        "username" => {
            let u = v.as_str().unwrap_or("").trim().to_string();
            if u.is_empty() {
                db_set("UPDATE accounts SET username = ?1 WHERE id = ?2", SV::Null)?;
            } else {
                if !valid_username(&u) {
                    return Err(bad(
                        "사용자 이름은 2~32자의 글자, 숫자, . _ - @ 만 쓸 수 있습니다",
                    ));
                }
                db_set(
                    "UPDATE accounts SET username = ?1 WHERE id = ?2",
                    SV::Text(u.clone()),
                )?;
            }
            Ok(format!("username={u}"))
        }
        "password" => {
            let hash = hash_checked(server, v.as_str().unwrap_or("").to_string()).await?;
            db_set(
                "UPDATE accounts SET pass = ?1 WHERE id = ?2",
                SV::Text(hash),
            )?;
            Ok("password reset".into())
        }
        "max_devices" | "quota_mb" => {
            let n = v.as_i64();
            if k == "max_devices" {
                check_limits(n, None)?
            } else {
                check_limits(None, n)?
            }
            db_set(
                &format!("UPDATE accounts SET {k} = ?1 WHERE id = ?2"),
                n.map(SV::Integer).unwrap_or(SV::Null),
            )?;
            Ok(format!(
                "{k}={}",
                n.map(|n| n.to_string()).unwrap_or_else(|| "default".into())
            ))
        }
        "state" => {
            let s = v.as_str().unwrap_or("");
            if !matches!(s, "active" | "pending" | "disabled") {
                return Err(bad("상태 값이 올바르지 않습니다"));
            }
            db_set(
                "UPDATE accounts SET state = ?1 WHERE id = ?2",
                SV::Text(s.into()),
            )?;
            if s != "active" {
                kick_account(server, id, "관리자가 계정을 정지했습니다");
            }
            Ok(format!("state={s}"))
        }
        _ => Err(bad(format!("알 수 없는 항목: {k}"))),
    }
}

async fn patch_account(
    State(server): S,
    a: Admin,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Res {
    let Value::Object(m) = body else {
        return Err(bad("JSON 객체가 필요합니다"));
    };
    let mut done = Vec::new();
    for (k, v) in &m {
        done.push(apply_account(&server, &id, k, v).await?);
    }
    let label = account_label(&server, &id);
    store::audit(
        &server.db(),
        &a.actor(),
        "account_update",
        &format!("{label}: {}", done.join(", ")),
    );
    server.contact_changed(&id);
    Ok(Json(json!({ "ok": true })))
}

fn remove_account(server: &Server, id: &str) -> Result<bool, ApiErr> {
    kick_account(server, id, "관리자가 계정을 삭제했습니다");
    let db = server.db();
    let n = db
        .execute("DELETE FROM accounts WHERE id = ?1", [id])
        .map_err(db_err)?;
    if n == 0 {
        return Ok(false);
    }
    let _ = db.execute("DELETE FROM deliveries WHERE account = ?1", [id]);
    let _ = db.execute("DELETE FROM links WHERE a = ?1 OR b = ?1", [id]);
    let _ = db.execute(
        "INSERT INTO removed (account, ts) VALUES (?1, ?2) ON CONFLICT(account) DO UPDATE SET ts = excluded.ts",
        params![id, now_ms()],
    );
    drop(db);
    server.hub.broadcast(
        &json!({ "t": "event", "e": "contact", "contact": { "account": id, "removed": true } }),
    );
    Ok(true)
}

async fn delete_account(State(server): S, a: Admin, Path(id): Path<String>) -> Res {
    let label = account_label(&server, &id);
    if !remove_account(&server, &id)? {
        return Err(ApiErr(StatusCode::NOT_FOUND, "계정이 없습니다".into()));
    }
    store::audit(&server.db(), &a.actor(), "account_delete", &label);
    janitor::sweep(&server, now_ms());
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Bulk {
    ids: Vec<String>,
    action: String,
}

async fn bulk_accounts(State(server): S, a: Admin, Json(b): Json<Bulk>) -> Res {
    let mut n = 0;
    for id in &b.ids {
        let label = account_label(&server, id);
        let ok = match b.action.as_str() {
            "approve" | "enable" => apply_account(&server, id, "state", &json!("active"))
                .await
                .is_ok(),
            "disable" => apply_account(&server, id, "state", &json!("disabled"))
                .await
                .is_ok(),
            "delete" => remove_account(&server, id)?,
            _ => return Err(bad("알 수 없는 작업")),
        };
        if ok {
            n += 1;
            store::audit(
                &server.db(),
                &a.actor(),
                &format!("account_{}", b.action),
                &label,
            );
            if b.action != "delete" {
                server.contact_changed(id);
            }
        }
    }
    if b.action == "delete" {
        janitor::sweep(&server, now_ms());
    }
    Ok(Json(json!({ "count": n })))
}

// ---------------------------------------------------------------- devices

async fn devices(State(server): S, _a: Admin) -> Res {
    let online = server.hub.online();
    let db = server.db();
    let mut st = db
        .prepare(
            "SELECT d.id, d.account, a.username, a.name, d.profile, d.pubkey, d.disabled, d.app, d.last_ip, d.created, d.last_seen,
               (SELECT COUNT(*) FROM deliveries x WHERE x.device = d.id AND x.acked IS NULL), a.state
             FROM devices d JOIN accounts a ON a.id = d.account ORDER BY d.last_seen DESC",
        )
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map([], |r| {
            let id: String = r.get(0)?;
            let profile: Value = serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or(json!({}));
            let pubkey: String = r.get(5)?;
            Ok(json!({
                "id": id, "account": r.get::<_, String>(1)?, "username": r.get::<_, Option<String>>(2)?,
                "account_name": r.get::<_, String>(3)?, "profile": profile,
                "key": store::key_fingerprint(&pubkey), "disabled": r.get::<_, i64>(6)? != 0,
                "app": r.get::<_, String>(7)?, "last_ip": r.get::<_, String>(8)?, "created": r.get::<_, i64>(9)?,
                "last_seen": r.get::<_, Option<i64>>(10)?, "pending": r.get::<_, i64>(11)?,
                "account_state": r.get::<_, String>(12)?, "online": online.contains(&id),
            }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "devices": rows })))
}

fn device_account(server: &Server, id: &str) -> Result<String, ApiErr> {
    server
        .db()
        .query_row("SELECT account FROM devices WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .map_err(|_| ApiErr(StatusCode::NOT_FOUND, "기기가 없습니다".into()))
}

#[derive(Deserialize)]
struct DevicePatch {
    disabled: bool,
}

async fn patch_device(
    State(server): S,
    a: Admin,
    Path(id): Path<String>,
    Json(p): Json<DevicePatch>,
) -> Res {
    let account = device_account(&server, &id)?;
    server
        .db()
        .execute(
            "UPDATE devices SET disabled = ?1 WHERE id = ?2",
            params![p.disabled as i64, id],
        )
        .map_err(db_err)?;
    if p.disabled {
        server.hub.kick(&id, "관리자가 기기를 정지했습니다");
    }
    store::audit(
        &server.db(),
        &a.actor(),
        if p.disabled {
            "device_disable"
        } else {
            "device_enable"
        },
        &id,
    );
    server.contact_changed(&account);
    Ok(Json(json!({ "ok": true })))
}

async fn delete_device(State(server): S, a: Admin, Path(id): Path<String>) -> Res {
    let account = device_account(&server, &id)?;
    server.hub.kick(&id, "관리자가 기기 등록을 해제했습니다");
    {
        let db = server.db();
        let _ = db.execute(
            "DELETE FROM deliveries WHERE device = ?1 AND acked IS NULL",
            [&id],
        );
        let _ = db.execute("DELETE FROM expired WHERE device = ?1", [&id]);
        db.execute("DELETE FROM devices WHERE id = ?1", [&id])
            .map_err(db_err)?;
        store::audit(
            &db,
            &a.actor(),
            "device_delete",
            &format!("{id} of {account}"),
        );
    }
    server.contact_changed(&account);
    janitor::sweep(&server, now_ms());
    Ok(Json(json!({ "ok": true })))
}

async fn kick_device(State(server): S, a: Admin, Path(id): Path<String>) -> Res {
    let ok = server.hub.kick(&id, "관리자가 연결을 끊었습니다");
    if ok {
        store::audit(&server.db(), &a.actor(), "device_kick", &id);
    }
    Ok(Json(json!({ "ok": ok })))
}

async fn connections(State(server): S, _a: Admin) -> Res {
    let conns = server.hub.snapshot();
    let db = server.db();
    let rows: Vec<Value> = conns
        .into_iter()
        .map(|c| {
            let (name, user): (String, Option<String>) = db
                .query_row(
                    "SELECT COALESCE(json_extract(d.profile, '$.name'), ''), a.username FROM devices d JOIN accounts a ON a.id = d.account WHERE d.id = ?1",
                    [&c.device],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap_or_default();
            let status: String = db
                .query_row("SELECT COALESCE(json_extract(profile, '$.status'), '') FROM devices WHERE id = ?1", [&c.device], |r| r.get(0))
                .unwrap_or_default();
            json!({ "device": c.device, "account": c.account, "ip": c.ip, "tls": c.tls, "app": c.app, "since": c.since,
                    "name": name, "username": user, "status": status })
        })
        .collect();
    Ok(Json(json!({ "connections": rows })))
}

// ---------------------------------------------------------------- stored messages

async fn messages(State(server): S, _a: Admin) -> Res {
    let db = server.db();
    // Metadata only; the body is shown per message (GET /api/messages/{mid}), which is audited.
    let mut st = db
        .prepare(
            "SELECT m.mid, m.from_account, m.from_name, m.bytes, m.files, m.created, m.text_expires, m.files_expires, m.files_gone,
               COALESCE(json_extract(m.body, '$.enc'), 'none'),
               (SELECT COUNT(*) FROM deliveries d WHERE d.mid = m.mid AND d.acked IS NULL),
               (SELECT COUNT(*) FROM deliveries d WHERE d.mid = m.mid),
               (SELECT COUNT(DISTINCT account) FROM deliveries d WHERE d.mid = m.mid),
               (SELECT group_concat(DISTINCT COALESCE(a.username, a.name)) FROM deliveries d JOIN accounts a ON a.id = d.account WHERE d.mid = m.mid)
             FROM messages m ORDER BY m.created DESC LIMIT 5000",
        )
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map([], |r| {
            let files: Vec<u64> = serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default();
            Ok(json!({
                "mid": r.get::<_, String>(0)?, "from": r.get::<_, String>(1)?, "from_name": r.get::<_, String>(2)?,
                "bytes": r.get::<_, i64>(3)?, "files": files.len(), "created": r.get::<_, i64>(5)?,
                "text_expires": r.get::<_, i64>(6)?, "files_expires": r.get::<_, i64>(7)?, "files_gone": r.get::<_, i64>(8)? != 0,
                "enc": r.get::<_, String>(9)?, "pending": r.get::<_, i64>(10)?, "deliveries": r.get::<_, i64>(11)?,
                "recipients": r.get::<_, i64>(12)?, "to": r.get::<_, Option<String>>(13)?,
            }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "messages": rows })))
}

fn hex_id(s: &str) -> Result<(), ApiErr> {
    if !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(bad("잘못된 ID"))
    }
}

fn not_found(what: &str) -> ApiErr {
    ApiErr(StatusCode::NOT_FOUND, format!("{what}을(를) 찾을 수 없습니다"))
}

/// Streams a stored file as a download named `name`.
async fn download(path: std::path::PathBuf, name: &str) -> Result<Response, ApiErr> {
    let f = tokio::fs::File::open(&path).await.map_err(|_| not_found("첨부파일"))?;
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || ".-_ ()".contains(c) { c } else { '_' })
        .collect();
    let utf8: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b".-_".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let disp = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{utf8}");
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(f));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, disp),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response())
}

/// One stored message. Plaintext bodies are shown (and the viewing is audited);
/// end-to-end encrypted ones only as metadata.
async fn message(State(server): S, a: Admin, Path(mid): Path<String>) -> Res {
    hex_id(&mid)?;
    let db = server.db();
    type Row = (String, String, String, String, String, i64, i64, i64, bool);
    let row: Option<Row> = db
        .query_row(
            "SELECT from_account, from_device, from_name, body, files, created, text_expires, files_expires, files_gone != 0
             FROM messages WHERE mid = ?1",
            [&mid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
        )
        .optional()
        .map_err(db_err)?;
    let Some((from, from_device, from_name, body, files, created, text_exp, files_exp, gone)) = row else {
        return Err(not_found("쪽지"));
    };
    let body: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let enc = body["enc"].as_str().unwrap_or("none").to_string();
    let plain = enc == "none";
    let sizes: Vec<u64> = serde_json::from_str(&files).unwrap_or_default();
    let gone = gone || files_exp <= now_ms();
    let blobs = store::blob_dir(&server.dir, &mid);
    let files: Vec<Value> = sizes
        .iter()
        .enumerate()
        .map(|(i, s)| {
            json!({
                "idx": i, "size": s,
                "name": if plain { body["names"][i].clone() } else { Value::Null },
                "available": plain && !gone && blobs.join(i.to_string()).exists(),
            })
        })
        .collect();
    let mut st = db
        .prepare(
            "SELECT d.device, d.account, d.acked, COALESCE(a.username, a.name), COALESCE(json_extract(v.profile, '$.name'), '')
             FROM deliveries d LEFT JOIN accounts a ON a.id = d.account LEFT JOIN devices v ON v.id = d.device
             WHERE d.mid = ?1 ORDER BY 4, 1",
        )
        .map_err(db_err)?;
    let deliveries: Vec<Value> = st
        .query_map([&mid], |r| {
            Ok(json!({
                "device": r.get::<_, String>(0)?, "account": r.get::<_, String>(1)?, "acked": r.get::<_, Option<i64>>(2)?,
                "account_name": r.get::<_, Option<String>>(3)?, "device_name": r.get::<_, String>(4)?,
            }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    drop(st);
    if plain {
        store::audit(&db, &a.actor(), "message_view", &format!("{mid} from {from_name} ({from})"));
    }
    Ok(Json(json!({
        "mid": mid, "from": from, "from_device": from_device, "from_name": from_name, "created": created,
        "text_expires": text_exp, "files_expires": files_exp, "files_gone": gone, "enc": enc,
        "content": if plain { json!({ "text": body["text"], "group": body["group"] }) } else { Value::Null },
        "files": files, "deliveries": deliveries,
    })))
}

async fn message_file(
    State(server): S,
    a: Admin,
    Path((mid, idx)): Path<(String, usize)>,
) -> Result<Response, ApiErr> {
    hex_id(&mid)?;
    let (body, gone): (String, bool) = server
        .db()
        .query_row(
            "SELECT body, files_gone != 0 OR files_expires <= ?2 FROM messages WHERE mid = ?1",
            params![mid, now_ms()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| not_found("쪽지"))?;
    let body: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    if body["enc"].as_str().unwrap_or("none") != "none" {
        return Err(ApiErr(StatusCode::FORBIDDEN, "종단간 암호화된 첨부파일은 열람할 수 없습니다".into()));
    }
    if gone {
        return Err(not_found("첨부파일"));
    }
    let name = file_name(&body["names"], idx);
    let resp = download(store::blob_dir(&server.dir, &mid).join(idx.to_string()), &name).await?;
    store::audit(&server.db(), &a.actor(), "message_file", &format!("{mid} #{idx} {name}"));
    Ok(resp)
}

fn file_name(names: &Value, idx: usize) -> String {
    names[idx]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("file{}", idx + 1))
}

// ---------------------------------------------------------------- audit archive

#[derive(Deserialize)]
struct ArchiveQ {
    q: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    limit: Option<i64>,
}

const ARCHIVE_COLS: &str =
    "mid, created, from_account, from_name, recipients, text, names, sizes, files_kept, grp, from_device";

fn archive_row(r: &rusqlite::Row, full: bool) -> rusqlite::Result<Value> {
    let names: Value = serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or(json!([]));
    let sizes: Vec<u64> = serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default();
    let text: String = r.get(5)?;
    let files: Vec<Value> = sizes
        .iter()
        .enumerate()
        .map(|(i, s)| json!({ "idx": i, "size": s, "name": names[i] }))
        .collect();
    let mut v = json!({
        "mid": r.get::<_, String>(0)?, "created": r.get::<_, i64>(1)?, "from": r.get::<_, String>(2)?,
        "from_name": r.get::<_, String>(3)?,
        "recipients": serde_json::from_str::<Value>(&r.get::<_, String>(4)?).unwrap_or(json!([])),
        "files": files, "bytes": sizes.iter().sum::<u64>(), "files_kept": r.get::<_, i64>(8)? != 0,
        "group": r.get::<_, Option<String>>(9)?.and_then(|g| serde_json::from_str::<Value>(&g).ok()),
        "from_device": r.get::<_, String>(10)?,
    });
    if full {
        v["text"] = json!(text);
    } else {
        v["preview"] = json!(text.chars().take(200).collect::<String>());
        v["length"] = json!(text.chars().count());
    }
    Ok(v)
}

/// Search over the archive. Every search is itself written to the audit log.
async fn archive_list(State(server): S, a: Admin, Query(q): Query<ArchiveQ>) -> Res {
    let query: String = q.q.unwrap_or_default().trim().chars().take(200).collect();
    let pat = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let (from, to) = (q.from.unwrap_or(0), q.to.unwrap_or(i64::MAX));
    let limit = q.limit.unwrap_or(1000).clamp(1, 20_000);
    let filter = "created >= ?1 AND created < ?2 AND (?3 = '' OR text LIKE ?4 ESCAPE '\\' OR from_name LIKE ?4 ESCAPE '\\'
                  OR recipients LIKE ?4 ESCAPE '\\' OR names LIKE ?4 ESCAPE '\\')";
    let db = server.db();
    let total: i64 = db
        .query_row(&format!("SELECT COUNT(*) FROM archive WHERE {filter}"), params![from, to, query, pat], |r| r.get(0))
        .map_err(db_err)?;
    let mut st = db
        .prepare(&format!("SELECT {ARCHIVE_COLS} FROM archive WHERE {filter} ORDER BY created DESC LIMIT ?5"))
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map(params![from, to, query, pat, limit], |r| archive_row(r, false))
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    drop(st);
    let all: i64 = db.query_row("SELECT COUNT(*) FROM archive", [], |r| r.get(0)).unwrap_or(0);
    let range = |t: i64, none: bool| if none { "-".to_string() } else { t.to_string() };
    let detail = format!(
        "query={query:?} from={} to={} -> {} rows",
        range(from, from == 0),
        range(to, to == i64::MAX),
        rows.len()
    );
    store::audit(&db, &a.actor(), "archive_list", &detail);
    Ok(Json(json!({ "archive": rows, "total": total, "all": all })))
}

async fn archive_get(State(server): S, a: Admin, Path(mid): Path<String>) -> Res {
    hex_id(&mid)?;
    let db = server.db();
    let v = db
        .query_row(&format!("SELECT {ARCHIVE_COLS} FROM archive WHERE mid = ?1"), [&mid], |r| archive_row(r, true))
        .optional()
        .map_err(db_err)?
        .ok_or_else(|| not_found("쪽지"))?;
    let from = v["from_name"].as_str().unwrap_or("").to_string();
    store::audit(&db, &a.actor(), "archive_view", &format!("{mid} from {from}"));
    Ok(Json(v))
}

async fn archive_file(
    State(server): S,
    a: Admin,
    Path((mid, idx)): Path<(String, usize)>,
) -> Result<Response, ApiErr> {
    hex_id(&mid)?;
    let (names, kept): (String, bool) = server
        .db()
        .query_row("SELECT names, files_kept != 0 FROM archive WHERE mid = ?1", [&mid], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|_| not_found("쪽지"))?;
    if !kept {
        return Err(not_found("보관된 첨부파일"));
    }
    let name = file_name(&serde_json::from_str(&names).unwrap_or(json!([])), idx);
    let resp = download(store::archive_dir(&server.dir, &mid).join(idx.to_string()), &name).await?;
    store::audit(&server.db(), &a.actor(), "archive_file", &format!("{mid} #{idx} {name}"));
    Ok(resp)
}

async fn archive_delete(State(server): S, a: Admin, Path(mid): Path<String>) -> Res {
    hex_id(&mid)?;
    let db = server.db();
    let n = db.execute("DELETE FROM archive WHERE mid = ?1", [&mid]).map_err(db_err)?;
    if n == 0 {
        return Err(not_found("쪽지"));
    }
    let _ = std::fs::remove_dir_all(store::archive_dir(&server.dir, &mid));
    store::audit(&db, &a.actor(), "archive_delete", &mid);
    Ok(Json(json!({ "ok": true })))
}

async fn delete_message(State(server): S, a: Admin, Path(mid): Path<String>) -> Res {
    if !mid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad("잘못된 ID"));
    }
    let db = server.db();
    store::drop_message(&db, &server.dir, &mid);
    store::audit(&db, &a.actor(), "message_purge", &mid);
    Ok(Json(json!({ "ok": true })))
}

async fn sweep(State(server): S, a: Admin) -> Res {
    let r = janitor::sweep(&server, now_ms());
    store::audit(&server.db(), &a.actor(), "sweep", &format!("{r:?}"));
    Ok(Json(
        json!({ "files": r.files, "messages": r.messages, "notices": r.notices }),
    ))
}

// ---------------------------------------------------------------- audit / admins

#[derive(Deserialize)]
struct AuditQ {
    limit: Option<i64>,
}

async fn audit(State(server): S, _a: Admin, Query(q): Query<AuditQ>) -> Res {
    let db = server.db();
    let mut st = db
        .prepare("SELECT ts, actor, action, detail FROM audit ORDER BY id DESC LIMIT ?1")
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map([q.limit.unwrap_or(1000).clamp(1, 20_000)], |r| {
            Ok(json!({ "ts": r.get::<_, i64>(0)?, "actor": r.get::<_, String>(1)?, "action": r.get::<_, String>(2)?,
                       "detail": r.get::<_, String>(3)? }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "audit": rows })))
}

async fn admins(State(server): S, a: Admin) -> Res {
    let db = server.db();
    let mut st = db
        .prepare("SELECT id, username, created, last_login FROM admins ORDER BY id")
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map([], |r| {
            let id: i64 = r.get(0)?;
            Ok(json!({ "id": id, "username": r.get::<_, String>(1)?, "created": r.get::<_, i64>(2)?,
                       "last_login": r.get::<_, Option<i64>>(3)?, "me": id == a.id }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "admins": rows })))
}

async fn create_admin(State(server): S, a: Admin, Json(c): Json<Cred>) -> Res {
    check_admin_cred(&server, &c)?;
    let pass = c.password.clone();
    let hash = tokio::task::spawn_blocking(move || hash_password(&pass))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    let db = server.db();
    db.execute(
        "INSERT INTO admins (username, pass, created) VALUES (?1, ?2, ?3)",
        params![c.username.trim(), hash, now_ms()],
    )
    .map_err(|_| bad("이미 있는 관리자 이름입니다"))?;
    store::audit(&db, &a.actor(), "admin_create", c.username.trim());
    Ok(Json(json!({ "ok": true })))
}

async fn delete_admin(State(server): S, a: Admin, Path(id): Path<i64>) -> Res {
    if id == a.id {
        return Err(bad("자기 자신은 삭제할 수 없습니다"));
    }
    let name: String = {
        let db = server.db();
        let name: String = db
            .query_row("SELECT username FROM admins WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .map_err(|_| ApiErr(StatusCode::NOT_FOUND, "관리자가 없습니다".into()))?;
        db.execute("DELETE FROM admins WHERE id = ?1", [id])
            .map_err(db_err)?;
        store::audit(&db, &a.actor(), "admin_delete", &name);
        name
    };
    server
        .admin_sessions
        .lock()
        .unwrap()
        .retain(|_, s| s.admin != id);
    Ok(Json(json!({ "ok": true, "username": name })))
}

#[derive(Deserialize)]
struct PassChange {
    old: String,
    new: String,
}

async fn change_password(State(server): S, a: Admin, Json(p): Json<PassChange>) -> Res {
    let hash: String = server
        .db()
        .query_row("SELECT pass FROM admins WHERE id = ?1", [a.id], |r| {
            r.get(0)
        })
        .map_err(db_err)?;
    let old = p.old.clone();
    if !tokio::task::spawn_blocking(move || verify_password(&old, &hash))
        .await
        .unwrap_or(false)
    {
        return Err(bad("현재 비밀번호가 틀렸습니다"));
    }
    check_admin_cred(
        &server,
        &Cred {
            username: a.username.clone(),
            password: p.new.clone(),
        },
    )?;
    let new = p.new.clone();
    let hash = tokio::task::spawn_blocking(move || hash_password(&new))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    let db = server.db();
    db.execute(
        "UPDATE admins SET pass = ?1 WHERE id = ?2",
        params![hash, a.id],
    )
    .map_err(db_err)?;
    store::audit(&db, &a.actor(), "admin_password", "");
    Ok(Json(json!({ "ok": true })))
}
