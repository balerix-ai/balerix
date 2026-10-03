//! The sidecar's own files on the agent claim (Spec O §6.1), under
//! `<agent>/.balerix/state/sidecar/`: on the claim, so they outlive a
//! sidecar restart, and outside every path the agent's sandbox is granted
//! (`home/`, `workspace/` and the single file `mise.toml`), so Claude
//! cannot read them.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The directory, from the agent claim's mount.
pub fn dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join(".balerix").join("state").join("sidecar")
}

/// Spec O §7.1, §10.4: the secret Claude presents on the hook hop to the
/// sidecar. The operator's token authenticates the sidecar to the Daemon
/// and never reaches the agent's files; this one is the sidecar's own.
/// Made on the first start (32 random bytes as hex) and reused after, so
/// a sidecar restart does not invalidate the `settings.json` a running
/// Claude has read.
pub fn hook_secret(dir: &Path) -> Result<String> {
    let path = dir.join("hook-secret");
    match std::fs::read_to_string(&path) {
        Ok(s) if is_secret(s.trim()) => return Ok(s.trim().to_string()),
        Ok(_) => tracing::warn!(path = %path.display(), "not a hook secret; making a new one"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
    let mut bytes = [0u8; 32];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("no randomness for the hook secret"))?;
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_atomic(&path, format!("{secret}\n").as_bytes())?;
    Ok(secret)
}

fn is_secret(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A temp file in the same directory, renamed over `path`: a reader sees
/// the old content or the new, never a part. Owner-only.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("a state file has a directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", parent.display()))?;
    let name = path
        .file_name()
        .context("a state file has a name")?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_secret_is_made_once_kept_private_and_reloaded() {
        let root = crate::test_dir();
        let d = dir(root.path());
        let first = hook_secret(&d).unwrap();
        assert!(is_secret(&first), "{first}");
        let mode = std::fs::metadata(d.join("hook-secret"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(hook_secret(&d).unwrap(), first, "a restart reuses it");
        // a damaged file is replaced, not trusted
        std::fs::write(d.join("hook-secret"), "short\n").unwrap();
        let next = hook_secret(&d).unwrap();
        assert!(is_secret(&next));
        assert_ne!(next, first);
        assert!(!d.join(".hook-secret.tmp").exists());
    }
}
