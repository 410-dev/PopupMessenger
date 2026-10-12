//! Server policy (sent to clients) and admin-only settings, stored as one JSON
//! document in the `kv` table.

use serde::{Deserialize, Serialize};

/// Hard ceiling for devices per account, whatever the policy says.
pub const MAX_DEVICES_CAP: u32 = 9999;
/// Ten years, the longest retention the UI accepts.
pub const MAX_RETENTION_HOURS: u32 = 87_600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    None,
    Password,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Registration {
    Closed,
    Approval,
    Open,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encryption {
    Plain,
    Tls,
    E2ee,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Contacts {
    All,
    Lookup,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Retention {
    pub text: u32,
    pub files: u32,
}

/// Sent to clients in `hello` and `event: policy`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub auth: Auth,
    pub registration: Registration,
    pub max_devices: u32,
    pub relay_max_files: u32,
    pub relay_max_file_size: u64,
    pub force_relay: bool,
    pub block_direct_outsiders: bool,
    pub encryption: Encryption,
    pub retention_hours: Retention,
    pub contacts: Contacts,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            auth: Auth::Password,
            registration: Registration::Closed,
            max_devices: 3,
            relay_max_files: 10,
            relay_max_file_size: 100 * 1024 * 1024,
            force_relay: false,
            block_direct_outsiders: false,
            encryption: Encryption::Tls,
            retention_hours: Retention {
                text: 168,
                files: 72,
            },
            contacts: Contacts::All,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server_name: String,
    pub policy: Policy,
    /// Include devices' computer names in contacts.
    pub show_host: bool,
    /// Total bytes of stored attachments (0 = unlimited).
    pub storage_quota_mb: u64,
    /// Stored attachment bytes per sending account (0 = unlimited).
    pub account_quota_mb: u64,
    /// Largest message body (text, names, group or ciphertext).
    pub max_body_kb: u32,
    pub min_password_len: u32,
    pub login_max_failures: u32,
    pub login_lockout_minutes: u32,
    /// A control connection silent this long is dropped.
    pub idle_timeout_secs: u32,
    pub audit_days: u32,
    pub admin_session_hours: u32,
    /// Keep a copy of every relayed message that is not end-to-end encrypted, even
    /// after delivery, so admins can review it. Clients are not told.
    pub archive: bool,
    /// Also keep the attachments of archived messages.
    pub archive_files: bool,
    pub archive_days: u32,
    /// Accept the protocol over WebSocket (HTTP upgrade on the relay port), for
    /// HTTPS-only proxies such as Cloudflare Tunnel.
    pub websocket: bool,
    /// A proxy in front terminates TLS: WebSocket connections it marks as HTTPS
    /// (X-Forwarded-Proto / CF-Visitor) count as encrypted, and its client IP header is used.
    pub proxy_tls: bool,
    /// Largest WebSocket message; attachments are streamed in pieces of this size.
    pub ws_chunk_kb: u32,
    /// Address users should enter, shown in the admin UI (e.g. https://relay.example.com).
    pub public_url: String,
    /// Web page on the relay port where account holders manage their own account.
    pub user_portal: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_name: "PopupMessenger Relay".into(),
            policy: Policy::default(),
            show_host: false,
            storage_quota_mb: 0,
            account_quota_mb: 0,
            max_body_kb: 1024,
            min_password_len: 8,
            login_max_failures: 5,
            login_lockout_minutes: 15,
            idle_timeout_secs: 90,
            audit_days: 90,
            admin_session_hours: 12,
            archive: false,
            archive_files: false,
            archive_days: 90,
            websocket: true,
            proxy_tls: false,
            ws_chunk_kb: 256,
            public_url: String::new(),
            user_portal: true,
        }
    }
}

impl Settings {
    /// Clamps every number into its accepted range.
    pub fn normalize(&mut self) {
        let p = &mut self.policy;
        p.max_devices = p.max_devices.clamp(1, MAX_DEVICES_CAP);
        p.retention_hours.text = p.retention_hours.text.clamp(1, MAX_RETENTION_HOURS);
        p.retention_hours.files = p.retention_hours.files.clamp(1, p.retention_hours.text);
        if !p.force_relay {
            p.block_direct_outsiders = false;
        }
        self.server_name = self.server_name.trim().chars().take(80).collect();
        if self.server_name.is_empty() {
            self.server_name = Settings::default().server_name;
        }
        // Frames are capped at 8 MiB; leave room for the envelope.
        self.max_body_kb = self.max_body_kb.clamp(1, 7 * 1024);
        self.min_password_len = self.min_password_len.clamp(4, 128);
        self.login_max_failures = self.login_max_failures.clamp(1, 1000);
        self.login_lockout_minutes = self.login_lockout_minutes.clamp(1, 7 * 24 * 60);
        self.idle_timeout_secs = self.idle_timeout_secs.clamp(45, 3600);
        self.audit_days = self.audit_days.clamp(1, 3650);
        self.admin_session_hours = self.admin_session_hours.clamp(1, 24 * 30);
        self.archive_days = self.archive_days.clamp(1, 3650);
        self.ws_chunk_kb = self.ws_chunk_kb.clamp(16, 4096);
        self.public_url = self.public_url.trim().chars().take(200).collect();
        if !self.archive {
            self.archive_files = false;
        }
    }
}

