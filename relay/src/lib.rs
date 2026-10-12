//! PopupMessenger relay server: the client protocol (docs/relay-protocol.md)
//! on one port and the admin web UI on another.
//!
//! One server hosts several tenancies (organizations), each with its own
//! database, policy and users (`tenant`). Server-wide data lives in `server.db`.

pub mod address;
pub mod api;
pub mod admin;
pub mod frame;
pub mod hub;
pub mod janitor;
pub mod migrate;
pub mod portal;
pub mod relay;
pub mod settings;
pub mod store;
pub mod tenant;
pub mod tls;
pub mod ws;

use rusqlite::{params, Connection, OptionalExtension};
use settings::{Limits, ServerSettings, Settings, TenantSettings};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use tenant::{Lockouts, Tenant, UNMIGRATED};
use tokio::net::TcpListener;

/// Relay protocol version: the first part of the server version, and what
/// clients must send in `hello`.
pub const PROTOCOL: u64 = 1;

/// Server version "<protocol>.<YYMMDD>.<NN>" (build date and build of that day),
/// kept in the VERSION file and bumped by bump-version.py.
pub fn version() -> &'static str {
    include_str!("../VERSION").trim()
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_format() {
        let v = super::version();
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(parts.len(), 3, "{v}");
        assert_eq!(parts[0], super::PROTOCOL.to_string(), "VERSION must start with PROTOCOL: run bump-version.py");
        assert!(parts[1].len() == 6 && parts[1].chars().all(|c| c.is_ascii_digit()), "{v}");
        assert!(parts[2].len() >= 2 && parts[2].chars().all(|c| c.is_ascii_digit()), "{v}");
    }
}

pub struct Config {
    pub data_dir: PathBuf,
    pub relay_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Subject names of the self-signed certificate.
    pub tls_names: Vec<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let addr = |k: &str, d: &str| {
            var(k)
                .unwrap_or_else(|| d.into())
                .parse()
                .unwrap_or_else(|_| d.parse().unwrap())
        };
        Self {
            data_dir: var("DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data")),
            relay_bind: addr("RELAY_BIND", "0.0.0.0:45443"),
            admin_bind: addr("ADMIN_BIND", "0.0.0.0:8080"),
            tls_cert: var("TLS_CERT").map(PathBuf::from),
            tls_key: var("TLS_KEY").map(PathBuf::from),
            tls_names: var("TLS_NAMES")
                .unwrap_or_else(|| "localhost".into())
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }
}

pub struct AdminSession {
    pub admin: i64,
    pub username: String,
    /// `None` for server admins, else the tenancy this admin manages.
    pub tenancy: Option<String>,
    pub expires: i64,
}

pub struct Server {
    pub id: String,
    pub started: i64,
    pub dir: PathBuf,
    /// Server-wide data (`server.db`): tenancies, admins, moved accounts, server audit log.
    db: Mutex<Connection>,
    settings: Arc<RwLock<ServerSettings>>,
    tenants: RwLock<BTreeMap<String, Arc<Tenant>>>,
    /// Folders of deleted tenancies that could not be removed yet (files still open).
    doomed: Mutex<Vec<PathBuf>>,
    pub tls: tls::Tls,
    /// Failed admin logins: key -> (failures, locked until ms).
    pub lockouts: Lockouts,
    pub admin_sessions: Mutex<HashMap<String, AdminSession>>,
    /// User portal (relay port) sessions: token -> session.
    pub user_sessions: Mutex<HashMap<String, portal::UserSession>>,
    pub relay_port: u16,
    /// Names of the TLS certificate made for this server.
    pub tls_names: Vec<String>,
}

pub struct Running {
    pub server: Arc<Server>,
    pub relay_addr: SocketAddr,
    pub admin_addr: SocketAddr,
}

/// Folder of a tenancy's data. The unmigrated tenancy keeps the original layout.
fn tenant_dir(data: &std::path::Path, id: &str) -> PathBuf {
    if id == UNMIGRATED {
        data.to_path_buf()
    } else {
        data.join("tenants").join(id)
    }
}

impl Server {
    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn server_settings(&self) -> ServerSettings {
        self.settings.read().unwrap().clone()
    }

    pub fn set_server_settings(&self, mut s: ServerSettings) -> ServerSettings {
        s.normalize();
        store::kv_set(&self.db(), "server_settings", &serde_json::to_string(&s).unwrap_or_default());
        *self.settings.write().unwrap() = s.clone();
        s
    }

