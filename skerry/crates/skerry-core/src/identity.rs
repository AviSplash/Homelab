//! Each Skerry install has a long-term X25519 key pair. The public key is the
//! device's identity: the device id is derived from it, and paired peers
//! remember it to recognise each other.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

use crate::transport::NOISE_PARAMS;

#[derive(Clone)]
pub struct Identity {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("public", &hex::encode(self.public)).finish_non_exhaustive()
    }
}

impl Identity {
    pub fn generate() -> Result<Identity> {
        let builder = snow::Builder::new(NOISE_PARAMS.parse()?);
        let kp = builder.generate_keypair()?;
        let mut private = [0u8; 32];
        let mut public = [0u8; 32];
        private.copy_from_slice(&kp.private);
        public.copy_from_slice(&kp.public);
        Ok(Identity { private, public })
    }

    /// Load the key pair from `path`, creating it on first run.
    pub fn load_or_create(path: &Path) -> Result<Identity> {
        if path.exists() {
            let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
            let private = decode32(lines.next().unwrap_or_default()).context("identity file: private key")?;
            let public = decode32(lines.next().unwrap_or_default()).context("identity file: public key")?;
            return Ok(Identity { private, public });
        }
        let id = Identity::generate()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = format!("{}\n{}\n", hex::encode(id.private), hex::encode(id.public));
        write_private(path, text.as_bytes())?;
        Ok(id)
    }

    pub fn device_id(&self) -> String {
        device_id_for(&self.public)
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }
}

fn decode32(s: &str) -> Result<[u8; 32]> {
    let v = hex::decode(s)?;
    if v.len() != 32 {
        bail!("expected 32 bytes, got {}", v.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

pub fn parse_public_key(s: &str) -> Result<[u8; 32]> {
    decode32(s)
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    fs::write(path, data)?;
    Ok(())
}

/// A short, stable identifier for a public key (16 hex characters).
pub fn device_id_for(public: &[u8]) -> String {
    let digest = Sha256::digest(public);
    hex::encode(&digest[..8])
}

/// A human-comparable fingerprint, e.g. `3f2a-9c41-07be-d5e0-11aa`.
pub fn fingerprint(public: &[u8]) -> String {
    let digest = hex::encode(Sha256::digest(public));
    digest.as_bytes()[..20].chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("identity.key");
        let a = Identity::load_or_create(&p).unwrap();
        let b = Identity::load_or_create(&p).unwrap();
        assert_eq!(a.public, b.public);
        assert_eq!(a.private, b.private);
        assert_eq!(a.device_id().len(), 16);
        assert_eq!(a.fingerprint().len(), 24);
    }
}
