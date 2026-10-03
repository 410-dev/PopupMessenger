//! End-to-end encryption for relayed messages (docs/relay-protocol.md 4.8).
//!
//! A random content key K per message encrypts the body (XChaCha20-Poly1305,
//! AAD = client message id) and, through HKDF, each attachment (STREAM over
//! 64 KiB chunks). K is wrapped for every recipient device with crypto_box,
//! using the sender's static key, so unwrapping also authenticates the sender.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use crypto_box::{PublicKey, SalsaBox, SecretKey};
use hkdf::Hkdf;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const ALG: &str = "x25519-xchacha20poly1305-v1";
pub const CHUNK: usize = 64 * 1024;
const TAG: u64 = 16;

pub type Key = [u8; 32];

/// (secret, public), both base64.
pub fn keypair() -> (String, String) {
    let sk = SecretKey::generate(&mut OsRng);
    (B64.encode(sk.to_bytes()), B64.encode(sk.public_key().as_bytes()))
}

fn key32(b64: &str) -> Result<[u8; 32], String> {
    B64.decode(b64).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| "잘못된 키".to_string())
}

/// First 8 bytes of SHA-256 of a public key, as hex (what users compare).
pub fn fingerprint(pubkey: &str) -> String {
    B64.decode(pubkey)
        .map(|b| Sha256::digest(b)[..8].iter().map(|x| format!("{x:02x}")).collect())
        .unwrap_or_default()
}