    /// The `<server>` part of addresses on this server.
    pub fn domain(&self) -> String {
        self.domains().into_iter().next().unwrap_or_else(|| "localhost".into())
    }

    /// Every name an address may use for this server, the canonical one first.
    pub fn domains(&self) -> Vec<String> {
        let s = self.server_settings();
        let url_host = {
            let u = s.public_url.trim().to_lowercase();
            let rest = u.split_once("://").map(|x| x.1).unwrap_or(&u);
            rest.split('/').next().unwrap_or("").to_string()
        };
        let mut out: Vec<String> = Vec::new();
        for d in std::iter::once(s.domain).chain([url_host]).chain(self.tls_names.iter().map(|n| n.to_lowercase())) {
            if !d.is_empty() && !out.contains(&d) {
                out.push(d);
            }
        }
        out
    }

    pub fn tenant(&self, id: &str) -> Option<Arc<Tenant>> {
        self.tenants.read().unwrap().get(id).cloned()
    }

    /// Where clients without tenancy support log on.
    pub fn unmigrated(&self) -> Arc<Tenant> {
        self.tenant(UNMIGRATED).expect("unmigrated tenancy")
    }

    pub fn tenants(&self) -> Vec<Arc<Tenant>> {
        self.tenants.read().unwrap().values().cloned().collect()
    }

    /// The unmigrated tenancy's effective settings: the whole server as it was
    /// before tenancies (used by tests and single-organization setups).
    pub fn settings(&self) -> Settings {
        self.unmigrated().settings()
    }

    /// Sets server settings and the unmigrated tenancy's policy from one document.
    /// What `Settings` does not cover (domain, exchange rules, API keys) is kept.
    pub fn set_settings(&self, s: Settings) -> Settings {
        let old = self.server_settings();
        self.set_server_settings(ServerSettings {
            domain: old.domain,
            exchange: old.exchange,
            ..ServerSettings::from(&s)
        });
        let t = self.unmigrated();
        let mut l = t.limits();
        l.max_body_kb = s.max_body_kb;
        let l = t.set_limits(l);
        self.save_limits(&t.id, &l);
        let old = t.tenant_settings();
        t.set_tenant_settings(TenantSettings {
            exchange: old.exchange,
            api_keys: old.api_keys,
            ..TenantSettings::from(&s)
        });
        t.settings()
    }

    /// The tenancy holding transfer session `token`, with its (device, account).
    pub fn session(&self, token: &str) -> Option<(Arc<Tenant>, String, String)> {
        self.tenants()
            .into_iter()
            .find_map(|t| t.hub.session(token).map(|(d, a)| (t, d, a)))
    }

    /// Bytes of attachments stored over all tenancies.
    pub fn stored_bytes(&self) -> u64 {
        self.tenants().iter().map(|t| t.stored_bytes()).sum()
    }

