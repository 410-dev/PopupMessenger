//! Relay transport shared by the desktop and mobile clients (docs/relay-protocol.md 1):
//! addresses, TLS with fingerprint pinning, the WebSocket bridge, frames and `hello`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

pub const DEFAULT_PORT: u16 = 45443;
pub const PROTO: u64 = 1;
pub const MAX_FRAME: u32 = 8 * 1024 * 1024;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
pub const IO_TIMEOUT: Duration = Duration::from_secs(60);

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Stream = Box<dyn Io>;

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------- address & TLS

/// Where a relay is and how to reach it.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    /// WebSocket (through HTTP proxies such as Cloudflare Tunnel) instead of raw TCP.
    pub ws: bool,
    /// Fixed by the scheme for WebSocket; probed (TLS first) for TCP.
    pub tls: Option<bool>,
    pub host: String,
    pub port: u16,
    /// Request path for WebSocket.
    pub path: String,
}

/// `https://host[/path]` and `wss://…` -> WebSocket over TLS (port 443);
/// `http://…` and `ws://…` -> WebSocket without TLS (port 80);
/// `host`, `host:port`, `relay://host:port`, `[v6]:port` -> raw TCP (port 45443).
pub fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    let bad = || "서버 주소가 올바르지 않습니다".to_string();
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let (ws, tls, rest) = [
        ("https://", true, Some(true)),
        ("wss://", true, Some(true)),
        ("http://", true, Some(false)),
        ("ws://", true, Some(false)),
        ("relay://", false, None),
        ("tcp://", false, None),
        ("tls://", false, Some(true)),
    ]
    .iter()
    .find(|(p, _, _)| lower.starts_with(p))
    .map(|(p, ws, tls)| (*ws, *tls, &s[p.len()..]))
    .unwrap_or((false, None, s));
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let default = match (ws, tls) {
        (true, Some(true)) => 443,
        (true, _) => 80,
        _ => DEFAULT_PORT,
    };
    let (host, port) = if let Some(r) = authority.strip_prefix('[') {
        let (h, after) = r.split_once(']').ok_or_else(bad)?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| bad())?,
            None => default,
        };
        (h.to_string(), port)
    } else if authority.matches(':').count() == 1 {
        let (h, p) = authority.split_once(':').unwrap();
        (h.to_string(), p.parse().map_err(|_| bad())?)
    } else {
        (authority.to_string(), default)
    };
    if host.is_empty() || host.contains(['/', ' ', '@', '?', '#']) || port == 0 {
        return Err(bad());
    }
    let path = if ws { path.to_string() } else { "/".into() };
    if path.contains([' ', '#']) {
        return Err(bad());
    }
    Ok(Endpoint { ws, tls, host, port, path })
}

impl Endpoint {
    pub fn authority(&self) -> String {
        let h = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let default = match (self.ws, self.tls) {
            (true, Some(true)) => 443,
            (true, _) => 80,
            _ => 0,
        };
        if self.port == default { h } else { format!("{h}:{}", self.port) }
    }

    /// The canonical form saved in the config.
    pub fn to_url(&self) -> String {
        if self.ws {
            let scheme = if self.tls == Some(false) { "http" } else { "https" };
            format!("{scheme}://{}{}", self.authority(), if self.path == "/" { "" } else { &self.path })
        } else {
            self.authority()
        }
    }
}

/// Largest WebSocket message this client sends; the server sets it in `hello`.
pub static WS_CHUNK: AtomicUsize = AtomicUsize::new(64 * 1024);

