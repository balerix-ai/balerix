//! Atomic file writes with explicit modes, by path and, under a directory
//! opened once, by descriptor without following links.

use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{
    AtFlags, CWD, Mode, OFlags, RawMode, fchmod, mkdirat, openat, renameat, unlinkat,
};
use rustix::io::Errno;

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

/// `remove_dir_all`, after giving every directory in the tree its owner's
/// `rwx` back: a sandboxed process that took the permissions off a
/// directory it made cannot keep the daemon from removing it. Links are
/// never followed (`symlink_metadata`); a missing `path` is not an error.
pub(crate) fn remove_tree(path: &Path) -> io::Result<()> {
    fn open_up(dir: &Path) {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                open_up(&entry.path());
            }
        }
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            open_up(path);
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
    let tmp = dir.join(temp_name(&name));
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

/// `.{name}.tmp-{pid}-{n}`, `n` a process-wide counter (#21).
fn temp_name(name: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!(".{name}.tmp-{}-{n}", std::process::id())
}

/// What `ensure_dir_at` does to a directory's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirMode {
    /// Created at the umask default; an existing directory is left alone.
    Default,
    /// 0700, created so and narrowed through the open descriptor.
    Private,
}

/// How often `ensure_dir_at` replaces what it found before giving up.
const DIR_ATTEMPTS: usize = 4;

fn mode(m: u32) -> Mode {
    Mode::from_raw_mode(m as RawMode)
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// One path component: not empty, `.`, `..`, nor holding a `/`.
fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a single path component"),
        ));
    }
    Ok(())
}

/// Creates `path` (with its ancestors) if missing and opens it as a
/// directory, without following a link at its last component; `Private`
/// narrows it to 0700 through the descriptor. For a root balerix made
/// itself, under its own state root: the ancestors resolve as usual. What
/// lies below is reached through `ensure_path_at` and `write_atomic_at`.
pub(crate) fn open_dir_root(path: &Path, dir_mode: DirMode) -> io::Result<OwnedFd> {
    fs::create_dir_all(path)?;
    let fd = openat(CWD, path, dir_flags(), Mode::empty())?;
    if dir_mode == DirMode::Private {
        fchmod(&fd, mode(0o700))?;
    }
    Ok(fd)
}

/// The directory `name` in `parent`, created if missing, opened without
/// following a link. A link, or any other non-directory, found there is
/// removed (`unlinkat`, which removes the link itself) and a real
/// directory made in its place. The mode is set through the descriptor,
/// never by path, so it lands on the directory that was opened.
pub(crate) fn ensure_dir_at(
    parent: BorrowedFd<'_>,
    name: &str,
    dir_mode: DirMode,
) -> io::Result<OwnedFd> {
    check_name(name)?;
    let create = match dir_mode {
        DirMode::Default => 0o777,
        DirMode::Private => 0o700,
    };
    for _ in 0..DIR_ATTEMPTS {
        match openat(parent, name, dir_flags(), Mode::empty()) {
            Ok(fd) => {
                if dir_mode == DirMode::Private {
                    fchmod(&fd, mode(0o700))?;
                }
                return Ok(fd);
            }
            Err(Errno::NOENT) => {}
            // ELOOP: a link (O_NOFOLLOW); ENOTDIR: a file, socket, FIFO…
            Err(Errno::LOOP | Errno::NOTDIR) => match unlinkat(parent, name, AtFlags::empty()) {
                // a directory by now (EISDIR; EPERM on macOS): open it next round
                Ok(()) | Err(Errno::NOENT | Errno::ISDIR | Errno::PERM) => {}
                Err(e) => return Err(e.into()),
            },
            Err(e) => return Err(e.into()),
        }
        match mkdirat(parent, name, mode(create)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err(io::Error::other(format!(
        "still not a directory after {DIR_ATTEMPTS} attempts to replace it"
    )))
}

/// `ensure_dir_at` for each component of `rel` in turn, from `root`; the
/// mode applies to the last, the ones before it are `Default`. `rel` must
/// be relative and plain (no `.`, `..` or root).
pub(crate) fn ensure_path_at(
    root: BorrowedFd<'_>,
    rel: &Path,
    dir_mode: DirMode,
) -> io::Result<OwnedFd> {
    let names = rel
        .components()
        .map(|c| match c {
            Component::Normal(n) => n.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "not a UTF-8 path component")
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: not a plain relative path", rel.display()),
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let Some((last, before)) = names.split_last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an empty relative path",
        ));
    };
    let mut dir: Option<OwnedFd> = None;
    for name in before {
        let parent = dir.as_ref().map_or(root, |d| d.as_fd());
        dir = Some(ensure_dir_at(parent, name, DirMode::Default)?);
    }
    let parent = dir.as_ref().map_or(root, |d| d.as_fd());
    ensure_dir_at(parent, last, dir_mode)
}

