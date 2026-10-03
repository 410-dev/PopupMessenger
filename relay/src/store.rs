//! SQLite schema and shared queries.

use base64::Engine;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings::Contacts;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS admins (
  id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE, pass TEXT NOT NULL,
  created INTEGER NOT NULL, last_login INTEGER);
CREATE TABLE IF NOT EXISTS accounts (
  id TEXT PRIMARY KEY, username TEXT UNIQUE COLLATE NOCASE, pass TEXT, name TEXT NOT NULL DEFAULT '',
  state TEXT NOT NULL DEFAULT 'active', max_devices INTEGER, quota_mb INTEGER, note TEXT NOT NULL DEFAULT '',
  created INTEGER NOT NULL, updated INTEGER NOT NULL, last_seen INTEGER);
CREATE TABLE IF NOT EXISTS devices (
  id TEXT PRIMARY KEY, account TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  secret TEXT NOT NULL, pubkey TEXT NOT NULL DEFAULT '', profile TEXT NOT NULL DEFAULT '{}',
  disabled INTEGER NOT NULL DEFAULT 0, app TEXT NOT NULL DEFAULT '', last_ip TEXT NOT NULL DEFAULT '',
  created INTEGER NOT NULL, last_seen INTEGER);
CREATE INDEX IF NOT EXISTS devices_account ON devices(account);
CREATE TABLE IF NOT EXISTS removed (account TEXT PRIMARY KEY, ts INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS links (a TEXT NOT NULL, b TEXT NOT NULL, ts INTEGER NOT NULL, PRIMARY KEY (a, b));
CREATE TABLE IF NOT EXISTS messages (
  mid TEXT PRIMARY KEY, from_account TEXT NOT NULL, from_device TEXT NOT NULL, from_name TEXT NOT NULL,
  body TEXT NOT NULL, files TEXT NOT NULL, bytes INTEGER NOT NULL, created INTEGER NOT NULL,
  text_expires INTEGER NOT NULL, files_expires INTEGER NOT NULL, files_gone INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS deliveries (
  mid TEXT NOT NULL REFERENCES messages(mid) ON DELETE CASCADE, device TEXT NOT NULL, account TEXT NOT NULL,
  acked INTEGER, PRIMARY KEY (mid, device));
CREATE INDEX IF NOT EXISTS deliveries_device ON deliveries(device, acked);
CREATE TABLE IF NOT EXISTS expired (
  device TEXT NOT NULL, from_account TEXT NOT NULL, from_name TEXT NOT NULL, count INTEGER NOT NULL,
  last INTEGER NOT NULL, PRIMARY KEY (device, from_account));
CREATE TABLE IF NOT EXISTS audit (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, actor TEXT NOT NULL,
  action TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '');
CREATE INDEX IF NOT EXISTS audit_ts ON audit(ts);
CREATE TABLE IF NOT EXISTS stats (day TEXT PRIMARY KEY, msgs INTEGER NOT NULL DEFAULT 0,
  bytes INTEGER NOT NULL DEFAULT 0, deliveries INTEGER NOT NULL DEFAULT 0, expired INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS archive (
  mid TEXT PRIMARY KEY, created INTEGER NOT NULL, from_account TEXT NOT NULL, from_device TEXT NOT NULL,
  from_name TEXT NOT NULL, recipients TEXT NOT NULL, text TEXT NOT NULL, names TEXT NOT NULL, sizes TEXT NOT NULL,
  grp TEXT, files_kept INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS archive_created ON archive(created);
CREATE INDEX IF NOT EXISTS accounts_created ON accounts(created);
CREATE INDEX IF NOT EXISTS accounts_state ON accounts(state, created);
CREATE INDEX IF NOT EXISTS devices_last_seen ON devices(last_seen);
CREATE INDEX IF NOT EXISTS deliveries_account ON deliveries(account, acked);
CREATE INDEX IF NOT EXISTS messages_created ON messages(created);
CREATE INDEX IF NOT EXISTS audit_action ON audit(action);
";

/// Which account an audit row is about, derived from its actor
/// ("user:<username>", "device:<id>", "dev:<id>").
const AUDIT_ACCOUNT: &str = "CASE
  WHEN actor LIKE 'user:%' THEN (SELECT id FROM accounts WHERE username = substr(actor, 6))
  WHEN actor LIKE 'device:%' THEN (SELECT account FROM devices WHERE id = substr(actor, 8))
  WHEN actor LIKE 'dev:%' THEN (SELECT account FROM devices WHERE id = substr(actor, 5)) END";

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.execute_batch(SCHEMA)?;
    // Added after the first release: the account an audit row concerns.
    let has_account: bool = c
        .prepare("SELECT 1 FROM pragma_table_info('audit') WHERE name = 'account'")?
        .exists([])?;
    if !has_account {
        c.execute_batch(&format!(
            "ALTER TABLE audit ADD COLUMN account TEXT;
             UPDATE audit SET account = ({AUDIT_ACCOUNT}) WHERE account IS NULL;"
        ))?;
    }
    c.execute_batch("CREATE INDEX IF NOT EXISTS audit_account ON audit(account, id);")?;
    // Added in 1.261004.03: whether messages are stored for a device while it is offline
    // ("relay receive"), per device and for the whole account.
    for table in ["devices", "accounts"] {
        let has: bool = c
            .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = 'hold'"))?
            .exists([])?;
        if !has {
            c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN hold INTEGER NOT NULL DEFAULT 1;"))?;
        }
    }
    Ok(c)
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

pub fn token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// First 8 bytes of SHA-256 of a base64 public key, as shown to users.
pub fn key_fingerprint(pubkey: &str) -> String {
    match base64::engine::general_purpose::STANDARD.decode(pubkey) {
        Ok(b) if !b.is_empty() => hex(&Sha256::digest(&b)[..8]),
        _ => String::new(),
    }
}

pub fn kv_get(c: &Connection, k: &str) -> Option<String> {
    c.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

pub fn kv_set(c: &Connection, k: &str, v: &str) {
    let _ = c.execute(
        "INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        [k, v],
    );
}

/// Records an action. The account it concerns is derived from a user or device actor.
pub fn audit(c: &Connection, actor: &str, action: &str, detail: &str) {
    let sql = format!(
        "INSERT INTO audit (ts, actor, action, detail, account)
         SELECT ?1, ?2, ?3, ?4, {} FROM (SELECT ?2 AS actor)",
        AUDIT_ACCOUNT
    );
    let _ = c.execute(&sql, params![now_ms(), actor, action, detail]);
}

/// Records an action about `account` (admin actions on it, portal actions).
pub fn audit_for(c: &Connection, actor: &str, action: &str, detail: &str, account: &str) {
    let _ = c.execute(
        "INSERT INTO audit (ts, actor, action, detail, account) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now_ms(), actor, action, detail, account],
    );
}

/// `%text%` for LIKE ... ESCAPE '\\'.
pub fn like(text: &str) -> String {
    format!("%{}%", text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

pub fn day() -> String {
    // UTC day; good enough for coarse daily counters.
    let days = now_ms() / 86_400_000;
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day) (Howard Hinnant's algorithm).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

pub fn stat(c: &Connection, col: &str, n: i64) {
    let sql = format!("INSERT INTO stats (day, {col}) VALUES (?1, ?2) ON CONFLICT(day) DO UPDATE SET {col} = {col} + ?2");
    let _ = c.execute(&sql, params![day(), n]);
}

pub fn bump(c: &Connection, account: &str) {
    let _ = c.execute(
        "UPDATE accounts SET updated = ?1 WHERE id = ?2",
        params![now_ms(), account],
    );
}

/// Adds `a` and `b` to each other's contacts. Returns whether the link is new.
pub fn link(c: &Connection, a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    let now = now_ms();
    let n1 = c
        .execute(
            "INSERT OR IGNORE INTO links (a, b, ts) VALUES (?1, ?2, ?3)",
            params![a, b, now],
        )
        .unwrap_or(0);
    let n2 = c
        .execute(
            "INSERT OR IGNORE INTO links (a, b, ts) VALUES (?1, ?2, ?3)",
            params![b, a, now],
        )
        .unwrap_or(0);
    n1 + n2 > 0
}

pub fn can_see(c: &Connection, scope: Contacts, viewer: &str, target: &str) -> bool {
    scope == Contacts::All
        || viewer == target
        || c.query_row(
            "SELECT 1 FROM links WHERE a = ?1 AND b = ?2",
            [viewer, target],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
}

fn rank(s: &str) -> u8 {
    match s {
        "online" => 2,
        "dnd" => 1,
        _ => 0,
    }
}

/// A contact entry (protocol 4.4). Accounts that are not active come back as removed.
pub fn contact(
    c: &Connection,
    online: &HashSet<String>,
    account: &str,
    show_host: bool,
) -> Option<Value> {
    let row: Option<(Option<String>, String, String, Option<i64>, bool, i64)> = c
        .query_row(
            "SELECT username, name, state, last_seen, hold != 0, updated FROM accounts WHERE id = ?1",
            [account],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .ok()
        .flatten();
    let Some((username, name, state, last_seen, acc_hold, updated)) = row else {
        return Some(json!({ "account": account, "removed": true }));
    };
    if state != "active" {
        return Some(json!({ "account": account, "removed": true }));
    }
    let mut st = c
        .prepare_cached("SELECT id, profile, pubkey, last_seen, hold != 0 FROM devices WHERE account = ?1 AND disabled = 0")
        .ok()?;
    let devs: Vec<(String, String, String, Option<i64>, bool)> = st
        .query_map([account], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .ok()?
        .filter_map(Result::ok)
        .collect();
    let mut best = "offline";
    let devices: Vec<Value> = devs
        .into_iter()
        .map(|(id, profile, pubkey, seen, hold)| {
            let p: Value = serde_json::from_str(&profile).unwrap_or(json!({}));
            let wanted = p["status"].as_str().unwrap_or("online");
            let status = if online.contains(&id) {
                wanted
            } else {
                "offline"
            };
            if rank(status) > rank(best) {
                best = if status == "online" { "online" } else { "dnd" };
            }
            json!({
                "id": id,
                "name": p["name"],
                "host": if show_host { p["host"].clone() } else { json!("") },
                "status": status,
                "online": status != "offline",
                "notify": p["notify"],
                "max_files": p["max_files"],
                "max_file_size": p["max_file_size"],
                "pubkey": pubkey,
                "last_seen": seen,
                // Messages are stored for it while it is offline.
                "hold": hold && acc_hold,
            })
        })
        .collect();
    Some(json!({
        "account": account,
        "username": username,
        "name": name,
        "status": best,
        "online": best != "offline",
        "last_seen": last_seen,
        // Bumped on every change: a client keeps the newest entry when a contacts
        // reply and a "contact" event cross.
        "updated": updated,
        "removed": false,
        "devices": devices,
    }))
}

/// Active, non-disabled devices of an account: (id, profile, stores while offline).
pub fn devices_of(c: &Connection, account: &str) -> Vec<(String, Value, bool)> {
    let Ok(mut st) = c.prepare_cached(
        "SELECT d.id, d.profile, d.hold != 0 AND a.hold != 0 FROM devices d JOIN accounts a ON a.id = d.account
         WHERE d.account = ?1 AND d.disabled = 0",
    ) else {
        return vec![];
    };
    st.query_map([account], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?))
    })
    .map(|it| {
        it.filter_map(Result::ok)
            .map(|(id, p, hold)| (id, serde_json::from_str(&p).unwrap_or(json!({})), hold))
            .collect()
    })
    .unwrap_or_default()
}

/// "Relay receive" of a device: "allow", "device_off" (this device only) or "all_off".
pub fn hold_mode(c: &Connection, device: &str) -> &'static str {
    let (dev, acc): (bool, bool) = c
        .query_row(
            "SELECT d.hold != 0, a.hold != 0 FROM devices d JOIN accounts a ON a.id = d.account WHERE d.id = ?1",
            [device],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((true, true));
    if !acc {
        "all_off"
    } else if !dev {
        "device_off"
    } else {
        "allow"
    }
}

pub fn pending_count(c: &Connection, device: &str) -> i64 {
    c.query_row(
        "SELECT COUNT(*) FROM deliveries WHERE device = ?1 AND acked IS NULL",
        [device],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

pub fn expired_count(c: &Connection, device: &str) -> i64 {
    c.query_row(
        "SELECT COALESCE(SUM(count), 0) FROM expired WHERE device = ?1",
        [device],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Deletes a message row (deliveries cascade) and its stored attachments.
pub fn drop_message(c: &Connection, dir: &Path, mid: &str) {
    let _ = c.execute("DELETE FROM messages WHERE mid = ?1", [mid]);
    let _ = std::fs::remove_dir_all(blob_dir(dir, mid));
}

pub fn blob_dir(dir: &Path, mid: &str) -> std::path::PathBuf {
    dir.join("blobs").join(mid)
}

pub fn archive_dir(dir: &Path, mid: &str) -> std::path::PathBuf {
    dir.join("archive").join(mid)
}
