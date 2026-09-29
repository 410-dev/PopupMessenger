use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Disk space kept free when accepting attachments.
pub const DISK_MARGIN: u64 = 512 * 1024 * 1024;
const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const INBOX_CAP: usize = 1000;

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

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub id: String,
    pub name: String,
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
}

impl Default for Settings {
    fn default() -> Self {
        let name = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".into());
        Self {
            id: new_id(),
            name,
            notify: true,
            autostart: true,
            max_files: 10,
            max_file_gb: 10.0,
            download_dir: String::new(),
            extra_hosts: String::new(),
            scan_span: 10,
            context_menu: true,
            signature: String::new(),
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
    #[serde(skip)]
    pub online: bool,
    #[serde(skip)]
    pub seen: Option<Instant>,
}

#[derive(Serialize)]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub ip: String,
    pub online: bool,
    pub notify: bool,
    pub count: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub path: String,
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
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Data {
    pub inbox: Vec<Message>,
    /// Sent message count per peer id; drives tray ordering.
    pub counts: HashMap<String, u32>,
    pub peers: HashMap<String, Peer>,
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
    pub scan: tokio::sync::Notify,
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
        Self {
            host: gethostname::gethostname().to_string_lossy().into_owned(),
            dir,
            port,
            settings: Mutex::new(settings),
            data: Mutex::new(data),
            cancels: Mutex::new(HashMap::new()),
            pending: Mutex::new((Vec::new(), None)),
            seq: AtomicU32::new(1),
            scan: tokio::sync::Notify::new(),
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
            online: true,
            notify: s.notify,
            max_files,
            max_file_size,
            free_space,
            ver: 1,
        }
    }

    pub fn peers(&self, online_only: bool) -> Vec<PeerView> {
        let d = self.data.lock().unwrap();
        let mut v: Vec<PeerView> = d
            .peers
            .values()
            .filter(|p| p.online || !online_only)
            .map(|p| PeerView {
                id: p.id.clone(),
                name: p.name.clone(),
                host: p.host.clone(),
                ip: p.ip.clone(),
                online: p.online,
                notify: p.notify,
                count: d.counts.get(&p.id).copied().unwrap_or(0),
            })
            .collect();
        v.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then(b.count.cmp(&a.count))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        v
    }

    pub fn unread(&self) -> usize {
        self.data.lock().unwrap().inbox.iter().filter(|m| !m.read).count()
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