// ---------------------------------------------------------------- tenancy split
//
// A server hosts several tenancies (organizations). What used to be one `Settings`
// document is split three ways, and `Settings` above is what a tenancy runs with
// (`effective`):
//
// - `ServerSettings`: how the server itself runs, set by server admins.
// - `TenantSettings`: a tenancy's own policy, set by its admins.
// - `Limits`: the physical resources a tenancy gets, set by server admins only.

/// Server-wide settings (server admins only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    pub server_name: String,
    /// Total bytes of stored attachments over all tenancies (0 = unlimited).
    pub storage_quota_mb: u64,
    pub idle_timeout_secs: u32,
    /// How long the server audit log (tenancy and server admin actions) is kept.
    pub audit_days: u32,
    pub admin_session_hours: u32,
    /// Failed admin logins before the admin name is locked.
    pub login_max_failures: u32,
    pub login_lockout_minutes: u32,
    pub websocket: bool,
    pub proxy_tls: bool,
    pub ws_chunk_kb: u32,
    pub public_url: String,
    pub user_portal: bool,
    /// The `<server>` part of addresses on this server (e.g. relay.example.com).
    /// Blank: the host of `public_url`, else the first TLS name.
    pub domain: String,
    pub exchange: ServerExchange,
}

/// What the server lets tenancies exchange beyond themselves. A tenancy's
/// `Limits` may make an exception either way.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerExchange {
    /// Messages between tenancies of this server.
    pub cross_tenancy: bool,
    /// Messages to other servers.
    pub send_remote: bool,
    /// Messages from other servers.
    pub receive_remote: bool,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self::from(&Settings::default())
    }
}

impl From<&Settings> for ServerSettings {
    fn from(s: &Settings) -> Self {
        Self {
            server_name: s.server_name.clone(),
            storage_quota_mb: s.storage_quota_mb,
            idle_timeout_secs: s.idle_timeout_secs,
            audit_days: s.audit_days,
            admin_session_hours: s.admin_session_hours,
            login_max_failures: s.login_max_failures,
            login_lockout_minutes: s.login_lockout_minutes,
            websocket: s.websocket,
            proxy_tls: s.proxy_tls,
            ws_chunk_kb: s.ws_chunk_kb,
            public_url: s.public_url.clone(),
            user_portal: s.user_portal,
            domain: String::new(),
            exchange: ServerExchange::default(),
        }
    }
}

impl ServerSettings {
    pub fn normalize(&mut self) {
        let mut s = Settings::default();
        self.apply_to(&mut s);
        s.audit_days = self.audit_days;
        s.login_max_failures = self.login_max_failures;
        s.login_lockout_minutes = self.login_lockout_minutes;
        s.normalize();
        let (domain, exchange) = (self.domain.clone(), self.exchange.clone());
        *self = Self::from(&s);
        self.domain = domain
            .trim()
            .trim_end_matches('.')
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(*c))
            .take(253)
            .collect();
        self.exchange = exchange;
    }

    /// The server's part of a tenancy's `Settings`. The audit retention and login
    /// lockout of `Settings` are the tenancy's; the server's own apply to admin
    /// logins and the server audit log.
    fn apply_to(&self, s: &mut Settings) {
        s.server_name = self.server_name.clone();
        s.storage_quota_mb = self.storage_quota_mb;
        s.idle_timeout_secs = self.idle_timeout_secs;
        s.admin_session_hours = self.admin_session_hours;
        s.websocket = self.websocket;
        s.proxy_tls = self.proxy_tls;
        s.ws_chunk_kb = self.ws_chunk_kb;
        s.public_url = self.public_url.clone();
        s.user_portal = self.user_portal;
    }
}

/// A tenancy's own policy (its admins and server admins).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TenantSettings {
    pub policy: Policy,
    pub show_host: bool,
    /// Stored attachment bytes per sending account (0 = no per-account limit).
    pub account_quota_mb: u64,
    pub min_password_len: u32,
    pub login_max_failures: u32,
    pub login_lockout_minutes: u32,
    pub audit_days: u32,
    pub archive: bool,
    pub archive_files: bool,
    pub archive_days: u32,
    pub exchange: Exchange,
    /// Account holders may issue API keys (bearer tokens) for scripts.
    pub api_keys: bool,
}

