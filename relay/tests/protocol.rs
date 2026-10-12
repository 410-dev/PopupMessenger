//! End-to-end tests against a real server on loopback ports.

use base64::Engine;
use popup_relay::relay::Io;
use popup_relay::settings::{Auth, Contacts, Encryption, Registration, Settings};
use popup_relay::{frame, janitor, start, store, Config, Running};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const T: Duration = Duration::from_secs(10);

async fn server(f: impl FnOnce(&mut Settings)) -> Running {
    let dir = std::env::temp_dir().join(format!("popup-relay-test-{}", store::new_id()));
    let r = start(Config {
        data_dir: dir,
        relay_bind: "127.0.0.1:0".parse().unwrap(),
        admin_bind: "127.0.0.1:0".parse().unwrap(),
        tls_cert: None,
        tls_key: None,
        tls_names: vec!["localhost".into()],
    })
    .await
    .unwrap();
    let mut s = r.server.settings();
    f(&mut s);
    r.server.set_settings(s);
    r
}

struct Client {
    io: Box<dyn Io>,
    rid: u64,
    events: Vec<Value>,
}

impl Client {
    async fn plain(r: &Running) -> Client {
        let s = TcpStream::connect(r.relay_addr).await.unwrap();
        Client {
            io: Box::new(s),
            rid: 0,
            events: vec![],
        }
    }

    async fn tls(r: &Running) -> Client {
        use tokio_rustls::rustls::pki_types::pem::PemObject;
        use tokio_rustls::rustls::{self, pki_types::CertificateDer, ClientConfig, RootCertStore};
        let pem = std::fs::read(r.server.dir.join("tls/cert.pem")).unwrap();
        let mut roots = RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(&pem) {
            roots.add(c.unwrap()).unwrap();
        }
        let cfg =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        let s = TcpStream::connect(r.relay_addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let t = tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(name, s)
            .await
            .unwrap();
        Client {
            io: Box::new(t),
            rid: 0,
            events: vec![],
        }
    }

    /// Sends a request and returns the reply with the same rid, keeping events aside.
    async fn req(&mut self, mut v: Value) -> Value {
        self.rid += 1;
        let rid = self.rid.to_string();
        v["rid"] = json!(rid);
        frame::write(&mut self.io, &v).await.unwrap();
        loop {
            let f = frame::read(&mut self.io, T)
                .await
                .unwrap()
                .expect("connection closed");
            if f["t"] == "event" {
                self.events.push(f);
            } else if f["rid"] == json!(rid) || f["rid"].is_null() {
                return f;
            }
        }
    }

    async fn hello(&mut self) -> Value {
        self.req(json!({ "t": "hello", "proto": 1, "app": "test" }))
            .await
    }

    async fn next_event(&mut self) -> Value {
        if !self.events.is_empty() {
            return self.events.remove(0);
        }
        frame::read(&mut self.io, T)
            .await
            .unwrap()
            .expect("connection closed")
    }

    /// Skips events until one named `e`.
    async fn wait_event(&mut self, e: &str) -> Value {
        loop {
            let v = self.next_event().await;
            if v["e"] == e {
                return v;
            }
        }
    }
}

fn secret(device: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("secret-of-{device}-0123456789"))
}

fn login(device: &str, user: Option<(&str, &str)>) -> Value {
    let mut v = json!({
        "t": "login", "id": device, "device_secret": secret(device), "pubkey": "",
        "profile": { "name": format!("user-{device}"), "host": "PC", "status": "online", "max_files": 10, "max_file_size": 0 },
    });
    if let Some((u, p)) = user {
        v["user"] = json!(u);
        v["pass"] = json!(p);
    }
    v
}

/// Logs a device on and returns its control connection plus the login reply.
async fn logon(r: &Running, device: &str, user: Option<(&str, &str)>) -> (Client, Value) {
    let mut c = Client::plain(r).await;
    assert_eq!(c.hello().await["t"], "hello");
    let l = c.req(login(device, user)).await;
    (c, l)
}

async fn transfer(r: &Running, session: &str) -> Client {
    let mut c = Client::plain(r).await;
    c.hello().await;
    let ok = c.req(json!({ "t": "auth", "session": session })).await;
    assert_eq!(ok["t"], "ok", "{ok}");
    c
}

async fn send(r: &Running, session: &str, to: &[&str], text: &str, files: &[&[u8]]) -> Value {
    let mut c = transfer(r, session).await;
    let sizes: Vec<Value> = files.iter().map(|f| json!({ "size": f.len() })).collect();
    let ready = c
        .req(json!({ "t": "send", "msg": "m1", "to": to, "files": sizes, "body": { "enc": "none", "text": text, "names": ["a.bin"] } }))
        .await;
    if ready["t"] != "ready" {
        return ready;
    }
    for f in files {
        c.io.write_all(f).await.unwrap();
    }
    c.io.flush().await.unwrap();
    frame::read(&mut c.io, T).await.unwrap().unwrap()
}

#[tokio::test]
async fn store_and_forward_without_accounts() {
    let r = server(|s| {
        s.policy.auth = Auth::None;
        s.policy.registration = Registration::Open;
        s.policy.encryption = Encryption::Plain;
    })
    .await;

    // B registers once, then goes away.
    let (mut b, lb) = logon(&r, "devb", None).await;
    assert_eq!(lb["t"], "login", "{lb}");
    let b_acc = lb["account"].as_str().unwrap().to_string();
    assert_eq!(b.req(json!({ "t": "logout" })).await["t"], "ok");
    drop(b);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut a, la) = logon(&r, "deva", None).await;
    let a_session = la["session"].as_str().unwrap().to_string();
    let contacts = a.req(json!({ "t": "contacts" })).await;
    let entry = contacts["list"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["account"] == json!(b_acc))
        .unwrap()
        .clone();
    assert_eq!(entry["online"], false);
    assert_eq!(
        entry["devices"][0]["host"], "",
        "hosts are hidden by default"
    );

    let file: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let sent = send(&r, &a_session, &[&b_acc], "hello", &[&file]).await;
    assert_eq!(sent["t"], "sent", "{sent}");
    assert_eq!(sent["result"][0]["state"], "stored");

    // A wrong device secret is refused.
    let mut bad = login("devb", None);
    bad["device_secret"] = json!(secret("other"));
    let mut c = Client::plain(&r).await;
    c.hello().await;
    assert_eq!(c.req(bad).await["code"], "auth_failed");

    // B comes back: told about the message, fetches it, acks it.
    let (mut b, lb) = logon(&r, "devb", None).await;
    let ev = b.wait_event("inbox").await;
    assert_eq!(ev["count"], 1);
    let inbox = b.req(json!({ "t": "inbox" })).await;
    let mid = inbox["list"][0]["mid"].as_str().unwrap().to_string();
    assert_eq!(inbox["list"][0]["files"][0]["size"], 300_000);

    let mut t = transfer(&r, lb["session"].as_str().unwrap()).await;
    let hdr = t.req(json!({ "t": "fetch", "mid": mid })).await;
    assert_eq!(hdr["body"]["text"], "hello");
    let mut got = vec![0u8; 300_000];
    t.io.read_exact(&mut got).await.unwrap();
    assert_eq!(got, file);
    assert_eq!(t.req(json!({ "t": "ack", "mid": mid })).await["t"], "ok");
    assert_eq!(
        t.req(json!({ "t": "ack", "mid": mid })).await["code"],
        "not_found"
    );

    let left: i64 = r
        .server
        .unmigrated()
        .db()
        .query_row("SELECT COUNT(*) FROM messages", [], |x| x.get(0))
        .unwrap();
    assert_eq!(left, 0);
    assert!(!store::blob_dir(&r.server.dir, &mid).exists());

    // A sees B come online through an event.
    let mut saw = false;
    for _ in 0..5 {
        let e = a.next_event().await;
        if e["e"] == "contact"
            && e["contact"]["account"] == json!(b_acc)
            && e["contact"]["online"] == true
        {
            saw = true;
            break;
        }
    }
    assert!(saw);
}

