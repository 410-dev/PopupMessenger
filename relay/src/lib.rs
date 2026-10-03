//! PopupMessenger relay server: the client protocol (docs/relay-protocol.md)
//! on one port and the admin web UI on another.

pub mod admin;
pub mod frame;
pub mod hub;
pub mod janitor;
pub mod relay;
pub mod settings;
pub mod store;
pub mod tls;
pub mod ws;

use rusqlite::Connection;
use serde_json::json;
use settings::Settings;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
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
    pub expires: i64,
}

pub struct Server {
    pub id: String,
    pub started: i64,
    pub dir: PathBuf,
    db: Mutex<Connection>,
    settings: RwLock<Settings>,
    pub hub: hub::Hub,
    pub tls: tls::Tls,
    /// Failed-login counters: key -> (failures, locked until ms).
    pub lockouts: Mutex<HashMap<String, (u32, i64)>>,
    pub admin_sessions: Mutex<HashMap<String, AdminSession>>,
    pub relay_port: u16,
}

pub struct Running {
    pub server: Arc<Server>,
    pub relay_addr: SocketAddr,
    pub admin_addr: SocketAddr,
}

impl Server {
    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    /// Saves settings and tells connected clients about policy changes.
    pub fn set_settings(&self, mut s: Settings) -> Settings {
        s.normalize();
        let old = self.settings();
        store::kv_set(
            &self.db(),
            "settings",
            &serde_json::to_string(&s).unwrap_or_default(),
        );
        *self.settings.write().unwrap() = s.clone();
        if old.policy != s.policy {
            if old.policy.auth != s.policy.auth {
                self.hub.kick_where(
                    "서버 로그인 방식이 바뀌었습니다. 다시 로그온하세요",
                    |_, _| true,
                );
            } else if s.policy.encryption != settings::Encryption::Plain {
                self.hub
                    .kick_where("서버가 암호화 연결을 요구합니다", |_, c| {
                        !c.tls
                    });
            }
            self.hub
                .broadcast(&json!({ "t": "event", "e": "policy", "policy": s.policy }));
        }
        s
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
        let now = store::now_ms();
        match self.lockouts.lock().unwrap().get(key) {
            Some(&(_, until)) if until > now => Err((until - now) / 1000 + 1),
            _ => Ok(()),
        }
    }

    pub fn login_failed(&self, key: &str) {
        let s = self.settings();
        let locked = {
            let mut l = self.lockouts.lock().unwrap();
            let e = l.entry(key.to_string()).or_insert((0, 0));
            e.0 += 1;
            let lock = e.0 >= s.login_max_failures;
            if lock {
                *e = (0, store::now_ms() + s.login_lockout_minutes as i64 * 60_000);
            }
            lock
        };
        if locked {
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

pub async fn start(cfg: Config) -> Result<Running, String> {
    std::fs::create_dir_all(cfg.data_dir.join("blobs"))
        .map_err(|e| format!("{}: {e}", cfg.data_dir.display()))?;
    let conn = store::open(&cfg.data_dir.join("relay.db")).map_err(|e| e.to_string())?;
    let id = store::kv_get(&conn, "server_id").unwrap_or_else(|| {
        let id = store::new_id();
        store::kv_set(&conn, "server_id", &id);
        id
    });
    let mut settings: Settings = store::kv_get(&conn, "settings")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    settings.normalize();
    store::kv_set(
        &conn,
        "settings",
        &serde_json::to_string(&settings).unwrap_or_default(),
    );
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
        settings: RwLock::new(settings),
        hub: hub::Hub::default(),
        tls,
        lockouts: Mutex::new(HashMap::new()),
        admin_sessions: Mutex::new(HashMap::new()),
        relay_port: relay_addr.port(),
    });
    // Attachments left half-written by a crash.
    janitor::clean_partial(&server);
    tokio::spawn(relay::serve(server.clone(), relay_l));
    tokio::spawn(admin::serve(server.clone(), admin_l));
    tokio::spawn(janitor::run(server.clone()));
    Ok(Running {
        server,
        relay_addr,
        admin_addr,
    })
}
