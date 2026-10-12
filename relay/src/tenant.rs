//! One tenancy (an organization hosted on this server): its own database, policy,
//! connected devices and login lockouts. Tenancies never see each other.

use crate::hub::Hub;
use crate::settings::{self, Encryption, Limits, ServerSettings, Settings, TenantSettings};
use crate::store;
use rusqlite::Connection;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

/// The tenancy that clients without tenancy support (and every account from before
/// tenancies existed) land in. Its database is the server's original `relay.db`.
pub const UNMIGRATED: &str = "unmigrated";

/// Tenancy IDs are what users type into the client: 2-32 of a-z, 0-9 and '-'.
pub fn valid_id(id: &str) -> bool {
    (2..=32).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

pub struct Tenant {
    pub id: String,
    /// Holds `relay.db`, `blobs/` and `archive/`.
    pub dir: PathBuf,
    name: RwLock<String>,
    db: Mutex<Connection>,
    settings: RwLock<TenantSettings>,
    limits: RwLock<Limits>,
    server: Arc<RwLock<ServerSettings>>,
    pub hub: Hub,
    /// Failed-login counters: key -> (failures, locked until ms).
    pub lockouts: Mutex<HashMap<String, (u32, i64)>>,
}

impl Tenant {
    pub fn open(
        id: &str,
        name: &str,
        dir: PathBuf,
        limits: Limits,
        server: Arc<RwLock<ServerSettings>>,
    ) -> Result<Tenant, String> {
        std::fs::create_dir_all(dir.join("blobs")).map_err(|e| format!("{}: {e}", dir.display()))?;
        let conn = store::open(&dir.join("relay.db")).map_err(|e| e.to_string())?;
        let mut ts: TenantSettings = store::kv_get(&conn, "tenant_settings")
            .and_then(|s| serde_json::from_str(&s).ok())
            // Before tenancies: one settings document for everything.
            .or_else(|| {
                store::kv_get(&conn, "settings")
                    .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
                    .map(|s| TenantSettings::from(&s))
            })
            .unwrap_or_default();
        ts.normalize(&limits);
        store::kv_set(&conn, "tenant_settings", &serde_json::to_string(&ts).unwrap_or_default());
        Ok(Tenant {
            id: id.to_string(),
            dir,
            name: RwLock::new(name.to_string()),
            db: Mutex::new(conn),
            settings: RwLock::new(ts),
            limits: RwLock::new(limits),
            server,
            hub: Hub::default(),
            lockouts: Mutex::new(HashMap::new()),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn name(&self) -> String {
        self.name.read().unwrap().clone()
    }

    pub fn set_name(&self, name: &str) {
        *self.name.write().unwrap() = name.to_string();
    }

    pub fn is_unmigrated(&self) -> bool {
        self.id == UNMIGRATED
    }

    /// What this tenancy runs with (its policy within its limits, on this server).
    pub fn settings(&self) -> Settings {
        settings::effective(
            &self.server.read().unwrap(),
            &self.settings.read().unwrap(),
            &self.limits.read().unwrap(),
        )
    }

    pub fn tenant_settings(&self) -> TenantSettings {
        self.settings.read().unwrap().clone()
    }

    pub fn limits(&self) -> Limits {
        self.limits.read().unwrap().clone()
    }

    /// Saves the tenancy's policy and tells its connected clients about changes.
    pub fn set_tenant_settings(&self, mut s: TenantSettings) -> TenantSettings {
        s.normalize(&self.limits());
        let old = self.tenant_settings();
        store::kv_set(&self.db(), "tenant_settings", &serde_json::to_string(&s).unwrap_or_default());
        *self.settings.write().unwrap() = s.clone();
        if old.policy != s.policy {
            if old.policy.auth != s.policy.auth {
                self.hub
                    .kick_where("서버 로그인 방식이 바뀌었습니다. 다시 로그온하세요", |_, _| true);
            } else if s.policy.encryption != Encryption::Plain {
                self.hub
                    .kick_where("서버가 암호화 연결을 요구합니다", |_, c| !c.tls);
            }
            self.hub
                .broadcast(&json!({ "t": "event", "e": "policy", "policy": s.policy }));
        }
        s
    }

    /// New limits; the policy is clamped into them.
    pub fn set_limits(&self, mut l: Limits) -> Limits {
        l.normalize();
        *self.limits.write().unwrap() = l.clone();
        self.set_tenant_settings(self.tenant_settings());
        l
    }

    /// Bytes of attachments stored for this tenancy.
    pub fn stored_bytes(&self) -> u64 {
        self.db()
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM messages WHERE files_gone = 0",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64
    }

    pub fn account_count(&self) -> i64 {
        self.db()
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Bumps the account's change time and pushes its contact entry to everyone who can see it.
    pub fn contact_changed(&self, account: &str) {
        let targets = self.hub.targets();
        let online = self.hub.online();
        let s = self.settings();
        let mut out = Vec::new();
        {
            let c = self.db();
            store::bump(&c, account);
            let Some(entry) = store::contact(&c, &online, account, s.show_host) else {
                return;
            };
            for (device, viewer) in targets {
                if store::can_see(&c, s.policy.contacts, &viewer, account) {
                    out.push((device, entry.clone()));
                }
            }
        }
        for (device, entry) in out {
            self.hub.send(
                &device,
                json!({ "t": "event", "e": "contact", "contact": entry }),
            );
        }
    }

    /// `Err(seconds left)` while `key` is locked out.
    pub fn locked(&self, key: &str) -> Result<(), i64> {
        locked(&self.lockouts, key)
    }

    pub fn login_failed(&self, key: &str) {
        let s = self.settings();
        if login_failed(&self.lockouts, key, s.login_max_failures, s.login_lockout_minutes) {
            let detail = format!(
                "{} failures, locked for {} min",
                s.login_max_failures, s.login_lockout_minutes
            );
            store::audit(&self.db(), key, "login_locked", &detail);
        }
    }

    pub fn login_ok(&self, key: &str) {
        self.lockouts.lock().unwrap().remove(key);
    }
}

pub type Lockouts = Mutex<HashMap<String, (u32, i64)>>;

pub fn locked(l: &Lockouts, key: &str) -> Result<(), i64> {
    let now = store::now_ms();
    match l.lock().unwrap().get(key) {
        Some(&(_, until)) if until > now => Err((until - now) / 1000 + 1),
        _ => Ok(()),
    }
}

/// Counts a failure; returns whether `key` just got locked.
pub fn login_failed(l: &Lockouts, key: &str, max: u32, minutes: u32) -> bool {
    let mut l = l.lock().unwrap();
    let e = l.entry(key.to_string()).or_insert((0, 0));
    e.0 += 1;
    let lock = e.0 >= max;
    if lock {
        *e = (0, store::now_ms() + minutes as i64 * 60_000);
    }
    lock
}
