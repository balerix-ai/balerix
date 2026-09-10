//! Private file writes. A copy of `balerix-runtime`'s `write_atomic`
//! narrowed to 0600: `server` must not depend on `runtime`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Writes `bytes` to `path` via a sibling temp file and rename, created
/// 0600 from the start, parent directory created if missing. The temp
/// name is `.{name}.tmp~{pid}`: `~` is outside every validated key
/// alphabet in this crate (e.g. `plugins::kv`'s `[A-Za-z0-9._/-]`), so a
/// caller that lists a directory by validated name can never mistake a
/// real entry for this leftover.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp~{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_0600_creates_parents_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a").join("b.txt");
        write_private(&p, b"one").unwrap();
        write_private(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_dir(dir.path().join("a")).unwrap().count(),
            1,
            "no temp file left"
        );
    }
}
