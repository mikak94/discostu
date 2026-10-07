//! Who is who.
//!
//! - Every install has a self-signed certificate, made once and kept. Its
//!   SHA-256 fingerprint *is* the peer id: TLS proves the other side holds
//!   the key, so an id can't be claimed by anyone else.
//! - A friends group is a shared random code. Peers prove they know it with
//!   an HMAC over the TLS session's exported keying material, which ties the
//!   proof to that one connection: it can't be replayed or relayed by a
//!   man in the middle, the broker included (it only sees [`Group::id`]).

use std::io;
use std::path::Path;

use ring::rand::SecureRandom;
use ring::{digest, hmac};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};

use crate::wire::PeerId;

pub struct Identity {
    pub cert: CertificateDer<'static>,
    key: Vec<u8>,
    pub id: PeerId,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl Identity {
    pub fn generate() -> Self {
        let ck = rcgen::generate_simple_self_signed(vec!["discostu".into()]).expect("self-signed certificate");
        let cert = ck.cert.der().clone();
        Self { id: peer_id(&cert), key: ck.signing_key.serialize_der(), cert }
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(&Stored { cert: self.cert.to_vec(), key: self.key.clone() }).expect("encode identity")
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let s: Stored = postcard::from_bytes(b).ok()?;
        let cert = CertificateDer::from(s.cert);
        Some(Self { id: peer_id(&cert), key: s.key, cert })
    }

    /// Loads the identity at `path`, creating it on first use.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        if let Ok(b) = std::fs::read(path)
            && let Some(id) = Self::from_bytes(&b)
        {
            return Ok(id);
        }
        let id = Self::generate();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, id.to_bytes())?;
        Ok(id)
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut ctx = digest::Context::new(&digest::SHA256);
    for p in parts {
        ctx.update(p);
    }
    ctx.finish().as_ref().try_into().expect("32 bytes")
}

/// The id a certificate stands for.
pub fn peer_id(cert: &[u8]) -> PeerId {
    let h = sha256(&[cert]);
    u64::from_le_bytes(h[..8].try_into().expect("8 bytes")).max(1)
}

/// Full SHA-256 of a certificate, hex.
pub fn fingerprint(cert: &[u8]) -> String {
    sha256(&[cert]).iter().map(|b| format!("{b:02x}")).collect()
}

pub type GroupId = [u8; 32];

/// A friends group, from its shared code. The empty code is the open LAN
/// group: everyone on the network without a code (the broker refuses it).
#[derive(Clone)]
pub struct Group {
    pub id: GroupId,
    key: [u8; 32],
    pub tag: u64,
    pub code: String,
}

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

impl Group {
    pub fn from_code(code: &str) -> Self {
        let code = normalize(code);
        let c = code.as_bytes();
        let tag = sha256(&[b"discostu/group-tag\0", c]);
        Self {
            id: sha256(&[b"discostu/group-id\0", c]),
            key: sha256(&[b"discostu/group-key\0", c]),
            tag: u64::from_le_bytes(tag[..8].try_into().expect("8 bytes")),
            code,
        }
    }

    pub fn is_open(&self) -> bool {
        self.code.is_empty()
    }

    /// Proof that `prover` knows the code, bound to one TLS session through
    /// its exported keying material.
    pub fn proof(&self, ekm: &[u8], prover: PeerId) -> [u8; 32] {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.key);
        let mut ctx = hmac::Context::with_key(&key);
        ctx.update(ekm);
        ctx.update(&prover.to_le_bytes());
        ctx.sign().as_ref().try_into().expect("32 bytes")
    }

    pub fn verify(&self, ekm: &[u8], prover: PeerId, proof: &[u8; 32]) -> bool {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.key);
        let mut msg = ekm.to_vec();
        msg.extend_from_slice(&prover.to_le_bytes());
        hmac::verify(&key, &msg, proof).is_ok()
    }
}

/// Upper-case, unambiguous letters and digits only, so codes survive being
/// read out loud or retyped with dashes and spaces.
pub fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// A fresh 80-bit code, shown as `XXXX-XXXX-XXXX-XXXX`.
pub fn new_code() -> String {
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut bytes).expect("system randomness");
    let chars: Vec<char> = bytes.iter().map(|b| CODE_ALPHABET[(*b & 31) as usize] as char).collect();
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

/// `XXXX-XXXX-…` for display.
pub fn pretty_code(code: &str) -> String {
    let n = normalize(code);
    let chars: Vec<char> = n.chars().collect();
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

pub fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    ring::rand::SystemRandom::new().fill(&mut b).expect("system randomness");
    u64::from_le_bytes(b).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip_keeps_id() {
        let a = Identity::generate();
        let b = Identity::from_bytes(&a.to_bytes()).unwrap();
        assert_eq!(a.id, b.id);
        assert_ne!(a.id, Identity::generate().id);
    }

    #[test]
    fn codes_normalize_and_prove() {
        let code = new_code();
        assert_eq!(code.len(), 19);
        let g = Group::from_code(&code.to_lowercase().replace('-', " "));
        assert_eq!(g.id, Group::from_code(&code).id);
        let ekm = [7u8; 32];
        let p = g.proof(&ekm, 42);
        assert!(g.verify(&ekm, 42, &p));
        assert!(!g.verify(&ekm, 43, &p), "proof is bound to the prover");
        assert!(!g.verify(&[8u8; 32], 42, &p), "proof is bound to the session");
        assert!(!Group::from_code("other").verify(&ekm, 42, &p));
    }
}
