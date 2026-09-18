//! Ports (Phase 2 spec §2.2) and the value types that cross them. Adapters
//! in `balerix-runtime` implement the traits; the reconciler drives them.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use balerix_api::{
    CredentialBundle, GitSettings, Timestamp, WorkspaceDiff, WorkspaceTree, WorkspaceVersion,
};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::name::{AgentId, AgentName, CrewName, FleetName};
use crate::plugin::ResolvedPlugin;
use crate::repo::RepoRef;

/// What the runner can see about one agent's process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running { pid: u32 },
    Exited { code: Option<i32> },
}

/// Sessions and windows the runner found for one fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedState {
    pub crews: BTreeMap<CrewName, BTreeMap<AgentName, ProcessState>>,
}

impl ObservedState {
    pub fn get(&self, id: &AgentId) -> Option<&ProcessState> {
        self.crews.get(&id.crew)?.get(&id.agent)
    }
    pub fn set(&mut self, id: &AgentId, s: ProcessState) {
        self.crews
            .entry(id.crew.clone())
            .or_default()
            .insert(id.agent.clone(), s);
    }
    pub fn remove(&mut self, id: &AgentId) {
        if let Some(c) = self.crews.get_mut(&id.crew) {
            c.remove(&id.agent);
        }
    }
    /// Every observed agent, as ids in `fleet`, sorted.
    pub fn agent_ids(&self, fleet: &FleetName) -> Vec<AgentId> {
        self.crews
            .iter()
            .flat_map(|(crew, agents)| {
                agents.keys().map(move |agent| AgentId {
                    fleet: fleet.clone(),
                    crew: crew.clone(),
                    agent: agent.clone(),
                })
            })
            .collect()
    }
}

/// Everything a runner needs to start one agent. Runner-agnostic (spec §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub cwd: PathBuf,
    /// The OUTER environment: `PATH` and nono's own `HOME`. The agent's
    /// environment is inside the nono profile (Phase 2 spec P2-5).
    pub env: BTreeMap<String, String>,
    pub argv: Vec<String>,
    /// Rendered `launch.sh`; what the runner actually executes.
    pub script: PathBuf,
}

/// Where an agent's Claude hooks post to, and the per-agent secret.
#[derive(Clone, PartialEq, Eq)]
pub struct HookTarget {
    pub url: String,
    pub secret: String,
}

impl fmt::Debug for HookTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookTarget")
            .field("url", &self.url)
            .field("secret", &"<redacted>")
            .finish()
    }
}

pub use balerix_api::Keep;

