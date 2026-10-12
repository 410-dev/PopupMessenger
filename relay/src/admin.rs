//! Admin web UI and its JSON API. The first person to open the UI creates the
//! first server admin account; after that every API call needs an admin session.
//!
//! Server admins manage tenancies, their resource limits and the server settings,
//! and can act within any tenancy. Tenancy admins manage one tenancy: its users,
//! policy and audit log. Tenancy-scoped calls name the tenancy with the
//! `X-Relay-Tenancy` header or `?tenancy=` (server admins only; a tenancy admin is
//! always in their own).
//!
//! CSRF: the session cookie is SameSite=Strict and every non-GET request must
//! carry `X-Relay-Admin: 1`, which a cross-site form cannot set.

use crate::relay::{hash_password, verify_password};
use crate::settings::{Limits, ServerSettings, TenantSettings, MAX_DEVICES_CAP};
use crate::store::{self, now_ms};
use crate::tenant::{Tenant, UNMIGRATED};
use crate::{janitor, AdminSession, Server};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use rusqlite::{params, Connection, OptionalExtension};
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

fn forbidden(msg: impl Into<String>) -> ApiErr {
    ApiErr(StatusCode::FORBIDDEN, msg.into())
}

fn db_err(e: rusqlite::Error) -> ApiErr {
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub struct Admin {
    pub id: i64,
    pub username: String,
    /// `None` for server admins.
    pub tenancy: Option<String>,
}

impl Admin {
    /// "admin:<name>" for tenancy admins, "admin:server/<name>" for server admins.
    fn actor(&self) -> String {
        match self.tenancy {
            Some(_) => format!("admin:{}", self.username),
            None => format!("admin:server/{}", self.username),
        }
    }

    fn is_server(&self) -> bool {
        self.tenancy.is_none()
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
            return Err(forbidden("missing X-Relay-Admin header"));
        }
        let token = cookie_token(&parts.headers).ok_or(ApiErr(
            StatusCode::UNAUTHORIZED,
            "로그인이 필요합니다".into(),
        ))?;
        let hours = server.server_settings().admin_session_hours as i64;
        let mut sessions = server.admin_sessions.lock().unwrap();
        let s = sessions.get_mut(&token).filter(|s| s.expires > now_ms());
        match s {
            Some(s) => {
                s.expires = now_ms() + hours * 3_600_000; // sliding expiry
                Ok(Admin {
                    id: s.admin,
                    username: s.username.clone(),
                    tenancy: s.tenancy.clone(),
                })
            }
            None => Err(ApiErr(
                StatusCode::UNAUTHORIZED,
                "로그인이 필요합니다".into(),
            )),
        }
    }
}

/// A server admin.
pub struct ServerAdmin(Admin);

impl FromRequestParts<Arc<Server>> for ServerAdmin {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> Result<Self, Self::Rejection> {
        let a = Admin::from_request_parts(parts, server).await?;
        if !a.is_server() {
            return Err(forbidden("서버 총괄 관리자만 할 수 있습니다"));
        }
        Ok(ServerAdmin(a))
    }
}

/// The tenancy a server admin asked for (`X-Relay-Tenancy` or `?tenancy=`).
fn requested_tenancy(parts: &Parts) -> Option<String> {
    let h = parts
        .headers
        .get("x-relay-tenancy")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let q = parts.uri.query().and_then(|q| {
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == "tenancy")
            .map(|(_, v)| v.to_string())
    });
    h.or(q).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty())
}

/// An admin acting within one tenancy.
pub struct Scope {
    pub server: Arc<Server>,
    pub tn: Arc<Tenant>,
    pub admin: Admin,
}

impl FromRequestParts<Arc<Server>> for Scope {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> Result<Self, Self::Rejection> {
        let admin = Admin::from_request_parts(parts, server).await?;
        let id = match &admin.tenancy {
            Some(t) => t.clone(),
            None => requested_tenancy(parts).ok_or_else(|| bad("Tenancy를 선택하세요"))?,
        };
        let tn = server
            .tenant(&id)
            .ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "없는 Tenancy입니다".into()))?;
        Ok(Scope {
            server: server.clone(),
            tn,
            admin,
        })
    }
}

/// An admin within one tenancy, or a server admin with none selected (server scope).
pub struct OptScope {
    pub server: Arc<Server>,
    pub tn: Option<Arc<Tenant>>,
    pub admin: Admin,
}

impl FromRequestParts<Arc<Server>> for OptScope {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> Result<Self, Self::Rejection> {
        let admin = Admin::from_request_parts(parts, server).await?;
        let id = admin.tenancy.clone().or_else(|| requested_tenancy(parts));
        let tn = match id {
            Some(id) => Some(
                server
                    .tenant(&id)
                    .ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "없는 Tenancy입니다".into()))?,
            ),
            None => None,
        };
        Ok(OptScope {
            server: server.clone(),
            tn,
            admin,
        })
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
        .route("/api/server", get(server_overview))
        .route("/api/server/settings", get(get_server_settings).put(put_server_settings))
        .route("/api/tenancies", get(tenancies).post(create_tenancy))
        .route("/api/tenancies/{id}", patch(patch_tenancy).delete(delete_tenancy))
        .route("/api/tenancies/{id}/purge", post(purge_tenancy))
        .route("/api/overview", get(overview))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/accounts", get(accounts).post(create_account))
        .route("/api/accounts/bulk", post(bulk_accounts))
        .route("/api/departments", get(departments).post(create_department))
        .route("/api/departments/{id}", patch(patch_department).delete(delete_department))
        .route("/api/departments/{id}/members", post(department_members))
        .route("/api/keys", get(api_keys).post(create_api_key))
        .route("/api/keys/{id}", delete(revoke_api_key))
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

