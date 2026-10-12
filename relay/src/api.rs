//! API keys and the HTTP API for scripts (`/api/v1/...` on the relay port).
//!
//! A key acts as its account: `Authorization: Bearer pm.<tenancy>.<id>.<secret>`.
//! A key that may send sends as the account (from device `api-<id>`); a key that
//! may receive has its own device `api-<id>`, so messages to the account wait for
//! it like for any other device until it acknowledges them. Keys only work while
//! the tenancy's policy allows them, and never in an end-to-end encrypted tenancy
//! (scripts send and read plaintext).

use crate::frame;
use crate::portal::{client_ip, ConnInfo};
use crate::relay::{self, Io};
use crate::store::{self, now_ms};
use crate::tenant::Tenant;
use crate::Server;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub struct ApiErr(StatusCode, String, &'static str);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1, "code": self.2 }))).into_response()
    }
}

fn err(status: StatusCode, code: &'static str, msg: impl Into<String>) -> ApiErr {
    ApiErr(status, msg.into(), code)
}

type Res = Result<Json<Value>, ApiErr>;

// ---------------------------------------------------------------- keys

/// Creates a key for `account`; the token is shown once and only its hash kept.
pub fn create(tn: &Tenant, account: &str, name: &str, send: bool, receive: bool, by: &str) -> Result<(String, String), String> {
    // Never on in an end-to-end encrypted tenancy (`TenantSettings::normalize`).
    if !tn.tenant_settings().api_keys {
        return Err("이 Tenancy는 API 키를 허용하지 않습니다".into());
    }
    if !send && !receive {
        return Err("보내기나 받기 중 하나는 허용해야 합니다".into());
    }
    let name: String = name.trim().chars().take(64).collect();
    let name = if name.is_empty() { "API".to_string() } else { name };
    let id = store::new_id();
    let token = format!("pm.{}.{id}.{}", tn.id, store::token());
    let now = now_ms();
    let mut c = tn.db();
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let active: bool = tx
        .query_row("SELECT state = 'active' FROM accounts WHERE id = ?1", [account], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?
        .unwrap_or(false);
    if !active {
        return Err("사용할 수 있는 계정이 아닙니다".into());
    }
    tx.execute(
        "INSERT INTO api_keys (id, account, name, hash, send, receive, created, created_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, account, name, store::sha256_hex(token.as_bytes()), send, receive, now, by],
    )
    .map_err(|e| e.to_string())?;
    if receive {
        let profile = json!({ "name": format!("API: {name}"), "host": "", "status": "online", "notify": false,
                              "max_files": 0, "max_file_size": 0 });
        // Nobody knows this secret: the device can only be used through the key.
        tx.execute(
            "INSERT INTO devices (id, account, secret, profile, app, created) VALUES (?1, ?2, ?3, ?4, 'api', ?5)",
            params![device_of(&id), account, store::sha256_hex(store::token().as_bytes()), profile.to_string(), now],
        )
        .map_err(|e| e.to_string())?;
    }
    store::audit_for(&tx, by, "api_key_create", &format!("{id} {name} send={send} receive={receive}"), account);
    tx.commit().map_err(|e| e.to_string())?;
    drop(c);
    tn.contact_changed(account);
    Ok((id, token))
}

pub fn device_of(key: &str) -> String {
    format!("api-{key}")
}

/// Revokes a key (of `account`, if given). Messages waiting for it are dropped.
pub fn revoke(tn: &Tenant, id: &str, account: Option<&str>, by: &str) -> Result<(), String> {
    let owner: String = tn
        .db()
        .query_row("SELECT account FROM api_keys WHERE id = ?1", [id], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?
        .filter(|a: &String| account.is_none_or(|x| x == a))
        .ok_or("API 키가 없습니다")?;
    {
        let c = tn.db();
        let dev = device_of(id);
        let _ = c.execute("DELETE FROM deliveries WHERE device = ?1", [&dev]);
        let _ = c.execute("DELETE FROM expired WHERE device = ?1", [&dev]);
        let _ = c.execute("DELETE FROM devices WHERE id = ?1", [&dev]);
        let _ = c.execute("DELETE FROM api_keys WHERE id = ?1", [id]);
        store::audit_for(&c, by, "api_key_revoke", id, &owner);
    }
    tn.contact_changed(&owner);
    crate::janitor::sweep_tenant(tn, now_ms());
    Ok(())
}

/// Keys of one account, or of the whole tenancy.
pub fn list(tn: &Tenant, account: Option<&str>) -> Vec<Value> {
    let c = tn.db();
    c.prepare(
        "SELECT k.id, k.account, COALESCE(a.username, a.name), k.name, k.send, k.receive, k.created, k.created_by,
                k.last_used, k.last_ip,
                (SELECT COUNT(*) FROM deliveries d WHERE d.device = 'api-' || k.id AND d.acked IS NULL)
         FROM api_keys k JOIN accounts a ON a.id = k.account
         WHERE ?1 = '' OR k.account = ?1 ORDER BY k.created DESC",
    )
    .and_then(|mut st| {
        st.query_map([account.unwrap_or("")], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?, "account": r.get::<_, String>(1)?, "account_name": r.get::<_, String>(2)?,
                "name": r.get::<_, String>(3)?, "send": r.get::<_, bool>(4)?, "receive": r.get::<_, bool>(5)?,
                "created": r.get::<_, i64>(6)?, "created_by": r.get::<_, String>(7)?,
                "last_used": r.get::<_, Option<i64>>(8)?, "last_ip": r.get::<_, String>(9)?,
                "pending": r.get::<_, i64>(10)?,
            }))
        })?
        .collect()
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------- authentication

/// A request authenticated by an API key.
pub struct Key {
    tn: Arc<Tenant>,
    id: String,
    account: String,
    user: String,
    send: bool,
    receive: bool,
}

impl Key {
    fn device(&self) -> String {
        device_of(&self.id)
    }
}

impl FromRequestParts<Arc<Server>> for Key {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> Result<Self, Self::Rejection> {
        let unauth = || err(StatusCode::UNAUTHORIZED, "auth_failed", "API 키가 올바르지 않습니다");
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .ok_or_else(unauth)?
            .to_string();
        let mut it = token.split('.');
        let (Some("pm"), Some(tenancy), Some(id), Some(_), None) = (it.next(), it.next(), it.next(), it.next(), it.next()) else {
            return Err(unauth());
        };
        let tn = server.tenant(tenancy).ok_or_else(unauth)?;
        let ip = parts
            .extensions
            .get::<ConnInfo>()
            .map(|c| client_ip(server, c, &parts.headers))
            .unwrap_or_default();
        let lock = format!("api:{id}");
        if let Err(s) = tn.locked(&lock) {
            return Err(err(StatusCode::TOO_MANY_REQUESTS, "locked", format!("{s}초 뒤에 다시 시도하세요")));
        }
        let row: Option<(String, String, bool, bool, String, String)> = tn
            .db()
            .query_row(
                "SELECT k.account, k.hash, k.send, k.receive, a.state, COALESCE(a.username, a.id)
                 FROM api_keys k JOIN accounts a ON a.id = k.account WHERE k.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()
            .ok()
            .flatten();
        let Some((account, _, send, receive, state, user)) =
            row.filter(|r| r.1 == store::sha256_hex(token.as_bytes()))
        else {
            tn.login_failed(&lock);
            return Err(unauth());
        };
        if state != "active" {
            return Err(err(StatusCode::FORBIDDEN, "disabled", "사용할 수 없는 계정입니다"));
        }
        if !tn.tenant_settings().api_keys {
            return Err(err(StatusCode::FORBIDDEN, "policy", "이 Tenancy는 API 키를 허용하지 않습니다"));
        }
        tn.login_ok(&lock);
        let _ = tn.db().execute(
            "UPDATE api_keys SET last_used = ?1, last_ip = ?2 WHERE id = ?3",
            params![now_ms(), ip, id],
        );
        Ok(Key {
            id: id.to_string(),
            tn,
            account,
            user,
            send,
            receive,
        })
    }
}

// ---------------------------------------------------------------- handlers

pub async fn me(State(server): State<Arc<Server>>, k: Key) -> Res {
    let domain = server.domain();
    Ok(Json(json!({
        "tenancy": k.tn.id, "account": k.account, "username": k.user, "key": k.id,
        "address": crate::address::device_address(&k.user, &k.device(), &k.tn.id, &domain),
        "send": k.send, "receive": k.receive,
    })))
}

#[derive(Deserialize)]
pub struct SendReq {
    /// Usernames or account IDs of this tenancy, or addresses.
    to: Vec<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    files: Vec<FileIn>,
}

#[derive(Deserialize)]
struct FileIn {
    name: String,
    /// Base64.
    data: String,
}

/// Sends through the client protocol's own `send`, so every rule applies alike.
pub async fn send(State(server): State<Arc<Server>>, k: Key, Json(r): Json<SendReq>) -> Res {
    if !k.send {
        return Err(err(StatusCode::FORBIDDEN, "policy", "이 API 키로는 보낼 수 없습니다"));
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut blobs = Vec::new();
    for f in &r.files {
        blobs.push(b64.decode(f.data.trim()).map_err(|_| err(StatusCode::BAD_REQUEST, "bad_request", format!("{}: base64가 아닙니다", f.name)))?);
    }
    let domain = server.domain();
    let to: Vec<String> = r
        .to
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| if t.contains('@') { t.to_string() } else { format!("{t}@{}:{domain}", k.tn.id) })
        .collect();
    let req = json!({
        "t": "send", "rid": "api", "to": to,
        "files": blobs.iter().map(|b| json!({ "size": b.len() })).collect::<Vec<_>>(),
        "body": { "enc": "none", "text": r.text, "names": r.files.iter().map(|f| f.name.clone()).collect::<Vec<_>>() },
    });
    let (mut ours, theirs) = tokio::io::duplex(256 * 1024);
    let (srv, tn, device, account) = (server.clone(), k.tn.clone(), k.device(), k.account.clone());
    let task = tokio::spawn(async move {
        let mut io: Box<dyn Io> = Box::new(theirs);
        relay::send(&srv, &tn, &mut io, &device, &account, &req).await
    });
    let t = Duration::from_secs(60);
    let first = frame::read(&mut ours, t)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "bad_request", e))?
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "bad_request", "no reply"))?;
    if first["t"] == "err" {
        let _ = task.await;
        let code = first["code"].as_str().unwrap_or("bad_request");
        let status = match code {
            "unknown_user" => StatusCode::NOT_FOUND,
            "quota" => StatusCode::INSUFFICIENT_STORAGE,
            "bad_request" => StatusCode::BAD_REQUEST,
            _ => StatusCode::FORBIDDEN,
        };
        return Err(ApiErr(status, first["reason"].as_str().unwrap_or("").to_string(), static_code(code)));
    }
    for b in &blobs {
        ours.write_all(b)
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "bad_request", e.to_string()))?;
    }
    let done = frame::read(&mut ours, t)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "bad_request", e))?
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "bad_request", "no reply"))?;
    let _ = task.await;
    Ok(Json(json!({ "mid": done["mid"], "result": done["result"] })))
}

