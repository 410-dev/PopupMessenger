//! What the phone keeps: settings, received and sent messages, and staged uploads.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const INBOX_CAP: usize = 1000;
/// Upper bound on group members kept from a received message.
pub const MAX_GROUP_MEMBERS: usize = 1000;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 16 hex digits, the same shape as desktop device ids.
pub fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// This device's id on the relay (protocol 3).
    pub id: String,
    pub name: String,
    /// "online", "dnd" or "offline" (hidden from others).
    pub status: String,
    /// Show a received message on screen right away (otherwise only the inbox badge).
    pub popup: bool,
    /// Attachment limits other people's apps check before sending here.
    pub max_files: u32,
    pub max_file_mb: u64,
    pub signature: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            status: "online".into(),
            popup: true,
            max_files: 10,
            max_file_mb: 1024,
            signature: String::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    /// Where it was saved; empty for files of sent messages and dropped attachments.
    #[serde(default)]
    pub path: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
}

/// Group details as the desktop app sends them (LAN protocol 3). Member ids are
/// device ids, or "r:<account>" for a whole account.
#[derive(Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub name: String,
    pub members: Vec<Member>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Message {
    pub id: String,
    /// Server message id.
    pub mid: String,
    /// The relay connection it came through (empty: before there were several).
    pub conn: String,
    pub from: String,
    pub from_device: String,
    pub from_name: String,
    /// The sending device's address (protocol 9); how people of other tenancies,
    /// who are not in the contacts, are shown and answered.
    pub from_address: String,
    pub text: String,
    pub files: Vec<FileEntry>,
    pub ts: u64,
    pub read: bool,
    pub note: String,
    pub group: Option<GroupInfo>,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            id: new_id(),
            mid: String::new(),
            conn: String::new(),
            from: String::new(),
            from_device: String::new(),
            from_name: String::new(),
            from_address: String::new(),
            text: String::new(),
            files: vec![],
            ts: now_ms(),
            read: false,
            note: String::new(),
            group: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Recipient {
    /// The relay connection it was sent through (empty: before there were several).
    #[serde(default)]
    pub conn: String,
    /// "<account>", "<account>/<device>" or an address.
    pub to: String,
    pub name: String,
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    /// "relay" (delivered to an online device) or "stored" (kept until it connects).
    #[serde(default)]
    pub via: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Sent {
    pub id: String,
    pub to: Vec<Recipient>,
    pub text: String,
    pub files: Vec<FileEntry>,
    pub ts: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Data {
    pub inbox: Vec<Message>,
    pub sent: Vec<Sent>,
    /// Messages sent per account; puts frequent contacts first.
    pub counts: HashMap<String, u32>,
}

pub struct Store {
    pub dir: PathBuf,
    /// Received attachments.
    pub files: PathBuf,
    /// Attachments being copied in from the webview before sending.
    pub stage: PathBuf,
    pub settings: Mutex<Settings>,
    pub data: Mutex<Data>,
    pub staged: Mutex<HashMap<String, Staged>>,
}

pub struct Staged {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
}

fn load<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn write_atomic(p: &Path, bytes: Vec<u8>) {
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, p);
    }
}

impl Store {
    pub fn open(dir: PathBuf, files: PathBuf, stage: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::create_dir_all(&files);
        // Leftovers from an upload the app was closed during.
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::create_dir_all(&stage);
        let settings: Settings = load(&dir.join("settings.json")).unwrap_or_default();
        let data: Data = load(&dir.join("data.json")).unwrap_or_default();
        let s = Self { dir, files, stage, settings: Mutex::new(settings), data: Mutex::new(data), staged: Mutex::new(HashMap::new()) };
        s.save_settings(); // keeps a newly made device id
        s
    }

    pub fn save_settings(&self) {
        let b = serde_json::to_vec_pretty(&*self.settings.lock().unwrap()).unwrap_or_default();
        write_atomic(&self.dir.join("settings.json"), b);
    }

    pub fn save_data(&self) {
        let b = serde_json::to_vec(&*self.data.lock().unwrap()).unwrap_or_default();
        write_atomic(&self.dir.join("data.json"), b);
    }

    pub fn my_id(&self) -> String {
        self.settings.lock().unwrap().id.clone()
    }

    pub fn push_message(&self, m: Message) {
        {
            let mut d = self.data.lock().unwrap();
            d.inbox.insert(0, m);
            if d.inbox.len() > INBOX_CAP {
                for old in d.inbox.drain(INBOX_CAP..) {
                    remove_files(&old.files);
                }
            }
        }
        self.save_data();
    }

    pub fn push_sent(&self, s: Sent) {
        {
            let mut d = self.data.lock().unwrap();
            for r in s.to.iter().filter(|r| r.ok) {
                let acc = r.to.split('/').next().unwrap_or("").to_string();
                *d.counts.entry(acc).or_default() += 1;
            }
            d.sent.insert(0, s);
            d.sent.truncate(INBOX_CAP);
        }
        self.save_data();
    }

    pub fn unread(&self) -> usize {
        self.data.lock().unwrap().inbox.iter().filter(|m| !m.read).count()
    }
}

/// Deletes a received message's attachments and the folder they were saved in.
pub fn remove_files(files: &[FileEntry]) {
    for f in files.iter().filter(|f| !f.path.is_empty()) {
        let p = Path::new(&f.path);
        let _ = std::fs::remove_file(p);
        if let Some(dir) = p.parent() {
            let _ = std::fs::remove_dir(dir); // only when empty
        }
    }
}

/// A file name that is safe on every platform.
pub fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = base.chars().map(|c| if c.is_control() || r#"<>:"|?*"#.contains(c) { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "file".into() } else { s.chars().take(200).collect() }
}

/// A folder of its own for each received message with attachments.
pub fn message_dir(base: &Path, sender: &str) -> PathBuf {
    let stamp = now_ms() / 1000;
    let sender = sanitize(sender);
    let mut dir = base.join(format!("{sender} {stamp}"));
    let mut n = 2;
    while dir.exists() {
        dir = base.join(format!("{sender} {stamp} ({n})"));
        n += 1;
    }
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// (temporary path, final path) for `name` in `dir`, not overwriting anything.
pub fn reserve_path(dir: &Path, name: &str) -> Option<(PathBuf, PathBuf)> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..10000 {
        let fin = if n == 1 { dir.join(name) } else { dir.join(format!("{stem} ({n}){ext}")) };
        let tmp = fin.with_file_name(format!(".{}.part", fin.file_name()?.to_string_lossy()));
        if !fin.exists() && std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).is_ok() {
            return Some((tmp, fin));
        }
    }
    None
}
