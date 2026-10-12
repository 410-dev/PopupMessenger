//! Addresses and what may be sent where.
//!
//! An address is `<id>[:<device>]@[<departments>:]<tenancy>:<server>`, e.g.
//! `john:948asfsdddfgjsa@developers:example:relay.example.com`:
//!
//! - `<id>`: a username (or account ID for accounts without one), or `*` for everyone.
//! - `<device>`: one device of it; missing or `*` for all of them.
//! - `<departments>`: comma-separated department IDs; recipients must be in one of
//!   them. Missing or `*`: any.
//! - `<tenancy>` and `<server>` are never wildcards. `<server>` may carry a port.
//!
//! Messages between tenancies must be allowed by the server (`ServerExchange`,
//! with per-tenancy exceptions in `Limits`) and by both tenancies (`Exchange`).
//! Other servers are not reachable yet: such addresses are checked against the
//! policy and then refused as `unsupported`.

use crate::settings::Exchange;
use crate::tenant::Tenant;
use crate::Server;
use std::fmt;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Address {
    pub user: String,
    pub device: Option<String>,
    pub departments: Vec<String>,
    pub tenancy: String,
    pub server: String,
}

pub fn is_address(s: &str) -> bool {
    s.contains('@')
}

pub fn parse(s: &str) -> Result<Address, String> {
    let bad = || format!("주소 형식이 올바르지 않습니다: {s}");
    let s = s.trim();
    // Usernames may contain '@'; the part after the last one never does.
    let (left, right) = s.rsplit_once('@').ok_or_else(bad)?;
    let (user, device) = match left.split_once(':') {
        Some((u, d)) => (u, Some(d)),
        None => (left, None),
    };
    if user.is_empty() || device.is_some_and(|d| d.is_empty() || d.contains(':')) {
        return Err(bad());
    }
    let mut parts: Vec<String> = right.split(':').map(str::to_string).collect();
    // "<host>:<port>" at the end.
    if parts.len() >= 3 && parts.last().is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        let port = parts.pop().unwrap_or_default();
        let host = parts.pop().unwrap_or_default();
        parts.push(format!("{host}:{port}"));
    }
    let (deps, tenancy, server) = match parts.as_slice() {
        [t, s] => ("", t.as_str(), s.as_str()),
        [d, t, s] => (d.as_str(), t.as_str(), s.as_str()),
        _ => return Err(bad()),
    };
    if tenancy.is_empty() || server.is_empty() || tenancy == "*" || server.contains('*') {
        return Err(bad());
    }
    let departments: Vec<String> = deps
        .split(',')
        .map(|d| d.trim().to_lowercase())
        .filter(|d| !d.is_empty() && d != "*")
        .collect();
    if deps.split(',').any(|d| d.trim() == "*") && !departments.is_empty() {
        return Err(bad());
    }
    Ok(Address {
        user: user.to_string(),
        device: device.filter(|d| *d != "*").map(str::to_string),
        departments,
        tenancy: tenancy.to_lowercase(),
        server: server.to_lowercase(),
    })
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.user)?;
        if let Some(d) = &self.device {
            write!(f, ":{d}")?;
        }
        write!(f, "@")?;
        if !self.departments.is_empty() {
            write!(f, "{}:", self.departments.join(","))?;
        }
        write!(f, "{}:{}", self.tenancy, self.server)
    }
}

/// The address of one device: `<username or account>:<device>@<tenancy>:<server>`.
pub fn device_address(user: &str, device: &str, tenancy: &str, server: &str) -> String {
    format!("{user}:{device}@{tenancy}:{server}")
}

pub struct Refused {
    pub code: &'static str,
    pub reason: String,
}

fn refused(code: &'static str, reason: impl Into<String>) -> Refused {
    Refused {
        code,
        reason: reason.into(),
    }
}

#[derive(Clone, Copy)]
pub enum Way {
    CrossTenancy,
    SendRemote,
    ReceiveRemote,
}

/// Whether the server lets tenancy `t` do `way` (its exception, else the server rule).
pub fn server_allows(server: &Server, t: &Tenant, way: Way) -> bool {
    let s = server.server_settings().exchange;
    let l = t.limits();
    match way {
        Way::CrossTenancy => l.cross_tenancy.unwrap_or(s.cross_tenancy),
        Way::SendRemote => l.send_remote.unwrap_or(s.send_remote),
        Way::ReceiveRemote => l.receive_remote.unwrap_or(s.receive_remote),
    }
}

/// `tenancy:server` against a tenancy's allow and deny lists. A pattern's tenancy
/// may be `*`; a pattern without a server means this server.
pub fn listed(ex: &Exchange, tenancy: &str, server: &str, own: &[String]) -> bool {
    let canon = |s: &str| -> String {
        if own.iter().any(|o| o == s) {
            own[0].clone()
        } else {
            s.to_string()
        }
    };
    let target = canon(server);
    let hit = |p: &String| {
        let (pt, ps) = match p.split_once(':') {
            Some((t, s)) => (t, canon(s)),
            None => (p.as_str(), own[0].clone()),
        };
        (pt == "*" || pt == tenancy) && (ps == "*" || ps == target)
    };
    !ex.deny.iter().any(hit) && (ex.allow.is_empty() || ex.allow.iter().any(hit))
}