/// Encrypts `plain` (text, names, group) for every (device id, public key).
pub fn seal(msg: &str, plain: &Value, recipients: &[(String, String)], my_secret: &str) -> Result<(Value, Key), String> {
    let k: Key = rand::random();
    let sk = SecretKey::from(key32(my_secret)?);
    let cipher = XChaCha20Poly1305::new(&k.into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let data = cipher
        .encrypt(&nonce, Payload { msg: plain.to_string().as_bytes(), aad: msg.as_bytes() })
        .map_err(|_| "암호화 실패".to_string())?;
    let mut keys = serde_json::Map::new();
    for (dev, pk) in recipients {
        let b = SalsaBox::new(&PublicKey::from(key32(pk)?), &sk);
        let n = SalsaBox::generate_nonce(&mut OsRng);
        let mut wrapped = n.to_vec();
        wrapped.extend(b.encrypt(&n, &k[..]).map_err(|_| "암호화 실패".to_string())?);
        keys.insert(dev.clone(), json!(B64.encode(wrapped)));
    }
    // `msg` travels in the clear so the receiver can check the AAD.
    let body = json!({ "enc": ALG, "msg": msg, "keys": keys, "nonce": B64.encode(nonce), "data": B64.encode(data) });
    Ok((body, k))
}

/// Decrypts a body addressed to `my_device`, checking it came from `sender_pub`.
pub fn open(body: &Value, my_device: &str, my_secret: &str, sender_pub: &str) -> Result<(Value, Key), String> {
    let bad = || "복호화할 수 없습니다".to_string();
    let msg = body["msg"].as_str().unwrap_or("");
    let wrapped = B64.decode(body["keys"][my_device].as_str().ok_or_else(bad)?).map_err(|_| bad())?;
    if wrapped.len() < 24 {
        return Err(bad());
    }
    let b = SalsaBox::new(&PublicKey::from(key32(sender_pub)?), &SecretKey::from(key32(my_secret)?));
    let k: Key = b
        .decrypt(wrapped[..24].into(), &wrapped[24..])
        .map_err(|_| bad())?
        .try_into()
        .map_err(|_| bad())?;
    let nonce = B64.decode(body["nonce"].as_str().ok_or_else(bad)?).map_err(|_| bad())?;
    if nonce.len() != 24 {
        return Err(bad());
    }
    let data = B64.decode(body["data"].as_str().ok_or_else(bad)?).map_err(|_| bad())?;
    let plain = XChaCha20Poly1305::new(&k.into())
        .decrypt(nonce.as_slice().into(), Payload { msg: &data, aad: msg.as_bytes() })
        .map_err(|_| bad())?;
    Ok((serde_json::from_slice(&plain).map_err(|_| bad())?, k))
}

/// Ciphertext size of an `n`-byte attachment.
pub fn cipher_len(n: u64) -> u64 {
    let chunks = n.div_ceil(CHUNK as u64).max(1);
    n + chunks * TAG
}

fn file_key(k: &Key, idx: usize) -> (XChaCha20Poly1305, [u8; 19]) {
    let mut okm = [0u8; 32 + 19];
    let mut info = b"file".to_vec();
    info.extend((idx as u32).to_be_bytes());
    Hkdf::<Sha256>::new(None, k).expand(&info, &mut okm).expect("hkdf length");
    let key: [u8; 32] = okm[..32].try_into().unwrap();
    let prefix: [u8; 19] = okm[32..].try_into().unwrap();
    (XChaCha20Poly1305::new(&key.into()), prefix)
}

/// Encrypts attachment `idx` chunk by chunk; call `next` with full CHUNK-sized
/// pieces and `last` with the final (possibly empty) piece.
pub struct FileSealer(Option<EncryptorBE32<XChaCha20Poly1305>>);

impl FileSealer {
    pub fn new(k: &Key, idx: usize) -> Self {
        let (c, p) = file_key(k, idx);
        Self(Some(EncryptorBE32::from_aead(c, &p.into())))
    }
    pub fn next(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.0.as_mut().ok_or("done")?.encrypt_next(chunk).map_err(|_| "암호화 실패".into())
    }
    pub fn last(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.0.take().ok_or("done")?.encrypt_last(chunk).map_err(|_| "암호화 실패".into())
    }
}

pub struct FileOpener(Option<DecryptorBE32<XChaCha20Poly1305>>);

impl FileOpener {
    pub fn new(k: &Key, idx: usize) -> Self {
        let (c, p) = file_key(k, idx);
        Self(Some(DecryptorBE32::from_aead(c, &p.into())))
    }
    pub fn next(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.0.as_mut().ok_or("done")?.decrypt_next(chunk).map_err(|_| "첨부파일 복호화 실패".into())
    }
    pub fn last(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.0.take().ok_or("done")?.decrypt_last(chunk).map_err(|_| "첨부파일 복호화 실패".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_and_file_roundtrip() {
        let (a_sk, a_pk) = keypair();
        let (b_sk, b_pk) = keypair();
        let plain = json!({ "text": "안녕", "names": ["a.txt"] });
        let (body, k) = seal("m1", &plain, &[("devb".into(), b_pk.clone())], &a_sk).unwrap();
        let (got, k2) = open(&body, "devb", &b_sk, &a_pk).unwrap();
        assert_eq!(got, plain);
        assert_eq!(k, k2);
        let mut moved = body.clone();
        moved["msg"] = json!("m2");
        assert!(open(&moved, "devb", &b_sk, &a_pk).is_err(), "AAD binds the message id");
        assert!(open(&body, "devb", &b_sk, &b_pk).is_err(), "wrong sender key");

        for n in [0usize, 1, CHUNK, CHUNK + 1, 3 * CHUNK] {
            let data: Vec<u8> = (0..n).map(|i| i as u8).collect();
            let mut s = FileSealer::new(&k, 2);
            let mut ct = Vec::new();
            let mut chunks = data.chunks(CHUNK).collect::<Vec<_>>();
            let last = chunks.pop().unwrap_or(&[]);
            for c in chunks {
                ct.extend(s.next(c).unwrap());
            }
            ct.extend(s.last(last).unwrap());
            assert_eq!(ct.len() as u64, cipher_len(n as u64));
            let mut o = FileOpener::new(&k, 2);
            let mut out = Vec::new();
            let mut rest = &ct[..];
            while rest.len() > CHUNK + 16 {
                out.extend(o.next(&rest[..CHUNK + 16]).unwrap());
                rest = &rest[CHUNK + 16..];
            }
            out.extend(o.last(rest).unwrap());
            assert_eq!(out, data);
        }
    }
}
