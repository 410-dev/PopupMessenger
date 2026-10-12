//! Moving an account from the unmigrated tenancy to a real one.
//!
//! The account keeps its ID, its devices keep their IDs and secrets, and messages
//! still waiting for its devices go along, so its clients carry on as if nothing
//! happened. Clients without tenancy support keep logging on without a tenancy and
//! are routed by the `moved` table; newer ones learn the tenancy from `login`.

use crate::store::{self, now_ms};
use crate::tenant::Tenant;
use crate::Server;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection};
use serde_json::json;

/// Rows of `table` matching `cond` (with `args`), every column.
struct Rows {
    cols: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
}

fn select(c: &Connection, table: &str, cond: &str, args: &[&str]) -> rusqlite::Result<Rows> {
    let mut st = c.prepare(&format!("SELECT * FROM {table} WHERE {cond}"))?;
    let cols: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    let n = cols.len();
    let rows = st
        .query_map(params_from_iter(args.iter()), |r| {
            (0..n).map(|i| r.get::<_, SqlValue>(i)).collect()
        })?
        .collect::<rusqlite::Result<Vec<Vec<SqlValue>>>>()?;
    Ok(Rows { cols, rows })
}

fn insert(c: &Connection, table: &str, r: &Rows) -> rusqlite::Result<()> {
    if r.rows.is_empty() {
        return Ok(());
    }
    let marks = vec!["?"; r.cols.len()].join(", ");
    let sql = format!(
        "INSERT OR IGNORE INTO {table} ({}) VALUES ({marks})",
        r.cols.join(", ")
    );
    let mut st = c.prepare(&sql)?;
    for row in &r.rows {
        st.execute(params_from_iter(row.iter()))?;
    }
    Ok(())
}

/// Everything that moves with one account.
struct Moving {
    label: String,
    username: Option<String>,
    devices: Vec<String>,
    account: Rows,
    device_rows: Rows,
    messages: Rows,
    deliveries: Rows,
    expired: Rows,
    /// Messages whose attachments are still stored.
    blobs: Vec<String>,
}

fn collect(c: &Connection, account: &str) -> Result<Moving, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let (username, name): (Option<String>, String) = c
        .query_row("SELECT username, name FROM accounts WHERE id = ?1", [account], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|_| "계정이 없습니다".to_string())?;
    let label = format!("{} ({account})", username.clone().unwrap_or(name));
    let device_rows = select(c, "devices", "account = ?1", &[account]).map_err(e)?;
    let devices: Vec<String> = c
        .prepare("SELECT id FROM devices WHERE account = ?1")
        .and_then(|mut st| st.query_map([account], |r| r.get(0))?.collect())
        .map_err(e)?;
    let waiting = "mid IN (SELECT mid FROM deliveries WHERE account = ?1 AND acked IS NULL)";
    let messages = select(c, "messages", waiting, &[account]).map_err(e)?;
    let deliveries = select(c, "deliveries", "account = ?1 AND acked IS NULL", &[account]).map_err(e)?;
    let expired = select(
        c,
        "expired",
        "device IN (SELECT id FROM devices WHERE account = ?1)",
        &[account],
    )
    .map_err(e)?;
    let blobs: Vec<String> = c
        .prepare(&format!("SELECT mid FROM messages WHERE {waiting} AND files_gone = 0 AND files != '[]'"))
        .and_then(|mut st| st.query_map([account], |r| r.get(0))?.collect())
        .map_err(e)?;
    Ok(Moving {
        label,
        username,
        devices,
        account: select(c, "accounts", "id = ?1", &[account]).map_err(e)?,
        device_rows,
        messages,
        deliveries,
        expired,
        blobs,
    })
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let (a, b) = (e.path(), to.join(e.file_name()));
        if std::fs::hard_link(&a, &b).is_err() {
            std::fs::copy(&a, &b)?;
        }
    }
    Ok(())
}

