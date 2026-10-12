//! Atomic file writes with explicit modes.

use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn ensure_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// `create_dir_all` then `0o700` on the leaf. Used for the directories that
/// hold an agent's secrets (`settings.json`, `.credentials.json`,
/// `hosts.yml`): the files are 0600, and a 0700 enclosing directory keeps
/// another local user from reaching them even if a mode is ever missed.
/// Only the leaf is narrowed — the ancestors are balerix's own state root.
pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Writes `contents` to `path` via a sibling temp file and rename, with
/// `mode` (e.g. `0o600`) applied before the rename. The temp file is
/// created with `mode` from the start (via `OpenOptionsExt::mode`), so a
/// secrets file is never briefly readable at the umask default while the
/// mode is fixed up after the fact. `set_permissions` is still applied
/// afterwards: `mode()` only governs permissions at creation time, and an
/// umask can only remove bits, so a restrictive umask (e.g. `0o077`) could
/// otherwise leave the file narrower than the caller asked for.
///
/// The temp name is `.{name}.tmp-{pid}-{n}`, `n` a process-wide counter, so
/// two threads writing one path never share a temp file (#21): a shared
/// name let one call unlink the other's in-flight file, or fail
/// `create_new` with `AlreadyExists`. The temp file is removed when the
/// write or rename fails, so a failed call leaves nothing behind.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    ensure_dir(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?
        .to_string_lossy();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp-{}-{n}", std::process::id()));
    // only a dead process with this pid can have left this name; without
    // the removal its leftover would fail `create_new` below
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let written = (|| {
        f.write_all(contents)?;
        f.sync_all()?;
        drop(f);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, path)
    })();
    written.inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_with_mode_and_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("f");
        write_atomic(&p, b"one", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_atomic(&p, b"two", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::read_dir(p.parent().unwrap()).unwrap().count(),
            1,
            "no temp file left"
        );
    }

    #[test]
    fn private_dirs_are_0700_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a").join("b");
        ensure_private_dir(&p).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o700);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&p).unwrap();
        assert_eq!(mode(&p), 0o700, "an existing dir is narrowed too");
    }

    #[test]
    fn concurrent_writes_of_one_path_all_succeed_and_leave_no_temp_file() {
        const THREADS: usize = 4;
        const ROUNDS: usize = 200;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        let barrier = std::sync::Barrier::new(THREADS);
        let contents: Vec<String> = (0..THREADS).map(|t| format!("writer {t}")).collect();
        std::thread::scope(|s| {
            for c in &contents {
                let (p, barrier) = (&p, &barrier);
                s.spawn(move || {
                    barrier.wait();
                    for _ in 0..ROUNDS {
                        write_atomic(p, c.as_bytes(), 0o600).unwrap();
                    }
                });
            }
        });
        assert!(contents.contains(&fs::read_to_string(&p).unwrap()));
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["f"], "no temp file left");
    }

    #[test]
    fn a_failed_rename_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("x"), b"").unwrap();
        // a file cannot replace a non-empty directory
        write_atomic(&p, b"x", 0o600).unwrap_err();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
