//! The per-plugin key/value store (plugins spec §4.1 `kv`): one 0600 file
//! per key under `plugins/<name>/kv/`, written atomically; secret entries
//! sealed by the vault with `<plugin>/<key>` as associated data. The
//! sandbox never sees this directory — the route is the only way in.

use std::fs;
use std::path::{Path, PathBuf};

use balerix_core::AgentName;

use super::PluginError;
use crate::fsutil::write_private;
use crate::vault::Vault;

const PLAIN: u8 = b'p';
const SEALED: u8 = b's';
pub const MAX_KEY: usize = 200;

/// `[A-Za-z0-9._/-]{1,200}`, no empty segment, no `.` or `..` segment.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("empty".into());
    }
    if key.len() > MAX_KEY {
        return Err(format!("longer than {MAX_KEY} bytes"));
    }
    if let Some(c) = key
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')))
    {
        return Err(format!("character {c:?} is not in [A-Za-z0-9._/-]"));
    }
    for seg in key.split('/') {
        match seg {
            "" => return Err("empty path segment".into()),
            "." | ".." => return Err(format!("segment {seg:?} is not allowed")),
            _ => {}
        }
    }
    Ok(())
}

pub struct PluginKv {
    state_root: PathBuf,
    vault: Vault,
}

impl PluginKv {
    /// `state_root` is `$XDG_STATE_HOME/balerix/plugins`.
    pub fn new(state_root: PathBuf, vault: Vault) -> Self {
        Self { state_root, vault }
    }

    fn dir(&self, name: &AgentName) -> PathBuf {
        self.state_root.join(name.as_str()).join("kv")
    }

    fn path(&self, name: &AgentName, key: &str) -> Result<PathBuf, PluginError> {
        validate_key(key).map_err(PluginError::KvKey)?;
        Ok(self.dir(name).join(key))
    }