/// First line of a tool's stderr, for one-line error displays.
pub fn first_line(s: &str) -> &str {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MaterializeError {
    #[error("{id}: {tool} {subcommand}: {}", first_line(stderr))]
    Tool {
        id: String,
        tool: String,
        subcommand: String,
        args: Vec<String>,
        stderr: String,
    },
    #[error("{id}: {path}: {message}")]
    Io {
        id: String,
        path: PathBuf,
        message: String,
    },
    #[error("{id}: sandbox.{path}: {message}")]
    SandboxConflict {
        id: String,
        path: String,
        message: String,
    },
    #[error("{id}: {message}")]
    Invalid { id: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerError {
    #[error("{id}: tmux {subcommand}: {}", first_line(stderr))]
    Tool {
        id: String,
        subcommand: String,
        args: Vec<String>,
        stderr: String,
    },
    #[error("{id}: cannot parse tmux output: {message}")]
    Parse { id: String, message: String },
}

/// Why a workspace read failed (Spec C §3.1). `Display` is what the
/// daemon answers a plugin with, except `Io`, which names a daemon-side
/// path and is logged instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// The agent's worktree directory does not exist.
    #[error("no workspace for agent {0}")]
    Missing(String),
    /// The worktree exists and the path does not.
    #[error("no such path")]
    NoSuchPath,
    #[error("workspace: invalid path: {0}")]
    InvalidPath(String),
    #[error("not a regular file")]
    NotAFile,
    #[error("not a directory")]
    NotADirectory,
    #[error("file larger than {} MiB", limit >> 20)]
    TooLarge { limit: u64 },
    #[error("{id}: git {subcommand}: {}", first_line(stderr))]
    Tool {
        id: String,
        subcommand: String,
        args: Vec<String>,
        stderr: String,
    },
    /// The repository's config names a clean/smudge/process filter; a diff would run it as the daemon.
    #[error("repository config sets {key}; workspace diff refused")]
    Filter { key: String },
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

/// Read-only access to an agent's worktree (Spec C §3.1). Sync like every
/// port; the daemon calls it in `spawn_blocking`. Implementations apply
/// `balerix_api::check_path` before any I/O and never follow a symlink.
pub trait WorkspaceReader: Send + Sync {
    /// The worktree against the merge-base with `base_ref`
    /// (`origin/<crew ref>`): committed, uncommitted and untracked alike.
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError>;
    /// One regular file, at most `WORKSPACE_FILE_LIMIT` bytes.
    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError>;
    /// One directory, never recursive; the empty path is the root.
    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError>;
    /// A cheap fingerprint of the worktree (Spec D §2): `HEAD`, the
    /// merge-base with `base_ref`, and every changed or untracked path's
    /// size and mtime. Equal fingerprints mean `diff` would answer the same.
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError>;
}

/// The two read-only tool tables a crew's pools are built from: the fleet's
/// own `defaults.tools` and this crew's `defaults.tools` layer. Each is that
/// level's declaration, never a merge of the level above (Spec E §4).
#[derive(Debug, Clone, Copy)]
pub struct CrewTools<'a> {
    pub fleet: &'a BTreeMap<String, String>,
    pub crew: &'a BTreeMap<String, String>,
}

/// Makes files exist (or not) for crews and agents.
pub trait Materializer: Send + Sync {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        repo: &RepoRef,
        git_ref: &str,
        git: &GitSettings,
        creds: &CredentialBundle,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError>;
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        creds: &CredentialBundle,
        hooks: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError>;
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError>;
    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError>;
    /// Files for one plugin (plugins spec §5.1): `home/`, profile,
    /// `launch.sh`, tools installed. `host` carries the daemon URL and the
    /// plugin's token.
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError>;
    /// Deletes `plugins/<name>/` — kv, scratch, home, everything
    /// (`plugin remove --purge`). Not part of any reconcile pass.
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError>;
}

/// Installs the daemon-level tool pool. Owned by one actor, never by the
/// reconciler (Spec F, F-1). Implementations must be idempotent and cheap
/// when nothing has changed: `Ok(())` means the pool matches the system
/// table *and* exists on disk.
pub trait SystemToolchain: Send + Sync {
    fn ensure_system_pool(&self) -> Result<(), MaterializeError>;
}

/// Makes processes exist (or not) and reports what it sees.
pub trait AgentRunner: Send + Sync {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError>;
    /// Creates the agent's window, or respawns it if it exists.
    fn ensure_agent(&self, agent: &AgentId, plan: &LaunchPlan) -> Result<(), RunnerError>;
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError>;
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError>;
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError>;
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError>;
    /// Presses each step in order, pausing `delay` after every one (Spec J
    /// §4.2). Returns when the last step has been sent.
    fn send_keys(
        &self,
        agent: &AgentId,
        steps: &[balerix_api::KeyStep],
        delay: std::time::Duration,
    ) -> Result<(), RunnerError>;
    /// A terminal on the agent's window (plugins spec §18.4). The runner
    /// decides how; nothing about tmux crosses this port.
    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError>;
}

/// A live terminal on one agent's window (plugins spec §18.4). Sync like
/// every port; the server bridges it to a WebSocket with one blocking
/// reader task. Dropping the stream ends the session.
pub trait PtyStream: Send {
    /// A reader of the terminal's output. Owned by the caller so a
    /// blocking reader thread can outlive the borrow.
    fn reader(&self) -> io::Result<Box<dyn Read + Send>>;
    /// The writer of keystrokes. Taken once: a second call fails.
    fn writer(&self) -> io::Result<Box<dyn Write + Send>>;
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    #[test]
    fn observed_state_set_get_remove_and_list() {
        let mut o = ObservedState::default();
        o.set(&id("f/c/b"), ProcessState::Running { pid: 2 });
        o.set(&id("f/c/a"), ProcessState::Exited { code: Some(1) });
        assert_eq!(
            o.get(&id("f/c/a")),
            Some(&ProcessState::Exited { code: Some(1) })
        );
        let ids: Vec<String> = o
            .agent_ids(&"f".parse().unwrap())
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(ids, vec!["f/c/a", "f/c/b"]);
        o.remove(&id("f/c/a"));
        assert_eq!(o.get(&id("f/c/a")), None);
    }

    #[test]
    fn hook_target_debug_redacts_the_secret() {
        let h = HookTarget {
            url: "https://127.0.0.1:7643".into(),
            secret: "s3cr3t".into(),
        };
        let d = format!("{h:?}");
        assert!(d.contains("127.0.0.1"));
        assert!(!d.contains("s3cr3t"));
        assert!(d.contains("<redacted>"));
    }

    #[test]
    fn tool_error_shows_the_first_non_empty_stderr_line() {
        let e = MaterializeError::Tool {
            id: "f/c/a".into(),
            tool: "git".into(),
            subcommand: "clone".into(),
            args: vec![],
            stderr: "\n  fatal: repository not found\nmore".into(),
        };
        assert_eq!(
            e.to_string(),
            "f/c/a: git clone: fatal: repository not found"
        );
        assert_eq!(first_line(""), "");
    }

    mod path_rule {
        use balerix_api::check_path;
        use proptest::prelude::*;
        use std::path::{Component, Path};

        fn segment() -> impl Strategy<Value = String> {
            prop_oneof![
                4 => "[a-z][a-z0-9._-]{0,6}".prop_map(String::from),
                1 => Just("..".to_string()),
                1 => Just(".".to_string()),
                1 => Just(String::new()),
                1 => Just(".git".to_string()),
                1 => Just(".GIT".to_string()),
            ]
        }

        proptest! {
            /// An accepted path joins under the root without `.`/`..`
            /// components, names no `.git` and has no empty segment; a
            /// refused one carries a reason.
            #[test]
            fn accepted_paths_stay_under_the_root(segments in prop::collection::vec(segment(), 0..6)) {
                let path = segments.join("/");
                match check_path(&path) {
                    Ok(()) => {
                        let joined = Path::new("/root").join(&path);
                        prop_assert!(joined
                            .components()
                            .all(|c| !matches!(c, Component::ParentDir | Component::CurDir)));
                        prop_assert!(joined.starts_with("/root"));
                        prop_assert!(!path.split('/').any(|s| s.eq_ignore_ascii_case(".git")));
                        prop_assert!(path.is_empty() || !path.split('/').any(str::is_empty));
                    }
                    Err(reason) => prop_assert!(!reason.is_empty()),
                }
            }
        }
    }
}
