//! Files under `$XDG_STATE_HOME/balerix/server/` (Phase 3 spec §3.3, §5):
//! admin token, vault key, the bound endpoint, the pid, the log.

use std::fs;
use std::path::{Path, PathBuf};

use crate::fsutil::write_private;
use crate::vault::random_hex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPaths {
    pub dir: PathBuf,
}

impl ServerPaths {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
    pub fn token(&self) -> PathBuf {
        self.dir.join("token")
    }
    pub fn vault_key(&self) -> PathBuf {
        self.dir.join("vault.key")
    }
    /// `http://127.0.0.1:<port>` of the running daemon; clients read it.
    pub fn endpoint(&self) -> PathBuf {
        self.dir.join("endpoint")
    }
    pub fn pid(&self) -> PathBuf {
        self.dir.join("balerix.pid")
    }
    pub fn log(&self) -> PathBuf {
        self.dir.join("server.log")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

fn io(path: &Path, e: std::io::Error) -> LifecycleError {
    LifecycleError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

fn read_trimmed(path: &Path) -> Result<Option<String>, LifecycleError> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let s = s.trim().to_string();
            Ok((!s.is_empty()).then_some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
    }
}

/// The admin bearer token: 32 random bytes as hex, 0600, created on first
/// `serve`; an empty file is regenerated.
pub fn load_or_create_token(path: &Path) -> Result<String, LifecycleError> {
    if let Some(t) = read_trimmed(path)? {
        return Ok(t);
    }
    let token = random_hex(32);
    write_private(path, token.as_bytes()).map_err(|e| io(path, e))?;
    Ok(token)
}

pub fn write_endpoint(path: &Path, url: &str) -> Result<(), LifecycleError> {
    write_private(path, url.as_bytes()).map_err(|e| io(path, e))
}

pub fn read_endpoint(path: &Path) -> Result<Option<String>, LifecycleError> {
    read_trimmed(path)
}

pub fn write_pid(path: &Path, pid: u32) -> Result<(), LifecycleError> {
    write_private(path, pid.to_string().as_bytes()).map_err(|e| io(path, e))
}

pub fn read_pid(path: &Path) -> Result<Option<u32>, LifecycleError> {
    Ok(read_trimmed(path)?.and_then(|s| s.parse().ok()))
}

pub fn remove_if_exists(path: &Path) -> Result<(), LifecycleError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn token_is_created_0600_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(dir.path().join("server"));
        let t1 = load_or_create_token(&paths.token()).unwrap();
        assert_eq!(t1.len(), 64);
        assert_eq!(
            std::fs::metadata(paths.token())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::write(paths.token(), format!("{t1}\n")).unwrap();
        assert_eq!(
            load_or_create_token(&paths.token()).unwrap(),
            t1,
            "trailing newline tolerated"
        );
        std::fs::write(paths.token(), "").unwrap();
        assert_ne!(
            load_or_create_token(&paths.token()).unwrap(),
            "",
            "an empty file is regenerated"
        );
    }

    #[test]
    fn endpoint_and_pid_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(dir.path().join("server"));
        assert_eq!(read_endpoint(&paths.endpoint()).unwrap(), None);
        write_endpoint(&paths.endpoint(), "http://127.0.0.1:4242").unwrap();
        assert_eq!(
            read_endpoint(&paths.endpoint()).unwrap().as_deref(),
            Some("http://127.0.0.1:4242")
        );
        assert_eq!(read_pid(&paths.pid()).unwrap(), None);
        write_pid(&paths.pid(), 4321).unwrap();
        assert_eq!(read_pid(&paths.pid()).unwrap(), Some(4321));
        remove_if_exists(&paths.pid()).unwrap();
        remove_if_exists(&paths.pid()).unwrap();
        assert_eq!(read_pid(&paths.pid()).unwrap(), None);
        assert_eq!(paths.log(), dir.path().join("server").join("server.log"));
        assert_eq!(
            paths.vault_key(),
            dir.path().join("server").join("vault.key")
        );
    }
}
