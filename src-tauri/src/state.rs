use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Disk space kept free when accepting attachments.
pub const DISK_MARGIN: u64 = 512 * 1024 * 1024;
const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const INBOX_CAP: usize = 1000;
/// Peers heard from within this window count as present on the network.
pub const RECENT: Duration = Duration::from_secs(95);
/// Protocol version sent in `Meta.ver`. 3: group info in message headers.
pub const PROTO_VER: u32 = 3;
/// Upper bound on group members kept from a received header.
pub const MAX_GROUP_MEMBERS: usize = 1000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

/// OS account name; the base of the default display name.
pub fn default_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".into())
}

/// Stable 64-bit FNV-1a of a computer name as 16 hex digits. Only used to tell
/// same-named users apart, so a non-cryptographic hash is fine.
pub fn host_hash(host: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in host.trim().to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// "<base>-<4 hex digits of the host hash>", moving to the next 4 digits while
/// the result is taken. Falls back to the first 4 if every slice is taken.
pub fn tagged(base: &str, host: &str, taken: impl Fn(&str) -> bool) -> String {
    let hash = host_hash(host);
    (0..4)
        .map(|i| format!("{base}-{}", &hash[i * 4..i * 4 + 4]))
        .find(|c| !taken(c))
        .unwrap_or_else(|| format!("{base}-{}", &hash[..4]))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub id: String,
    pub name: String,
    /// The user typed `name` in settings. Otherwise it is the default, which is
    /// renamed to "<name>-<hash>" automatically if someone on the LAN has the same name.
    pub name_custom: bool,
    /// Show peers' computer names in the tray and recipient lists.
    pub show_host: bool,
    pub notify: bool,
    pub autostart: bool,
    pub max_files: u32,
    pub max_file_gb: f64,
    pub download_dir: String,
    /// Extra hosts to probe when broadcast does not reach them (comma/space separated).
    pub extra_hosts: String,
    /// How many ports from 55000 are probed on each host.
    pub scan_span: u16,
    pub context_menu: bool,
    /// Appended to every new message and reply.
    pub signature: String,
    /// Performance mode keeps one pre-rendered hidden window so new windows appear
    /// instantly; efficiency mode (default) creates every window from scratch.
    pub perf_mode: bool,
    /// Presence shown to others; chosen from the tray menu.
    pub status: Status,
    /// Reset `status` to online on every start.
    pub auto_online: bool,
    /// Save each message's attachments into their own sub-folder.
    pub split_folders: bool,
}

/// Presence. `Offline` hides this user from everyone's lists; `Dnd` still
/// receives but without popups, and senders are told so.
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Dnd,
    Offline,
    /// Also what unknown values from newer versions read as.
    #[default]
    #[serde(other)]
    Online,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Online => "온라인",
            Status::Dnd => "방해금지",
            Status::Offline => "오프라인",
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: default_name(),
            name_custom: false,
            show_host: false,
            notify: true,
            autostart: true,
            max_files: 10,
            max_file_gb: 10.0,
            download_dir: String::new(),
            extra_hosts: String::new(),
            scan_span: 10,
            context_menu: true,
            signature: String::new(),
            perf_mode: false,
            status: Status::Online,
            auto_online: true,
            split_folders: true,
        }
    }
}

/// Metadata exchanged over the discovery port.
#[derive(Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub host: String,
    pub port: u16,
    #[serde(default = "yes")]
    pub online: bool,
    #[serde(default = "yes")]
    pub notify: bool,
    #[serde(default)]
    pub max_files: u32,
    #[serde(default)]
    pub max_file_size: u64,
    #[serde(default)]
    pub free_space: u64,
    #[serde(default)]
    pub ver: u32,
    /// Missing in 0.1.0/0.1.1 peers, which are always online.
    #[serde(default)]
    pub status: Status,
    /// The relay account this device is registered to (0.1.8+), so LAN peers on
    /// the same relay server can fall back to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayId>,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RelayId {
    pub server: String,
    pub account: String,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub host: String,
    pub ip: String,
    pub port: u16,
    pub notify: bool,
    pub max_files: u32,
    pub max_file_size: u64,
    pub status: Status,
    /// Protocol version from the peer's last announcement.
    pub ver: u32,
    /// Relay account this peer belongs to: its own for "r:<account>" peers, and the
    /// owning account for a LAN peer that is also one of that account's devices.
    pub relay_account: String,
    /// Relay account the device itself announced on the LAN for our relay server.
    #[serde(default)]
    pub relay_self: String,
    /// Recently seen and not in `Offline` status.
    #[serde(skip)]
    pub online: bool,
    #[serde(skip)]
    pub seen: Option<Instant>,
}

