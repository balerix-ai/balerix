//! The sandbox the daemon's own `mise trust` and `mise install` run in:
//! an agent's and a plugin's tools, and every pool level. Each call is a
//! nono start of its own, under a profile written for that call into a
//! fresh scratch directory (`.nono-mise-*`) beside the files it is for:
//! nono's `$HOME` is that directory, and mise's `HOME`/`TMPDIR` its
//! `home/`, the only part of it the profile grants. The scratch directory
//! goes when the call ends, whatever the sandboxed side did to it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use balerix_core::MaterializeError;
use serde_json::Value;

use crate::fsutil::write_atomic;
use crate::launch::outer_path;
use crate::sandbox::{Grants, SYSTEM_READ, render_mise_profile};
use crate::socket_policy::{SocketPolicy, apply_to_profile, wrap_command};
use crate::tools::{Cmd, ToolPaths};
use crate::workspace::{ScratchHome, sweep_stale_homes_in};

/// Variables every install may take from the daemon's environment: the
/// proxy settings, in both cases, and where the CA certificates are.
pub const PASSED_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Variables only a pool install takes from the daemon's environment:
/// what runs there is the daemon's own tables, never an agent's or a
/// plugin's.
pub const POOL_VARS: &[&str] = &["GITHUB_TOKEN"];

/// Which installs a call is: an agent's or a plugin's, or a pool level's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Owned,
    Pool,
}

impl Scope {
    /// The names a call of this scope lets through (`allow_vars`).
    pub fn names(self) -> Vec<&'static str> {
        match self {
            Scope::Owned => PASSED_VARS.to_vec(),
            Scope::Pool => PASSED_VARS.iter().chain(POOL_VARS).copied().collect(),
        }
    }
}

/// What the daemon's installs may take from its environment, read once
/// where the binary starts (`serve`, the pod sidecar, the pool Job). The
/// values reach nono as its own environment and pass through by name
/// (`allow_vars`): no profile file ever holds them. A token or a proxy
/// URL with a password are secrets, so `Debug` prints the names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct InstallEnv {
    vars: BTreeMap<String, String>,
}

impl std::fmt::Debug for InstallEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallEnv")
            .field("names", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl InstallEnv {
    /// `PASSED_VARS` and `POOL_VARS` as `get` answers them; an empty or
    /// non-UTF-8 value is left out.
    pub fn from_env(get: impl Fn(&str) -> Option<OsString>) -> Self {
        let vars = PASSED_VARS
            .iter()
            .chain(POOL_VARS)
            .filter_map(|k| {
                let v = get(k)?.into_string().ok()?;
                (!v.is_empty()).then(|| ((*k).to_string(), v))
            })
            .collect();
        Self { vars }
    }

    /// The values a call of `scope` gets.
    fn values(&self, scope: Scope) -> BTreeMap<String, String> {
        let names = scope.names();
        self.vars
            .iter()
            .filter(|(k, _)| names.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// `SSL_CERT_FILE` and `SSL_CERT_DIR` when they name an absolute path
    /// outside the system prefixes the profiles already read.
    pub fn cert_reads(&self) -> Vec<PathBuf> {
        ["SSL_CERT_FILE", "SSL_CERT_DIR"]
            .iter()
            .filter_map(|k| self.vars.get(*k))
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && !SYSTEM_READ.iter().any(|s| p.starts_with(s)))
            .collect()
    }
}

/// One sandboxed mise call before its scratch directory exists: the
/// profile's name, the grants and mise's variables, and which
/// environment it may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCall {
    pub name: String,
    pub description: String,
    pub grants: Grants,
    pub vars: BTreeMap<String, String>,
    pub scope: Scope,
}

impl InstallCall {
    /// The profile with `home` as mise's `HOME` (and `TMPDIR` below it),
    /// granted read-write beside `grants.allow`; the certificate paths in
    /// `env` added to `read`. Before the socket policy.
    pub fn profile(&self, home: &Path, env: &InstallEnv) -> Value {
        let mut grants = self.grants.clone();
        grants.read.extend(env.cert_reads());
        grants.allow.push(home.to_path_buf());
        let mut vars = self.vars.clone();
        vars.insert("HOME".into(), home.display().to_string());
        vars.insert("TMPDIR".into(), home.join("tmp").display().to_string());
        render_mise_profile(
            &self.name,
            &self.description,
            &grants,
            &vars,
            &self.scope.names(),
        )
    }
}

/// Where a sandboxed install runs and what it is told.
pub(crate) struct SandboxedInstall<'a> {
    pub tools: &'a ToolPaths,
    pub policy: SocketPolicy,
    pub env: &'a InstallEnv,
    /// Named in errors: the agent's or plugin's id, the pool's crew.
    pub id: &'a str,
    /// Where the scratch directories are made: a root the daemon alone
    /// writes (an agent's or plugin's root, a pool's parent).
    pub scratch_in: &'a Path,
    /// mise's argv, output and exit, per call.
    pub log: &'a Path,
    /// nono's own log.
    pub nono_log: &'a Path,
    /// Bounds each call (the system pool's).
    pub timeout: Option<Duration>,
    /// The pools the install resolves through and cannot write: the only
    /// directories a refused runtime-link write may name and still leave
    /// the install done (`only_links_refused`).
    pub shared: &'a [PathBuf],
}