/// `path` relative to `root`, for `ensure_path_at`.
pub(crate) fn relative_to<'a>(root: &Path, path: &'a Path) -> io::Result<&'a Path> {
    path.strip_prefix(root)
        .map_err(|_| io::Error::other(format!("not under {}", root.display())))
}

/// `write_atomic` under a directory descriptor: the temp file is created
/// in `dir` with `O_CREAT|O_EXCL|O_NOFOLLOW` at `mode`, written, given
/// `mode` through its descriptor (an umask can only narrow the creation
/// mode), synced, and renamed over `name` within `dir`. A link at `name`
/// is replaced, never written through. Same unique temp name as
/// `write_atomic`, removed when any step fails.
pub(crate) fn write_atomic_at(
    dir: BorrowedFd<'_>,
    name: &str,
    contents: &[u8],
    file_mode: u32,
) -> io::Result<()> {
    check_name(name)?;
    let tmp = temp_name(name);
    // only a dead process with this pid can have left this name
    let _ = unlinkat(dir, tmp.as_str(), AtFlags::empty());
    let fd = openat(
        dir,
        tmp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        mode(file_mode),
    )?;
    let mut f = fs::File::from(fd);
    let written = (|| {
        f.write_all(contents)?;
        fchmod(&f, mode(file_mode))?;
        f.sync_all()?;
        drop(f);
        renameat(dir, tmp.as_str(), dir, name)?;
        Ok(())
    })();
    written.inspect_err(|_| {
        let _ = unlinkat(dir, tmp.as_str(), AtFlags::empty());
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

    fn open(p: &Path) -> OwnedFd {
        open_dir_root(p, DirMode::Default).unwrap()
    }

    #[test]
    fn write_atomic_at_writes_with_mode_replaces_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let fd = open(dir.path());
        let p = dir.path().join("f");
        write_atomic_at(fd.as_fd(), "f", b"one", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_atomic_at(fd.as_fd(), "f", b"two", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        write_atomic_at(fd.as_fd(), "../g", b"x", 0o600).unwrap_err();
        write_atomic_at(fd.as_fd(), "..", b"x", 0o600).unwrap_err();
    }

    #[test]
    fn write_atomic_at_fails_cleanly_onto_a_non_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d").join("x"), b"").unwrap();
        write_atomic_at(open(dir.path()).as_fd(), "d", b"x", 0o600).unwrap_err();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn ensure_path_at_replaces_links_and_files_with_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let other = dir.path().join("other");
        fs::create_dir_all(&other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
        let fd = open_dir_root(&root, DirMode::Private).unwrap();
        std::os::unix::fs::symlink(&other, root.join("a")).unwrap();
        fs::write(root.join("f"), b"").unwrap();
        let leaf = ensure_path_at(fd.as_fd(), Path::new("a/b"), DirMode::Private).unwrap();
        ensure_path_at(fd.as_fd(), Path::new("f"), DirMode::Default).unwrap();
        write_atomic_at(leaf.as_fd(), "x", b"x", 0o600).unwrap();
        let meta = |p: &Path| fs::symlink_metadata(p).unwrap();
        assert!(meta(&root.join("a")).is_dir());
        assert!(meta(&root.join("f")).is_dir());
        assert_eq!(meta(&root.join("a/b")).permissions().mode() & 0o777, 0o700);
        assert!(root.join("a/b/x").is_file());
        assert_eq!(meta(&other).permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
        for bad in ["", "/abs", "a/../b", "./a"] {
            ensure_path_at(fd.as_fd(), Path::new(bad), DirMode::Default).unwrap_err();
        }
    }

    #[test]
    fn remove_tree_removes_a_tree_whose_directories_lost_their_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("t");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/f"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path(), root.join("a/link")).unwrap();
        fs::set_permissions(root.join("a/b"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o000)).unwrap();
        remove_tree(&root).unwrap();
        assert!(!root.exists());
        assert!(dir.path().exists(), "the link's target stays");
        remove_tree(&root).unwrap();
    }
}
