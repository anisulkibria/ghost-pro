//! Encryption for the few customer secrets the platform keeps.
//!
//! Today that is one thing: the DNS-host API tokens a customer connects once
//! so new domains can be set up without pasting a token every time. They are
//! sealed with AES-256-GCM under a key that lives in a file beside the data,
//! not in the database, so a leaked database dump is not a set of live DNS
//! credentials. Each value is bound to where it belongs (`aad`): a sealed
//! token copied into another account's row does not open.
//!
//! The key file is made on first use. Losing it is not a disaster: the
//! sealed values stop opening, and the customer is asked to reconnect.
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

use crate::{ProError, Result};

/// Where the key lives, relative to the data directory.
pub const KEY_FILE: &str = "secrets/credentials.key";

/// Format marker, so the scheme can change without guessing.
const VERSION: &str = "v1";

pub struct SecretBox {
    key: LessSafeKey,
}

impl SecretBox {
    /// Load the key from `data_dir`, creating it (mode 0600) if absent.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(KEY_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_key(&path)?,
            Err(e) => return Err(ProError::internal(format!("{}: {e}", path.display()))),
        };
        Self::from_key(&bytes)
            .map_err(|_| ProError::Config(format!("{} is not a 32-byte key", path.display())))
    }

    pub fn from_key(bytes: &[u8]) -> Result<Self> {
        let unbound = UnboundKey::new(&AES_256_GCM, bytes)
            .map_err(|_| ProError::Config("credential key must be 32 bytes".into()))?;
        Ok(Self { key: LessSafeKey::new(unbound) })
    }

    /// Encrypt `plaintext`, bound to `aad`.
    pub fn seal(&self, plaintext: &str, aad: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| ProError::internal("no randomness for a nonce"))?;
        let mut buf = plaintext.as_bytes().to_vec();
        self.key
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad.as_bytes()), &mut buf)
            .map_err(|_| ProError::internal("could not encrypt a credential"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&buf);
        Ok(format!("{VERSION}:{}", STANDARD.encode(out)))
    }

    /// Decrypt what [`Self::seal`] made with the same `aad`.
    ///
    /// `None` for anything that does not open — a different key, a different
    /// owner, a damaged value. The caller treats all of those the same way:
    /// the credential has to be connected again.
    pub fn open(&self, sealed: &str, aad: &str) -> Option<String> {
        let data = STANDARD.decode(sealed.strip_prefix(VERSION)?.strip_prefix(':')?).ok()?;
        if data.len() < NONCE_LEN {
            return None;
        }
        let (nonce, rest) = data.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut buf = rest.to_vec();
        let plain = self.key.open_in_place(nonce, Aad::from(aad.as_bytes()), &mut buf).ok()?;
        String::from_utf8(plain.to_vec()).ok()
    }
}

fn create_key(path: &PathBuf) -> Result<Vec<u8>> {
    let mut key = vec![0u8; 32];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| ProError::internal("no randomness for a key"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ProError::internal(format!("{}: {e}", dir.display())))?;
    }
    write_private(path, &key).map_err(|e| ProError::internal(format!("{}: {e}", path.display())))?;
    tracing::info!("created the credential key at {}", path.display());
    Ok(key)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // create_new: two processes racing on first use must not each write a
    // different key and have one silently win.
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed() -> SecretBox {
        SecretBox::from_key(&[7u8; 32]).unwrap()
    }

    #[test]
    fn a_sealed_value_opens_with_the_same_owner() {
        let b = boxed();
        let sealed = b.seal("cf-token-123", "acct1:cloudflare").unwrap();
        assert!(!sealed.contains("cf-token-123"));
        assert_eq!(b.open(&sealed, "acct1:cloudflare").as_deref(), Some("cf-token-123"));
    }

    #[test]
    fn a_sealed_value_does_not_open_for_another_owner_or_key() {
        let b = boxed();
        let sealed = b.seal("secret", "acct1:cloudflare").unwrap();
        assert_eq!(b.open(&sealed, "acct2:cloudflare"), None);
        assert_eq!(SecretBox::from_key(&[8u8; 32]).unwrap().open(&sealed, "acct1:cloudflare"), None);
        assert_eq!(b.open("garbage", "acct1:cloudflare"), None);
    }

    #[test]
    fn the_key_file_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("pro-secretbox-{}", crate::ids::object_id()));
        let first = SecretBox::load_or_create(&dir).unwrap();
        let sealed = first.seal("x", "a").unwrap();
        let second = SecretBox::load_or_create(&dir).unwrap();
        assert_eq!(second.open(&sealed, "a").as_deref(), Some("x"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(KEY_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