/// A call's scratch directory: nono's `$HOME`, holding the profile and
/// mise's `home/`.
struct Scratch {
    dir: ScratchHome,
    profile: PathBuf,
}

impl SandboxedInstall<'_> {
    fn io(&self, path: &Path, e: impl std::fmt::Display) -> MaterializeError {
        MaterializeError::Io {
            id: self.id.to_string(),
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }

    /// `mise trust <config>` under `trust`, then `mise install` under
    /// `install` (one call may serve as both). Fails closed: where nono
    /// cannot run, or the sandbox cannot start, nothing is installed.
    ///
    /// mise also rewrites a tool's runtime links (`latest`, the version
    /// prefixes, from mise 2026.10 `global`) in every install directory
    /// it resolves through, the pools in `shared` included, and exits
    /// non-zero when it may not. A failed `mise install` therefore counts
    /// as done when every error it printed is such a refusal in one of
    /// `shared` and `mise install --dry-run-code` under the same profile
    /// finds nothing missing; anything else fails, and is logged at
    /// `warn` when refusals were among it.
    pub(crate) fn run(
        &self,
        trust: &InstallCall,
        install: &InstallCall,
        config: &Path,
    ) -> Result<(), MaterializeError> {
        for dir in [
            self.scratch_in,
            self.log.parent().unwrap_or(self.scratch_in),
        ] {
            std::fs::create_dir_all(dir).map_err(|e| self.io(dir, e))?;
        }
        // the scratch directories a SIGKILLed daemon could not remove
        sweep_stale_homes_in(self.scratch_in);
        self.mise(trust, &["trust", &config.display().to_string()])?;
        let Err(e) = self.mise(install, &["install"]) else {
            return Ok(());
        };
        let stderr = match &e {
            MaterializeError::Tool { stderr, .. } => stderr.as_str(),
            _ => return Err(e),
        };
        match only_links_refused(stderr, self.shared) {
            Refusals::All if self.nothing_missing(install) => {
                tracing::info!(
                    id = self.id,
                    "mise install could not rewrite runtime links in a pool it reads; every \
                     tool it names is installed"
                );
                Ok(())
            }
            Refusals::Some => {
                tracing::warn!(
                    id = self.id,
                    error = %e,
                    "mise install failed with errors besides refused runtime links"
                );
                Err(e)
            }
            _ => Err(e),
        }
    }

    /// A fresh scratch directory with `call`'s profile in it, under the
    /// socket policy, and an empty `home/` (0700, with `tmp/`).
    fn scratch(&self, call: &InstallCall) -> Result<Scratch, MaterializeError> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = ScratchHome::in_dir(self.scratch_in, ".nono-mise-").map_err(|e| {
            self.io(
                self.scratch_in,
                format!("cannot make a scratch directory: {e}"),
            )
        })?;
        let home = dir.path().join("home");
        for d in [home.clone(), home.join("tmp")] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&d)
                .map_err(|e| self.io(&d, e))?;
        }
        let profile = apply_to_profile(
            call.profile(&home, self.env),
            self.policy,
            &[],
            &self.tools.balerix,
        );
        let path = dir.path().join("profile.json");
        write_atomic(
            &path,
            &serde_json::to_vec_pretty(&profile).unwrap_or_default(),
            0o600,
        )
        .map_err(|e| self.io(&path, e))?;
        Ok(Scratch { dir, profile: path })
    }

    /// nono's words up to and including the `--`, then the socket
    /// policy's wrapper.
    fn nono_args(&self, profile: &Path) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-s".into(),
            "--log-file".into(),
            self.nono_log.display().to_string(),
            "run".into(),
            "--no-audit".into(),
            "--profile".into(),
            profile.display().to_string(),
            "--".into(),
        ];
        args.extend(wrap_command(self.policy, &self.tools.balerix, Vec::new()));
        args
    }

    /// nono from an empty environment but `HOME` (the scratch directory),
    /// `PATH` and the values `scope` may take, which the profile lets
    /// through by name; `cwd=/`, so no project `mise.toml` on the way up
    /// is discovered.
    fn nono(&self, scratch: &Scratch, scope: Scope) -> Cmd {
        let mut cmd = Cmd::new(&self.tools.nono)
            .env_clear()
            .env("HOME", scratch.dir.path().display().to_string())
            .env("PATH", outer_path(self.tools))
            .envs(&self.env.values(scope))
            .cwd(Path::new("/"))
            .args(self.nono_args(&scratch.profile));
        if let Some(d) = self.timeout {
            cmd = cmd.timeout(d);
        }
        cmd
    }

    /// One `mise <args>` under `call`, logged to `log`. A failure is
    /// reported as mise's; when `/bin/true` cannot start under the same
    /// profile either, it says the sandbox did not start and names
    /// nono's log.
    fn mise(&self, call: &InstallCall, args: &[&str]) -> Result<(), MaterializeError> {
        let scratch = self.scratch(call)?;
        let cmd = self
            .nono(&scratch, call.scope)
            .args([self.tools.mise.display().to_string()])
            .args(args.iter().copied())
            .log(self.log);
        let Err(f) = cmd.run() else {
            return Ok(());
        };
        let stderr = if self.sandbox_starts(call) {
            f.stderr
        } else {
            format!(
                "the sandbox did not start (see {}): {}",
                self.nono_log.display(),
                f.stderr
            )
        };
        Err(MaterializeError::Tool {
            id: self.id.to_string(),
            tool: "mise".into(),
            subcommand: args.first().copied().unwrap_or_default().to_string(),
            args: f.args,
            stderr,
        })
    }

    /// Whether `mise install --dry-run-code` under `call` finds nothing to
    /// install.
    fn nothing_missing(&self, call: &InstallCall) -> bool {
        let Ok(scratch) = self.scratch(call) else {
            return false;
        };
        self.nono(&scratch, call.scope)
            .args([self.tools.mise.display().to_string()])
            .args(["install", "--dry-run-code"])
            .log(self.log)
            .run()
            .is_ok()
    }

    /// Whether `/bin/true` runs under `call`'s profile.
    fn sandbox_starts(&self, call: &InstallCall) -> bool {
        let Ok(scratch) = self.scratch(call) else {
            return false;
        };
        self.nono(&scratch, call.scope)
            .args(["/bin/true"])
            .timeout(SANDBOX_START_DEADLINE)
            .run()
            .is_ok()
    }
}

