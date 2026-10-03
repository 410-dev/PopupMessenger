//! TLS 1.3 server config. Uses the operator's certificate when given, otherwise
//! a self-signed one created on first start under `<data>/tls`.

use crate::store::sha256_hex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{self, ServerConfig};

pub struct Tls {
    pub config: Arc<ServerConfig>,
    /// SHA-256 of the leaf certificate (hex), for clients pinning a self-signed cert.
    pub fingerprint: String,
    pub self_signed: bool,
}

pub fn load(
    data: &Path,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    names: &[String],
) -> Result<Tls, String> {
    let (cert, key, self_signed) = match (cert, key) {
        (Some(c), Some(k)) => (c, k, false),
        _ => {
            let dir = data.join("tls");
            let (c, k) = (dir.join("cert.pem"), dir.join("key.pem"));
            if !c.exists() || !k.exists() {
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let g = rcgen::generate_simple_self_signed(names.to_vec())
                    .map_err(|e| e.to_string())?;
                std::fs::write(&c, g.cert.pem()).map_err(|e| e.to_string())?;
                std::fs::write(&k, g.key_pair.serialize_pem()).map_err(|e| e.to_string())?;
            }
            (c, k, true)
        }
    };
    let cert_pem = std::fs::read(&cert).map_err(|e| format!("{}: {e}", cert.display()))?;
    let key_pem = std::fs::read(&key).map_err(|e| format!("{}: {e}", key.display()))?;
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("인증서를 읽을 수 없습니다: {e:?}"))?;
    let leaf = chain.first().ok_or("인증서 파일이 비어 있습니다")?;
    let fingerprint = sha256_hex(leaf.as_ref());
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| format!("개인 키를 읽을 수 없습니다: {e:?}"))?;
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| e.to_string())?;
    Ok(Tls {
        config: Arc::new(config),
        fingerprint,
        self_signed,
    })
}