#[tokio::test]
async fn multi_device_delivery_and_expiry() {
    let r = server(|s| {
        s.policy.auth = Auth::Password;
        s.policy.registration = Registration::Closed;
        s.policy.encryption = Encryption::Plain;
        s.policy.max_devices = 2;
        s.policy.contacts = Contacts::Lookup;
    })
    .await;

    let mut c = Client::plain(&r).await;
    c.hello().await;
    let mut reg = login("k1", Some(("kim", "password1")));
    reg["t"] = json!("register");
    assert_eq!(c.req(reg.clone()).await["code"], "registration_closed");

    let mut s = r.server.settings();
    s.policy.registration = Registration::Open;
    r.server.set_settings(s);
    let mut c = Client::plain(&r).await;
    c.hello().await;
    let k1 = c.req(reg).await;
    assert_eq!(k1["t"], "login", "{k1}");
    let kim = k1["account"].as_str().unwrap().to_string();

    let (_k2c, k2) = logon(&r, "k2", Some(("kim", "password1"))).await;
    assert_eq!(k2["account"], json!(kim));
    let (_k3c, k3) = logon(&r, "k3", Some(("kim", "password1"))).await;
    assert_eq!(k3["code"], "device_limit");
    drop(_k2c);
    drop(c);

    let mut c = Client::plain(&r).await;
    c.hello().await;
    let mut reg = login("l1", Some(("lee", "password2")));
    reg["t"] = json!("register");
    let l1 = c.req(reg).await;
    let l_session = l1["session"].as_str().unwrap().to_string();

    // Lookup scope: lee only sees kim after looking kim up.
    let before = c.req(json!({ "t": "contacts" })).await;
    assert_eq!(before["list"].as_array().unwrap().len(), 1);
    let found = c.req(json!({ "t": "lookup", "query": "KIM" })).await;
    assert_eq!(found["contact"]["devices"].as_array().unwrap().len(), 2);

    let sent = send(&r, &l_session, &[&kim], "for both devices", &[]).await;
    assert_eq!(sent["result"][0]["devices"], 2);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // k1 acks; the message stays for k2.
    let (mut k1c, k1) = logon(&r, "k1", Some(("kim", "password1"))).await;
    let inbox = k1c.req(json!({ "t": "inbox" })).await;
    let mid = inbox["list"][0]["mid"].as_str().unwrap().to_string();
    let mut t = transfer(&r, k1["session"].as_str().unwrap()).await;
    t.req(json!({ "t": "fetch", "mid": mid })).await;
    assert_eq!(t.req(json!({ "t": "ack", "mid": mid })).await["t"], "ok");
    let pending: i64 = r
        .server
        .unmigrated()
        .db()
        .query_row(
            "SELECT COUNT(*) FROM deliveries WHERE acked IS NULL",
            [],
            |x| x.get(0),
        )
        .unwrap();
    assert_eq!(pending, 1);

    // Retention passes: k2 never fetched, so it gets a per-sender expiry count.
    let swept = janitor::sweep(&r.server, store::now_ms() + 400 * 3_600_000);
    assert_eq!((swept.messages, swept.notices), (1, 1));
    let (mut k2c, _) = logon(&r, "k2", Some(("kim", "password1"))).await;
    let inbox = k2c.req(json!({ "t": "inbox" })).await;
    assert_eq!(inbox["list"].as_array().unwrap().len(), 0);
    assert_eq!(inbox["expired"][0]["count"], 1);
    assert_eq!(inbox["expired"][0]["from_name"], "user-l1");
    k2c.req(json!({ "t": "expired_ack" })).await;
    assert_eq!(
        k2c.req(json!({ "t": "inbox" })).await["expired"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // A registered device logs on with its device secret alone; a new device cannot.
    let (_, l) = logon(&r, "k1", None).await;
    assert_eq!(l["t"], "login", "{l}");
    assert_eq!(l["username"], "kim");
    let (_, l) = logon(&r, "k9", None).await;
    assert_eq!(l["code"], "auth_required");

    // Lockout after repeated wrong passwords.
    for _ in 0..5 {
        let (_, l) = logon(&r, "k1", Some(("kim", "wrong-pass"))).await;
        assert_eq!(l["code"], "auth_failed");
    }
    let (_, l) = logon(&r, "k1", Some(("kim", "password1"))).await;
    assert_eq!(l["code"], "locked");
}

#[tokio::test]
async fn tls_required_and_e2ee() {
    let r = server(|s| {
        s.policy.auth = Auth::None;
        s.policy.registration = Registration::Open;
        s.policy.encryption = Encryption::E2ee;
        s.policy.relay_max_files = 1;
    })
    .await;

    let mut c = Client::plain(&r).await;
    c.hello().await;
    let e = frame::read(&mut c.io, T).await.unwrap().unwrap();
    assert_eq!(e["code"], "tls_required");

    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    let on = |dev: &str| {
        let mut v = login(dev, None);
        v["pubkey"] = json!(key);
        v
    };
    let mut a = Client::tls(&r).await;
    a.hello().await;
    let la = a.req(on("ea")).await;
    assert_eq!(la["t"], "login", "{la}");
    let mut b = Client::tls(&r).await;
    b.hello().await;
    let lb = b.req(on("eb")).await;
    let b_acc = lb["account"].as_str().unwrap();

    let mut t = Client::tls(&r).await;
    t.hello().await;
    t.req(json!({ "t": "auth", "session": la["session"] }))
        .await;
    let plain = t.req(json!({ "t": "send", "msg": "x", "to": [b_acc], "files": [], "body": { "enc": "none", "text": "hi" } })).await;
    assert_eq!(plain["code"], "policy");
    let too_many = t
        .req(json!({ "t": "send", "msg": "x", "to": [b_acc], "files": [{"size": 1}, {"size": 1}], "body": { "enc": "x", "keys": {} } }))
        .await;
    assert_eq!(too_many["code"], "policy");
    let nokey = t.req(json!({ "t": "send", "msg": "x", "to": [b_acc], "files": [], "body": { "enc": "x", "keys": {} } })).await;
    assert_eq!(nokey["code"], "keys");
    let ok = t
        .req(json!({ "t": "send", "msg": "x", "to": [b_acc], "files": [],
                     "body": { "enc": "x", "keys": { "eb": "K-B", "zz": "other" }, "data": "..." } }))
        .await;
    assert_eq!(ok["t"], "ready", "{ok}");
    let sent = frame::read(&mut t.io, T).await.unwrap().unwrap();
    assert_eq!(sent["result"][0]["state"], "delivered");

    b.wait_event("inbox").await;
    let mid = b.req(json!({ "t": "inbox" })).await["list"][0]["mid"].clone();
    let mut tb = Client::tls(&r).await;
    tb.hello().await;
    tb.req(json!({ "t": "auth", "session": lb["session"] }))
        .await;
    let f = tb.req(json!({ "t": "fetch", "mid": mid })).await;
    assert_eq!(
        f["body"]["keys"],
        json!({ "eb": "K-B" }),
        "only the device's own key"
    );

    // Switching the server to password login drops everyone.
    let mut s = r.server.settings();
    s.policy.auth = Auth::Password;
    r.server.set_settings(s);
    a.wait_event("kicked").await;
}

// ---------------------------------------------------------------- admin API

async fn http(
    r: &Running,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
    csrf: bool,
) -> (u16, Vec<String>, Value) {
    let mut s = TcpStream::connect(r.admin_addr).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    if csrf {
        req.push_str("X-Relay-Admin: 1\r\n");
    }
    // Server admins act within a tenancy; unless a test names one, the unmigrated one.
    if !path.contains("tenancy=") && !path.starts_with("/api/tenancies") && !path.starts_with("/api/server") {
        req.push_str("X-Relay-Tenancy: unmigrated\r\n");
    }
    if let Some(c) = cookie {
        req.push_str(&format!("Cookie: {c}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let cookies = head
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .or_else(|| l.strip_prefix("Set-Cookie: "))
        })
        .map(|c| c.split(';').next().unwrap().to_string())
        .collect();
    (
        status,
        cookies,
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn admin_first_login_and_api() {
    let r = server(|s| s.policy.encryption = Encryption::Plain).await;
    let (st, _, v) = http(&r, "GET", "/api/session", None, None, false).await;
    assert_eq!((st, v["setup"].clone()), (200, json!(true)));

    let cred = json!({ "username": "root", "password": "correct horse" });
    let (st, _, _) = http(&r, "POST", "/api/setup", None, Some(cred.clone()), false).await;
    assert_eq!(st, 403, "CSRF header required");
    let (st, cookies, _) = http(&r, "POST", "/api/setup", None, Some(cred.clone()), true).await;
    assert_eq!(st, 200);
    let cookie = cookies[0].clone();
    let (st, _, _) = http(
        &r,
        "POST",
        "/api/setup",
        None,
        Some(json!({ "username": "evil", "password": "another one" })),
        true,
    )
    .await;
    assert_eq!(st, 409, "setup only works once");

    let (st, _, _) = http(&r, "GET", "/api/accounts", None, None, false).await;
    assert_eq!(st, 401);
    let (st, _, v) = http(&r, "GET", "/api/overview", Some(&cookie), None, false).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["policy"]["registration"], "closed",
        "outsider registration is off by default"
    );

    let (st, _, v) = http(
        &r,
        "POST",
        "/api/accounts",
        Some(&cookie),
        Some(json!({ "username": "park", "password": "password9", "max_devices": 10000 })),
        true,
    )
    .await;
    assert_eq!(st, 400, "{v}");
    let (st, _, v) = http(
        &r,
        "POST",
        "/api/accounts",
        Some(&cookie),
        Some(json!({ "username": "park", "password": "password9", "max_devices": 9999 })),
        true,
    )
    .await;
    assert_eq!(st, 200, "{v}");
    let id = v["id"].as_str().unwrap().to_string();

    let (_, la) = logon(&r, "p1", Some(("park", "password9"))).await;
    assert_eq!(la["t"], "login", "{la}");
    let (st, _, _) = http(
        &r,
        "PATCH",
        &format!("/api/accounts/{id}"),
        Some(&cookie),
        Some(json!({ "state": "disabled" })),
        true,
    )
    .await;
    assert_eq!(st, 200);
    let (_, la) = logon(&r, "p1", Some(("park", "password9"))).await;
    assert_eq!(la["code"], "disabled");

    let mut s: Value = http(&r, "GET", "/api/settings", Some(&cookie), None, false)
        .await
        .2["settings"]
        .clone();
    s["policy"]["max_devices"] = json!(123_456);
    let (_, _, v) = http(&r, "PUT", "/api/settings", Some(&cookie), Some(s), true).await;
    assert_eq!(v["settings"]["policy"]["max_devices"], 9999, "hard cap");

    let (_, _, v) = http(&r, "GET", "/api/audit", Some(&cookie), None, false).await;
    assert!(v["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["action"] == "settings"));

    let (st, _, _) = http(&r, "GET", "/", None, None, false).await;
    assert_eq!(st, 200);
}

#[tokio::test]
async fn admin_audit_of_plaintext_messages() {
    let r = server(|s| {
        s.policy.auth = Auth::None;
        s.policy.registration = Registration::Open;
        s.policy.encryption = Encryption::Plain;
        s.archive = true;
        s.archive_files = true;
    })
    .await;
    let (_, cookies, _) = http(&r, "POST", "/api/setup", None, Some(json!({ "username": "root", "password": "correct horse" })), true).await;
    let cookie = cookies[0].clone();

    let (mut b, lb) = logon(&r, "aud-b", None).await;
    let b_acc = lb["account"].as_str().unwrap().to_string();
    let (_a, la) = logon(&r, "aud-a", None).await;
    let sent = send(&r, la["session"].as_str().unwrap(), &[&b_acc], "감사 대상 본문", &[b"attachment-bytes"]).await;
    let mid = sent["mid"].as_str().unwrap().to_string();

    // While stored: the admin can read the body and download the attachment.
    let (st, _, v) = http(&r, "GET", &format!("/api/messages/{mid}"), Some(&cookie), None, false).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["content"]["text"], "감사 대상 본문");
    assert_eq!(v["files"][0]["name"], "a.bin");
    assert_eq!(v["deliveries"][0]["acked"], Value::Null);

    // Delivered and acked: the queue entry is gone but the archive keeps it.
    b.wait_event("inbox").await;
    let mut t = transfer(&r, lb["session"].as_str().unwrap()).await;
    t.req(json!({ "t": "fetch", "mid": mid })).await;
    let mut got = vec![0u8; 16];
    t.io.read_exact(&mut got).await.unwrap();
    t.req(json!({ "t": "ack", "mid": mid })).await;
    let (st, _, _) = http(&r, "GET", &format!("/api/messages/{mid}"), Some(&cookie), None, false).await;
    assert_eq!(st, 404);

    let (_, _, v) = http(&r, "GET", "/api/archive?q=%EA%B0%90%EC%82%AC", Some(&cookie), None, false).await;
    assert_eq!(v["total"], 1, "{v}");
    assert_eq!(v["items"][0]["preview"], "감사 대상 본문");
    assert_eq!(v["items"][0]["recipients"][0]["account"], json!(b_acc));
    let (_, _, v) = http(&r, "GET", "/api/archive?q=nothing-matches", Some(&cookie), None, false).await;
    assert_eq!(v["total"], 0);

    let mut s = TcpStream::connect(r.admin_addr).await.unwrap();
    let req = format!("GET /api/archive/{mid}/files/0?tenancy=unmigrated HTTP/1.1\r\nHost: x\r\nConnection: close\r\nCookie: {cookie}\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("filename*=UTF-8''a.bin"));
    assert!(text.ends_with("attachment-bytes"));

    // Every look is in the audit log.
    let (_, _, v) = http(&r, "GET", "/api/audit", Some(&cookie), None, false).await;
    let actions: Vec<&str> = v["items"].as_array().unwrap().iter().filter_map(|a| a["action"].as_str()).collect();
    for want in ["message_view", "archive_list", "archive_file"] {
        assert!(actions.contains(&want), "{want} missing in {actions:?}");
    }

    // Retention removes archived copies and their files.
    janitor::sweep(&r.server, store::now_ms() + 91 * 86_400_000);
    let n: i64 = r.server.unmigrated().db().query_row("SELECT COUNT(*) FROM archive", [], |x| x.get(0)).unwrap();
    assert_eq!(n, 0);
    assert!(!store::archive_dir(&r.server.dir, &mid).exists());
}

#[tokio::test]
async fn e2ee_messages_are_never_archived() {
    let r = server(|s| {
        s.policy.auth = Auth::None;
        s.policy.registration = Registration::Open;
        s.policy.encryption = Encryption::E2ee;
        s.archive = true;
    })
    .await;
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    let mut b = Client::tls(&r).await;
    b.hello().await;
    let mut lb = login("eb", None);
    lb["pubkey"] = json!(key);
    let lb = b.req(lb).await;
    let mut a = Client::tls(&r).await;
    a.hello().await;
    let mut la = login("ea", None);
    la["pubkey"] = json!(key);
    let la = a.req(la).await;
    let mut t = Client::tls(&r).await;
    t.hello().await;
    t.req(json!({ "t": "auth", "session": la["session"] })).await;
    let ok = t
        .req(json!({ "t": "send", "msg": "x", "to": [lb["account"]], "files": [],
                     "body": { "enc": "x", "msg": "x", "keys": { "eb": "K" }, "data": "secret" } }))
        .await;
    assert_eq!(ok["t"], "ready");
    frame::read(&mut t.io, T).await.unwrap().unwrap();
    let n: i64 = r.server.unmigrated().db().query_row("SELECT COUNT(*) FROM archive", [], |x| x.get(0)).unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------- WebSocket transport

async fn ws_client(r: &Running, https_proxy: bool) -> Client {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let tcp = TcpStream::connect(r.relay_addr).await.unwrap();
    let mut req = "ws://pms-relay.example/".into_client_request().unwrap();
    if https_proxy {
        req.headers_mut().insert("x-forwarded-proto", "https".parse().unwrap());
        req.headers_mut().insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
    }
    let (ws, _) = tokio_tungstenite::client_async(req, tcp).await.unwrap();
    Client { io: Box::new(popup_relay::ws::bridge(ws, 16 * 1024)), rid: 0, events: vec![] }
}

#[tokio::test]
async fn websocket_behind_tls_proxy_streams_large_files() {
    let r = server(|s| {
        s.policy.auth = Auth::None;
        s.policy.registration = Registration::Open;
        s.policy.encryption = Encryption::Tls;
        s.policy.relay_max_file_size = 1 << 30;
        s.ws_chunk_kb = 16;
    })
    .await;

    // Not trusted as TLS until the admin says a proxy terminates it.
    let mut c = ws_client(&r, true).await;
    let h = c.hello().await;
    assert_eq!(h["transport"]["kind"], "ws");
    assert_eq!(h["transport"]["ws_chunk"], 16 * 1024);
    assert_eq!(frame::read(&mut c.io, T).await.unwrap().unwrap()["code"], "tls_required");

    let mut s = r.server.settings();
    s.proxy_tls = true;
    r.server.set_settings(s);
    let mut b = ws_client(&r, true).await;
    assert_eq!(b.hello().await["transport"]["kind"], "ws-proxy");
    let lb = b.req(login("wsb", None)).await;
    assert_eq!(lb["t"], "login", "{lb}");
    let mut a = ws_client(&r, true).await;
    a.hello().await;
    let la = a.req(login("wsa", None)).await;
    let conns = r.server.unmigrated().hub.snapshot();
    assert!(conns.iter().all(|c| c.transport == "ws-proxy" && c.ip == "203.0.113.7"));

    // A 3 MB attachment crosses as ~200 WebSocket messages of 16 KB.
    let file: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut t = ws_client(&r, true).await;
    t.hello().await;
    assert_eq!(t.req(json!({ "t": "auth", "session": la["session"] })).await["t"], "ok");
    let ready = t
        .req(json!({ "t": "send", "msg": "w", "to": [lb["account"]], "files": [{ "size": file.len() }],
                     "body": { "enc": "none", "text": "ws", "names": ["big.bin"] } }))
        .await;
    assert_eq!(ready["t"], "ready", "{ready}");
    t.io.write_all(&file).await.unwrap();
    t.io.flush().await.unwrap();
    let sent = frame::read(&mut t.io, T).await.unwrap().unwrap();
    assert_eq!(sent["result"][0]["state"], "delivered", "{sent}");

    let mid = b.wait_event("inbox").await;
    assert_eq!(mid["count"], 1);
    let mid = b.req(json!({ "t": "inbox" })).await["list"][0]["mid"].clone();
    let mut f = ws_client(&r, true).await;
    f.hello().await;
    f.req(json!({ "t": "auth", "session": lb["session"] })).await;
    f.req(json!({ "t": "fetch", "mid": mid })).await;
    let mut got = vec![0u8; file.len()];
    f.io.read_exact(&mut got).await.unwrap();
    assert!(got == file);
    assert_eq!(f.req(json!({ "t": "ack", "mid": mid })).await["t"], "ok");

    // WebSocket can be switched off entirely.
    let mut s = r.server.settings();
    s.websocket = false;
    r.server.set_settings(s);
    let tcp = TcpStream::connect(r.relay_addr).await.unwrap();
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    assert!(tokio_tungstenite::client_async("ws://x/".into_client_request().unwrap(), tcp).await.is_err());
}

// ---------------------------------------------------------------- user portal & paging

/// Raw HTTP/1.1 request to any address; returns (status, set-cookie values, JSON body).
async fn http_at(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (u16, Vec<String>, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: relay.test\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(c) = cookie {
        req.push_str(&format!("Cookie: {c}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let cookies = head
        .lines()
        .filter_map(|l| l.strip_prefix("set-cookie: ").or_else(|| l.strip_prefix("Set-Cookie: ")))
        .map(|c| c.split(';').next().unwrap().to_string())
        .collect();
    (status, cookies, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn user_portal_self_service() {
    let r = server(|s| {
        s.policy.auth = Auth::Password;
        s.policy.registration = Registration::Open;
        s.policy.encryption = Encryption::Tls;
    })
    .await;
    for (dev, user) in [("pd1", "pat"), ("pq1", "quinn")] {
        let mut c = Client::tls(&r).await;
        c.hello().await;
        let mut reg = login(dev, Some((user, "portal-pass1")));
        reg["t"] = json!("register");
        assert_eq!(c.req(reg).await["t"], "login");
    }
    let mut pd2 = Client::tls(&r).await;
    pd2.hello().await;
    assert_eq!(pd2.req(login("pd2", Some(("pat", "portal-pass1")))).await["t"], "login");

    let csrf = ("X-Relay-User", "1");
    let cred = json!({ "username": "pat", "password": "portal-pass1" });
    // The page is served on the relay port, but plain HTTP must not carry passwords.
    let (st, _, _) = http_at(r.relay_addr, "GET", "/", None, None, &[]).await;
    assert_eq!(st, 200);
    let (st, _, v) = http_at(r.relay_addr, "POST", "/api/me/login", None, Some(cred.clone()), &[csrf]).await;
    assert_eq!(st, 400, "{v}");
    // Behind a TLS proxy the admin trusts, it works.
    let mut s = r.server.settings();
    s.proxy_tls = true;
    r.server.set_settings(s);
    let https = ("X-Forwarded-Proto", "https");
    let (st, _, _) = http_at(r.relay_addr, "POST", "/api/me/login", None, Some(cred.clone()), &[https]).await;
    assert_eq!(st, 403, "CSRF header required");
    let (st, cookies, v) = http_at(r.relay_addr, "POST", "/api/me/login", None, Some(cred), &[csrf, https]).await;
    assert_eq!(st, 200, "{v}");
    let cookie = cookies[0].clone();

    let (st, _, me) = http_at(r.relay_addr, "GET", "/api/me", Some(&cookie), None, &[]).await;
    assert_eq!(st, 200, "{me}");
    assert_eq!(me["account"]["username"], "pat");
    assert_eq!(me["devices"].as_array().unwrap().len(), 2);

    // Someone else's device cannot be touched.
    let (st, _, _) = http_at(r.relay_addr, "DELETE", "/api/me/devices/pq1", Some(&cookie), None, &[csrf]).await;
    assert_eq!(st, 404);
    // Own device: kick, then unregister.
    let (st, _, v) = http_at(r.relay_addr, "POST", "/api/me/devices/pd2/kick", Some(&cookie), None, &[csrf]).await;
    assert_eq!((st, v["ok"].clone()), (200, json!(true)));
    let (st, _, _) = http_at(r.relay_addr, "DELETE", "/api/me/devices/pd2", Some(&cookie), None, &[csrf]).await;
    assert_eq!(st, 200);
    let (_, _, me) = http_at(r.relay_addr, "GET", "/api/me", Some(&cookie), None, &[]).await;
    assert_eq!(me["devices"].as_array().unwrap().len(), 1);

    // Activity: own events only, never what admins did.
    let acc = me["account"]["id"].as_str().unwrap().to_string();
    store::audit_for(&r.server.unmigrated().db(), "admin:root", "account_update", "secret admin note", &acc);
    let (_, _, act) = http_at(r.relay_addr, "GET", "/api/me/activity", Some(&cookie), None, &[]).await;
    let actions: Vec<&str> = act["items"].as_array().unwrap().iter().filter_map(|x| x["action"].as_str()).collect();
    for want in ["register", "device_add", "user_login", "user_device_kick", "user_device_delete"] {
        assert!(actions.contains(&want), "{want} missing in {actions:?}");
    }
    assert!(!act.to_string().contains("secret admin note"));

    // Changing the password ends the other web sessions but keeps this one.
    let other = json!({ "username": "pat", "password": "portal-pass1" });
    let (_, other, _) = http_at(r.relay_addr, "POST", "/api/me/login", None, Some(other), &[csrf, https]).await;
    let change = json!({ "old": "portal-pass1", "new": "portal-pass2" });
    let (st, _, v) = http_at(r.relay_addr, "POST", "/api/me/password", Some(&cookie), Some(change), &[csrf]).await;
    assert_eq!(st, 200, "{v}");
    let (st, _, _) = http_at(r.relay_addr, "GET", "/api/me", Some(&other[0]), None, &[]).await;
    assert_eq!(st, 401);
    let (st, _, _) = http_at(r.relay_addr, "GET", "/api/me", Some(&cookie), None, &[]).await;
    assert_eq!(st, 200);

    // WebSocket clients still work on the same port.
    let mut c = ws_client(&r, true).await;
    assert_eq!(c.hello().await["t"], "hello");

    // The portal can be switched off.
    let mut s = r.server.settings();
    s.user_portal = false;
    r.server.set_settings(s);
    let (st, _, _) = http_at(r.relay_addr, "GET", "/", None, None, &[]).await;
    assert_eq!(st, 404);
}

#[tokio::test]
async fn admin_lists_are_paged_on_the_server() {
    let r = server(|s| s.policy.encryption = Encryption::Plain).await;
    let setup = json!({ "username": "root", "password": "correct horse" });
    let (_, cookies, _) = http(&r, "POST", "/api/setup", None, Some(setup), true).await;
    let cookie = cookies[0].clone();
    {
        let tn = r.server.unmigrated();
        let db = tn.db();
        for i in 0..250i64 {
            let state = if i % 10 == 0 { "pending" } else { "active" };
            db.execute(
                "INSERT INTO accounts (id, username, name, state, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                rusqlite::params![format!("{i:016x}"), format!("user{i:03}"), format!("name{i}"), state, 1_000 + i],
            )
            .unwrap();
            db.execute(
                "INSERT INTO devices (id, account, secret, created, last_seen) VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![format!("dev{i:03}"), format!("{i:016x}"), "x", 1_000 + i],
            )
            .unwrap();
        }
    }
    let get = |path: String| {
        let cookie = cookie.clone();
        let r = &r;
        async move { http(r, "GET", &path, Some(&cookie), None, false).await.2 }
    };
    let v = get("/api/accounts?per=100&page=3".into()).await;
    assert_eq!((v["total"].clone(), v["items"].as_array().unwrap().len()), (json!(250), 50));
    assert_eq!(v["counts"]["pending"], 25);
    let v = get("/api/accounts?per=10&sort=username&dir=asc".into()).await;
    assert_eq!(v["items"][0]["username"], "user000");
    let v = get("/api/accounts?filter=pending&per=10&page=2".into()).await;
    assert_eq!(v["total"], 25);
    assert!(v["items"].as_array().unwrap().iter().all(|a| a["state"] == "pending"));
    let v = get("/api/accounts?q=user12".into()).await;
    assert_eq!(v["total"], 10, "user120..user129");
    let v = get("/api/accounts?sort=x%3BDROP%20TABLE%20accounts&per=10".into()).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 10, "unknown sort keys fall back to the default");

    let v = get("/api/devices?per=50&page=2&sort=id&dir=asc".into()).await;
    assert_eq!(v["items"][0]["id"], "dev050");
    assert_eq!(v["counts"]["all"], 250);
    let v = get(format!("/api/devices?account={:016x}", 7)).await;
    assert_eq!(v["total"], 1);
    let v = get("/api/devices?filter=online".into()).await;
    assert_eq!(v["total"], 0);

    let v = get("/api/audit?per=10&tenancy=".into()).await;
    assert!(v["actions"].as_array().unwrap().iter().any(|a| a == "admin_setup"));
    let v = get("/api/connections".into()).await;
    assert_eq!(v["total"], 0);
    let v = get("/api/messages".into()).await;
    assert_eq!(v["total"], 0);
}

// ---------------------------------------------------------------- device targets & relay receive

#[tokio::test]
async fn device_targets_and_relay_receive_setting() {
    let r = server(|s| {
        s.policy.auth = Auth::Password;
        s.policy.registration = Registration::Open;
        s.policy.encryption = Encryption::Plain;
    })
    .await;
    let mut c = Client::plain(&r).await;
    assert!(c.hello().await["features"].as_array().unwrap().iter().any(|f| f == "device_target"));
    let mut reg = login("m1", Some(("min", "password1")));
    reg["t"] = json!("register");
    let m1 = c.req(reg).await;
    let min = m1["account"].as_str().unwrap().to_string();
    drop(c);
    let (m2c, _) = logon(&r, "m2", Some(("min", "password1"))).await;
    drop(m2c);
    let mut c = Client::plain(&r).await;
    c.hello().await;
    let mut reg = login("o1", Some(("oh", "password2")));
    reg["t"] = json!("register");
    let o1 = c.req(reg).await;
    let o_session = o1["session"].as_str().unwrap().to_string();

    // One device only.
    let to = format!("{min}/m2");
    let sent = send(&r, &o_session, &[&to], "just m2", &[]).await;
    assert_eq!(sent["result"][0]["to"], json!(to), "{sent}");
    assert_eq!(sent["result"][0]["devices"], 1);
    let n: i64 = r.server.unmigrated().db().query_row("SELECT COUNT(*) FROM deliveries WHERE device = 'm1'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);

    // m2 turns relay receive off for itself: offline m2 gets nothing, m1 still does.
    let (mut m2c, _) = logon(&r, "m2", Some(("min", "password1"))).await;
    assert_eq!(m2c.req(json!({ "t": "hold" })).await["mode"], "allow");
    assert_eq!(m2c.req(json!({ "t": "hold", "mode": "device_off" })).await["mode"], "device_off");
    let contact = c.req(json!({ "t": "lookup", "query": "min" })).await["contact"].clone();
    let hold: Vec<bool> = contact["devices"].as_array().unwrap().iter().map(|d| d["hold"].as_bool().unwrap()).collect();
    assert_eq!(hold.iter().filter(|h| !**h).count(), 1, "{contact}");
    // Online, it still receives directly.
    let sent = send(&r, &o_session, &[&to], "m2 online", &[]).await;
    assert_eq!(sent["result"][0]["state"], "delivered", "{sent}");
    drop(m2c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent = send(&r, &o_session, &[&to], "m2 offline", &[]).await;
    assert_eq!(sent["code"], "offline", "{sent}");
    let sent = send(&r, &o_session, &[&min], "whole account", &[]).await;
    assert_eq!(sent["result"][0]["devices"], 1, "only m1 stores: {sent}");

    // All devices off from another device of the account.
    let (mut m1c, _) = logon(&r, "m1", Some(("min", "password1"))).await;
    assert_eq!(m1c.req(json!({ "t": "hold" })).await["mode"], "allow");
    assert_eq!(m1c.req(json!({ "t": "hold", "mode": "all_off" })).await["mode"], "all_off");
    drop(m1c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent = send(&r, &o_session, &[&min], "nobody stores", &[]).await;
    assert_eq!(sent["code"], "offline", "{sent}");
    let (mut m1c, _) = logon(&r, "m1", Some(("min", "password1"))).await;
    assert_eq!(m1c.req(json!({ "t": "hold", "mode": "allow" })).await["mode"], "allow");
    drop(m1c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent = send(&r, &o_session, &[&min], "m1 stores again", &[]).await;
    assert_eq!(sent["result"][0]["devices"], 1, "m2 is still off: {sent}");
}

#[tokio::test]
async fn own_devices_and_contact_freshness() {
    let r = server(|s| {
        s.policy.auth = Auth::Password;
        s.policy.registration = Registration::Open;
        s.policy.encryption = Encryption::Plain;
    })
    .await;
    let mut c = Client::plain(&r).await;
    c.hello().await;
    let mut reg = login("p1", Some(("pia", "password1")));
    reg["t"] = json!("register");
    let p1 = c.req(reg).await;
    let pia = p1["account"].as_str().unwrap().to_string();
    let p1_session = p1["session"].as_str().unwrap().to_string();
    let (_p2c, _) = logon(&r, "p2", Some(("pia", "password1"))).await;

    // My other device, one at a time; never myself or my whole account.
    let sent = send(&r, &p1_session, &[&format!("{pia}/p2")], "to my other PC", &[]).await;
    assert_eq!(sent["result"][0]["devices"], 1, "{sent}");
    let sent = send(&r, &p1_session, &[&format!("{pia}/p1")], "to myself", &[]).await;
    assert_eq!(sent["t"], "err", "{sent}");
    let sent = send(&r, &p1_session, &[&pia], "to my account", &[]).await;
    assert_eq!(sent["t"], "err", "{sent}");

    // Contact entries carry a revision that grows with every change.
    let a = c.req(json!({ "t": "contacts" })).await["list"][0]["updated"].as_i64().unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    c.req(json!({ "t": "profile", "profile": { "name": "Pia", "status": "dnd" } })).await;
    let b = c.req(json!({ "t": "contacts" })).await["list"][0]["updated"].as_i64().unwrap();
    assert!(b > a, "{a} -> {b}");
}

// ---------------------------------------------------------------- tenancies

async fn admin_cookie(r: &Running) -> String {
    let cred = json!({ "username": "root", "password": "correct horse" });
    let (st, cookies, _) = http(r, "POST", "/api/setup", None, Some(cred), true).await;
    assert_eq!(st, 200);
    cookies[0].clone()
}

/// Logs a device on in `tenancy` (as a client with tenancy support).
async fn logon_in(r: &Running, tenancy: &str, device: &str) -> (Client, Value) {
    let mut c = Client::plain(r).await;
    let h = c.req(json!({ "t": "hello", "proto": 1, "app": "test", "tenancy": tenancy })).await;
    assert_eq!(h["t"], "hello", "{h}");
    assert_eq!(h["tenancy"]["id"], tenancy);
    let l = c.req(login(device, None)).await;
    (c, l)
}

/// Creates tenancy `id` whose policy lets devices sign themselves up over plain TCP.
async fn open_tenancy(r: &Running, cookie: &str, id: &str) {
    let body = json!({ "id": id, "name": format!("{id} Inc."), "admin": { "username": "boss", "password": "boss-password" } });
    let (st, _, v) = http(r, "POST", "/api/tenancies", Some(cookie), Some(body), true).await;
    assert_eq!(st, 200, "{v}");
    let path = format!("/api/settings?tenancy={id}");
    let (_, _, v) = http(r, "GET", &path, Some(cookie), None, false).await;
    let mut s = v["settings"].clone();
    s["policy"]["auth"] = json!("none");
    s["policy"]["registration"] = json!("open");
    s["policy"]["encryption"] = json!("plain");
    let (st, _, v) = http(r, "PUT", &path, Some(cookie), Some(s), true).await;
    assert_eq!(st, 200, "{v}");
}

fn open_policy(s: &mut Settings) {
    s.policy.encryption = Encryption::Plain;
    s.policy.auth = Auth::None;
    s.policy.registration = Registration::Open;
}

#[tokio::test]
async fn tenancies_are_isolated() {
    let r = server(open_policy).await;
    let cookie = admin_cookie(&r).await;
    let (st, _, _) = http(&r, "POST", "/api/tenancies", Some(&cookie), Some(json!({ "id": "Bad ID" })), true).await;
    assert_eq!(st, 400);
    open_tenancy(&r, &cookie, "acme").await;
    let (st, _, _) = http(&r, "POST", "/api/tenancies", Some(&cookie), Some(json!({ "id": "acme" })), true).await;
    assert_eq!(st, 400, "IDs are unique");

    // The tenancy's own admin sees only that tenancy, whatever it asks for.
    let cred = json!({ "tenancy": "acme", "username": "boss", "password": "boss-password" });
    let (st, ck, v) = http(&r, "POST", "/api/login", None, Some(cred), true).await;
    assert_eq!(st, 200, "{v}");
    let boss = ck[0].clone();
    let (st, _, _) = http(&r, "GET", "/api/tenancies", Some(&boss), None, false).await;
    assert_eq!(st, 403);
    let (st, _, _) = http(&r, "GET", "/api/server/settings", Some(&boss), None, false).await;
    assert_eq!(st, 403);
    let (_, _, v) = http(&r, "GET", "/api/overview?tenancy=unmigrated", Some(&boss), None, false).await;
    assert_eq!(v["tenancy"]["id"], "acme");
    let wrong = json!({ "tenancy": "unmigrated", "username": "boss", "password": "boss-password" });
    let (st, _, _) = http(&r, "POST", "/api/login", None, Some(wrong), true).await;
    assert_eq!(st, 401, "admins belong to one tenancy");

    let mut c = Client::plain(&r).await;
    let e = c.req(json!({ "t": "hello", "proto": 1, "app": "test", "tenancy": "nope" })).await;
    assert_eq!(e["code"], "unknown_tenancy");

    let (mut a, la) = logon_in(&r, "acme", "ta1").await;
    assert_eq!(la["tenancy"]["id"], "acme", "{la}");
    let (_b, lb) = logon_in(&r, "acme", "ta2").await;
    let (_u, lu) = logon(&r, "u1", None).await;
    assert_eq!(lu["tenancy"]["id"], "unmigrated");
    // The same installation may join several tenancies of one server.
    let (_u_in_acme, l) = logon_in(&r, "acme", "u1").await;
    assert_eq!(l["t"], "login", "{l}");
    assert_ne!(l["account"], lu["account"]);

    let v = a.req(json!({ "t": "contacts" })).await;
    let accounts: Vec<&Value> = v["list"].as_array().unwrap().iter().map(|c| &c["account"]).collect();
    assert!(accounts.contains(&&lb["account"]));
    assert!(!accounts.contains(&&lu["account"]), "other tenancies are invisible");

    let s = lu["session"].as_str().unwrap();
    let v = send(&r, s, &[lb["account"].as_str().unwrap()], "hi", &[]).await;
    assert_eq!(v["code"], "unknown_user", "{v}");
    let v = send(&r, la["session"].as_str().unwrap(), &[lb["account"].as_str().unwrap()], "hi", &[]).await;
    assert_eq!(v["result"][0]["state"], "delivered", "{v}");
}

#[tokio::test]
async fn unmigrated_accounts_move_and_old_clients_follow() {
    let r = server(open_policy).await;
    let cookie = admin_cookie(&r).await;
    open_tenancy(&r, &cookie, "acme").await;

    let (mut u, lu) = logon(&r, "old1", None).await;
    let acc = lu["account"].as_str().unwrap().to_string();
    let (mut u2, l2) = logon(&r, "old2", None).await;
    let v = send(&r, l2["session"].as_str().unwrap(), &[&acc], "before the move", &[b"file-bytes"]).await;
    assert_eq!(v["t"], "sent", "{v}");

    let body = json!({ "ids": [acc], "action": "move", "tenancy": "acme" });
    let (_, _, v) = http(&r, "POST", "/api/accounts/bulk", Some(&cookie), Some(body), true).await;
    assert_eq!(v["count"], 1, "{v}");
    u.wait_event("kicked").await;
    loop {
        let e = u2.wait_event("contact").await;
        if e["contact"]["account"] == json!(acc) && e["contact"]["removed"] == true {
            break;
        }
    }

    // An old client logs on as before and lands in the new tenancy.
    let (mut u, l) = logon(&r, "old1", None).await;
    assert_eq!(l["tenancy"]["id"], "acme", "{l}");
    assert_eq!(l["tenancy"]["moved"], true);
    assert_eq!(l["account"], json!(acc));
    let inbox = u.req(json!({ "t": "inbox" })).await;
    let mid = inbox["list"][0]["mid"].as_str().unwrap().to_string();
    let mut t = transfer(&r, l["session"].as_str().unwrap()).await;
    let f = t.req(json!({ "t": "fetch", "mid": mid })).await;
    assert_eq!(f["body"]["text"], "before the move");
    let mut got = vec![0u8; 10];
    t.io.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"file-bytes");
    // A client with tenancy support names it directly.
    drop(u);
    let (_u, l) = logon_in(&r, "acme", "old1").await;
    assert_eq!(l["account"], json!(acc));
    assert_eq!(l["tenancy"]["moved"], false);

    // A tenancy with users cannot be deleted; deleting them all first works.
    let (st, _, _) = http(&r, "DELETE", "/api/tenancies/acme", Some(&cookie), None, true).await;
    assert_eq!(st, 400);
    let purge = |c: &str| json!({ "confirm": c });
    let (st, _, _) = http(&r, "POST", "/api/tenancies/acme/purge", Some(&cookie), Some(purge("acm")), true).await;
    assert_eq!(st, 400);
    let (_, _, v) = http(&r, "POST", "/api/tenancies/acme/purge", Some(&cookie), Some(purge("acme")), true).await;
    assert_eq!(v["count"], 1, "{v}");
    let (st, _, _) = http(&r, "DELETE", "/api/tenancies/acme", Some(&cookie), None, true).await;
    assert_eq!(st, 200);
    let mut c = Client::plain(&r).await;
    let e = c.req(json!({ "t": "hello", "proto": 1, "app": "test", "tenancy": "acme" })).await;
    assert_eq!(e["code"], "unknown_tenancy");
    let (st, _, _) = http(&r, "DELETE", "/api/tenancies/unmigrated", Some(&cookie), None, true).await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn existing_data_becomes_the_unmigrated_tenancy() {
    // A data folder from before tenancies: one relay.db with admins and settings.
    let dir = std::env::temp_dir().join(format!("popup-relay-test-{}", store::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    {
        let c = store::open(&dir.join("relay.db")).unwrap();
        store::kv_set(&c, "server_id", "0123456789abcdef");
        let mut s = Settings::default();
        s.server_name = "Old relay".into();
        open_policy(&mut s);
        store::kv_set(&c, "settings", &serde_json::to_string(&s).unwrap());
        let hash = popup_relay::relay::hash_password("correct horse").unwrap();
        c.execute(
            "INSERT INTO admins (username, pass, created) VALUES ('root', ?1, 0)",
            [hash],
        )
        .unwrap();
    }
    let r = start(Config {
        data_dir: dir,
        relay_bind: "127.0.0.1:0".parse().unwrap(),
        admin_bind: "127.0.0.1:0".parse().unwrap(),
        tls_cert: None,
        tls_key: None,
        tls_names: vec!["localhost".into()],
    })
    .await
    .unwrap();
    assert_eq!(r.server.id, "0123456789abcdef");
    let mut c = Client::plain(&r).await;
    let h = c.hello().await;
    assert_eq!(h["server"]["name"], "Old relay");
    assert_eq!(h["tenancy"]["id"], "unmigrated");
    assert_eq!(h["policy"]["auth"], "none");
    let cred = json!({ "username": "root", "password": "correct horse" });
    let (st, _, v) = http(&r, "POST", "/api/login", None, Some(cred), true).await;
    assert_eq!(st, 200, "the old admins are server admins now: {v}");
}

/// Changes one tenancy's settings through the admin API.
async fn tenancy_settings(r: &Running, cookie: &str, id: &str, f: impl FnOnce(&mut Value)) {
    let path = format!("/api/settings?tenancy={id}");
    let (_, _, v) = http(r, "GET", &path, Some(cookie), None, false).await;
    let mut s = v["settings"].clone();
    f(&mut s);
    let (st, _, v) = http(r, "PUT", &path, Some(cookie), Some(s), true).await;
    assert_eq!(st, 200, "{v}");
}

#[tokio::test]
async fn departments_addresses_and_cross_tenancy_rules() {
    let r = server(open_policy).await;
    let cookie = admin_cookie(&r).await;
    open_tenancy(&r, &cookie, "acme").await;
    open_tenancy(&r, &cookie, "beta").await;

    let (_a1, l1) = logon_in(&r, "acme", "a1").await;
    let (_a2, l2) = logon_in(&r, "acme", "a2").await;
    let (mut a3, l3) = logon_in(&r, "acme", "a3").await;
    let (mut b1, lb) = logon_in(&r, "beta", "b1").await;
    assert_eq!(l1["address"], json!(format!("{}:a1@acme:localhost", l1["account"].as_str().unwrap())));

    // Departments: a2 and a3 are developers, a3 also in ops.
    let acme = |p: &str| format!("{p}?tenancy=acme");
    for (id, name) in [("dev", "개발팀"), ("ops", "운영팀")] {
        let (st, _, v) = http(&r, "POST", &acme("/api/departments"), Some(&cookie), Some(json!({ "id": id, "name": name })), true).await;
        assert_eq!(st, 200, "{v}");
    }
    let (st, _, _) = http(&r, "POST", &acme("/api/departments"), Some(&cookie), Some(json!({ "id": "Bad Id" })), true).await;
    assert_eq!(st, 400);
    let add = json!({ "add": [l2["account"], l3["account"]] });
    http(&r, "POST", &acme("/api/departments/dev/members"), Some(&cookie), Some(add), true).await;
    let a3acc = l3["account"].as_str().unwrap();
    let body = json!({ "departments": ["dev", "ops"] });
    let (st, _, v) = http(&r, "PATCH", &acme(&format!("/api/accounts/{a3acc}")), Some(&cookie), Some(body), true).await;
    assert_eq!(st, 200, "{v}");
    let (_, _, v) = http(&r, "GET", "/api/accounts?department=ops&tenancy=acme", Some(&cookie), None, false).await;
    assert_eq!(v["total"], 1, "{v}");
    let deps = a3.req(json!({ "t": "departments" })).await;
    assert_eq!(deps["list"].as_array().unwrap().len(), 2, "{deps}");
    let c = a3.req(json!({ "t": "contacts" })).await;
    let me = c["list"].as_array().unwrap().iter().find(|x| x["account"] == l3["account"]).unwrap().clone();
    assert_eq!(me["departments"], json!(["dev", "ops"]));

    // Wildcards within a tenancy: everyone in a department, minus the sender.
    let s1 = l1["session"].as_str().unwrap();
    let v = send(&r, s1, &["*@dev:acme:localhost"], "to developers", &[]).await;
    let got: Vec<&Value> = v["result"].as_array().unwrap().iter().map(|x| &x["account"]).collect();
    assert_eq!(got.len(), 2, "{v}");
    assert!(got.contains(&&l2["account"]) && got.contains(&&l3["account"]));
    let v = send(&r, s1, &["*@ops,nothing:acme:localhost"], "to ops", &[]).await;
    assert_eq!(v["result"].as_array().unwrap().len(), 1, "{v}");
    let r1 = a3.req(json!({ "t": "resolve", "address": "*:a2@*:acme:localhost" })).await;
    assert_eq!(r1["list"].as_array().unwrap().len(), 1, "{r1}");
    assert_eq!(r1["list"][0]["devices"][0]["id"], "a2");

    // Other tenancies: closed until the server and both tenancies allow it.
    let to_beta = format!("{}@beta:localhost", lb["account"].as_str().unwrap());
    let v = send(&r, s1, &[&to_beta], "hello beta", &[]).await;
    assert_eq!(v["code"], "policy", "{v}");
    let (_, _, mut ss) = http(&r, "GET", "/api/server/settings", Some(&cookie), None, false).await;
    ss["settings"]["exchange"]["cross_tenancy"] = json!(true);
    let (st, _, _) = http(&r, "PUT", "/api/server/settings", Some(&cookie), Some(ss["settings"].clone()), true).await;
    assert_eq!(st, 200);
    let v = send(&r, s1, &[&to_beta], "hello beta", &[]).await;
    assert_eq!(v["code"], "policy", "acme does not send yet: {v}");
    tenancy_settings(&r, &cookie, "acme", |s| s["exchange"]["send_local"] = json!(true)).await;
    let v = send(&r, s1, &[&to_beta], "hello beta", &[]).await;
    assert_eq!(v["code"], "policy", "beta does not receive yet: {v}");
    tenancy_settings(&r, &cookie, "beta", |s| s["exchange"]["receive_local"] = json!(true)).await;
    let v = send(&r, s1, &[&to_beta], "hello beta", &[b"cross"]).await;
    assert_eq!(v["result"][0]["state"], "delivered", "{v}");
    assert_eq!(v["result"][0]["tenancy"], "beta");
    b1.wait_event("inbox").await;
    let inbox = b1.req(json!({ "t": "inbox" })).await;
    assert_eq!(inbox["list"][0]["from_address"], json!(format!("{}:a1@acme:localhost", l1["account"].as_str().unwrap())));
    let mid = inbox["list"][0]["mid"].as_str().unwrap().to_string();
    let mut t = transfer(&r, lb["session"].as_str().unwrap()).await;
    let f = t.req(json!({ "t": "fetch", "mid": mid })).await;
    assert_eq!(f["body"]["text"], "hello beta");
    let mut got = vec![0u8; 5];
    t.io.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"cross");
    // The other direction is still closed, and a deny list wins over the rest.
    let v = send(&r, lb["session"].as_str().unwrap(), &[&format!("{}@acme:localhost", l1["account"].as_str().unwrap())], "back", &[]).await;
    assert_eq!(v["code"], "policy", "{v}");
    tenancy_settings(&r, &cookie, "beta", |s| s["exchange"]["deny"] = json!(["acme"])).await;
    let v = send(&r, s1, &[&to_beta], "denied", &[]).await;
    assert_eq!(v["code"], "policy", "{v}");
    // A server exception for one tenancy overrides the server rule.
    tenancy_settings(&r, &cookie, "beta", |s| s["exchange"]["deny"] = json!([])).await;
    let (st, _, _) = http(&r, "PATCH", "/api/tenancies/beta", Some(&cookie), Some(json!({ "limits": { "cross_tenancy": false } })), true).await;
    assert_eq!(st, 200);
    let v = send(&r, s1, &[&to_beta], "excepted", &[]).await;
    assert_eq!(v["code"], "policy", "{v}");

    // Other servers: refused by policy, and unsupported once allowed.
    let far = "kim@corp:far.example";
    let v = send(&r, s1, &[far], "far away", &[]).await;
    assert_eq!(v["code"], "policy", "{v}");
    ss["settings"]["exchange"]["send_remote"] = json!(true);
    http(&r, "PUT", "/api/server/settings", Some(&cookie), Some(ss["settings"].clone()), true).await;
    tenancy_settings(&r, &cookie, "acme", |s| s["exchange"]["send_remote"] = json!(true)).await;
    let v = send(&r, s1, &[far], "far away", &[]).await;
    assert_eq!(v["code"], "unsupported", "{v}");
    let v = send(&r, s1, &["not an address@"], "x", &[]).await;
    assert_eq!(v["code"], "bad_request", "{v}");
}

#[tokio::test]
async fn api_keys_send_and_receive_for_scripts() {
    let r = server(open_policy).await;
    let cookie = admin_cookie(&r).await;
    open_tenancy(&r, &cookie, "acme").await;
    let (_a1, l1) = logon_in(&r, "acme", "a1").await;
    let (_a2, l2) = logon_in(&r, "acme", "a2").await;
    let a1 = l1["account"].as_str().unwrap();
    let a2 = l2["account"].as_str().unwrap();

    let new_key = json!({ "account": a1, "name": "ci", "send": true, "receive": true });
    let (st, _, _) = http(&r, "POST", "/api/keys?tenancy=acme", Some(&cookie), Some(new_key.clone()), true).await;
    assert_eq!(st, 400, "the tenancy does not allow keys yet");
    tenancy_settings(&r, &cookie, "acme", |s| s["api_keys"] = json!(true)).await;
    let (st, _, v) = http(&r, "POST", "/api/keys?tenancy=acme", Some(&cookie), Some(new_key), true).await;
    assert_eq!(st, 200, "{v}");
    let token = v["token"].as_str().unwrap().to_string();
    let key_id = v["id"].as_str().unwrap().to_string();
    assert!(token.starts_with("pm.acme."));
    let bearer = format!("Bearer {token}");
    let auth = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];

    let (st, _, me) = http_at(r.relay_addr, "GET", "/api/v1/me", None, None, &auth).await;
    assert_eq!(st, 200, "{me}");
    assert_eq!(me["address"], json!(format!("{a1}:api-{key_id}@acme:localhost")));
    let bad = [("Authorization", "Bearer pm.acme.nope.nope")];
    let (st, _, _) = http_at(r.relay_addr, "GET", "/api/v1/me", None, None, &bad).await;
    assert_eq!(st, 401);

    // Sending: plain text and an inline attachment, to an account ID.
    let data = base64::engine::general_purpose::STANDARD.encode(b"report");
    let body = json!({ "to": [a2], "text": "from a script", "files": [{ "name": "r.txt", "data": data }] });
    let (st, _, v) = http_at(r.relay_addr, "POST", "/api/v1/send", None, Some(body), &auth).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["result"][0]["state"], "delivered", "{v}");
    let body = json!({ "to": ["nobody"], "text": "x" });
    let (st, _, v) = http_at(r.relay_addr, "POST", "/api/v1/send", None, Some(body), &auth).await;
    assert_eq!((st, v["code"].clone()), (404, json!("unknown_user")), "{v}");

    // Receiving: the key has its own device, so messages to the account wait for it.
    let v = send(&r, l2["session"].as_str().unwrap(), &[a1], "for the script", &[b"payload"]).await;
    assert_eq!(v["result"][0]["devices"], 2, "{v}");
    let (_, _, v) = http_at(r.relay_addr, "GET", "/api/v1/inbox", None, None, &auth).await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["text"], "for the script");
    assert_eq!(items[0]["files"][0]["name"], "a.bin");
    let mid = items[0]["mid"].as_str().unwrap().to_string();
    let mut s = TcpStream::connect(r.relay_addr).await.unwrap();
    let req = format!("GET /api/v1/messages/{mid}/files/0 HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: {bearer}\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    assert!(String::from_utf8_lossy(&out).ends_with("payload"));
    let (st, _, _) = http_at(r.relay_addr, "POST", &format!("/api/v1/messages/{mid}/ack"), None, None, &auth).await;
    assert_eq!(st, 200);
    let (_, _, v) = http_at(r.relay_addr, "GET", "/api/v1/inbox", None, None, &auth).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0);

    // Keys stop working when revoked, or when the tenancy turns them off.
    let (_, _, v) = http(&r, "GET", "/api/keys?tenancy=acme", Some(&cookie), None, false).await;
    assert_eq!(v["items"][0]["name"], "ci", "{v}");
    tenancy_settings(&r, &cookie, "acme", |s| s["api_keys"] = json!(false)).await;
    let (st, _, _) = http_at(r.relay_addr, "GET", "/api/v1/me", None, None, &auth).await;
    assert_eq!(st, 403);
    tenancy_settings(&r, &cookie, "acme", |s| s["api_keys"] = json!(true)).await;
    let (st, _, _) = http(&r, "DELETE", &format!("/api/keys/{key_id}?tenancy=acme"), Some(&cookie), None, true).await;
    assert_eq!(st, 200);
    let (st, _, _) = http_at(r.relay_addr, "GET", "/api/v1/me", None, None, &auth).await;
    assert_eq!(st, 401);
    let c = l2["session"].as_str().unwrap();
    let v = send(&r, c, &[a1], "after revoke", &[]).await;
    assert_eq!(v["result"][0]["devices"], 1, "the key's device is gone: {v}");
}