/// The most `SandboxedInstall::sandbox_starts` waits for `/bin/true`.
const SANDBOX_START_DEADLINE: Duration = Duration::from_secs(10);

/// What a failed `mise install` printed, against the refused runtime-link
/// writes `SandboxedInstall::run` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusals {
    /// No refusal at all.
    None,
    /// Refusals, and other errors beside them.
    Some,
    /// Refusals and nothing else: every line is one, or mise's own
    /// summary and footer of them, or a warning.
    All,
}

/// Classifies `stderr`. A refusal is mise's `failed to rebuild runtime
/// symlinks for <tool> in <dir>: … Permission denied (os error 13)` with
/// `<dir>` inside one of `shared`; a refusal anywhere else (the install's
/// own data dir) is another error.
pub(crate) fn only_links_refused(stderr: &str, shared: &[PathBuf]) -> Refusals {
    let in_shared = |line: &str| {
        shared
            .iter()
            .any(|p| line.contains(&format!(" in {}/", p.display())))
    };
    let mut refusals = 0usize;
    let mut other = false;
    for line in stderr.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let summary = line == "mise ERROR failed to rebuild runtime symlinks"
            || line
                .strip_prefix("mise ERROR ")
                .and_then(|rest| rest.split_once(' '))
                .is_some_and(|(n, rest)| {
                    n.parse::<u32>().is_ok() && rest == "runtime symlink repair(s) failed:"
                })
            || line.starts_with("mise ERROR Version: ")
            || line.starts_with("mise ERROR Run with --verbose")
            || line.starts_with("mise WARN")
            || line.starts_with("[WARN]");
        if line.starts_with("failed to rebuild runtime symlinks for ")
            && line.ends_with("Permission denied (os error 13)")
            && in_shared(line)
        {
            refusals += 1;
        } else if !summary {
            other = true;
        }
    }
    match (refusals, other) {
        (0, _) => Refusals::None,
        (_, true) => Refusals::Some,
        (_, false) => Refusals::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_refused_link_writes_in_a_pool_are_accepted() {
        let pool = PathBuf::from("/s/daemon/mise/installs");
        let shared = [PathBuf::from("/s/daemon/mise")];
        let new = format!(
            "mise ERROR failed to rebuild runtime symlinks\n\
             mise ERROR 1 runtime symlink repair(s) failed:\n\
             failed to rebuild runtime symlinks for gh in {}/gh: failed rm: {}/gh/global: Permission denied (os error 13)\n\
             mise ERROR Version: 2026.10.8 linux-x64 (2026-10-11)\n\
             mise ERROR Run with --verbose or MISE_VERBOSE=1 for more information\n",
            pool.display(),
            pool.display()
        );
        assert_eq!(only_links_refused(&new, &shared), Refusals::All);
        let old = format!(
            "failed to rebuild runtime symlinks for jq in {}/jq: failed to ln -sf ./1.8.2 {}/jq/latest: Permission denied (os error 13)\n\
             mise ERROR Version: 2026.9.2 linux-x64 (2026-09-07)\n\
             mise ERROR Run with --verbose or MISE_VERBOSE=1 for more information\n",
            pool.display(),
            pool.display()
        );
        assert_eq!(only_links_refused(&old, &shared), Refusals::All);
        assert_eq!(
            only_links_refused(&format!("{old}mise ERROR failed to download x\n"), &shared),
            Refusals::Some
        );
        assert_eq!(
            only_links_refused(&old, &[PathBuf::from("/elsewhere")]),
            Refusals::None,
            "a refusal outside the pools is not one"
        );
        assert_eq!(
            only_links_refused("mise ERROR failed to download x\n", &shared),
            Refusals::None
        );
        assert_eq!(only_links_refused("", &shared), Refusals::None);
    }

    #[test]
    fn the_install_env_keeps_the_token_for_pools_and_prints_no_value() {
        let env = InstallEnv::from_env(|k| match k {
            "GITHUB_TOKEN" => Some("ghp_secret".into()),
            "https_proxy" => Some("http://u:p@proxy:3128".into()),
            "SSL_CERT_FILE" => Some("/opt/ca/bundle.pem".into()),
            "SSL_CERT_DIR" => Some("/etc/ssl/certs".into()),
            "NO_PROXY" => Some(String::new().into()),
            _ => None,
        });
        let owned = env.values(Scope::Owned);
        assert_eq!(
            owned.keys().collect::<Vec<_>>(),
            ["SSL_CERT_DIR", "SSL_CERT_FILE", "https_proxy"]
        );
        assert_eq!(env.values(Scope::Pool)["GITHUB_TOKEN"], "ghp_secret");
        assert!(!Scope::Owned.names().contains(&"GITHUB_TOKEN"));
        assert!(Scope::Pool.names().contains(&"GITHUB_TOKEN"));
        assert_eq!(env.cert_reads(), [PathBuf::from("/opt/ca/bundle.pem")]);
        let shown = format!("{env:?}");
        assert!(
            !shown.contains("ghp_secret") && !shown.contains("u:p@"),
            "{shown}"
        );
    }
}