    fn io(path: &Path, e: std::io::Error) -> PluginError {
        PluginError::Kv {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }

    fn aad(name: &AgentName, key: &str) -> String {
        format!("{name}/{key}")
    }

    pub fn get(&self, name: &AgentName, key: &str) -> Result<Option<Vec<u8>>, PluginError> {
        let path = self.path(name, key)?;
        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Self::io(&path, e)),
        };
        match raw.split_first() {
            Some((&PLAIN, rest)) => Ok(Some(rest.to_vec())),
            Some((&SEALED, rest)) => self
                .vault
                .open(&Self::aad(name, key), rest)
                .map(Some)
                .map_err(|e| PluginError::Kv {
                    path,
                    message: e.to_string(),
                }),
            _ => Err(PluginError::Kv {
                path,
                message: "unknown entry format".into(),
            }),
        }
    }

    pub fn put(
        &self,
        name: &AgentName,
        key: &str,
        bytes: &[u8],
        secret: bool,
    ) -> Result<(), PluginError> {
        let path = self.path(name, key)?;
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if secret {
            out.push(SEALED);
            let sealed =
                self.vault
                    .seal(&Self::aad(name, key), bytes)
                    .map_err(|e| PluginError::Kv {
                        path: path.clone(),
                        message: e.to_string(),
                    })?;
            out.extend_from_slice(&sealed);
        } else {
            out.push(PLAIN);
            out.extend_from_slice(bytes);
        }
        write_private(&path, &out).map_err(|e| Self::io(&path, e))
    }

    /// `Ok(false)` when there was nothing to delete.
    pub fn delete(&self, name: &AgentName, key: &str) -> Result<bool, PluginError> {
        let path = self.path(name, key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Self::io(&path, e)),
        }
    }

    /// Every key under the prefix, sorted. `write_private`'s leftover temp
    /// files (`.{name}.tmp~{pid}`) are never keys, so they are skipped.
    pub fn list(&self, name: &AgentName, prefix: &str) -> Result<Vec<String>, PluginError> {
        let dir = self.dir(name);
        let mut keys = Vec::new();
        if dir.exists() {
            Self::walk(&dir, &dir, &mut keys)?;
        }
        keys.retain(|k| k.starts_with(prefix));
        keys.sort();
        Ok(keys)
    }

    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), PluginError> {
        for entry in fs::read_dir(dir).map_err(|e| Self::io(dir, e))? {
            let entry = entry.map_err(|e| Self::io(dir, e))?;
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().to_string();
            if Self::is_temp(&file_name) {
                continue;
            }
            if path.is_dir() {
                Self::walk(root, &path, out)?;
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }

    /// `write_private`'s leftover temp file is named `.{name}.tmp~{pid}`.
    /// A leading `.` alone is not enough to tell: `validate_key` allows a
    /// segment to start with `.` (only a bare `.` or `..` segment is
    /// rejected), so a key like `._` is a real key, not a temp file. `~`
    /// is outside `validate_key`'s alphabet (`[A-Za-z0-9._/-]`), so no
    /// valid key segment can ever contain `.tmp~`; the match is exact.
    fn is_temp(file_name: &str) -> bool {
        file_name.starts_with('.') && file_name.contains(".tmp~")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Vault;
    use proptest::prelude::*;
    use std::os::unix::fs::PermissionsExt;

    fn kv(dir: &std::path::Path, key: u8) -> PluginKv {
        PluginKv::new(dir.join("plugins"), Vault::from_key([key; 32]))
    }
    fn flow() -> AgentName {
        "flow".parse().unwrap()
    }

    #[test]
    fn keys_are_validated_before_any_path_is_built() {
        for ok in ["a", "state/f/c/a", "x.y-z_1", "A/B", &"k".repeat(200)] {
            assert_eq!(validate_key(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            "..",
            "a/../b",
            "a b",
            "ä",
            &"k".repeat(201),
        ] {
            assert!(validate_key(bad).is_err(), "{bad:?} accepted");
        }
        let dir = tempfile::tempdir().unwrap();
        let e = kv(dir.path(), 1).get(&flow(), "../x").unwrap_err();
        assert!(e.to_string().starts_with("kv: invalid key:"), "{e}");
        assert!(!dir.path().join("plugins").exists(), "nothing touched");
    }

    #[test]
    fn plain_and_secret_entries_round_trip_and_secrets_are_sealed_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = kv(dir.path(), 1);
        assert_eq!(store.get(&flow(), "state/f/c/a").unwrap(), None);
        store
            .put(&flow(), "state/f/c/a", b"working", false)
            .unwrap();
        store
            .put(&flow(), "token", b"hunter2-SECRET", true)
            .unwrap();
        assert_eq!(
            store.get(&flow(), "state/f/c/a").unwrap().as_deref(),
            Some(&b"working"[..])
        );
        assert_eq!(
            store.get(&flow(), "token").unwrap().as_deref(),
            Some(&b"hunter2-SECRET"[..])
        );
        let file = dir.path().join("plugins/flow/kv/token");
        let raw = std::fs::read(&file).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"SECRET"), "sealed");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // another vault cannot open it; another plugin's key is another aad
        let e = kv(dir.path(), 2).get(&flow(), "token").unwrap_err();
        assert!(e.to_string().contains("ciphertext rejected"), "{e}");
        assert_eq!(
            store.list(&flow(), "").unwrap(),
            vec!["state/f/c/a".to_string(), "token".to_string()]
        );
        assert_eq!(
            store.list(&flow(), "state/").unwrap(),
            vec!["state/f/c/a".to_string()]
        );
        assert!(store.list(&"web".parse().unwrap(), "").unwrap().is_empty());
        store.put(&flow(), "state/f/c/a", b"review", false).unwrap();
        assert_eq!(
            store.get(&flow(), "state/f/c/a").unwrap().as_deref(),
            Some(&b"review"[..])
        );
        assert!(store.delete(&flow(), "token").unwrap());
        assert!(!store.delete(&flow(), "token").unwrap());
        assert_eq!(store.get(&flow(), "token").unwrap(), None);
        assert_eq!(
            std::fs::read_dir(dir.path().join("plugins/flow/kv"))
                .unwrap()
                .count(),
            1,
            "no temp files left"
        );
    }

    #[test]
    fn keys_that_look_like_temp_files_still_list_but_a_planted_stale_temp_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = kv(dir.path(), 1);
        // Neither key contains write_private's `~` marker, so both are real
        // keys per `validate_key`, not leftover temp files.
        store.put(&flow(), ".x.tmp-1", b"a", false).unwrap();
        store.put(&flow(), "a/.tmp-42", b"b", false).unwrap();
        assert_eq!(
            store.list(&flow(), "").unwrap(),
            vec![".x.tmp-1".to_string(), "a/.tmp-42".to_string()]
        );
        assert_eq!(
            store.get(&flow(), ".x.tmp-1").unwrap().as_deref(),
            Some(&b"a"[..])
        );
        assert_eq!(
            store.get(&flow(), "a/.tmp-42").unwrap().as_deref(),
            Some(&b"b"[..])
        );

        // A stale temp file planted by hand, using write_private's own
        // marker (`~`), is excluded from every listing.
        std::fs::write(dir.path().join("plugins/flow/kv/.x.tmp~999"), b"stale").unwrap();
        assert_eq!(
            store.list(&flow(), "").unwrap(),
            vec![".x.tmp-1".to_string(), "a/.tmp-42".to_string()],
            "the planted `.x.tmp~999` is not a key"
        );
    }

    proptest! {
        #[test]
        fn any_valid_key_and_bytes_round_trip(
            key in "[A-Za-z0-9._-]{1,12}(/[A-Za-z0-9._-]{1,12}){0,3}",
            bytes in proptest::collection::vec(any::<u8>(), 0..2048),
            secret in any::<bool>(),
        ) {
            prop_assume!(validate_key(&key).is_ok());
            let dir = tempfile::tempdir().unwrap();
            let store = kv(dir.path(), 3);
            store.put(&flow(), &key, &bytes, secret).unwrap();
            prop_assert_eq!(store.get(&flow(), &key).unwrap(), Some(bytes));
            prop_assert_eq!(store.list(&flow(), "").unwrap(), vec![key]);
        }
    }
}
