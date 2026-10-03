//! Logged-on devices and their control connections.
//!
//! Lock order: never take the database lock while holding a hub lock.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};

pub enum Out {
    Frame(Value),
    Close,
}

pub struct Conn {
    pub tx: mpsc::UnboundedSender<Out>,
    /// Wakes the reader so a kicked connection ends at once.
    pub kill: Arc<Notify>,
    pub token: String,
    pub account: String,
    pub ip: String,
    pub tls: bool,
    pub app: String,
    pub since: i64,
}

#[derive(Serialize)]
pub struct ConnView {
    pub device: String,
    pub account: String,
    pub ip: String,
    pub tls: bool,
    pub app: String,
    pub since: i64,
}

#[derive(Default)]
pub struct Hub {
    conns: Mutex<HashMap<String, Conn>>,
    /// Session token -> (device, account).
    sessions: Mutex<HashMap<String, (String, String)>>,
}

impl Hub {
    /// Registers a logged-on device and closes its previous connection, if any.
    pub fn add(&self, device: &str, c: Conn) {
        self.sessions
            .lock()
            .unwrap()
            .insert(c.token.clone(), (device.to_string(), c.account.clone()));
        let old = self.conns.lock().unwrap().insert(device.to_string(), c);
        if let Some(old) = old {
            self.sessions.lock().unwrap().remove(&old.token);
            close(&old, "다른 곳에서 같은 기기로 로그온했습니다");
        }
    }

    /// Forgets the device if `token` is still its current session. Returns whether it was.
    pub fn remove(&self, device: &str, token: &str) -> bool {
        self.sessions.lock().unwrap().remove(token);
        let mut conns = self.conns.lock().unwrap();
        if conns.get(device).is_some_and(|c| c.token == token) {
            conns.remove(device);
            true
        } else {
            false
        }
    }

    pub fn session(&self, token: &str) -> Option<(String, String)> {
        self.sessions.lock().unwrap().get(token).cloned()
    }

    pub fn online(&self) -> HashSet<String> {
        self.conns.lock().unwrap().keys().cloned().collect()
    }

    pub fn is_online(&self, device: &str) -> bool {
        self.conns.lock().unwrap().contains_key(device)
    }

    pub fn send(&self, device: &str, v: Value) {
        if let Some(c) = self.conns.lock().unwrap().get(device) {
            let _ = c.tx.send(Out::Frame(v));
        }
    }

    /// (device, account) of every connection, for fan-out decisions made outside the lock.
    pub fn targets(&self) -> Vec<(String, String)> {
        self.conns
            .lock()
            .unwrap()
            .iter()
            .map(|(d, c)| (d.clone(), c.account.clone()))
            .collect()
    }

    pub fn kick(&self, device: &str, reason: &str) -> bool {
        let c = self.conns.lock().unwrap().remove(device);
        match c {
            Some(c) => {
                self.sessions.lock().unwrap().remove(&c.token);
                close(&c, reason);
                true
            }
            None => false,
        }
    }

    /// Kicks every connection matching `f`; returns how many.
    pub fn kick_where(&self, reason: &str, f: impl Fn(&str, &Conn) -> bool) -> usize {
        let ids: Vec<String> = self
            .conns
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, c)| f(d, c))
            .map(|(d, _)| d.clone())
            .collect();
        ids.iter().filter(|d| self.kick(d, reason)).count()
    }

    pub fn broadcast(&self, v: &Value) {
        for c in self.conns.lock().unwrap().values() {
            let _ = c.tx.send(Out::Frame(v.clone()));
        }
    }

    pub fn snapshot(&self) -> Vec<ConnView> {
        self.conns
            .lock()
            .unwrap()
            .iter()
            .map(|(d, c)| ConnView {
                device: d.clone(),
                account: c.account.clone(),
                ip: c.ip.clone(),
                tls: c.tls,
                app: c.app.clone(),
                since: c.since,
            })
            .collect()
    }
}

fn close(c: &Conn, reason: &str) {
    let _ = c.tx.send(Out::Frame(
        json!({ "t": "event", "e": "kicked", "reason": reason }),
    ));
    let _ = c.tx.send(Out::Close);
    c.kill.notify_one();
}
