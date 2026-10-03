//! The command line. Mount paths carry Spec O §6.1's defaults; tests point
//! them at temp roots.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "balerix-agent",
    version,
    about = "The balerix agent pod: the sidecar and the agent container's entrypoint (Spec O §6)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// The sidecar: materialise the agent, link to the Daemon, forward hooks.
    Sidecar(SidecarArgs),
    /// The agent container: start the tmux server once the sidecar is ready.
    Run(RunArgs),
    /// The sync Job: the crew cache and the crew pool (Spec O §8.3).
    CrewSync(CrewSyncArgs),
    /// A pool Job: the daemon pool or a fleet's pool (§20.3).
    PoolSync(PoolSyncArgs),
    /// The harvest Job: the agent's branch into the crew cache (§8.4).
    Harvest(HarvestArgs),
}

#[derive(Debug, Args)]
pub struct SidecarArgs {
    /// The agent bundle the operator mounted: the resolved agent, the
    /// credentials and the Daemon token (Spec O §5.4).
    #[arg(long, default_value = "/balerix/secret/agent.json")]
    pub bundle: PathBuf,
    /// The authority that signed the Daemon's certificate (§10.3).
    #[arg(long, default_value = "/balerix/tls/ca.crt")]
    pub ca: PathBuf,
    /// The agent claim (§6.1).
    #[arg(long, default_value = "/balerix/agent")]
    pub agent_dir: PathBuf,
    /// The read-only crew slice: repo/.git/objects and the three pools.
    #[arg(long, default_value = "/balerix/shared")]
    pub shared_dir: PathBuf,
    /// The run directory both containers mount: the tmux socket, the markers.
    #[arg(long, default_value = "/balerix/run")]
    pub run_dir: PathBuf,
    /// Claude posts hooks to 127.0.0.1 on this port (§7.1); 0 picks a free one.
    #[arg(long, default_value_t = 7643)]
    pub hook_port: u16,
    /// The `balerix` binary launch.sh runs (agent-supervise, hook-relay);
    /// found on PATH when absent.
    #[arg(long)]
    pub balerix: Option<PathBuf>,
    /// Where a start-up failure's one-line reason goes (the pod reads it
    /// as the container's termination message, §6.2 step 2).
    #[arg(long, default_value = "/dev/termination-log")]
    pub termination_log: PathBuf,
    /// Accept an http:// daemon_url. Tests only; a pod's Daemon is https.
    #[arg(long, hide = true)]
    pub allow_plain_http: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long, default_value = "/balerix/run")]
    pub run_dir: PathBuf,
    /// How long to wait for the sidecar's start marker.
    #[arg(long, default_value_t = 600)]
    pub start_timeout_secs: u64,
}

/// Where a Job finds the slice, its scratch directory and its outcome.
#[derive(Debug, Args)]
pub struct JobDirs {
    /// The crew's slice, mounted read-write where the pod has it read-only.
    #[arg(long, default_value = "/balerix/shared")]
    pub shared_dir: PathBuf,
    /// An emptyDir: the gh config, the git profile, nono's home and logs.
    #[arg(long, default_value = "/balerix/scratch")]
    pub scratch_dir: PathBuf,
    /// One line: the outcome the operator reads.
    #[arg(long, default_value = "/dev/termination-log")]
    pub termination_log: PathBuf,
}

/// `--tool node=22.11.0`: one entry of a tool table.
fn tool(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((name, version)) if !name.is_empty() && !version.is_empty() => {
            Ok((name.to_string(), version.to_string()))
        }
        _ => Err(format!("{s:?}: expected <name>=<version>")),
    }
}

#[derive(Debug, Args)]
pub struct CrewSyncArgs {
    /// `<fleet>/<crew>`.
    #[arg(long)]
    pub crew: String,
    #[arg(long)]
    pub repo: String,
    #[arg(long = "ref")]
    pub git_ref: String,
    /// The GitHub token as a mounted file (`git.auth: gh`), never an argument.
    #[arg(long)]
    pub gh_token_file: Option<PathBuf>,
    /// The crew's own tool table.
    #[arg(long = "tool", value_parser = tool)]
    pub tools: Vec<(String, String)>,
    #[command(flatten)]
    pub dirs: JobDirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Level {
    Daemon,
    Fleet,
}

#[derive(Debug, Args)]
pub struct PoolSyncArgs {
    #[arg(long, value_enum)]
    pub level: Level,
    /// The fleet's name; required with `--level fleet`.
    #[arg(long)]
    pub fleet: Option<String>,
    /// The table to install. With `--level daemon` and none given, the
    /// system table this binary embeds (`claude`, `gh`).
    #[arg(long = "tool", value_parser = tool)]
    pub tools: Vec<(String, String)>,
    /// Install an empty daemon table rather than the embedded one.
    #[arg(long, conflicts_with = "tools")]
    pub no_tools: bool,
    #[command(flatten)]
    pub dirs: JobDirs,
}

#[derive(Debug, Args)]
pub struct HarvestArgs {
    /// `<fleet>/<crew>/<agent>`.
    #[arg(long)]
    pub agent: String,
    /// The agent claim, mounted read-only.
    #[arg(long, default_value = "/balerix/agent")]
    pub agent_dir: PathBuf,
    #[command(flatten)]
    pub dirs: JobDirs,
}

/// Ends a command with its reason as the termination message: the first
/// line of `e`, written to `log` (best effort: the path may not exist off
/// a pod) and to stderr. Always exit 1.
pub fn terminate(log: &Path, who: &str, e: &anyhow::Error) -> ExitCode {
    let reason = format!("{e:#}");
    write_line(log, reason.lines().next().unwrap_or(""));
    eprintln!("balerix-agent {who}: {reason}");
    ExitCode::FAILURE
}

/// A Job's end: its one-line outcome on success, `terminate` otherwise.
pub fn finish(log: &Path, who: &str, outcome: anyhow::Result<String>) -> ExitCode {
    match outcome {
        Ok(line) => {
            write_line(log, &line);
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => terminate(log, who, &e),
    }
}

fn write_line(log: &Path, line: &str) {
    if let Err(write) = std::fs::write(log, format!("{line}\n")) {
        tracing::debug!(path = %log.display(), "no termination log: {write}");
    }
}