/// Pumps bytes between a WebSocket and a duplex stream the protocol code reads and writes.
fn ws_bridge<S: Io + 'static>(ws: tokio_tungstenite::WebSocketStream<S>) -> tokio::io::DuplexStream {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let (ours, theirs) = tokio::io::duplex(512 * 1024);
    let (mut sink, mut source) = ws.split();
    let (mut rd, mut wr) = tokio::io::split(theirs);
    tauri::async_runtime::spawn(async move {
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            let chunk = WS_CHUNK.load(Ordering::Relaxed).clamp(1024, buf.len());
            match rd.read(&mut buf[..chunk]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(Message::binary(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });
    tauri::async_runtime::spawn(async move {
        while let Some(Ok(m)) = source.next().await {
            match m {
                Message::Binary(b) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = wr.shutdown().await;
    });
    ours
}

#[derive(Clone, Default)]
pub struct Seen {
    pub fingerprint: String,
    pub trusted: bool,
}

/// Accepts a certificate that is valid for the host under the bundled Mozilla
/// roots, or whose fingerprint matches the pin. Without a pin (probing) it
/// accepts anything and records what it saw.
#[derive(Debug)]
struct Verifier {
    pin: Option<String>,
    seen: Arc<Mutex<(String, bool)>>,
    web: Arc<WebPkiServerVerifier>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        ee: &CertificateDer<'_>,
        inter: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        use sha2::Digest;
        let fp = hex(&sha2::Sha256::digest(ee.as_ref()));
        let trusted = self.web.verify_server_cert(ee, inter, name, ocsp, now).is_ok();
        *self.seen.lock().unwrap() = (fp.clone(), trusted);
        match &self.pin {
            Some(p) if !trusted && *p != fp => Err(rustls::Error::General("certificate fingerprint changed".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Connects to `addr`; with TLS, verifies against `pin` (None = accept and report).
/// `tls` applies to raw TCP addresses; WebSocket URLs carry it in their scheme.
pub async fn connect(addr: &str, tls: bool, pin: Option<&str>) -> Result<(Stream, Seen), String> {
    let ep = parse_endpoint(addr)?;
    let tls = ep.tls.unwrap_or(tls);
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((ep.host.as_str(), ep.port)))
        .await
        .map_err(|_| "서버 연결 시간 초과".to_string())?
        .map_err(|e| match e.raw_os_error() {
            Some(11001) | Some(11004) => format!("서버 주소({})를 찾을 수 없습니다. 주소와 DNS 설정을 확인하세요", ep.host),
            _ => format!("서버에 연결할 수 없습니다: {e}"),
        })?;
    let _ = tcp.set_nodelay(true);
    let (io, seen): (Stream, Seen) = if !tls {
        (Box::new(tcp), Seen::default())
    } else {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = Arc::new(RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() });
        let web = WebPkiServerVerifier::builder_with_provider(roots, provider.clone()).build().map_err(|e| e.to_string())?;
        let seen = Arc::new(Mutex::new((String::new(), false)));
        let verifier = Verifier { pin: pin.map(str::to_string), seen: seen.clone(), web, provider: provider.clone() };
        // WebSocket through HTTPS proxies negotiates whatever they offer (TLS 1.2 or 1.3).
        let versions: &[&rustls::SupportedProtocolVersion] =
            if ep.ws { &[&rustls::version::TLS13, &rustls::version::TLS12] } else { &[&rustls::version::TLS13] };
        let cfg = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        let name = ServerName::try_from(ep.host.clone()).map_err(|_| "서버 주소가 올바르지 않습니다".to_string())?;
        let r = timeout(CONNECT_TIMEOUT, tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(name, tcp)).await;
        let (fp, trusted) = seen.lock().unwrap().clone();
        match r {
            Ok(Ok(s)) => (Box::new(s), Seen { fingerprint: fp, trusted }),
            Ok(Err(_)) if pin.is_some_and(|p| !fp.is_empty() && p != fp && !trusted) => {
                return Err(format!("서버 인증서가 바뀌었습니다 (지문 {}…). 관리자에게 확인한 뒤 서버를 다시 연결하세요", &fp[..16]))
            }
            Ok(Err(e)) => return Err(format!("암호화 연결 실패: {e}")),
            Err(_) => return Err("암호화 연결 시간 초과".into()),
        }
    };
    if !ep.ws {
        return Ok((io, seen));
    }
    let url = format!("{}://{}{}", if tls { "wss" } else { "ws" }, ep.authority(), ep.path);
    let (ws, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::client_async(url.as_str(), io))
        .await
        .map_err(|_| "WebSocket 연결 시간 초과".to_string())?
        .map_err(|e| format!("WebSocket 연결 실패 (서버의 WebSocket 설정과 프록시 주소를 확인하세요): {e}"))?;
    Ok((Box::new(ws_bridge(ws)), seen))
}

pub async fn write_frame<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, v: &Value) -> Result<(), String> {
    let b = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    timeout(IO_TIMEOUT, async {
        w.write_all(&(b.len() as u32).to_be_bytes()).await?;
        w.write_all(&b).await?;
        w.flush().await
    })
    .await
    .map_err(|_| "시간 초과".to_string())?
    .map_err(|e| e.to_string())
}

pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(r: &mut R, wait: Duration) -> Result<Value, String> {
    timeout(wait, async {
        let mut len = [0u8; 4];
        r.read_exact(&mut len).await.map_err(|_| "서버 연결이 끊어졌습니다".to_string())?;
        let n = u32::from_be_bytes(len);
        if n > MAX_FRAME {
            return Err("잘못된 데이터".to_string());
        }
        let mut b = vec![0u8; n as usize];
        r.read_exact(&mut b).await.map_err(|_| "서버 연결이 끊어졌습니다".to_string())?;
        serde_json::from_slice(&b).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "서버 응답 시간 초과".to_string())?
}

pub async fn hello(io: &mut Stream) -> Result<Value, String> {
    let app = env!("CARGO_PKG_VERSION");
    write_frame(io, &json!({ "t": "hello", "rid": "h", "proto": PROTO, "app": app })).await?;
    let h = read_frame(io, IO_TIMEOUT).await?;
    if h["t"] != "hello" {
        return Err(h["reason"].as_str().unwrap_or("릴레이 서버가 아닙니다").to_string());
    }
    if let Some(n) = h["transport"]["ws_chunk"].as_u64() {
        WS_CHUNK.store(n as usize, Ordering::Relaxed);
    }
    Ok(h)
}

pub fn err_text(v: &Value) -> String {
    v["reason"].as_str().unwrap_or("서버가 요청을 거부했습니다").to_string()
}

/// Connects without trusting anything yet and returns what the user must confirm.
pub async fn probe(addr: &str) -> Result<Value, String> {
    let ep = parse_endpoint(addr)?;
    let url = ep.to_url();
    let (tls, mut io, seen) = match ep.tls {
        Some(tls) => {
            let (io, seen) = connect(&url, tls, None).await?;
            (tls, io, seen)
        }
        None => match connect(&url, true, None).await {
            Ok((io, seen)) => (true, io, seen),
            Err(tls_err) if tls_err.contains("찾을 수 없습니다") => return Err(tls_err),
            Err(tls_err) => match connect(&url, false, None).await {
                Ok((io, seen)) => (false, io, seen),
                Err(_) => return Err(tls_err),
            },
        },
    };
    let h = hello(&mut io).await?;
    // A plain connection to a TLS-only server gets tls_required right after hello.
    if !tls {
        if let Ok(e) = read_frame(&mut io, Duration::from_millis(500)).await {
            if e["code"] == "tls_required" {
                return Err(if ep.ws {
                    "서버가 이 연결을 암호화된 연결로 인정하지 않습니다. https:// 주소를 쓰거나, 관리 화면에서 \"프록시가 TLS 처리\"를 켜세요".into()
                } else {
                    "서버가 암호화 연결만 받는데 암호화 연결에 실패했습니다".into()
                });
            }
        }
    }
    Ok(json!({
        "url": url, "tls": tls, "ws": ep.ws, "fingerprint": seen.fingerprint, "trusted": seen.trusted,
        "server": h["server"], "policy": h["policy"],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        let e = parse_endpoint("https://pms-relay.hysong.dev").unwrap();
        assert_eq!((e.ws, e.tls, e.host.as_str(), e.port, e.path.as_str()), (true, Some(true), "pms-relay.hysong.dev", 443, "/"));
        assert_eq!(e.to_url(), "https://pms-relay.hysong.dev");
        let e = parse_endpoint("HTTPS://relay.example.com:8443/pm/").unwrap();
        assert_eq!((e.port, e.path.as_str(), e.to_url().as_str()), (8443, "/pm/", "https://relay.example.com:8443/pm/"));
        let e = parse_endpoint("ws://10.0.0.5:45443").unwrap();
        assert_eq!((e.ws, e.tls, e.port), (true, Some(false), 45443));
        assert_eq!(e.to_url(), "http://10.0.0.5:45443");
        let e = parse_endpoint("relay.example.com").unwrap();
        assert_eq!((e.ws, e.tls, e.port, e.to_url().as_str()), (false, None, 45443, "relay.example.com:45443"));
        let e = parse_endpoint("[::1]:9000").unwrap();
        assert_eq!(e.to_url(), "[::1]:9000");
        assert!(parse_endpoint("https://").is_err());
        assert!(parse_endpoint("https://user@host").is_err());
    }
}