    /// The tenancy an unmigrated device (`kind` "device") or lower-case username
    /// ("user") was moved to.
    pub fn moved_to(&self, kind: &str, key: &str) -> Option<Arc<Tenant>> {
        let id: String = self
            .db()
            .query_row(
                "SELECT tenancy FROM moved WHERE kind = ?1 AND key = ?2",
                [kind, key],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()?;
        self.tenant(&id)
    }

    fn save_limits(&self, id: &str, l: &Limits) {
        let _ = self.db().execute(
            "UPDATE tenancies SET limits = ?1 WHERE id = ?2",
            params![serde_json::to_string(l).unwrap_or_default(), id],
        );
    }

    pub fn create_tenancy(&self, id: &str, name: &str, note: &str, limits: Limits) -> Result<Arc<Tenant>, String> {
        if !tenant::valid_id(id) {
            return Err("Tenancy ID는 2~32자의 영문 소문자, 숫자, - 만 쓸 수 있습니다".into());
        }
        if self.tenant(id).is_some() || id == UNMIGRATED {
            return Err("이미 있는 Tenancy ID입니다".into());
        }
        let dir = tenant_dir(&self.dir, id);
        if dir.exists() {
            return Err("같은 ID의 Tenancy를 지우는 중입니다. 잠시 뒤에 다시 시도하세요".into());
        }
        let mut limits = limits;
        limits.normalize();
        let name = clean_name(name, id);
        let note: String = note.chars().take(500).collect();
        let t = Arc::new(Tenant::open(id, &name, dir, limits.clone(), self.settings.clone())?);
        self.db()
            .execute(
                "INSERT INTO tenancies (id, name, note, limits, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, name, note, serde_json::to_string(&limits).unwrap_or_default(), store::now_ms()],
            )
            .map_err(|e| e.to_string())?;
        self.tenants.write().unwrap().insert(id.to_string(), t.clone());
        Ok(t)
    }

    /// Renames a tenancy, changes its note or its resource limits.
    pub fn update_tenancy(&self, id: &str, name: Option<&str>, note: Option<&str>, limits: Option<Limits>) -> Result<(), String> {
        let t = self.tenant(id).ok_or("없는 Tenancy입니다")?;
        if let Some(n) = name {
            let n = clean_name(n, id);
            let _ = self.db().execute("UPDATE tenancies SET name = ?1 WHERE id = ?2", params![n, id]);
            t.set_name(&n);
        }
        if let Some(n) = note {
            let n: String = n.chars().take(500).collect();
            let _ = self.db().execute("UPDATE tenancies SET note = ?1 WHERE id = ?2", params![n, id]);
        }
        if let Some(l) = limits {
            let l = t.set_limits(l);
            self.save_limits(id, &l);
        }
        Ok(())
    }

    /// Deletes an empty tenancy with everything it stored.
    pub fn delete_tenancy(&self, id: &str) -> Result<(), String> {
        if id == UNMIGRATED {
            return Err("Unmigrated Tenancy는 지울 수 없습니다".into());
        }
        let t = self.tenant(id).ok_or("없는 Tenancy입니다")?;
        let n = t.account_count();
        if n > 0 {
            return Err(format!("사용자 {n}명이 남아 있습니다. 먼저 모든 사용자를 삭제하세요"));
        }
        t.hub.kick_where("Tenancy가 삭제되었습니다", |_, _| true);
        self.tenants.write().unwrap().remove(id);
        {
            let db = self.db();
            let _ = db.execute("DELETE FROM tenancies WHERE id = ?1", [id]);
            let _ = db.execute("DELETE FROM admins WHERE tenancy = ?1", [id]);
            let _ = db.execute("DELETE FROM moved WHERE tenancy = ?1", [id]);
        }
        self.admin_sessions
            .lock()
            .unwrap()
            .retain(|_, s| s.tenancy.as_deref() != Some(id));
        self.user_sessions.lock().unwrap().retain(|_, s| s.tenancy != id);
        let dir = t.dir.clone();
        drop(t);
        let _ = std::fs::write(dir.join(".deleted"), b"");
        self.doomed.lock().unwrap().push(dir);
        self.remove_doomed();
        Ok(())
    }

    /// Removes folders of deleted tenancies once nothing has their files open.
    pub fn remove_doomed(&self) {
        self.doomed
            .lock()
            .unwrap()
            .retain(|d| std::fs::remove_dir_all(d).is_err() && d.exists());
    }

    /// Moves an account of the unmigrated tenancy, with its devices and the messages
    /// waiting for them, to tenancy `to`. Clients without tenancy support keep
    /// logging on as before and are routed there. Returns the account's label.
    pub fn move_account(&self, account: &str, to: &str) -> Result<String, String> {
        let src = self.unmigrated();
        let dst = self
            .tenant(to)
            .filter(|t| !t.is_unmigrated())
            .ok_or("옮길 Tenancy가 없습니다")?;
        migrate::move_account(self, &src, &dst, account)
    }

    /// `Err(seconds left)` while an admin login `key` is locked out.
    pub fn locked(&self, key: &str) -> Result<(), i64> {
        tenant::locked(&self.lockouts, key)
    }

    pub fn login_failed(&self, key: &str) {
        let s = self.server_settings();
        if tenant::login_failed(&self.lockouts, key, s.login_max_failures, s.login_lockout_minutes) {
            let detail = format!(
                "{} failures, locked for {} min",
                s.login_max_failures, s.login_lockout_minutes
            );
            store::audit_server(&self.db(), key, "login_locked", &detail, None);
        }
    }

    pub fn login_ok(&self, key: &str) {
        self.lockouts.lock().unwrap().remove(key);
    }
}

fn clean_name(name: &str, id: &str) -> String {
    let n: String = name.trim().chars().take(80).collect();
    if n.is_empty() {
        id.to_string()
    } else {
        n
    }
}

/// Opens `server.db`. On the first start after tenancies were added, the server's
/// existing data becomes the unmigrated tenancy: its admins become server admins
/// and its settings are split between the server and that tenancy.
fn open_server_db(data: &std::path::Path) -> Result<(Connection, String, ServerSettings), String> {
    let c = store::open_server(&data.join("server.db")).map_err(|e| e.to_string())?;
    if store::kv_get(&c, "server_id").is_none() {
        let old = store::open(&data.join("relay.db")).map_err(|e| e.to_string())?;
        let id = store::kv_get(&old, "server_id").unwrap_or_else(store::new_id);
        let legacy: Settings = store::kv_get(&old, "settings")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let limits = Limits {
            max_body_kb: legacy.max_body_kb,
            ..Limits::default()
        };
        let _ = c.execute(
            "INSERT OR IGNORE INTO tenancies (id, name, limits, created) VALUES (?1, 'Unmigrated', ?2, ?3)",
            params![UNMIGRATED, serde_json::to_string(&limits).unwrap_or_default(), store::now_ms()],
        );
        let admins: Vec<(String, String, i64, Option<i64>)> = old
            .prepare("SELECT username, pass, created, last_login FROM admins ORDER BY id")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect())
            .unwrap_or_default();
        for (u, p, created, last) in admins {
            let _ = c.execute(
                "INSERT OR IGNORE INTO admins (tenancy, username, pass, created, last_login) VALUES ('', ?1, ?2, ?3, ?4)",
                params![u, p, created, last],
            );
        }
        store::kv_set(
            &c,
            "server_settings",
            &serde_json::to_string(&ServerSettings::from(&legacy)).unwrap_or_default(),
        );
        store::kv_set(&c, "server_id", &id);
    }
    let id = store::kv_get(&c, "server_id").unwrap_or_default();
    let mut s: ServerSettings = store::kv_get(&c, "server_settings")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    s.normalize();
    store::kv_set(&c, "server_settings", &serde_json::to_string(&s).unwrap_or_default());
    Ok((c, id, s))
}