#[derive(Serialize)]
pub struct PeerView {
    pub id: String,
    pub name: String,
    /// `name`, or "<name>-<hash>" when another listed user has the same name
    /// and computer names are hidden.
    pub display: String,
    pub host: String,
    pub ip: String,
    pub online: bool,
    pub status: Status,
    pub notify: bool,
    pub count: u32,
    /// A relay contact ("r:<account>").
    pub relay: bool,
    /// Online, or reachable through the relay (stored if offline).
    pub reach: bool,
    /// Relay account: the contact's own, or the one a LAN device belongs to.
    pub account: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub path: String,
}

/// A saved recipient list. Members are peer ids, so renames keep membership.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    /// Default for "show the group name and other recipients" when sending.
    pub reveal: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
}

/// Group details sent to v3+ peers when the sender chose to reveal them.
#[derive(Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub name: String,
    pub members: Vec<Member>,
}

#[derive(Serialize)]
pub struct MemberView {
    pub id: String,
    pub display: String,
    pub online: bool,
    pub status: Status,
    /// Offline but reachable through the relay.
    pub relay: bool,
}

#[derive(Serialize)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub reveal: bool,
    pub members: Vec<MemberView>,
    pub online: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from_id: String,
    pub from_name: String,
    pub from_ip: String,
    pub text: String,
    pub files: Vec<FileEntry>,
    pub ts: u64,
    pub read: bool,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub group: Option<GroupInfo>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Data {
    pub inbox: Vec<Message>,
    /// Sent message count per peer id; drives tray ordering.
    pub counts: HashMap<String, u32>,
    pub peers: HashMap<String, Peer>,
    pub sent: Vec<Sent>,
    pub groups: Vec<Group>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Recipient {
    pub id: String,
    pub name: String,
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    /// "lan", "relay" or "stored" (kept on the relay for an offline recipient).
    #[serde(default)]
    pub via: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Sent {
    pub id: String,
    pub to: Vec<Recipient>,
    pub text: String,
    /// `path` is the local source file.
    pub files: Vec<FileEntry>,
    pub ts: u64,
    /// Name of the group it was sent to.
    #[serde(default)]
    pub group: Option<String>,
}

pub struct AppState {
    pub dir: PathBuf,
    pub port: u16,
    pub host: String,
    pub settings: Mutex<Settings>,
    pub data: Mutex<Data>,
    pub cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    pub pending: Mutex<(Vec<String>, Option<Instant>)>,
    pub seq: AtomicU32,
    /// Logical window label -> real label, for windows that were spare windows.
    pub aliases: Mutex<HashMap<String, String>>,
    /// The hidden pre-rendered window (label, page ready) in performance mode.
    pub spare: Mutex<Option<(String, bool)>>,
    pub scan: tokio::sync::Notify,
    pub relay: crate::relay::Relay,
}

fn load<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn store(p: &Path, bytes: Vec<u8>) {
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, p);
    }
}

impl AppState {
    pub fn new(dir: PathBuf, port: u16, mut settings: Settings, default_dl: PathBuf) -> Self {
        if settings.download_dir.trim().is_empty() {
            settings.download_dir = default_dl.to_string_lossy().into_owned();
        }
        let _ = std::fs::create_dir_all(&settings.download_dir);
        let data: Data = load(&dir.join("data.json")).unwrap_or_default();
        let dir_for_relay = dir.clone();
        Self {
            host: gethostname::gethostname().to_string_lossy().into_owned(),
            dir,
            port,
            settings: Mutex::new(settings),
            data: Mutex::new(data),
            cancels: Mutex::new(HashMap::new()),
            pending: Mutex::new((Vec::new(), None)),
            seq: AtomicU32::new(1),
            aliases: Mutex::new(HashMap::new()),
            spare: Mutex::new(None),
            scan: tokio::sync::Notify::new(),
            relay: crate::relay::Relay::load(&dir_for_relay),
        }
    }