fn static_code(c: &str) -> &'static str {
    match c {
        "unknown_user" => "unknown_user",
        "quota" => "quota",
        "policy" => "policy",
        "offline" => "offline",
        "keys" => "keys",
        "unsupported" => "unsupported",
        _ => "bad_request",
    }
}

fn need_receive(k: &Key) -> Result<(), ApiErr> {
    if k.receive {
        Ok(())
    } else {
        Err(err(StatusCode::FORBIDDEN, "policy", "이 API 키로는 받을 수 없습니다"))
    }
}

/// One waiting message, with its text (bodies are plaintext in tenancies with API keys).
fn message_json(r: &rusqlite::Row, now: i64) -> rusqlite::Result<Value> {
    let files: String = r.get(5)?;
    let gone = r.get::<_, i64>(6)? != 0 || r.get::<_, i64>(7)? <= now;
    let body: Value = serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or(json!({}));
    let sizes: Vec<u64> = serde_json::from_str(&files).unwrap_or_default();
    let plain = body["enc"].as_str().unwrap_or("none") == "none";
    Ok(json!({
        "mid": r.get::<_, String>(0)?, "from": r.get::<_, String>(1)?, "from_device": r.get::<_, String>(2)?,
        "from_name": r.get::<_, String>(3)?, "from_address": r.get::<_, String>(9)?, "ts": r.get::<_, i64>(4)?,
        "text": if plain { body["text"].clone() } else { Value::Null },
        "encrypted": !plain,
        "group": if plain { body["group"].clone() } else { Value::Null },
        "files": if gone { vec![] } else {
            sizes.iter().enumerate().map(|(i, s)| json!({ "idx": i, "size": s, "name": if plain { body["names"][i].clone() } else { Value::Null } })).collect()
        },
        "files_expired": gone && !sizes.is_empty(),
    }))
}

