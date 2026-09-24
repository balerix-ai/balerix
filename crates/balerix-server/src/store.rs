//! `FleetStore` over `fleets/<name>/{fleet.json,secrets.enc}` (Phase 3
//! spec §3.3). Secrets are written first so a crash between the two writes
//! leaves a readable pair.

use std::fs;
use std::path::{Path, PathBuf};

use balerix_core::{FleetName, FleetRecord, FleetSecrets, FleetStore, StoreError};

use crate::fsutil::write_private;
use crate::vault::Vault;

const RECORD: &str = "fleet.json";
const SECRETS: &str = "secrets.enc";

pub struct FileFleetStore {
    root: PathBuf,
    vault: Vault,
}

impl FileFleetStore {
    /// `fleets_dir` is `$XDG_STATE_HOME/balerix/fleets`.
    pub fn new(fleets_dir: PathBuf, vault: Vault) -> Self {
        Self {
            root: fleets_dir,
            vault,
        }
    }

    pub fn fleet_dir(&self, name: &FleetName) -> PathBuf {
        self.root.join(name.as_str())
    }

    fn io(path: &Path, e: impl std::fmt::Display) -> StoreError {
        StoreError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }

    fn corrupt(path: &Path, e: impl std::fmt::Display) -> StoreError {
        StoreError::Corrupt {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }

    fn load_one(&self, dir: &Path) -> Result<Option<(FleetRecord, FleetSecrets)>, StoreError> {
        let record_path = dir.join(RECORD);
        let text = match fs::read_to_string(&record_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(dir = %dir.display(), "fleet directory without a record; skipped");
                return Ok(None);
            }
            Err(e) => return Err(Self::io(&record_path, e)),
        };
        let record: FleetRecord =
            serde_json::from_str(&text).map_err(|e| Self::corrupt(&record_path, e))?;
        let secrets_path = dir.join(SECRETS);
        let secrets = match fs::read(&secrets_path) {
            Ok(blob) => {
                let plain = self
                    .vault
                    .open(record.name(), &blob)
                    .map_err(|e| Self::corrupt(&secrets_path, e))?;
                serde_json::from_slice(&plain).map_err(|e| Self::corrupt(&secrets_path, e))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FleetSecrets::default(),
            Err(e) => return Err(Self::io(&secrets_path, e)),
        };
        Ok(Some((record, secrets)))
    }
}

impl FleetStore for FileFleetStore {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Self::io(&self.root, e)),
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        let mut out = Vec::new();
        for dir in dirs {
            if let Some(pair) = self.load_one(&dir)? {
                out.push(pair);
            }
        }
        Ok(out)
    }

    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError> {
        let dir = self.root.join(record.name());
        fs::create_dir_all(&dir).map_err(|e| Self::io(&dir, e))?;
        let secrets_path = dir.join(SECRETS);
        let plain = serde_json::to_vec(secrets).map_err(|e| Self::io(&secrets_path, e))?;
        let blob = self
            .vault
            .seal(record.name(), &plain)
            .map_err(|e| Self::io(&secrets_path, e))?;
        write_private(&secrets_path, &blob).map_err(|e| Self::io(&secrets_path, e))?;
        let record_path = dir.join(RECORD);
        let text = serde_json::to_vec_pretty(record).map_err(|e| Self::io(&record_path, e))?;
        write_private(&record_path, &text).map_err(|e| Self::io(&record_path, e))
    }

    fn purge(&self, name: &FleetName) -> Result<(), StoreError> {
        let dir = self.fleet_dir(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Self::io(&dir, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{CredentialBundle, FleetSpec};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    fn store(dir: &Path) -> FileFleetStore {
        FileFleetStore::new(dir.join("fleets"), Vault::from_key([1u8; 32]))
    }
    fn record(name: &str) -> FleetRecord {
        FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
            ..Default::default()
        })
    }
    fn secrets() -> FleetSecrets {
        FleetSecrets {
            credentials: CredentialBundle {
                gh_token: Some("gho_SECRET".into()),
                ..CredentialBundle::default()
            },
            hook_secrets: BTreeMap::from([("f/c/a".to_string(), "hook-SECRET".to_string())]),
        }
    }
    fn name(s: &str) -> FleetName {
        s.parse().unwrap()
    }

    #[test]
    fn put_then_load_all_round_trips_records_and_secrets_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert!(
            s.load_all().unwrap().is_empty(),
            "no fleets dir yet is not an error"
        );
        let mut b = record("b");
        b.generation = 4;
        s.put(&b, &secrets()).unwrap();
        s.put(&record("a"), &FleetSecrets::default()).unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0.name(), "a");
        assert_eq!(all[1].0.generation, 4);
        assert_eq!(all[1].1, secrets());
        let dir_b = s.fleet_dir(&name("b"));
        for f in ["fleet.json", "secrets.enc"] {
            assert_eq!(
                std::fs::metadata(dir_b.join(f))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{f}"
            );
        }
        let enc = std::fs::read(dir_b.join("secrets.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&enc).contains("SECRET"));
        // put replaces
        s.put(&b, &FleetSecrets::default()).unwrap();
        assert_eq!(s.load_all().unwrap()[1].1, FleetSecrets::default());
    }

    #[test]
    fn purge_removes_the_whole_fleet_directory() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.put(&record("a"), &FleetSecrets::default()).unwrap();
        std::fs::create_dir_all(s.fleet_dir(&name("a")).join("crews").join("c")).unwrap();
        s.purge(&name("a")).unwrap();
        assert!(!s.fleet_dir(&name("a")).exists());
        s.purge(&name("a")).unwrap(); // idempotent
    }

    #[test]
    fn corrupt_files_and_foreign_keys_are_errors_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.put(&record("a"), &secrets()).unwrap();
        let rec = s.fleet_dir(&name("a")).join("fleet.json");
        std::fs::write(&rec, "{ not json").unwrap();
        let e = s.load_all().unwrap_err();
        assert!(matches!(e, StoreError::Corrupt { .. }));
        assert!(e.to_string().starts_with(&rec.display().to_string()), "{e}");

        s.put(&record("a"), &secrets()).unwrap();
        let other = FileFleetStore::new(dir.path().join("fleets"), Vault::from_key([2u8; 32]));
        let e = other.load_all().unwrap_err();
        assert!(
            e.to_string()
                .ends_with("ciphertext rejected (wrong key, wrong fleet, or tampered)"),
            "{e}"
        );
        assert!(e.to_string().contains("secrets.enc"));
    }

    #[test]
    fn a_directory_without_a_record_is_skipped_and_missing_secrets_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        std::fs::create_dir_all(s.fleet_dir(&name("stray"))).unwrap();
        s.put(&record("a"), &secrets()).unwrap();
        std::fs::remove_file(s.fleet_dir(&name("a")).join("secrets.enc")).unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1, FleetSecrets::default());
    }

    /// Spec L §5: the owner is in `fleet.json`, and only when there is one.
    #[test]
    fn the_owner_survives_the_file_store() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let mut owned = record("a");
        owned.owner = Some("github".into());
        s.put(&owned, &FleetSecrets::default()).unwrap();
        let text = std::fs::read_to_string(s.fleet_dir(&name("a")).join("fleet.json")).unwrap();
        assert!(text.contains("\"owner\": \"github\""), "{text}");
        assert_eq!(s.load_all().unwrap()[0].0.owner.as_deref(), Some("github"));
        s.put(&record("b"), &FleetSecrets::default()).unwrap();
        let text = std::fs::read_to_string(s.fleet_dir(&name("b")).join("fleet.json")).unwrap();
        assert!(
            !text.contains("owner"),
            "an unowned record has no key: {text}"
        );
    }
}