    pub fn load_settings(dir: &Path) -> Settings {
        load(&dir.join("settings.json")).unwrap_or_default()
    }

    pub fn save_settings(&self) {
        let b = serde_json::to_vec_pretty(&*self.settings.lock().unwrap()).unwrap_or_default();
        store(&self.dir.join("settings.json"), b);
    }

    pub fn save_data(&self) {
        let b = serde_json::to_vec(&*self.data.lock().unwrap()).unwrap_or_default();
        store(&self.dir.join("data.json"), b);
    }

    pub fn my_id(&self) -> String {
        self.settings.lock().unwrap().id.clone()
    }

    /// (max files, max size per file, download dir) after applying free disk space.
    pub fn limits(&self) -> (u32, u64, PathBuf, u64) {
        let (files, gb, dir) = {
            let s = self.settings.lock().unwrap();
            (s.max_files, s.max_file_gb, PathBuf::from(&s.download_dir))
        };
        let _ = std::fs::create_dir_all(&dir);
        let free = fs2::available_space(&dir).unwrap_or(0);
        let mut size = (gb.max(0.0) * GB) as u64;
        if free > 0 {
            size = size.min(free.saturating_sub(DISK_MARGIN));
        }
        (files, size, dir, free)
    }

    pub fn my_meta(&self) -> Meta {
        let (max_files, max_file_size, _, free_space) = self.limits();
        let s = self.settings.lock().unwrap();
        Meta {
            id: s.id.clone(),
            name: s.name.clone(),
            host: self.host.clone(),
            port: self.port,
            online: s.status != Status::Offline,
            status: s.status,
            notify: s.notify,
            max_files,
            max_file_size,
            free_space,
            ver: PROTO_VER,
            relay: self.relay.identity(),
        }
    }

    /// With a relay, each relay account is one entry ("r:<account>", sending to the
    /// user: LAN first, then the relay) that reads as online while any of its devices
    /// is; its LAN devices are listed too while online (`account` ties them to it),
    /// and offline ones are hidden behind the account.
    pub fn peers(&self, online_only: bool) -> Vec<PeerView> {
        let show_host = self.settings.lock().unwrap().show_host;
        let relay = self.relay.configured();
        let d = self.data.lock().unwrap();
        let mut v: Vec<PeerView> = d
            .peers
            .values()
            .filter(|p| {
                if p.id.starts_with("r:") {
                    relay
                } else {
                    p.online || !relay || p.relay_account.is_empty()
                }
            })
            .map(|p| account_view(p, &d, show_host, relay))
            .filter(|p| p.online || !online_only)
            .collect();
        v.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then(b.count.cmp(&a.count))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        if !show_host {
            // Same-named online users are told apart by a tag from their computer name.
            let mut taken: HashSet<String> = v.iter().filter(|p| p.online).map(|p| p.name.to_lowercase()).collect();
            for i in 0..v.len() {
                let dup = v[i].online
                    && v.iter()
                        .filter(|q| q.online && q.name.eq_ignore_ascii_case(&v[i].name))
                        // An account and its own devices are the same person.
                        .filter(|q| q.id == v[i].id || q.account.is_empty() || q.account != v[i].account)
                        .count()
                        > 1;
                if dup {
                    let host = d.peers.get(&v[i].id).map(|p| p.host.clone()).unwrap_or_default();
                    let tag = tagged(&v[i].name, &host, |c| taken.contains(&c.to_lowercase()));
                    taken.insert(tag.to_lowercase());
                    v[i].display = tag;
                }
            }
        }
        v
    }

