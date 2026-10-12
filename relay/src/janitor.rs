//! Periodic cleanup: attachment and message expiry, finished messages, old audit rows.

use crate::store::{self, now_ms};
use crate::tenant::Tenant;
use crate::Server;
use rusqlite::params;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub async fn run(server: Arc<Server>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        sweep(&server, now_ms());
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Swept {
    pub files: usize,
    pub messages: usize,
    pub notices: usize,
}

/// One cleanup pass over every tenancy as of `now` (ms). Public so tests can trigger it.
pub fn sweep(server: &Server, now: i64) -> Swept {
    let mut out = Swept::default();
    for t in server.tenants() {
        let r = sweep_tenant(&t, now);
        out.files += r.files;
        out.messages += r.messages;
        out.notices += r.notices;
    }
    let audit_days = server.server_settings().audit_days as i64;
    let _ = server.db().execute(
        "DELETE FROM audit WHERE ts < ?1",
        [now - audit_days * 86_400_000],
    );
    server
        .admin_sessions
        .lock()
        .unwrap()
        .retain(|_, s| s.expires > now);
    server.user_sessions.lock().unwrap().retain(|_, s| s.expires > now);
    server
        .lockouts
        .lock()
        .unwrap()
        .retain(|_, l| l.0 > 0 || l.1 > now);
    server.remove_doomed();
    out
}

/// One cleanup pass over one tenancy (also run from its admin UI).
pub fn sweep_tenant(t: &Tenant, now: i64) -> Swept {
    let mut out = Swept::default();
    let (audit_days, archive_days) = {
        let s = t.settings();
        (s.audit_days as i64, s.archive_days as i64)
    };
    let mut notify = Vec::new();
    {
        let c = t.db();

        // Attachments past their retention: delete the blobs, keep the text.
        let mids: Vec<String> = c
            .prepare("SELECT mid FROM messages WHERE files_gone = 0 AND files_expires <= ?1 AND files != '[]'")
            .and_then(|mut s| s.query_map([now], |r| r.get(0))?.collect())
            .unwrap_or_default();
        for mid in &mids {
            let _ = std::fs::remove_dir_all(store::blob_dir(&t.dir, mid));
            let _ = c.execute("UPDATE messages SET files_gone = 1 WHERE mid = ?1", [mid]);
        }
        out.files = mids.len();

        // Messages past their retention: leave a per-sender count for every device
        // that never fetched it, then delete.
        let expired: Vec<(String, String, String, i64)> = c
            .prepare("SELECT mid, from_account, from_name, created FROM messages WHERE text_expires <= ?1")
            .and_then(|mut s| s.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect())
            .unwrap_or_default();
        for (mid, from, name, created) in &expired {
            let devices: Vec<String> = c
                .prepare("SELECT device FROM deliveries WHERE mid = ?1 AND acked IS NULL")
                .and_then(|mut s| s.query_map([mid], |r| r.get(0))?.collect())
                .unwrap_or_default();
            for d in &devices {
                let _ = c.execute(
                    "INSERT INTO expired (device, from_account, from_name, count, last) VALUES (?1, ?2, ?3, 1, ?4)
                     ON CONFLICT(device, from_account) DO UPDATE SET count = count + 1, from_name = excluded.from_name,
                     last = MAX(last, excluded.last)",
                    params![d, from, name, created],
                );
                notify.push(d.clone());
            }
            out.notices += devices.len();
            store::drop_message(&c, &t.dir, mid);
        }
        out.messages = expired.len();
        if out.notices > 0 {
            store::stat(&c, "expired", out.notices as i64);
        }

        // Messages nobody is waiting for any more (devices removed or moved).
        let done: Vec<String> = c
            .prepare("SELECT mid FROM messages m WHERE NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.mid = m.mid AND d.acked IS NULL)")
            .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
            .unwrap_or_default();
        for mid in &done {
            store::drop_message(&c, &t.dir, mid);
        }

        let _ = c.execute(
            "DELETE FROM audit WHERE ts < ?1",
            [now - audit_days * 86_400_000],
        );

        // Audit archive past its retention.
        let old: Vec<String> = c
            .prepare("SELECT mid FROM archive WHERE created < ?1")
            .and_then(|mut s| s.query_map([now - archive_days * 86_400_000], |r| r.get(0))?.collect())
            .unwrap_or_default();
        for mid in &old {
            let _ = c.execute("DELETE FROM archive WHERE mid = ?1", [mid]);
            let _ = std::fs::remove_dir_all(store::archive_dir(&t.dir, mid));
        }
        let _ = c.execute(
            "DELETE FROM removed WHERE ts < ?1",
            [now - 365 * 86_400_000],
        );
    }
    t.lockouts
        .lock()
        .unwrap()
        .retain(|_, l| l.0 > 0 || l.1 > now);

    notify.sort();
    notify.dedup();
    for d in notify {
        if t.hub.is_online(&d) {
            let (n, x) = {
                let c = t.db();
                (store::pending_count(&c, &d), store::expired_count(&c, &d))
            };
            t.hub.send(
                &d,
                json!({ "t": "event", "e": "inbox", "count": n, "expired": x }),
            );
        }
    }
    out
}

/// Removes `.part` files and blob folders without a message row (left by a crash).
pub fn clean_partial(t: &Tenant) {
    let Ok(rd) = std::fs::read_dir(t.dir.join("blobs")) else {
        return;
    };
    let c = t.db();
    for e in rd.flatten() {
        let mid = e.file_name().to_string_lossy().into_owned();
        let known = c
            .query_row("SELECT 1 FROM messages WHERE mid = ?1", [&mid], |_| Ok(()))
            .is_ok();
        if !known {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}
