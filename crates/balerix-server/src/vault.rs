//! Encryption at rest for a fleet's secrets (architecture spec D4, disk
//! half; Phase 3 spec §3.3). XChaCha20-Poly1305 with a random 24-byte nonce
//! per write and the fleet name as associated data.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::Rng;

use crate::fsutil::write_private;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

#[derive(Clone)]
pub struct Vault {
    key: [u8; KEY_LEN],
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vault(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: expected a {KEY_LEN}-byte key, found {len} bytes")]
    BadKey { path: PathBuf, len: usize },
    #[error("encryption failed")]
    Seal,
    #[error("ciphertext rejected (wrong key, wrong fleet, or tampered)")]
    Tampered,
}

impl Vault {
    pub fn from_key(key: [u8; KEY_LEN]) -> Self {
        Self { key }
    }

    /// Reads the key at `path`, or creates it (0600) from 32 random bytes.
    pub fn load_or_create(path: &Path) -> Result<Self, VaultError> {
        let io = |e: std::io::Error| VaultError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        };
        match fs::read(path) {
            Ok(bytes) => {
                let key: [u8; KEY_LEN] =
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| VaultError::BadKey {
                            path: path.to_path_buf(),
                            len: bytes.len(),
                        })?;
                Ok(Self { key })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut key = [0u8; KEY_LEN];
                rand::rng().fill_bytes(&mut key);
                write_private(path, &key).map_err(io)?;
                Ok(Self { key })
            }
            Err(e) => Err(io(e)),
        }
    }

    /// `nonce ‖ ciphertext`.
    pub fn seal(&self, aad: &str, plain: &[u8]) -> Result<Vec<u8>, VaultError> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let ct = XChaCha20Poly1305::new((&self.key).into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plain,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| VaultError::Seal)?;
        let mut out = nonce.to_vec();
        out.extend(ct);
        Ok(out)
    }

    pub fn open(&self, aad: &str, blob: &[u8]) -> Result<Vec<u8>, VaultError> {
        if blob.len() < NONCE_LEN {
            return Err(VaultError::Tampered);
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| VaultError::Tampered)?;
        XChaCha20Poly1305::new((&self.key).into())
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| VaultError::Tampered)
    }
}

/// `bytes` random bytes as lowercase hex: the admin token and the per-agent
/// hook secrets.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::os::unix::fs::PermissionsExt;

    fn vault() -> Vault {
        Vault::from_key([7u8; 32])
    }

    #[test]
    fn seals_and_opens_with_the_same_key_and_aad() {
        let blob = vault().seal("payments", b"hello").unwrap();
        assert_ne!(&blob[24..], b"hello");
        assert_eq!(vault().open("payments", &blob).unwrap(), b"hello");
        assert_eq!(vault().open("other", &blob), Err(VaultError::Tampered));
        assert_eq!(
            Vault::from_key([8u8; 32]).open("payments", &blob),
            Err(VaultError::Tampered)
        );
        assert_eq!(
            vault().open("payments", &blob[..10]),
            Err(VaultError::Tampered)
        );
    }

    #[test]
    fn two_seals_of_the_same_plaintext_differ() {
        let a = vault().seal("f", b"x").unwrap();
        let b = vault().seal("f", b"x").unwrap();
        assert_ne!(a, b, "fresh nonce every time");
    }

    #[test]
    fn load_or_create_makes_a_0600_key_and_reloads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server").join("vault.key");
        let v1 = Vault::load_or_create(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&path).unwrap().len(), 32);
        let v2 = Vault::load_or_create(&path).unwrap();
        let blob = v1.seal("f", b"same key").unwrap();
        assert_eq!(v2.open("f", &blob).unwrap(), b"same key");
        std::fs::write(&path, b"short").unwrap();
        assert_eq!(
            Vault::load_or_create(&path).unwrap_err(),
            VaultError::BadKey {
                path: path.clone(),
                len: 5
            }
        );
        assert!(format!("{v1:?}").contains("<redacted>"));
        assert!(!format!("{v1:?}").contains('7'));
    }

    #[test]
    fn random_hex_has_the_requested_width_and_alphabet() {
        let s = random_hex(32);
        assert_eq!(s.len(), 64);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(random_hex(16), random_hex(16));
    }

    proptest! {
        #[test]
        fn open_inverts_seal(plain in proptest::collection::vec(any::<u8>(), 0..512), aad in "[a-z0-9-]{1,20}") {
            let v = vault();
            let blob = v.seal(&aad, &plain).unwrap();
            prop_assert_eq!(v.open(&aad, &blob).unwrap(), plain);
        }

        #[test]
        fn any_flipped_byte_is_rejected(plain in proptest::collection::vec(any::<u8>(), 1..128), idx in any::<prop::sample::Index>()) {
            let v = vault();
            let mut blob = v.seal("f", &plain).unwrap();
            let i = idx.index(blob.len());
            blob[i] ^= 0x01;
            prop_assert_eq!(v.open("f", &blob), Err(VaultError::Tampered));
        }
    }
}
