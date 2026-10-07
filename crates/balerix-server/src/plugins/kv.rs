//! The per-plugin key/value store (plugins spec §4.1 `kv`): one 0600 file
//! per key under `plugins/<name>/kv/`, written atomically; secret entries
//! sealed by the vault with `<plugin>/<key>` as associated data. The
//! sandbox never sees this directory — the route is the only way in.

use std::fs;
use std::path::{Path, PathBuf};

use balerix_api::check_kv_key;
use balerix_core::AgentName;

use super::PluginError;
use crate::fsutil::write_private;
use crate::vault::Vault;

const PLAIN: u8 = b'p';
const SEALED: u8 = b's';

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
        check_kv_key(key).map_err(PluginError::KvKey)?;
        Ok(self.dir(name).join(key))
    }

    fn io(path: &Path, e: std::io::Error) -> PluginError {
        PluginError::Kv {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }

    /// A key against the tree the other keys made (#14): it is a
    /// directory of other keys (`a` beside `a/b`, `IsADirectory`), or a
    /// path segment of it is a value (`a/b` beside `a`, `NotADirectory`).
    /// These are Linux's errnos (EISDIR, ENOTDIR): elsewhere (macOS
    /// `unlink` of a directory answers EPERM) a clash surfaces as a
    /// storage error, a 500, instead.
    fn collides(e: &std::io::Error) -> bool {
        use std::io::ErrorKind::{IsADirectory, NotADirectory};
        matches!(e.kind(), IsADirectory | NotADirectory)
    }

    /// `put`'s clash: `collides`, or the `AlreadyExists` that
    /// `create_dir_all` answers when the key's parent is a value (`a/b`
    /// beside `a`). Only then: `AlreadyExists` from anything else (a temp
    /// file) is no key's fault.
    fn put_collides(&self, name: &AgentName, key: &str, e: &std::io::Error) -> bool {
        Self::collides(e)
            || (e.kind() == std::io::ErrorKind::AlreadyExists
                && key
                    .match_indices('/')
                    .any(|(i, _)| self.dir(name).join(&key[..i]).is_file()))
    }

    /// Read and delete treat a missing key and a colliding one alike:
    /// either way no value is stored under it.
    fn absent(e: &std::io::Error) -> bool {
        e.kind() == std::io::ErrorKind::NotFound || Self::collides(e)
    }

    fn aad(name: &AgentName, key: &str) -> String {
        format!("{name}/{key}")
    }

    pub fn get(&self, name: &AgentName, key: &str) -> Result<Option<Vec<u8>>, PluginError> {
        let path = self.path(name, key)?;
        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if Self::absent(&e) => return Ok(None),
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
        write_private(&path, &out).map_err(|e| {
            if self.put_collides(name, key, &e) {
                PluginError::KvConflict(key.to_string())
            } else {
                Self::io(&path, e)
            }
        })
    }

    /// `Ok(false)` when there was nothing to delete.
    pub fn delete(&self, name: &AgentName, key: &str) -> Result<bool, PluginError> {
        let path = self.path(name, key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if Self::absent(&e) => Ok(false),
            Err(e) => Err(Self::io(&path, e)),
        }
    }

    /// Every key under the prefix, sorted. `write_private`'s leftover temp
    /// files (`.{name}.tmp~{pid}-{n}`) are never keys, so they are skipped.
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

    /// `write_private`'s leftover temp file is named `.{name}.tmp~{pid}-{n}`.
    /// A leading `.` alone is not enough to tell: `check_kv_key` allows a
    /// segment to start with `.` (only a bare `.` or `..` segment is
    /// rejected), so a key like `._` is a real key, not a temp file. `~`
    /// is outside `check_kv_key`'s alphabet (`[A-Za-z0-9._/-]`), so no
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
        // the grammar itself is `balerix_api::check_kv_key`'s test
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
    fn a_key_that_is_a_directory_or_under_a_value_conflicts_and_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = kv(dir.path(), 1);
        store.put(&flow(), "a", b"v", false).unwrap();
        store.put(&flow(), "b/c", b"v", false).unwrap();
        for key in ["a/x", "a/x/y", "b"] {
            let e = store.put(&flow(), key, b"x", false).unwrap_err();
            assert!(matches!(e, PluginError::KvConflict(_)), "{key}: {e:?}");
            assert_eq!(
                e.to_string(),
                format!(
                    "kv: key {key:?} conflicts with an existing key: a key cannot be both a value and a directory of other keys"
                )
            );
            assert_eq!(store.get(&flow(), key).unwrap(), None, "{key}");
            assert!(!store.delete(&flow(), key).unwrap(), "{key}");
        }
        assert_eq!(
            store.list(&flow(), "").unwrap(),
            vec!["a".to_string(), "b/c".to_string()],
            "nothing written, no temp file left"
        );
        let leftovers = std::fs::read_dir(dir.path().join("plugins/flow/kv"))
            .unwrap()
            .chain(std::fs::read_dir(dir.path().join("plugins/flow/kv/b")).unwrap())
            .count();
        assert_eq!(leftovers, 3, "a, b, b/c only");
    }

    /// Concurrent puts to one key all succeed and leave one whole write:
    /// each has its own temp file, so none takes another's for its own or
    /// reads its `EEXIST` as a key conflict.
    #[test]
    fn concurrent_puts_to_one_key_all_succeed_and_leave_one_whole_value() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(kv(dir.path(), 1));
        for round in 0..20 {
            let writers: Vec<_> = (0..16u8)
                .map(|i| {
                    let store = store.clone();
                    std::thread::spawn(move || {
                        store.put(&flow(), "state/x", &vec![i; 64 << 10], i % 2 == 0)
                    })
                })
                .collect();
            for w in writers {
                w.join()
                    .unwrap()
                    .unwrap_or_else(|e| panic!("round {round}: {e:?}"));
            }
            let v = store.get(&flow(), "state/x").unwrap().unwrap();
            assert_eq!(v.len(), 64 << 10, "round {round}");
            assert!(v.iter().all(|b| *b == v[0]), "round {round}: torn value");
        }
        assert_eq!(
            store.list(&flow(), "").unwrap(),
            vec!["state/x".to_string()],
            "no temp file left"
        );
        let left = std::fs::read_dir(dir.path().join("plugins/flow/kv/state"))
            .unwrap()
            .count();
        assert_eq!(left, 1, "no temp file left on disk");
    }

    #[test]
    fn keys_that_look_like_temp_files_still_list_but_a_planted_stale_temp_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = kv(dir.path(), 1);
        // Neither key contains write_private's `~` marker, so both are real
        // keys per `check_kv_key`, not leftover temp files.
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
            prop_assume!(check_kv_key(&key).is_ok());
            let dir = tempfile::tempdir().unwrap();
            let store = kv(dir.path(), 3);
            store.put(&flow(), &key, &bytes, secret).unwrap();
            prop_assert_eq!(store.get(&flow(), &key).unwrap(), Some(bytes));
            prop_assert_eq!(store.list(&flow(), "").unwrap(), vec![key]);
        }
    }
}