/// May `src` send to `dst`, both on this server?
pub fn may_cross(server: &Server, src: &Tenant, dst: &Tenant) -> Result<(), Refused> {
    if src.id == dst.id {
        return Ok(());
    }
    let own = server.domains();
    let (se, de) = (src.tenant_settings().exchange, dst.tenant_settings().exchange);
    if !server_allows(server, src, Way::CrossTenancy) || !server_allows(server, dst, Way::CrossTenancy) {
        return Err(refused("policy", "이 서버는 Tenancy 사이의 쪽지를 허용하지 않습니다"));
    }
    if !se.send_local || !listed(&se, &dst.id, &own[0], &own) {
        return Err(refused("policy", format!("내 Tenancy 정책상 {} Tenancy로 보낼 수 없습니다", dst.id)));
    }
    if !de.receive_local || !listed(&de, &src.id, &own[0], &own) {
        return Err(refused("policy", format!("{} Tenancy가 이 Tenancy의 쪽지를 받지 않습니다", dst.id)));
    }
    Ok(())
}

/// What becomes of a send to another server: refused by policy, or (when allowed)
/// `unsupported` until servers can reach each other.
fn remote(server: &Server, src: &Tenant, a: &Address) -> Refused {
    let own = server.domains();
    let ex = src.tenant_settings().exchange;
    if !server_allows(server, src, Way::SendRemote) {
        return refused("policy", "이 서버는 다른 서버로 쪽지를 보내지 않습니다");
    }
    if !ex.send_remote || !listed(&ex, &a.tenancy, &a.server, &own) {
        return refused("policy", format!("내 Tenancy 정책상 {}:{}로 보낼 수 없습니다", a.tenancy, a.server));
    }
    refused("unsupported", "다른 서버로 보내는 기능은 아직 지원하지 않습니다")
}

/// Accounts an address names: its tenancy and, per account, the device it is
/// limited to (None = all). At most `max` accounts.
pub struct Resolved {
    pub tn: Arc<Tenant>,
    pub accounts: Vec<(String, Option<String>)>,
}

pub fn resolve(server: &Server, src: &Tenant, a: &Address, max: usize) -> Result<Resolved, Refused> {
    let own = server.domains();
    if !own.contains(&a.server) {
        return Err(remote(server, src, a));
    }
    let tn = server
        .tenant(&a.tenancy)
        .ok_or_else(|| refused("unknown_user", format!("이 서버에 {} Tenancy가 없습니다", a.tenancy)))?;
    may_cross(server, src, &tn)?;
    let c = tn.db();
    let ids: Vec<String> = if a.user == "*" {
        c.prepare("SELECT id FROM accounts WHERE state = 'active' ORDER BY id")
            .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect())
            .unwrap_or_default()
    } else {
        c.query_row(
            "SELECT id FROM accounts WHERE (username = ?1 OR id = ?1) AND state = 'active'",
            [&a.user],
            |r| r.get(0),
        )
        .map(|id| vec![id])
        .unwrap_or_default()
    };
    let ids: Vec<String> = if a.departments.is_empty() {
        ids
    } else {
        ids.into_iter()
            .filter(|id| {
                crate::store::departments_of(&c, id)
                    .iter()
                    .any(|d| a.departments.contains(d))
            })
            .collect()
    };
    // A device names one installation: only the account it belongs to.
    let ids: Vec<String> = match &a.device {
        None => ids,
        Some(d) => ids
            .into_iter()
            .filter(|id| {
                c.query_row("SELECT 1 FROM devices WHERE id = ?1 AND account = ?2", [d, id], |_| Ok(()))
                    .is_ok()
            })
            .collect(),
    };
    drop(c);
    if ids.is_empty() {
        return Err(refused("unknown_user", format!("받는 사람을 찾을 수 없습니다: {a}")));
    }
    if ids.len() > max {
        return Err(refused("policy", format!("받는 사람이 너무 많습니다 (최대 {max}명)")));
    }
    Ok(Resolved {
        accounts: ids.into_iter().map(|id| (id, a.device.clone())).collect(),
        tn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addresses() {
        let a = parse("john:948asfsdddfgjsa@developers:example:relay.example.com").unwrap();
        assert_eq!(a.user, "john");
        assert_eq!(a.device.as_deref(), Some("948asfsdddfgjsa"));
        assert_eq!(a.departments, vec!["developers"]);
        assert_eq!((a.tenancy.as_str(), a.server.as_str()), ("example", "relay.example.com"));
        assert_eq!(a.to_string(), "john:948asfsdddfgjsa@developers:example:relay.example.com");

        let a = parse("*@dev,ops:example:relay.example.com:45443").unwrap();
        assert_eq!((a.user.as_str(), a.device.clone()), ("*", None));
        assert_eq!(a.departments, vec!["dev", "ops"]);
        assert_eq!(a.server, "relay.example.com:45443");

        let a = parse("kim@corp.kr:*@*:acme:relay.test").unwrap();
        assert_eq!((a.user.as_str(), a.device.clone(), a.departments.len()), ("kim@corp.kr", None, 0));

        for bad in ["john", "john@relay", "@acme:relay", "john@*:relay", "john@acme:*", "john:@acme:relay", "a@b:c:d:e"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn allow_and_deny_lists() {
        let own = vec!["relay.test".to_string(), "localhost".to_string()];
        let ex = |allow: &[&str], deny: &[&str]| Exchange {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Exchange::default()
        };
        assert!(listed(&ex(&[], &[]), "acme", "relay.test", &own));
        assert!(listed(&ex(&["acme"], &[]), "acme", "localhost", &own));
        assert!(!listed(&ex(&["acme"], &[]), "other", "relay.test", &own));
        assert!(listed(&ex(&["*:far.example"], &[]), "x", "far.example", &own));
        assert!(!listed(&ex(&["*:far.example"], &["x:far.example"]), "x", "far.example", &own));
        assert!(!listed(&ex(&[], &["*:relay.test"]), "acme", "localhost", &own));
    }
}