fn asset(ct: &'static str, body: impl Into<axum::body::Body>) -> Response {
    let body: axum::body::Body = body.into();
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

/// The page links its script and stylesheet with the server version, so a browser
/// never keeps using the previous build's UI after an upgrade.
async fn index() -> Response {
    let v = crate::version();
    let html = include_str!("../web/index.html")
        .replace("\"app.js\"", &format!("\"app.js?v={v}\""))
        .replace("\"style.css\"", &format!("\"style.css?v={v}\""));
    let mut r = asset("text/html; charset=utf-8", html);
    r.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    r
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

fn server_admin_count(server: &Server) -> i64 {
    server
        .db()
        .query_row("SELECT COUNT(*) FROM admins WHERE tenancy = ''", [], |r| r.get(0))
        .unwrap_or(0)
}

fn tenancy_json(t: &Tenant) -> Value {
    json!({ "id": t.id, "name": t.name(), "unmigrated": t.is_unmigrated() })
}

async fn session_info(State(server): S, h: HeaderMap) -> Json<Value> {
    let session = cookie_token(&h).and_then(|t| {
        server
            .admin_sessions
            .lock()
            .unwrap()
            .get(&t)
            .filter(|s| s.expires > now_ms())
            .map(|s| (s.username.clone(), s.tenancy.clone()))
    });
    let tenancy = session
        .as_ref()
        .and_then(|(_, t)| t.as_ref())
        .and_then(|t| server.tenant(t));
    let ss = server.server_settings();
    Json(json!({
        "setup": server_admin_count(&server) == 0,
        "user": session.as_ref().map(|s| &s.0),
        // A server admin when logged in without a tenancy.
        "server_admin": session.as_ref().is_some_and(|s| s.1.is_none()),
        "tenancy": tenancy.as_ref().map(|t| tenancy_json(t)),
        "server_name": ss.server_name,
        "min_password_len": tenancy.as_ref().map(|t| t.settings().min_password_len.max(8)).unwrap_or(8),
        "version": crate::version(),
    }))
}

fn start_session(server: &Server, id: i64, username: &str, tenancy: Option<String>, body: Value) -> Response {
    let token = store::token();
    let hours = server.server_settings().admin_session_hours as i64;
    server.admin_sessions.lock().unwrap().insert(
        token.clone(),
        AdminSession {
            admin: id,
            username: username.to_string(),
            tenancy,
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
    /// Blank or missing: a server admin.
    #[serde(default)]
    tenancy: String,
    username: String,
    password: String,
}

/// Admin passwords are at least 8 characters, or the tenancy's minimum if longer.
fn check_admin_cred(tn: Option<&Tenant>, c: &Cred) -> Result<(), ApiErr> {
    let u = c.username.trim();
    if !(2..=32).contains(&u.chars().count())
        || !u.chars().all(|c| c.is_alphanumeric() || "._-@".contains(c))
    {
        return Err(bad(
            "관리자 이름은 2~32자의 글자, 숫자, . _ - @ 만 쓸 수 있습니다",
        ));
    }
    let min = tn.map(|t| t.settings().min_password_len).unwrap_or(0).max(8);
    if (c.password.chars().count() as u32) < min {
        return Err(bad(format!("비밀번호는 {min}자 이상이어야 합니다")));
    }
    Ok(())
}

async fn hash_async(pass: String) -> Result<String, ApiErr> {
    tokio::task::spawn_blocking(move || hash_password(&pass))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)
}

fn need_admin_header(h: &HeaderMap) -> Result<(), ApiErr> {
    if h.get("x-relay-admin").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(forbidden("missing X-Relay-Admin header"));
    }
    Ok(())
}

/// Writes an admin action to the tenancy's audit log, or the server's.
fn audit_in(server: &Server, tn: Option<&Tenant>, actor: &str, action: &str, detail: &str) {
    match tn {
        Some(t) => store::audit(&t.db(), actor, action, detail),
        None => store::audit_server(&server.db(), actor, action, detail, None),
    }
}

async fn setup(
    State(server): S,
    h: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(c): Json<Cred>,
) -> Result<Response, ApiErr> {
    need_admin_header(&h)?;
    check_admin_cred(None, &c)?;
    let hash = hash_async(c.password.clone()).await?;
    let user = c.username.trim().to_string();
    let id = {
        let db = server.db();
        // Only succeeds while there is no server admin, so two people racing cannot both win.
        let n = db
            .execute(
                "INSERT INTO admins (tenancy, username, pass, created) SELECT '', ?1, ?2, ?3
                 WHERE NOT EXISTS (SELECT 1 FROM admins WHERE tenancy = '')",
                params![user, hash, now_ms()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(ApiErr(
                StatusCode::CONFLICT,
                "이미 관리자 계정이 있습니다".into(),
            ));
        }
        store::audit_server(
            &db,
            &format!("admin:server/{user}"),
            "admin_setup",
            &format!("first admin from {}", addr.ip()),
            None,
        );
        db.last_insert_rowid()
    };
    Ok(start_session(&server, id, &user, None, json!({ "user": user })))
}

async fn login(
    State(server): S,
    h: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(c): Json<Cred>,
) -> Result<Response, ApiErr> {
    need_admin_header(&h)?;
    let tenancy = c.tenancy.trim().to_lowercase();
    let tn = if tenancy.is_empty() {
        None
    } else {
        // Unknown tenancies fail like a wrong password.
        server.tenant(&tenancy)
    };
    let key = format!("admin:{tenancy}/{}", c.username.trim().to_lowercase());
    if let Err(s) = server.locked(&key) {
        return Err(ApiErr(
            StatusCode::TOO_MANY_REQUESTS,
            format!("로그인 실패가 많아 {}분 동안 잠겼습니다", (s + 59) / 60),
        ));
    }
    let row: Option<(i64, String, String)> = server
        .db()
        .query_row(
            "SELECT id, username, pass FROM admins WHERE tenancy = ?1 AND username = ?2",
            [tenancy.as_str(), c.username.trim()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(db_err)?;
    let pass = c.password.clone();
    let hash = row.as_ref().map(|r| r.2.clone());
    let ok = tokio::task::spawn_blocking(move || hash.is_some_and(|h| verify_password(&pass, &h)))
        .await
        .unwrap_or(false);
    let actor = |u: &str| {
        if tenancy.is_empty() {
            format!("admin:server/{u}")
        } else {
            format!("admin:{u}")
        }
    };
    let Some((id, user, _)) = row.filter(|_| ok && (tenancy.is_empty() || tn.is_some())) else {
        server.login_failed(&key);
        let detail = if tenancy.is_empty() || tn.is_some() {
            addr.ip().to_string()
        } else {
            format!("{} (tenancy {tenancy})", addr.ip())
        };
        audit_in(&server, tn.as_deref(), &actor(c.username.trim()), "admin_login_failed", &detail);
        return Err(ApiErr(
            StatusCode::UNAUTHORIZED,
            "이름 또는 비밀번호가 틀렸습니다".into(),
        ));
    };
    server.login_ok(&key);
    let _ = server.db().execute(
        "UPDATE admins SET last_login = ?1 WHERE id = ?2",
        params![now_ms(), id],
    );
    audit_in(&server, tn.as_deref(), &actor(&user), "admin_login", &addr.ip().to_string());
    let scope = tn.as_ref().map(|t| t.id.clone());
    let body = json!({ "user": user, "tenancy": tn.as_ref().map(|t| tenancy_json(t)) });
    Ok(start_session(&server, id, &user, scope, body))
}

async fn logout(State(server): S, _a: Admin, h: HeaderMap) -> Response {
    if let Some(t) = cookie_token(&h) {
        server.admin_sessions.lock().unwrap().remove(&t);
    }
    let cookie = format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Json(json!({}))).into_response()
}

// ---------------------------------------------------------------- server (server admins)

fn server_info(server: &Server) -> Value {
    let s = server.server_settings();
    let file = |n: &str| std::fs::metadata(server.dir.join(n)).map(|m| m.len()).unwrap_or(0);
    json!({
        "id": server.id, "name": s.server_name, "version": crate::version(), "started": server.started, "now": now_ms(),
        "relay_port": server.relay_port, "tls_fingerprint": server.tls.fingerprint, "tls_self_signed": server.tls.self_signed,
        "data_dir": server.dir.display().to_string(), "db_bytes": file("server.db") + file("server.db-wal"),
        "public_url": s.public_url, "websocket": s.websocket, "proxy_tls": s.proxy_tls, "ws_chunk_kb": s.ws_chunk_kb,
        "storage_quota_mb": s.storage_quota_mb, "domain": server.domain(), "exchange": s.exchange,
    })
}

/// A tenancy's row in the server admin's tenancy list.
fn tenancy_row(server: &Server, t: &Tenant) -> Value {
    let (note, created): (String, i64) = server
        .db()
        .query_row("SELECT note, created FROM tenancies WHERE id = ?1", [&t.id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap_or_default();
    let admins: i64 = server
        .db()
        .query_row("SELECT COUNT(*) FROM admins WHERE tenancy = ?1", [&t.id], |r| r.get(0))
        .unwrap_or(0);
    let (accounts, pending, devices): (i64, i64, i64) = t
        .db()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM accounts), (SELECT COUNT(*) FROM accounts WHERE state = 'pending'),
                    (SELECT COUNT(*) FROM devices)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or_default();
    let s = t.tenant_settings();
    json!({
        "id": t.id, "name": t.name(), "note": note, "created": created, "unmigrated": t.is_unmigrated(),
        "limits": t.limits(), "accounts": accounts, "pending": pending, "devices": devices,
        "online": t.hub.online().len(), "stored_bytes": t.stored_bytes(), "admins": admins,
        "auth": s.policy.auth, "encryption": s.policy.encryption, "registration": s.policy.registration,
    })
}

async fn server_overview(State(server): S, _a: ServerAdmin) -> Res {
    let list: Vec<Value> = server.tenants().iter().map(|t| tenancy_row(&server, t)).collect();
    let moved: i64 = server
        .db()
        .query_row("SELECT COUNT(DISTINCT tenancy || key) FROM moved WHERE kind = 'device'", [], |r| r.get(0))
        .unwrap_or(0);
    Ok(Json(json!({
        "server": server_info(&server),
        "tenancies": list,
        "stored_bytes": server.stored_bytes(),
        "moved_devices": moved,
    })))
}

async fn get_server_settings(State(server): S, _a: ServerAdmin) -> Res {
    Ok(Json(json!({ "settings": server.server_settings() })))
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

fn changes(old: &impl serde::Serialize, new: &impl serde::Serialize) -> Vec<String> {
    let mut out = Vec::new();
    diff(
        &serde_json::to_value(old).unwrap_or_default(),
        &serde_json::to_value(new).unwrap_or_default(),
        "",
        &mut out,
    );
    out
}

async fn put_server_settings(State(server): S, ServerAdmin(a): ServerAdmin, Json(s): Json<ServerSettings>) -> Res {
    let old = server.server_settings();
    let new = server.set_server_settings(s);
    let ch = changes(&old, &new);
    if !ch.is_empty() {
        store::audit_server(&server.db(), &a.actor(), "server_settings", &ch.join(", "), None);
    }
    Ok(Json(json!({ "settings": new, "changes": ch })))
}

async fn tenancies(State(server): S, _a: ServerAdmin) -> Res {
    let list: Vec<Value> = server.tenants().iter().map(|t| tenancy_row(&server, t)).collect();
    Ok(Json(json!({ "items": list })))
}

#[derive(Deserialize)]
struct NewAdmin {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct NewTenancy {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    limits: Limits,
    /// Optional first admin of the tenancy.
    #[serde(default)]
    admin: Option<NewAdmin>,
}

async fn create_tenancy(State(server): S, ServerAdmin(a): ServerAdmin, Json(n): Json<NewTenancy>) -> Res {
    let id = n.id.trim().to_lowercase();
    let first = match &n.admin {
        Some(x) => {
            let c = Cred { tenancy: id.clone(), username: x.username.clone(), password: x.password.clone() };
            check_admin_cred(None, &c)?;
            Some((c.username.trim().to_string(), hash_async(c.password).await?))
        }
        None => None,
    };
    let t = server.create_tenancy(&id, &n.name, &n.note, n.limits).map_err(bad)?;
    let db = server.db();
    store::audit_server(&db, &a.actor(), "tenancy_create", &format!("{} ({})", t.id, t.name()), Some(&t.id));
    if let Some((user, hash)) = first {
        db.execute(
            "INSERT INTO admins (tenancy, username, pass, created) VALUES (?1, ?2, ?3, ?4)",
            params![t.id, user, hash, now_ms()],
        )
        .map_err(db_err)?;
        store::audit_server(&db, &a.actor(), "admin_create", &format!("{user} of {}", t.id), Some(&t.id));
    }
    Ok(Json(json!({ "id": t.id })))
}

#[derive(Deserialize)]
struct TenancyPatch {
    name: Option<String>,
    note: Option<String>,
    limits: Option<Limits>,
}

async fn patch_tenancy(
    State(server): S,
    ServerAdmin(a): ServerAdmin,
    Path(id): Path<String>,
    Json(p): Json<TenancyPatch>,
) -> Res {
    let t = server
        .tenant(&id)
        .ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "없는 Tenancy입니다".into()))?;
    let (old_name, old_limits) = (t.name(), t.limits());
    server
        .update_tenancy(&id, p.name.as_deref(), p.note.as_deref(), p.limits)
        .map_err(bad)?;
    let mut ch = Vec::new();
    if t.name() != old_name {
        ch.push(format!("name: {old_name} → {}", t.name()));
    }
    if p.note.is_some() {
        ch.push("note".into());
    }
    ch.extend(changes(&old_limits, &t.limits()).into_iter().map(|c| format!("limits.{c}")));
    if !ch.is_empty() {
        store::audit_server(&server.db(), &a.actor(), "tenancy_update", &format!("{id}: {}", ch.join(", ")), Some(&id));
    }
    Ok(Json(tenancy_row(&server, &t)))
}

async fn delete_tenancy(State(server): S, ServerAdmin(a): ServerAdmin, Path(id): Path<String>) -> Res {
    let name = server.tenant(&id).map(|t| t.name()).unwrap_or_default();
    server.delete_tenancy(&id).map_err(bad)?;
    store::audit_server(&server.db(), &a.actor(), "tenancy_delete", &format!("{id} ({name})"), Some(&id));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Purge {
    /// The tenancy ID typed again.
    confirm: String,
}

/// Deletes every account of a tenancy (so that it can then be deleted).
async fn purge_tenancy(
    State(server): S,
    ServerAdmin(a): ServerAdmin,
    Path(id): Path<String>,
    Json(p): Json<Purge>,
) -> Res {
    let t = server
        .tenant(&id)
        .ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "없는 Tenancy입니다".into()))?;
    if p.confirm.trim() != id {
        return Err(bad("확인용 Tenancy ID가 다릅니다"));
    }
    let ids: Vec<String> = t
        .db()
        .prepare("SELECT id FROM accounts")
        .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect())
        .map_err(db_err)?;
    let mut n = 0;
    for acc in &ids {
        if remove_account(&t, acc)? {
            n += 1;
        }
    }
    store::audit(&t.db(), &a.actor(), "account_purge", &format!("{n} accounts"));
    store::audit_server(&server.db(), &a.actor(), "tenancy_purge", &format!("{id}: {n} accounts"), Some(&id));
    janitor::sweep_tenant(&t, now_ms());
    Ok(Json(json!({ "count": n })))
}

// ---------------------------------------------------------------- overview

async fn overview(Scope { server, tn, .. }: Scope) -> Res {
    let s = tn.settings();
    let conns = tn.hub.snapshot();
    let db = tn.db();
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
    drop(db);
    let file = |n: &str| std::fs::metadata(tn.dir.join(n)).map(|m| m.len()).unwrap_or(0);
    let mut info = server_info(&server);
    info["db_bytes"] = json!(file("relay.db") + file("relay.db-wal"));
    info["data_dir"] = json!(tn.dir.display().to_string());
    Ok(Json(json!({
        "server": info,
        "tenancy": tenancy_json(&tn),
        "limits": tn.limits(),
        "policy": s.policy,
        "counts": counts,
        "stats": stats,
    })))
}

// ---------------------------------------------------------------- settings

async fn get_settings(Scope { tn, .. }: Scope) -> Res {
    Ok(Json(json!({
        "settings": tn.tenant_settings(),
        "limits": tn.limits(),
        "max_devices_cap": MAX_DEVICES_CAP,
    })))
}

async fn put_settings(Scope { tn, admin: a, .. }: Scope, Json(s): Json<TenantSettings>) -> Res {
    let old = tn.tenant_settings();
    let new = tn.set_tenant_settings(s);
    let ch = changes(&old, &new);
    if !ch.is_empty() {
        store::audit(&tn.db(), &a.actor(), "settings", &ch.join(", "));
    }
    Ok(Json(json!({ "settings": new, "limits": tn.limits(), "changes": ch })))
}

// ---------------------------------------------------------------- paging

/// Common list parameters: `?page=1&per=100&q=...&sort=col&dir=asc|desc&filter=...`.
#[derive(Deserialize, Default)]
struct ListQ {
    page: Option<i64>,
    per: Option<i64>,
    q: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    filter: Option<String>,
    /// Devices of one account.
    account: Option<String>,
    /// Accounts in one department.
    department: Option<String>,
}

impl ListQ {
    fn per(&self, max: i64) -> i64 {
        self.per.unwrap_or(100).clamp(10, max)
    }
    fn page(&self) -> i64 {
        self.page.unwrap_or(1).clamp(1, 1_000_000)
    }
    fn offset(&self, max: i64) -> i64 {
        (self.page() - 1) * self.per(max)
    }
    fn query(&self) -> String {
        self.q.clone().unwrap_or_default().trim().chars().take(200).collect()
    }
    fn filter(&self) -> String {
        self.filter.clone().unwrap_or_default()
    }
    fn asc(&self) -> bool {
        self.dir.as_deref() == Some("asc")
    }
    /// ORDER BY from a whitelist of sortable columns (never raw input).
    fn order(&self, cols: &[(&str, &str)], default: &str, tie: &str) -> String {
        match self.sort.as_deref().and_then(|k| cols.iter().find(|c| c.0 == k)) {
            Some((_, expr)) => format!("ORDER BY {expr} {} NULLS LAST, {tie}", if self.asc() { "ASC" } else { "DESC" }),
            None => format!("ORDER BY {default}, {tie}"),
        }
    }
}

fn page_json(items: Vec<Value>, total: i64, q: &ListQ, max: i64, extra: Value) -> Json<Value> {
    let mut v = json!({ "items": items, "total": total, "page": q.page(), "per": q.per(max) });
    if let Value::Object(m) = extra {
        for (k, x) in m {
            v[k] = x;
        }
    }
    Json(v)
}

// ---------------------------------------------------------------- accounts

fn online_by_account(tn: &Tenant) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for c in tn.hub.snapshot() {
        *m.entry(c.account).or_insert(0) += 1;
    }
    m
}

async fn accounts(Scope { tn, .. }: Scope, Query(q): Query<ListQ>) -> Res {
    const MAX: i64 = 500;
    let online = online_by_account(&tn);
    let text = q.query();
    let state = match q.filter().as_str() {
        f @ ("active" | "pending" | "disabled") => f.to_string(),
        _ => String::new(),
    };
    let filter = "(?1 = '' OR a.state = ?1) AND (?2 = '' OR a.username LIKE ?3 ESCAPE '\\' OR a.name LIKE ?3 ESCAPE '\\'
                  OR a.id LIKE ?3 ESCAPE '\\' OR a.note LIKE ?3 ESCAPE '\\')
                  AND (?4 = '' OR EXISTS (SELECT 1 FROM members m WHERE m.account = a.id AND m.department = ?4))";
    let dep = q.department.clone().unwrap_or_default();
    let order = q.order(
        &[
            ("state", "a.state"), ("username", "a.username"), ("name", "a.name"), ("id", "a.id"), ("devices", "devs"),
            ("pending", "pend"), ("stored_bytes", "stored"), ("quota_mb", "a.quota_mb"), ("last_seen", "a.last_seen"),
            ("created", "a.created"), ("note", "a.note"),
        ],
        "a.created DESC",
        "a.rowid DESC",
    );
    let db = tn.db();
    let pat = store::like(&text);
    let total: i64 = db
        .query_row(&format!("SELECT COUNT(*) FROM accounts a WHERE {filter}"), params![state, text, pat, dep], |r| r.get(0))
        .map_err(db_err)?;
    let mut st = db
        .prepare(&format!(
            "SELECT a.id, a.username, a.name, a.state, a.max_devices, a.quota_mb, a.note, a.created, a.updated, a.last_seen,
               (SELECT COUNT(*) FROM devices d WHERE d.account = a.id) AS devs,
               (SELECT COUNT(*) FROM deliveries x WHERE x.account = a.id AND x.acked IS NULL) AS pend,
               (SELECT COALESCE(SUM(bytes), 0) FROM messages m WHERE m.from_account = a.id AND m.files_gone = 0) AS stored,
               a.pass IS NOT NULL,
               (SELECT group_concat(department, ',') FROM members m WHERE m.account = a.id)
             FROM accounts a WHERE {filter} {order} LIMIT ?5 OFFSET ?6"
        ))
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map(params![state, text, pat, dep, q.per(MAX), q.offset(MAX)], |r| {
            let deps: Vec<String> = r
                .get::<_, Option<String>>(14)?
                .map(|s| s.split(',').map(str::to_string).collect())
                .unwrap_or_default();
            let id: String = r.get(0)?;
            Ok(json!({
                "id": id, "username": r.get::<_, Option<String>>(1)?, "name": r.get::<_, String>(2)?,
                "state": r.get::<_, String>(3)?, "max_devices": r.get::<_, Option<i64>>(4)?,
                "quota_mb": r.get::<_, Option<i64>>(5)?, "note": r.get::<_, String>(6)?,
                "created": r.get::<_, i64>(7)?, "updated": r.get::<_, i64>(8)?, "last_seen": r.get::<_, Option<i64>>(9)?,
                "devices": r.get::<_, i64>(10)?, "pending": r.get::<_, i64>(11)?, "stored_bytes": r.get::<_, i64>(12)?,
                "has_password": r.get::<_, bool>(13)?, "departments": deps,
                "online": online.get(&id).copied().unwrap_or(0),
            }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    drop(st);
    let mut counts = json!({ "all": 0, "active": 0, "pending": 0, "disabled": 0 });
    let mut st = db.prepare("SELECT state, COUNT(*) FROM accounts GROUP BY state").map_err(db_err)?;
    for (k, n) in st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(db_err)?
        .filter_map(Result::ok)
    {
        counts["all"] = json!(counts["all"].as_i64().unwrap_or(0) + n);
        counts[k] = json!(n);
    }
    Ok(page_json(rows, total, &q, MAX, json!({ "counts": counts })))
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

async fn hash_checked(tn: &Tenant, pass: String) -> Result<String, ApiErr> {
    let min = tn.settings().min_password_len;
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

async fn create_account(Scope { tn, admin: a, .. }: Scope, Json(n): Json<NewAccount>) -> Res {
    let user = n.username.trim().to_string();
    if !valid_username(&user) {
        return Err(bad(
            "사용자 이름은 2~32자의 글자, 숫자, . _ - @ 만 쓸 수 있습니다",
        ));
    }
    check_limits(n.max_devices, n.quota_mb)?;
    room_for_account(&tn).map_err(bad)?;
    let hash = hash_checked(&tn, n.password).await?;
    let id = store::new_id();
    let now = now_ms();
    let name = if n.name.trim().is_empty() {
        user.clone()
    } else {
        n.name.trim().to_string()
    };
    {
        let db = tn.db();
        db.execute(
            "INSERT INTO accounts (id, username, pass, name, state, max_devices, quota_mb, note, created, updated)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?8, ?8)",
            params![id, user, hash, name, n.max_devices, n.quota_mb, n.note, now],
        )
        .map_err(|_| bad("이미 쓰이는 사용자 이름입니다"))?;
        store::audit(&db, &a.actor(), "account_create", &format!("{user} ({id})"));
    }
    tn.contact_changed(&id);
    Ok(Json(json!({ "id": id })))
}

fn account_label(tn: &Tenant, id: &str) -> String {
    tn
        .db()
        .query_row(
            "SELECT COALESCE(username, name) FROM accounts WHERE id = ?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .map(|n| format!("{n} ({id})"))
        .unwrap_or_else(|_| id.to_string())
}

fn kick_account(tn: &Tenant, id: &str, reason: &str) -> usize {
    tn.hub.kick_where(reason, |_, c| c.account == id)
}

/// Applies one field change from a PATCH body. Returns an audit fragment.
async fn apply_account(tn: &Tenant, id: &str, k: &str, v: &Value) -> Result<String, ApiErr> {
    let db_set = |sql: &str, val: rusqlite::types::Value| -> Result<(), ApiErr> {
        let n = tn
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
        "departments" => {
            // The whole list of departments the account is in.
            let want: Vec<String> = v
                .as_array()
                .ok_or_else(|| bad("departments는 목록이어야 합니다"))?
                .iter()
                .filter_map(|d| d.as_str())
                .map(|d| d.trim().to_lowercase())
                .collect();
            let mut db = tn.db();
            let tx = db.transaction().map_err(db_err)?;
            for d in &want {
                let known = tx
                    .query_row("SELECT 1 FROM departments WHERE id = ?1", [d], |_| Ok(()))
                    .optional()
                    .map_err(db_err)?
                    .is_some();
                if !known {
                    return Err(bad(format!("없는 부서입니다: {d}")));
                }
            }
            tx.execute("DELETE FROM members WHERE account = ?1", [id]).map_err(db_err)?;
            for d in &want {
                tx.execute(
                    "INSERT OR IGNORE INTO members (department, account) VALUES (?1, ?2)",
                    params![d, id],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(format!("departments={}", want.join(",")))
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
            let hash = hash_checked(tn, v.as_str().unwrap_or("").to_string()).await?;
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
                kick_account(tn, id, "관리자가 계정을 정지했습니다");
            }
            Ok(format!("state={s}"))
        }
        _ => Err(bad(format!("알 수 없는 항목: {k}"))),
    }
}

async fn patch_account(
    Scope { tn, admin: a, .. }: Scope,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Res {
    let Value::Object(m) = body else {
        return Err(bad("JSON 객체가 필요합니다"));
    };
    let mut done = Vec::new();
    for (k, v) in &m {
        done.push(apply_account(&tn, &id, k, v).await?);
    }
    let label = account_label(&tn, &id);
    store::audit(
        &tn.db(),
        &a.actor(),
        "account_update",
        &format!("{label}: {}", done.join(", ")),
    );
    tn.contact_changed(&id);
    Ok(Json(json!({ "ok": true })))
}

fn remove_account(tn: &Tenant, id: &str) -> Result<bool, ApiErr> {
    kick_account(tn, id, "관리자가 계정을 삭제했습니다");
    let db = tn.db();
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
    tn.hub.broadcast(
        &json!({ "t": "event", "e": "contact", "contact": { "account": id, "removed": true } }),
    );
    Ok(true)
}

async fn delete_account(Scope { tn, admin: a, .. }: Scope, Path(id): Path<String>) -> Res {
    let label = account_label(&tn, &id);
    if !remove_account(&tn, &id)? {
        return Err(ApiErr(StatusCode::NOT_FOUND, "계정이 없습니다".into()));
    }
    store::audit(&tn.db(), &a.actor(), "account_delete", &label);
    janitor::sweep_tenant(&tn, now_ms());
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Bulk {
    ids: Vec<String>,
    action: String,
    /// "move": the tenancy to move unmigrated accounts to.
    #[serde(default)]
    tenancy: String,
}

async fn bulk_accounts(Scope { server, tn, admin: a }: Scope, Json(b): Json<Bulk>) -> Res {
    if b.action == "move" {
        return move_accounts(&server, &tn, &a, &b);
    }
    let mut n = 0;
    for id in &b.ids {
        let label = account_label(&tn, id);
        let ok = match b.action.as_str() {
            "approve" | "enable" => apply_account(&tn, id, "state", &json!("active"))
                .await
                .is_ok(),
            "disable" => apply_account(&tn, id, "state", &json!("disabled"))
                .await
                .is_ok(),
            "delete" => remove_account(&tn, id)?,
            _ => return Err(bad("알 수 없는 작업")),
        };
        if ok {
            n += 1;
            store::audit(
                &tn.db(),
                &a.actor(),
                &format!("account_{}", b.action),
                &label,
            );
            if b.action != "delete" {
                tn.contact_changed(id);
            }
        }
    }
    if b.action == "delete" {
        janitor::sweep_tenant(&tn, now_ms());
    }
    Ok(Json(json!({ "count": n })))
}

/// Moves accounts of the unmigrated tenancy to another one (server admins only).
/// Accounts that cannot move (name or device already there) are reported, not fatal.
fn move_accounts(server: &Server, tn: &Tenant, a: &Admin, b: &Bulk) -> Res {
    if !a.is_server() {
        return Err(forbidden("서버 총괄 관리자만 사용자를 다른 Tenancy로 옮길 수 있습니다"));
    }
    if tn.id != UNMIGRATED {
        return Err(bad("Unmigrated Tenancy의 사용자만 옮길 수 있습니다"));
    }
    let to = b.tenancy.trim().to_lowercase();
    let mut moved = 0;
    let mut failed = Vec::new();
    for id in &b.ids {
        match server.move_account(id, &to) {
            Ok(label) => {
                moved += 1;
                store::audit_server(&server.db(), &a.actor(), "account_move", &format!("{label} → {to}"), Some(&to));
            }
            Err(e) => failed.push(json!({ "id": id, "error": e })),
        }
    }
    Ok(Json(json!({ "count": moved, "failed": failed })))
}

/// `Err` when the tenancy already has as many accounts as its limit allows.
pub fn room_for_account(tn: &Tenant) -> Result<(), String> {
    let max = tn.limits().max_accounts;
    if max > 0 && tn.account_count() >= max as i64 {
        return Err(format!("이 Tenancy의 사용자 수 한도({max}명)에 도달했습니다"));
    }
    Ok(())
}

// ---------------------------------------------------------------- devices

async fn devices(Scope { tn, .. }: Scope, Query(q): Query<ListQ>) -> Res {
    const MAX: i64 = 500;
    let online = tn.hub.online();
    let text = q.query();
    let pat = store::like(&text);
    let filter = q.filter();
    let only_online: Vec<&String> = if filter == "online" { online.iter().collect() } else { vec![] };
    let online_json = serde_json::to_string(&only_online).unwrap_or_else(|_| "[]".into());
    let account = q.account.clone().unwrap_or_default();
    let cond = "(?1 = '' OR d.account = ?1)
        AND (?2 NOT IN ('online', 'disabled', 'waiting')
             OR (?2 = 'online' AND d.id IN (SELECT value FROM json_each(?3)))
             OR (?2 = 'disabled' AND d.disabled != 0)
             OR (?2 = 'waiting' AND EXISTS (SELECT 1 FROM deliveries x WHERE x.device = d.id AND x.acked IS NULL)))
        AND (?4 = '' OR json_extract(d.profile, '$.name') LIKE ?5 ESCAPE '\\' OR a.username LIKE ?5 ESCAPE '\\'
             OR a.name LIKE ?5 ESCAPE '\\' OR d.id LIKE ?5 ESCAPE '\\' OR d.account LIKE ?5 ESCAPE '\\'
             OR d.last_ip LIKE ?5 ESCAPE '\\' OR json_extract(d.profile, '$.host') LIKE ?5 ESCAPE '\\')";
    let order = q.order(
        &[
            ("name", "json_extract(d.profile, '$.name')"), ("account", "COALESCE(a.username, a.name)"),
            ("account_state", "a.state"), ("status", "json_extract(d.profile, '$.status')"),
            ("host", "json_extract(d.profile, '$.host')"), ("id", "d.id"), ("app", "d.app"), ("last_ip", "d.last_ip"),
            ("pending", "pend"), ("last_seen", "d.last_seen"), ("created", "d.created"),
        ],
        "d.last_seen DESC",
        "d.rowid DESC",
    );
    let db = tn.db();
    let base = params![account, filter, online_json, text, pat];
    let total: i64 = db
        .query_row(&format!("SELECT COUNT(*) FROM devices d JOIN accounts a ON a.id = d.account WHERE {cond}"), base, |r| r.get(0))
        .map_err(db_err)?;
    let mut st = db
        .prepare(&format!(
            "SELECT d.id, d.account, a.username, a.name, d.profile, d.pubkey, d.disabled, d.app, d.last_ip, d.created, d.last_seen,
               (SELECT COUNT(*) FROM deliveries x WHERE x.device = d.id AND x.acked IS NULL) AS pend, a.state
             FROM devices d JOIN accounts a ON a.id = d.account WHERE {cond} {order} LIMIT ?6 OFFSET ?7"
        ))
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map(params![account, filter, online_json, text, pat, q.per(MAX), q.offset(MAX)], |r| {
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
    drop(st);
    let one = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    let counts = json!({
        "all": one("SELECT COUNT(*) FROM devices"),
        "online": online.len(),
        "disabled": one("SELECT COUNT(*) FROM devices WHERE disabled != 0"),
        "waiting": one("SELECT COUNT(DISTINCT device) FROM deliveries WHERE acked IS NULL"),
    });
    Ok(page_json(rows, total, &q, MAX, json!({ "counts": counts })))
}

fn device_account(tn: &Tenant, id: &str) -> Result<String, ApiErr> {
    tn
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
    Scope { tn, admin: a, .. }: Scope,
    Path(id): Path<String>,
    Json(p): Json<DevicePatch>,
) -> Res {
    let account = device_account(&tn, &id)?;
    tn
        .db()
        .execute(
            "UPDATE devices SET disabled = ?1 WHERE id = ?2",
            params![p.disabled as i64, id],
        )
        .map_err(db_err)?;
    if p.disabled {
        tn.hub.kick(&id, "관리자가 기기를 정지했습니다");
    }
    store::audit(
        &tn.db(),
        &a.actor(),
        if p.disabled {
            "device_disable"
        } else {
            "device_enable"
        },
        &id,
    );
    tn.contact_changed(&account);
    Ok(Json(json!({ "ok": true })))
}

async fn delete_device(Scope { tn, admin: a, .. }: Scope, Path(id): Path<String>) -> Res {
    let account = device_account(&tn, &id)?;
    // An API key's device goes with the key.
    if let Some(key) = id.strip_prefix("api-") {
        if crate::api::revoke(&tn, key, None, &a.actor()).is_ok() {
            return Ok(Json(json!({ "ok": true })));
        }
    }
    tn.hub.kick(&id, "관리자가 기기 등록을 해제했습니다");
    {
        let db = tn.db();
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
    tn.contact_changed(&account);
    janitor::sweep_tenant(&tn, now_ms());
    Ok(Json(json!({ "ok": true })))
}

async fn kick_device(Scope { tn, admin: a, .. }: Scope, Path(id): Path<String>) -> Res {
    let ok = tn.hub.kick(&id, "관리자가 연결을 끊었습니다");
    if ok {
        store::audit(&tn.db(), &a.actor(), "device_kick", &id);
    }
    Ok(Json(json!({ "ok": ok })))
}

async fn connections(Scope { tn, .. }: Scope, Query(q): Query<ListQ>) -> Res {
    const MAX: i64 = 500;
    let mut conns = tn.hub.snapshot();
    let all = conns.len();
    let plain = conns.iter().filter(|c| !c.tls).count();
    let text = q.query().to_lowercase();
    let db = tn.db();
    if !text.is_empty() {
        // Names live in the database; ids, IPs and versions in memory.
        let ids: Vec<&String> = conns.iter().map(|c| &c.device).collect();
        let by_name: std::collections::HashSet<String> = db
            .prepare(
                "SELECT d.id FROM devices d JOIN accounts a ON a.id = d.account
                 WHERE d.id IN (SELECT value FROM json_each(?1))
                   AND (json_extract(d.profile, '$.name') LIKE ?2 ESCAPE '\\' OR a.username LIKE ?2 ESCAPE '\\' OR a.name LIKE ?2 ESCAPE '\\')",
            )
            .and_then(|mut st| {
                st.query_map(params![serde_json::to_string(&ids).unwrap_or_default(), store::like(&text)], |r| r.get(0))?
                    .collect()
            })
            .unwrap_or_default();
        conns.retain(|c| {
            by_name.contains(&c.device)
                || format!("{} {} {} {}", c.device, c.account, c.ip, c.app).to_lowercase().contains(&text)
        });
    }
    let asc = q.asc();
    match q.sort.as_deref() {
        Some("ip") => conns.sort_by(|a, b| a.ip.cmp(&b.ip)),
        Some("app") => conns.sort_by(|a, b| a.app.cmp(&b.app)),
        Some("transport") => conns.sort_by(|a, b| a.transport.cmp(b.transport)),
        Some("device") => conns.sort_by(|a, b| a.device.cmp(&b.device)),
        Some("account") => conns.sort_by(|a, b| a.account.cmp(&b.account)),
        // "since" (default: newest first) and "dur" (longest first)
        Some("dur") => conns.sort_by(|a, b| b.since.cmp(&a.since)),
        _ => conns.sort_by(|a, b| a.since.cmp(&b.since)),
    }
    if !asc {
        conns.reverse();
    }
    let total = conns.len() as i64;
    let rows: Vec<Value> = conns
        .into_iter()
        .skip(q.offset(MAX) as usize)
        .take(q.per(MAX) as usize)
        .map(|c| {
            let (name, user, status): (String, Option<String>, String) = db
                .query_row(
                    "SELECT COALESCE(json_extract(d.profile, '$.name'), ''), a.username, COALESCE(json_extract(d.profile, '$.status'), '')
                     FROM devices d JOIN accounts a ON a.id = d.account WHERE d.id = ?1",
                    [&c.device],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap_or_default();
            json!({ "device": c.device, "account": c.account, "ip": c.ip, "tls": c.tls, "transport": c.transport, "app": c.app,
                    "since": c.since, "name": name, "username": user, "status": status })
        })
        .collect();
    Ok(page_json(rows, total, &q, MAX, json!({ "all": all, "plain": plain })))
}

// ---------------------------------------------------------------- stored messages

async fn messages(Scope { tn, .. }: Scope, Query(q): Query<ListQ>) -> Res {
    const MAX: i64 = 500;
    let text = q.query();
    let pat = store::like(&text);
    let cond = "(?1 = '' OR m.from_name LIKE ?2 ESCAPE '\\' OR m.mid LIKE ?2 ESCAPE '\\' OR m.from_account LIKE ?2 ESCAPE '\\'
                 OR EXISTS (SELECT 1 FROM deliveries d JOIN accounts a ON a.id = d.account
                            WHERE d.mid = m.mid AND (a.username LIKE ?2 ESCAPE '\\' OR a.name LIKE ?2 ESCAPE '\\')))";
    let order = q.order(
        &[
            ("created", "m.created"), ("from_name", "m.from_name"), ("pending", "pend"), ("files", "json_array_length(m.files)"),
            ("bytes", "m.bytes"), ("enc", "json_extract(m.body, '$.enc')"), ("files_expires", "m.files_expires"),
            ("text_expires", "m.text_expires"), ("mid", "m.mid"),
        ],
        "m.created DESC",
        "m.rowid DESC",
    );
    let db = tn.db();
    let (total, bytes, pending): (i64, i64, i64) = db
        .query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(CASE WHEN m.files_gone = 0 THEN m.bytes END), 0),
                   COALESCE(SUM((SELECT COUNT(*) FROM deliveries d WHERE d.mid = m.mid AND d.acked IS NULL)), 0)
                 FROM messages m WHERE {cond}"
            ),
            params![text, pat],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(db_err)?;
    // Metadata only; the body is shown per message (GET /api/messages/{mid}), which is audited.
    let mut st = db
        .prepare(&format!(
            "SELECT m.mid, m.from_account, m.from_name, m.bytes, m.files, m.created, m.text_expires, m.files_expires, m.files_gone,
               COALESCE(json_extract(m.body, '$.enc'), 'none'),
               (SELECT COUNT(*) FROM deliveries d WHERE d.mid = m.mid AND d.acked IS NULL) AS pend,
               (SELECT COUNT(*) FROM deliveries d WHERE d.mid = m.mid),
               (SELECT COUNT(DISTINCT account) FROM deliveries d WHERE d.mid = m.mid),
               (SELECT group_concat(DISTINCT COALESCE(a.username, a.name)) FROM deliveries d JOIN accounts a ON a.id = d.account WHERE d.mid = m.mid)
             FROM messages m WHERE {cond} {order} LIMIT ?3 OFFSET ?4"
        ))
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map(params![text, pat, q.per(MAX), q.offset(MAX)], |r| {
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
    Ok(page_json(rows, total, &q, MAX, json!({ "bytes": bytes, "pending": pending })))
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
async fn message(Scope { tn, admin: a, .. }: Scope, Path(mid): Path<String>) -> Res {
    hex_id(&mid)?;
    let db = tn.db();
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
    let blobs = store::blob_dir(&tn.dir, &mid);
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
    Scope { tn, admin: a, .. }: Scope,
    Path((mid, idx)): Path<(String, usize)>,
) -> Result<Response, ApiErr> {
    hex_id(&mid)?;
    let (body, gone): (String, bool) = tn
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
    let resp = download(store::blob_dir(&tn.dir, &mid).join(idx.to_string()), &name).await?;
    store::audit(&tn.db(), &a.actor(), "message_file", &format!("{mid} #{idx} {name}"));
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
    page: Option<i64>,
    per: Option<i64>,
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
async fn archive_list(Scope { tn, admin: a, .. }: Scope, Query(q): Query<ArchiveQ>) -> Res {
    let query: String = q.q.unwrap_or_default().trim().chars().take(200).collect();
    let pat = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let (from, to) = (q.from.unwrap_or(0), q.to.unwrap_or(i64::MAX));
    let per = q.per.unwrap_or(100).clamp(10, 20_000);
    let page = q.page.unwrap_or(1).max(1);
    let filter = "created >= ?1 AND created < ?2 AND (?3 = '' OR text LIKE ?4 ESCAPE '\\' OR from_name LIKE ?4 ESCAPE '\\'
                  OR recipients LIKE ?4 ESCAPE '\\' OR names LIKE ?4 ESCAPE '\\')";
    let db = tn.db();
    let total: i64 = db
        .query_row(&format!("SELECT COUNT(*) FROM archive WHERE {filter}"), params![from, to, query, pat], |r| r.get(0))
        .map_err(db_err)?;
    let mut st = db
        .prepare(&format!("SELECT {ARCHIVE_COLS} FROM archive WHERE {filter} ORDER BY created DESC LIMIT ?5 OFFSET ?6"))
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map(params![from, to, query, pat, per, (page - 1) * per], |r| archive_row(r, false))
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    drop(st);
    let all: i64 = db.query_row("SELECT COUNT(*) FROM archive", [], |r| r.get(0)).unwrap_or(0);
    let range = |t: i64, none: bool| if none { "-".to_string() } else { t.to_string() };
    let detail = format!(
        "query={query:?} from={} to={} page={page} -> {} rows",
        range(from, from == 0),
        range(to, to == i64::MAX),
        rows.len()
    );
    store::audit(&db, &a.actor(), "archive_list", &detail);
    Ok(Json(json!({ "items": rows, "total": total, "all": all, "page": page, "per": per })))
}

async fn archive_get(Scope { tn, admin: a, .. }: Scope, Path(mid): Path<String>) -> Res {
    hex_id(&mid)?;
    let db = tn.db();
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
    Scope { tn, admin: a, .. }: Scope,
    Path((mid, idx)): Path<(String, usize)>,
) -> Result<Response, ApiErr> {
    hex_id(&mid)?;
    let (names, kept): (String, bool) = tn
        .db()
        .query_row("SELECT names, files_kept != 0 FROM archive WHERE mid = ?1", [&mid], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|_| not_found("쪽지"))?;
    if !kept {
        return Err(not_found("보관된 첨부파일"));
    }
    let name = file_name(&serde_json::from_str(&names).unwrap_or(json!([])), idx);
    let resp = download(store::archive_dir(&tn.dir, &mid).join(idx.to_string()), &name).await?;
    store::audit(&tn.db(), &a.actor(), "archive_file", &format!("{mid} #{idx} {name}"));
    Ok(resp)
}

async fn archive_delete(Scope { tn, admin: a, .. }: Scope, Path(mid): Path<String>) -> Res {
    hex_id(&mid)?;
    let db = tn.db();
    let n = db.execute("DELETE FROM archive WHERE mid = ?1", [&mid]).map_err(db_err)?;
    if n == 0 {
        return Err(not_found("쪽지"));
    }
    let _ = std::fs::remove_dir_all(store::archive_dir(&tn.dir, &mid));
    store::audit(&db, &a.actor(), "archive_delete", &mid);
    Ok(Json(json!({ "ok": true })))
}

async fn delete_message(Scope { tn, admin: a, .. }: Scope, Path(mid): Path<String>) -> Res {
    if !mid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad("잘못된 ID"));
    }
    let db = tn.db();
    store::drop_message(&db, &tn.dir, &mid);
    store::audit(&db, &a.actor(), "message_purge", &mid);
    Ok(Json(json!({ "ok": true })))
}

async fn sweep(Scope { tn, admin: a, .. }: Scope) -> Res {
    let r = janitor::sweep_tenant(&tn, now_ms());
    store::audit(&tn.db(), &a.actor(), "sweep", &format!("{r:?}"));
    Ok(Json(
        json!({ "files": r.files, "messages": r.messages, "notices": r.notices }),
    ))
}

// ---------------------------------------------------------------- audit / admins

/// Audit log page of a tenancy, or with none selected (server admins) the server's
/// own: tenancy and server admin actions. `per` goes up to 20000 for CSV export.
async fn audit(OptScope { server, tn, admin: _ }: OptScope, Query(q): Query<ListQ>) -> Res {
    match &tn {
        Some(t) => audit_page(&t.db(), &q),
        None => audit_page(&server.db(), &q),
    }
}

fn audit_page(db: &Connection, q: &ListQ) -> Res {
    const MAX: i64 = 20_000;
    let text = q.query();
    let pat = store::like(&text);
    let action = q.filter();
    let cond = "(?1 = '' OR action = ?1) AND (?2 = '' OR actor LIKE ?3 ESCAPE '\\' OR detail LIKE ?3 ESCAPE '\\')";
    let total: i64 = db
        .query_row(&format!("SELECT COUNT(*) FROM audit WHERE {cond}"), params![action, text, pat], |r| r.get(0))
        .map_err(db_err)?;
    let mut st = db
        .prepare(&format!("SELECT ts, actor, action, detail, account FROM audit WHERE {cond} ORDER BY id DESC LIMIT ?4 OFFSET ?5"))
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map(params![action, text, pat, q.per(MAX), q.offset(MAX)], |r| {
            Ok(json!({ "ts": r.get::<_, i64>(0)?, "actor": r.get::<_, String>(1)?, "action": r.get::<_, String>(2)?,
                       "detail": r.get::<_, String>(3)?, "account": r.get::<_, Option<String>>(4)? }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    drop(st);
    let actions: Vec<String> = db
        .prepare("SELECT action FROM audit GROUP BY action ORDER BY action")
        .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect())
        .unwrap_or_default();
    Ok(page_json(rows, total, q, MAX, json!({ "actions": actions })))
}

/// Admins of the selected tenancy, or with none selected the server admins.
async fn admins(OptScope { server, tn, admin: a }: OptScope) -> Res {
    let scope = tn.as_ref().map(|t| t.id.clone()).unwrap_or_default();
    let db = server.db();
    let mut st = db
        .prepare("SELECT id, username, created, last_login FROM admins WHERE tenancy = ?1 ORDER BY id")
        .map_err(db_err)?;
    let rows: Vec<Value> = st
        .query_map([&scope], |r| {
            let id: i64 = r.get(0)?;
            Ok(json!({ "id": id, "username": r.get::<_, String>(1)?, "created": r.get::<_, i64>(2)?,
                       "last_login": r.get::<_, Option<i64>>(3)?, "me": id == a.id }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "admins": rows, "tenancy": tn.map(|t| tenancy_json(&t)) })))
}

async fn create_admin(OptScope { server, tn, admin: a }: OptScope, Json(c): Json<NewAdmin>) -> Res {
    let c = Cred { tenancy: String::new(), username: c.username, password: c.password };
    check_admin_cred(tn.as_deref(), &c)?;
    let hash = hash_async(c.password.clone()).await?;
    let scope = tn.as_ref().map(|t| t.id.clone()).unwrap_or_default();
    server
        .db()
        .execute(
            "INSERT INTO admins (tenancy, username, pass, created) VALUES (?1, ?2, ?3, ?4)",
            params![scope, c.username.trim(), hash, now_ms()],
        )
        .map_err(|_| bad("이미 있는 관리자 이름입니다"))?;
    audit_in(&server, tn.as_deref(), &a.actor(), "admin_create", c.username.trim());
    Ok(Json(json!({ "ok": true })))
}

async fn delete_admin(OptScope { server, tn, admin: a }: OptScope, Path(id): Path<i64>) -> Res {
    if id == a.id {
        return Err(bad("자기 자신은 삭제할 수 없습니다"));
    }
    let scope = tn.as_ref().map(|t| t.id.clone()).unwrap_or_default();
    let name: String = {
        let db = server.db();
        let name: String = db
            .query_row("SELECT username FROM admins WHERE id = ?1 AND tenancy = ?2", params![id, scope], |r| {
                r.get(0)
            })
            .map_err(|_| ApiErr(StatusCode::NOT_FOUND, "관리자가 없습니다".into()))?;
        db.execute("DELETE FROM admins WHERE id = ?1", [id])
            .map_err(db_err)?;
        name
    };
    audit_in(&server, tn.as_deref(), &a.actor(), "admin_delete", &name);
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
    let tn = a.tenancy.as_deref().and_then(|t| server.tenant(t));
    check_admin_cred(
        tn.as_deref(),
        &Cred {
            tenancy: String::new(),
            username: a.username.clone(),
            password: p.new.clone(),
        },
    )?;
    let hash = hash_async(p.new.clone()).await?;
    server
        .db()
        .execute(
            "UPDATE admins SET pass = ?1 WHERE id = ?2",
            params![hash, a.id],
        )
        .map_err(db_err)?;
    audit_in(&server, tn.as_deref(), &a.actor(), "admin_password", "");
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- departments

async fn departments(Scope { tn, .. }: Scope) -> Res {
    let db = tn.db();
    let list: Vec<Value> = db
        .prepare(
            "SELECT d.id, d.name, d.note, d.created, (SELECT COUNT(*) FROM members m WHERE m.department = d.id)
             FROM departments d ORDER BY d.name, d.id",
        )
        .and_then(|mut st| {
            st.query_map([], |r| {
                Ok(json!({ "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "note": r.get::<_, String>(2)?,
                           "created": r.get::<_, i64>(3)?, "members": r.get::<_, i64>(4)? }))
            })?
            .collect()
        })
        .map_err(db_err)?;
    Ok(Json(json!({ "items": list })))
}

#[derive(Deserialize)]
struct NewDepartment {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    note: String,
}

fn dep_name(name: &str, id: &str) -> String {
    let n: String = name.trim().chars().take(80).collect();
    if n.is_empty() {
        id.to_string()
    } else {
        n
    }
}

async fn create_department(Scope { tn, admin: a, .. }: Scope, Json(n): Json<NewDepartment>) -> Res {
    let id = n.id.trim().to_lowercase();
    if !store::valid_department(&id) {
        return Err(bad("부서 ID는 1~32자의 영문 소문자, 숫자, . _ - 만 쓸 수 있습니다"));
    }
    let db = tn.db();
    db.execute(
        "INSERT INTO departments (id, name, note, created) VALUES (?1, ?2, ?3, ?4)",
        params![id, dep_name(&n.name, &id), n.note.chars().take(500).collect::<String>(), now_ms()],
    )
    .map_err(|_| bad("이미 있는 부서 ID입니다"))?;
    store::audit(&db, &a.actor(), "department_create", &id);
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct DepartmentPatch {
    /// A new ID (members follow).
    id: Option<String>,
    name: Option<String>,
    note: Option<String>,
}

/// Accounts in a department, to refresh their contact entries after a change.
fn members_of(tn: &Tenant, dep: &str) -> Vec<String> {
    tn.db()
        .prepare("SELECT account FROM members WHERE department = ?1")
        .and_then(|mut st| st.query_map([dep], |r| r.get(0))?.collect())
        .unwrap_or_default()
}

fn department_exists(tn: &Tenant, id: &str) -> Result<(), ApiErr> {
    let known = tn
        .db()
        .query_row("SELECT 1 FROM departments WHERE id = ?1", [id], |_| Ok(()))
        .optional()
        .map_err(db_err)?
        .is_some();
    if known {
        Ok(())
    } else {
        Err(ApiErr(StatusCode::NOT_FOUND, "부서가 없습니다".into()))
    }
}

async fn patch_department(
    Scope { tn, admin: a, .. }: Scope,
    Path(id): Path<String>,
    Json(p): Json<DepartmentPatch>,
) -> Res {
    department_exists(&tn, &id)?;
    let mut done = Vec::new();
    let mut cur = id.clone();
    {
        let db = tn.db();
        if let Some(n) = &p.name {
            db.execute("UPDATE departments SET name = ?1 WHERE id = ?2", params![dep_name(n, &id), id])
                .map_err(db_err)?;
            done.push(format!("name={}", dep_name(n, &id)));
        }
        if let Some(n) = &p.note {
            db.execute(
                "UPDATE departments SET note = ?1 WHERE id = ?2",
                params![n.chars().take(500).collect::<String>(), id],
            )
            .map_err(db_err)?;
            done.push("note".into());
        }
        if let Some(new) = p.id.as_ref().map(|s| s.trim().to_lowercase()).filter(|s| *s != id) {
            if !store::valid_department(&new) {
                return Err(bad("부서 ID는 1~32자의 영문 소문자, 숫자, . _ - 만 쓸 수 있습니다"));
            }
            db.execute("UPDATE departments SET id = ?1 WHERE id = ?2", params![new, id])
                .map_err(|_| bad("이미 있는 부서 ID입니다"))?;
            done.push(format!("id={new}"));
            cur = new;
        }
        store::audit(&db, &a.actor(), "department_update", &format!("{id}: {}", done.join(", ")));
    }
    if cur != id {
        for acc in members_of(&tn, &cur) {
            tn.contact_changed(&acc);
        }
    }
    Ok(Json(json!({ "id": cur })))
}

async fn delete_department(Scope { tn, admin: a, .. }: Scope, Path(id): Path<String>) -> Res {
    department_exists(&tn, &id)?;
    let members = members_of(&tn, &id);
    tn.db()
        .execute("DELETE FROM departments WHERE id = ?1", [&id])
        .map_err(db_err)?;
    store::audit(&tn.db(), &a.actor(), "department_delete", &id);
    for acc in members {
        tn.contact_changed(&acc);
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct MembersPatch {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

async fn department_members(
    Scope { tn, admin: a, .. }: Scope,
    Path(id): Path<String>,
    Json(p): Json<MembersPatch>,
) -> Res {
    department_exists(&tn, &id)?;
    {
        let db = tn.db();
        for acc in &p.add {
            db.execute(
                "INSERT OR IGNORE INTO members (department, account) SELECT ?1, id FROM accounts WHERE id = ?2",
                params![id, acc],
            )
            .map_err(db_err)?;
        }
        for acc in &p.remove {
            db.execute("DELETE FROM members WHERE department = ?1 AND account = ?2", params![id, acc])
                .map_err(db_err)?;
        }
        store::audit(
            &db,
            &a.actor(),
            "department_members",
            &format!("{id}: +{} -{}", p.add.len(), p.remove.len()),
        );
    }
    for acc in p.add.iter().chain(&p.remove) {
        tn.contact_changed(acc);
    }
    Ok(Json(json!({ "members": members_of(&tn, &id).len() })))
}

// ---------------------------------------------------------------- API keys

#[derive(Deserialize)]
struct KeysQ {
    account: Option<String>,
}

async fn api_keys(Scope { tn, .. }: Scope, Query(q): Query<KeysQ>) -> Res {
    Ok(Json(json!({
        "allowed": tn.tenant_settings().api_keys,
        "items": crate::api::list(&tn, q.account.as_deref().filter(|a| !a.is_empty())),
    })))
}

#[derive(Deserialize)]
struct NewKey {
    account: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    send: bool,
    #[serde(default)]
    receive: bool,
}

/// Issues a key for an account; the token is in the reply only.
async fn create_api_key(Scope { tn, admin: a, .. }: Scope, Json(k): Json<NewKey>) -> Res {
    let (id, token) =
        crate::api::create(&tn, &k.account, &k.name, k.send, k.receive, &a.actor()).map_err(bad)?;
    Ok(Json(json!({ "id": id, "token": token })))
}

async fn revoke_api_key(Scope { tn, admin: a, .. }: Scope, Path(id): Path<String>) -> Res {
    crate::api::revoke(&tn, &id, None, &a.actor()).map_err(|e| ApiErr(StatusCode::NOT_FOUND, e))?;
    Ok(Json(json!({ "ok": true })))
}
