//! The balerix agent pod (Spec O §6, §7). `sidecar` materialises one agent
//! on the claim with `balerix-runtime`, drives it over a tmux socket the
//! `agent` container's server listens on, forwards Claude's hooks to the
//! Daemon and holds one outbound link to it. `run` is the agent
//! container's entrypoint: it starts that tmux server once the sidecar
//! says the agent is ready to launch.

pub mod attach;
mod body_limit;
pub mod bundle;
pub mod cli;
pub mod hooks;
pub mod jobs;
pub mod link;
pub mod run;
pub mod sidecar;
pub mod state;
pub mod tls;

/// A unit test's scratch directory under the crate's `target/tmp`, never
/// `/tmp` (nono grants it by default). `CARGO_TARGET_TMPDIR` is set for
/// integration tests only; `scripts/agent.sh` builds into `agent/target`,
/// so this is the same directory.
#[cfg(test)]
pub(crate) fn test_dir() -> tempfile::TempDir {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}
