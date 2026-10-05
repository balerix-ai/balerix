//! Managed fleets in Kubernetes mode (Spec O §23.3): a plugin's fleet
//! requests, kept for the operator to write as Fleets. One file per row,
//! `<dir>/<name>.json`, so a Daemon restart still lists what the operator
//! has not applied yet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use balerix_api::{DownQuery, ManagedFleet};

pub struct ManagedStore {
    dir: PathBuf,
    rows: Mutex<BTreeMap<String, ManagedFleet>>,
}

impl ManagedStore {
    /// Every `*.json` under `dir` (`<state>/managed`). A file that does
    /// not parse is logged and skipped: one bad row must not cost the
    /// operator the others.
    pub fn load(dir: PathBuf) -> Arc<Self> {
        let mut rows = BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for path in entries.flatten().map(|e| e.path()) {
                if path.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                match read_row(&path) {
                    Ok(row) => {
                        rows.insert(row.name.clone(), row);
                    }
                    Err(e) => tracing::warn!(
                        path = %path.display(),
                        "skipping a managed fleet that does not load: {e}"
                    ),
                }
            }
        }
        Arc::new(Self {
            dir,
            rows: Mutex::new(rows),
        })
    }

    /// A plugin's apply: the request is live again (`down` cleared).
    /// Every write holds the lock across its file, so memory and disk
    /// agree under concurrent calls.
    pub fn put(&self, row: ManagedFleet) {
        let row = ManagedFleet { down: None, ..row };
        let mut rows = self.lock();
        self.persist(&row);
        rows.insert(row.name.clone(), row);
    }

    /// The plugin's `DELETE`: the row stays, carrying its query, until the
    /// plugin applies the name again, is dropped, or the record is purged.
    pub fn mark_down(&self, name: &str, down: DownQuery) {
        let mut rows = self.lock();
        if let Some(row) = rows.get_mut(name) {
            row.down = Some(down);
            self.persist(row);
        }
    }

    pub fn forget(&self, name: &str) {
        let mut rows = self.lock();
        if rows.remove(name).is_some() {
            self.unlink(name);
        }
    }

    /// A plugin dropped from the operator's list takes its requests with
    /// it (§23.2); the names it had, by name.
    pub fn forget_plugin(&self, plugin: &str) -> Vec<String> {
        let mut rows = self.lock();
        let names: Vec<String> = rows
            .values()
            .filter(|r| r.plugin == plugin)
            .map(|r| r.name.clone())
            .collect();
        for name in &names {
            rows.remove(name);
            self.unlink(name);
        }
        names
    }

    /// `GET /v1/managed-fleets`, by name.
    pub fn list(&self) -> Vec<ManagedFleet> {
        self.lock().values().cloned().collect()
    }

    // A row's name is a `FleetName`, so it is safe as a file name.
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    fn persist(&self, row: &ManagedFleet) {
        let path = self.path(&row.name);
        let result = serde_json::to_vec(row)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::fsutil::write_private(&path, &bytes));
        if let Err(e) = result {
            // The row is still served from memory; only a restart loses it.
            tracing::warn!(fleet = %row.name, path = %path.display(), "could not persist a managed fleet: {e}");
        }
    }

    fn unlink(&self, name: &str) {
        let path = self.path(name);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(fleet = name, path = %path.display(), "could not remove a managed fleet: {e}");
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, ManagedFleet>> {
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn read_row(path: &Path) -> Result<ManagedFleet, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, plugin: &str) -> ManagedFleet {
        ManagedFleet {
            name: name.into(),
            plugin: plugin.into(),
            file: json!({ "crews": { "c": { "repo": "acme/api" } } }),
            down: None,
        }
    }

    #[test]
    fn a_put_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManagedStore::load(dir.path().join("managed"));
        store.put(row("gh-1", "fake"));
        let again = ManagedStore::load(dir.path().join("managed"));
        assert_eq!(again.list(), vec![row("gh-1", "fake")]);
    }

    #[test]
    fn mark_down_keeps_the_row_with_its_query() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManagedStore::load(dir.path().to_path_buf());
        store.put(row("gh-1", "fake"));
        let q = DownQuery {
            keep_repos: true,
            ..DownQuery::default()
        };
        store.mark_down("gh-1", q);
        store.mark_down("absent", q);
        let want = vec![ManagedFleet {
            down: Some(q),
            ..row("gh-1", "fake")
        }];
        assert_eq!(store.list(), want);
        assert_eq!(ManagedStore::load(dir.path().to_path_buf()).list(), want);
        // a new apply of the name makes it live again
        store.put(row("gh-1", "fake"));
        assert_eq!(store.list(), vec![row("gh-1", "fake")]);
    }

    #[test]
    fn forget_plugin_removes_and_returns_only_that_plugins_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManagedStore::load(dir.path().to_path_buf());
        store.put(row("a", "fake"));
        store.put(row("b", "other"));
        store.put(row("c", "fake"));
        assert_eq!(store.forget_plugin("fake"), vec!["a", "c"]);
        assert_eq!(store.list(), vec![row("b", "other")]);
        assert_eq!(
            ManagedStore::load(dir.path().to_path_buf()).list(),
            vec![row("b", "other")]
        );
        store.forget("b");
        assert_eq!(store.list(), vec![]);
        assert_eq!(ManagedStore::load(dir.path().to_path_buf()).list(), vec![]);
    }

    #[test]
    fn a_corrupt_file_is_skipped_with_the_others_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManagedStore::load(dir.path().to_path_buf());
        store.put(row("a", "fake"));
        store.put(row("c", "fake"));
        std::fs::write(dir.path().join("b.json"), b"{ not json").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"ignored").unwrap();
        assert_eq!(
            ManagedStore::load(dir.path().to_path_buf()).list(),
            vec![row("a", "fake"), row("c", "fake")]
        );
    }
}