    /// Groups with their members resolved to display names, in saved order.
    pub fn groups(&self) -> Vec<GroupView> {
        let show_host = self.settings.lock().unwrap().show_host;
        let relay = self.relay.configured();
        let shown = self.peers(false);
        let all: Vec<PeerView> = {
            let d = self.data.lock().unwrap();
            d.peers.values().map(|p| account_view(p, &d, show_host, relay)).collect()
        };
        let groups = self.data.lock().unwrap().groups.clone();
        groups
            .into_iter()
            .map(|g| {
                let members: Vec<MemberView> = g
                    .members
                    .iter()
                    .map(|id| match shown.iter().chain(&all).find(|p| &p.id == id) {
                        Some(p) => MemberView {
                            id: id.clone(),
                            display: p.display.clone(),
                            online: p.online,
                            status: p.status,
                            relay: !p.online && p.reach,
                        },
                        None => MemberView { id: id.clone(), display: id.clone(), online: false, status: Status::Offline, relay: false },
                    })
                    .collect();
                GroupView {
                    id: g.id,
                    name: g.name,
                    reveal: g.reveal,
                    online: members.iter().filter(|m| m.online).count(),
                    members,
                }
            })
            .collect()
    }

    /// Removes a user that is offline, not on the LAN and not a relay contact,
    /// together with its send count and group memberships.
    pub fn forget_peer(&self, id: &str) -> Result<(), String> {
        {
            let mut d = self.data.lock().unwrap();
            match d.peers.get(id) {
                None => {}
                Some(p) if p.online || id.starts_with("r:") || !p.relay_account.is_empty() => {
                    return Err("접속 중이거나 릴레이 연락처에 있는 사용자는 지울 수 없습니다".into());
                }
                Some(_) => {}
            }
            d.peers.remove(id);
            d.counts.remove(id);
            for g in d.groups.iter_mut() {
                g.members.retain(|m| m != id);
            }
        }
        self.save_data();
        Ok(())
    }

    /// Whether a user seen on the LAN recently (other than this one) uses `name`.
    pub fn name_in_use(&self, name: &str) -> bool {
        let me = self.my_id();
        let name = name.trim();
        self.data.lock().unwrap().peers.values().any(|p| {
            p.id != me
                && p.name.trim().eq_ignore_ascii_case(name)
                && p.seen.is_some_and(|t| t.elapsed() < RECENT)
        })
    }

    pub fn unread(&self) -> usize {
        self.data.lock().unwrap().inbox.iter().filter(|m| !m.read).count()
    }

    pub fn push_sent(&self, s: Sent) {
        {
            let mut d = self.data.lock().unwrap();
            d.sent.insert(0, s);
            d.sent.truncate(INBOX_CAP);
        }
        self.save_data();
    }

    pub fn push_message(&self, m: Message) {
        {
            let mut d = self.data.lock().unwrap();
            d.inbox.insert(0, m);
            d.inbox.truncate(INBOX_CAP);
        }
        self.save_data();
    }
}

/// The LAN devices of relay account `acc`.
pub fn devices_of<'a>(d: &'a Data, acc: &'a str) -> impl Iterator<Item = &'a Peer> + 'a {
    d.peers.values().filter(move |p| !p.id.starts_with("r:") && p.relay_account == acc)
}

/// `peer_view`, with a relay account taking its online state and status from the
/// LAN when the relay does not show it online.
fn account_view(p: &Peer, d: &Data, show_host: bool, relay: bool) -> PeerView {
    let mut v = peer_view(p, d, show_host, relay);
    if v.relay && !v.online {
        if let Some(dev) = devices_of(d, &p.relay_account).filter(|x| x.online).min_by_key(|x| x.status == Status::Dnd) {
            v.online = true;
            v.status = dev.status;
        }
    }
    v
}

fn peer_view(p: &Peer, d: &Data, show_host: bool, relay: bool) -> PeerView {
    PeerView {
        id: p.id.clone(),
        name: p.name.clone(),
        display: p.name.clone(),
        host: if show_host { p.host.clone() } else { String::new() },
        ip: if p.id.starts_with("r:") { "릴레이".into() } else { p.ip.clone() },
        online: p.online,
        status: p.status,
        notify: p.notify,
        count: d.counts.get(&p.id).copied().unwrap_or(0),
        relay: p.id.starts_with("r:"),
        reach: p.online || (relay && !p.relay_account.is_empty()),
        account: p.relay_account.clone(),
    }
}