/// What a tenancy exchanges with other tenancies, here or on other servers. The
/// server's `ServerExchange` (and the tenancy's exceptions) must allow it too.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Exchange {
    /// Send messages and attachments to other tenancies of this server.
    pub send_local: bool,
    /// Receive from other tenancies of this server.
    pub receive_local: bool,
    /// Send to tenancies on other servers.
    pub send_remote: bool,
    /// Receive from tenancies on other servers.
    pub receive_remote: bool,
    /// "tenancy:server" patterns (`*` for any tenancy of a server; a bare tenancy
    /// means one on this server). If not empty, only these.
    pub allow: Vec<String>,
    /// Never these, whatever `allow` says.
    pub deny: Vec<String>,
}

impl Exchange {
    fn normalize(&mut self) {
        for l in [&mut self.allow, &mut self.deny] {
            let mut v: Vec<String> = l
                .iter()
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty() && p.len() <= 300)
                .collect();
            v.sort();
            v.dedup();
            v.truncate(1000);
            *l = v;
        }
    }
}

impl Default for TenantSettings {
    fn default() -> Self {
        Self::from(&Settings::default())
    }
}

impl From<&Settings> for TenantSettings {
    fn from(s: &Settings) -> Self {
        Self {
            policy: s.policy.clone(),
            show_host: s.show_host,
            account_quota_mb: s.account_quota_mb,
            min_password_len: s.min_password_len,
            login_max_failures: s.login_max_failures,
            login_lockout_minutes: s.login_lockout_minutes,
            audit_days: s.audit_days,
            archive: s.archive,
            archive_files: s.archive_files,
            archive_days: s.archive_days,
            exchange: Exchange::default(),
            api_keys: false,
        }
    }
}

impl TenantSettings {
    /// Clamps into range and into what the tenancy's `limits` allow.
    pub fn normalize(&mut self, limits: &Limits) {
        let mut s = Settings::default();
        self.apply_to(&mut s);
        s.normalize();
        let (mut exchange, api_keys) = (self.exchange.clone(), self.api_keys);
        exchange.normalize();
        *self = Self::from(&s);
        self.exchange = exchange;
        // Scripts send plaintext; an end-to-end encrypted tenancy cannot have them.
        self.api_keys = api_keys && self.policy.encryption != Encryption::E2ee;
        if limits.max_file_size_mb > 0 {
            let cap = limits.max_file_size_mb * 1024 * 1024;
            self.policy.relay_max_file_size = self.policy.relay_max_file_size.min(cap);
        }
        if !limits.archive_files {
            self.archive_files = false;
        }
    }

    fn apply_to(&self, s: &mut Settings) {
        s.policy = self.policy.clone();
        s.show_host = self.show_host;
        s.account_quota_mb = self.account_quota_mb;
        s.min_password_len = self.min_password_len;
        s.login_max_failures = self.login_max_failures;
        s.login_lockout_minutes = self.login_lockout_minutes;
        s.audit_days = self.audit_days;
        s.archive = self.archive;
        s.archive_files = self.archive_files;
        s.archive_days = self.archive_days;
    }
}

/// Physical resources a tenancy may use (server admins only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Stored attachment bytes of the whole tenancy (0 = unlimited).
    pub storage_quota_mb: u64,
    /// Largest attachment the tenancy may allow (0 = no cap).
    pub max_file_size_mb: u64,
    /// Largest message body.
    pub max_body_kb: u32,
    /// Accounts in the tenancy (0 = unlimited).
    pub max_accounts: u32,
    /// Whether the tenancy may keep attachments in its message archive.
    pub archive_files: bool,
    /// Exceptions to the server's `ServerExchange` for this tenancy (None = as the server says).
    pub cross_tenancy: Option<bool>,
    pub send_remote: Option<bool>,
    pub receive_remote: Option<bool>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            storage_quota_mb: 0,
            max_file_size_mb: 0,
            max_body_kb: Settings::default().max_body_kb,
            max_accounts: 0,
            archive_files: true,
            cross_tenancy: None,
            send_remote: None,
            receive_remote: None,
        }
    }
}

impl Limits {
    pub fn normalize(&mut self) {
        // Frames are capped at 8 MiB; leave room for the envelope.
        self.max_body_kb = self.max_body_kb.clamp(1, 7 * 1024);
    }
}

/// What a tenancy runs with: its policy within its limits, on this server.
pub fn effective(server: &ServerSettings, tenant: &TenantSettings, limits: &Limits) -> Settings {
    let mut s = Settings::default();
    tenant.apply_to(&mut s);
    server.apply_to(&mut s);
    s.max_body_kb = limits.max_body_kb;
    s.normalize();
    s
}