pub fn move_account(server: &Server, src: &Tenant, dst: &Tenant, account: &str) -> Result<String, String> {
    let m = collect(&src.db(), account)?;
    {
        let c = dst.db();
        if let Some(u) = &m.username {
            let taken: bool = c
                .query_row("SELECT 1 FROM accounts WHERE username = ?1", [u], |_| Ok(true))
                .unwrap_or(false);
            if taken {
                return Err(format!("{} Tenancy에 이미 '{u}' 사용자가 있습니다", dst.id));
            }
        }
        for d in &m.devices {
            let taken: bool = c
                .query_row("SELECT 1 FROM devices WHERE id = ?1", [d], |_| Ok(true))
                .unwrap_or(false);
            if taken {
                return Err(format!("{} Tenancy에 이미 등록된 기기가 있습니다: {d}", dst.id));
            }
        }
        let taken: bool = c
            .query_row("SELECT 1 FROM accounts WHERE id = ?1", [account], |_| Ok(true))
            .unwrap_or(false);
        if taken {
            return Err(format!("{} Tenancy에 같은 계정 ID가 있습니다", dst.id));
        }
        let max = dst.limits().max_accounts;
        if max > 0 && dst.account_count() >= max as i64 {
            return Err(format!("{} Tenancy의 사용자 수 한도({max}명)에 도달했습니다", dst.id));
        }
    }

    // Attachments first: a message row must never point at missing files.
    for mid in &m.blobs {
        let from = store::blob_dir(&src.dir, mid);
        if from.exists() {
            copy_dir(&from, &store::blob_dir(&dst.dir, mid)).map_err(|e| format!("첨부파일 복사 실패: {e}"))?;
        }
    }
    src.hub
        .kick_where("계정이 다른 Tenancy로 옮겨졌습니다. 다시 로그온하세요", |_, c| c.account == account);
    {
        let mut c = dst.db();
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let e = |e: rusqlite::Error| e.to_string();
        insert(&tx, "accounts", &m.account).map_err(e)?;
        tx.execute("UPDATE accounts SET updated = ?1 WHERE id = ?2", params![now_ms(), account])
            .map_err(e)?;
        insert(&tx, "devices", &m.device_rows).map_err(e)?;
        insert(&tx, "messages", &m.messages).map_err(e)?;
        insert(&tx, "deliveries", &m.deliveries).map_err(e)?;
        insert(&tx, "expired", &m.expired).map_err(e)?;
        tx.execute("DELETE FROM removed WHERE account = ?1", [account]).map_err(e)?;
        store::audit_for(&tx, "server", "account_moved_in", &format!("{} from {}", m.label, src.id), account);
        tx.commit().map_err(e)?;
    }
    {
        let c = server.db();
        let now = now_ms();
        for d in &m.devices {
            let _ = c.execute(
                "INSERT INTO moved (kind, key, tenancy, ts) VALUES ('device', ?1, ?2, ?3)
                 ON CONFLICT(kind, key) DO UPDATE SET tenancy = excluded.tenancy, ts = excluded.ts",
                params![d, dst.id, now],
            );
        }
        if let Some(u) = &m.username {
            let _ = c.execute(
                "INSERT INTO moved (kind, key, tenancy, ts) VALUES ('user', ?1, ?2, ?3)
                 ON CONFLICT(kind, key) DO UPDATE SET tenancy = excluded.tenancy, ts = excluded.ts",
                params![u.to_lowercase(), dst.id, now],
            );
        }
    }
    // Gone from the unmigrated tenancy: its contacts see it removed.
    {
        let c = src.db();
        let _ = c.execute("DELETE FROM deliveries WHERE account = ?1", [account]);
        let _ = c.execute(
            "DELETE FROM expired WHERE device IN (SELECT id FROM devices WHERE account = ?1)",
            [account],
        );
        let _ = c.execute("DELETE FROM accounts WHERE id = ?1", [account]);
        let _ = c.execute("DELETE FROM links WHERE a = ?1 OR b = ?1", [account]);
        let _ = c.execute(
            "INSERT INTO removed (account, ts) VALUES (?1, ?2) ON CONFLICT(account) DO UPDATE SET ts = excluded.ts",
            params![account, now_ms()],
        );
        store::audit_for(&c, "server", "account_moved_out", &format!("{} to {}", m.label, dst.id), account);
    }
    src.hub.broadcast(
        &json!({ "t": "event", "e": "contact", "contact": { "account": account, "removed": true } }),
    );
    crate::janitor::sweep_tenant(src, now_ms());
    Ok(m.label)
}
