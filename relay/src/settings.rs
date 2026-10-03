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