pub async fn start(cfg: Config) -> Result<Running, String> {
    std::fs::create_dir_all(cfg.data_dir.join("blobs"))
        .map_err(|e| format!("{}: {e}", cfg.data_dir.display()))?;
    let (conn, id, ss) = open_server_db(&cfg.data_dir)?;
    let ss = Arc::new(RwLock::new(ss));
    let rows: Vec<(String, String, String)> = conn
        .prepare("SELECT id, name, limits FROM tenancies")
        .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
        .map_err(|e| e.to_string())?;
    let mut tenants = BTreeMap::new();
    for (tid, name, limits) in rows {
        let mut limits: Limits = serde_json::from_str(&limits).unwrap_or_default();
        limits.normalize();
        let t = Tenant::open(&tid, &name, tenant_dir(&cfg.data_dir, &tid), limits, ss.clone())?;
        tenants.insert(tid, Arc::new(t));
    }
    if !tenants.contains_key(UNMIGRATED) {
        return Err("server.db has no unmigrated tenancy".into());
    }
    // Folders of tenancies deleted while their files were still open.
    let mut doomed = Vec::new();
    if let Ok(rd) = std::fs::read_dir(cfg.data_dir.join("tenants")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !tenants.contains_key(&name) && e.path().join(".deleted").exists() {
                doomed.push(e.path());
            }
        }
    }
    let tls = tls::load(
        &cfg.data_dir,
        cfg.tls_cert.clone(),
        cfg.tls_key.clone(),
        &cfg.tls_names,
    )?;

    let relay_l = TcpListener::bind(cfg.relay_bind)
        .await
        .map_err(|e| format!("{}: {e}", cfg.relay_bind))?;
    let admin_l = TcpListener::bind(cfg.admin_bind)
        .await
        .map_err(|e| format!("{}: {e}", cfg.admin_bind))?;
    let relay_addr = relay_l.local_addr().map_err(|e| e.to_string())?;
    let admin_addr = admin_l.local_addr().map_err(|e| e.to_string())?;

    let server = Arc::new(Server {
        id,
        started: store::now_ms(),
        dir: cfg.data_dir.clone(),
        db: Mutex::new(conn),
        settings: ss,
        tenants: RwLock::new(tenants),
        doomed: Mutex::new(doomed),
        tls,
        lockouts: Mutex::new(HashMap::new()),
        admin_sessions: Mutex::new(HashMap::new()),
        user_sessions: Mutex::new(HashMap::new()),
        relay_port: relay_addr.port(),
        tls_names: cfg.tls_names.clone(),
    });
    server.remove_doomed();
    // Attachments left half-written by a crash.
    for t in server.tenants() {
        janitor::clean_partial(&t);
    }
    tokio::spawn(relay::serve(server.clone(), relay_l));
    tokio::spawn(admin::serve(server.clone(), admin_l));
    tokio::spawn(janitor::run(server.clone()));
    Ok(Running {
        server,
        relay_addr,
        admin_addr,
    })
}