const COLS: &str = "m.mid, m.from_account, m.from_device, m.from_name, m.created, m.files, m.files_gone, m.files_expires, m.body, m.from_addr";

pub async fn inbox(k: Key) -> Res {
    need_receive(&k)?;
    let now = now_ms();
    let c = k.tn.db();
    let list: Vec<Value> = c
        .prepare(&format!(
            "SELECT {COLS} FROM deliveries d JOIN messages m ON m.mid = d.mid
             WHERE d.device = ?1 AND d.acked IS NULL ORDER BY m.created LIMIT 500"
        ))
        .and_then(|mut st| st.query_map([k.device()], |r| message_json(r, now))?.collect())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "bad_request", e.to_string()))?;
    Ok(Json(json!({ "items": list })))
}

fn waiting(k: &Key, mid: &str) -> Result<Value, ApiErr> {
    let now = now_ms();
    k.tn.db()
        .query_row(
            &format!(
                "SELECT {COLS} FROM deliveries d JOIN messages m ON m.mid = d.mid
                 WHERE d.mid = ?1 AND d.device = ?2 AND d.acked IS NULL"
            ),
            params![mid, k.device()],
            |r| message_json(r, now),
        )
        .optional()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "bad_request", e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "보관함에 없는 쪽지입니다"))
}

pub async fn message(k: Key, Path(mid): Path<String>) -> Res {
    need_receive(&k)?;
    Ok(Json(waiting(&k, &mid)?))
}

pub async fn file(k: Key, Path((mid, idx)): Path<(String, usize)>) -> Result<Response, ApiErr> {
    need_receive(&k)?;
    let m = waiting(&k, &mid)?;
    if m["files"].as_array().is_none_or(|f| idx >= f.len()) {
        return Err(err(StatusCode::NOT_FOUND, "not_found", "첨부파일이 없습니다"));
    }
    let path = store::blob_dir(&k.tn.dir, &mid).join(idx.to_string());
    let f = tokio::fs::File::open(&path)
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, "not_found", "첨부파일이 없습니다"))?;
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(f));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response())
}

pub async fn ack(k: Key, Path(mid): Path<String>) -> Res {
    need_receive(&k)?;
    relay::ack(&k.tn, &k.device(), &mid).map_err(|e| err(StatusCode::NOT_FOUND, "not_found", e.reason))?;
    Ok(Json(json!({ "ok": true })))
}
