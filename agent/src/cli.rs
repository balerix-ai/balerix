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

/// Ends the sidecar with its reason as the termination message: the first
/// line of `e`, written to `log` (best effort: the path may not exist off
/// a pod) and to stderr. Always exit 1.
pub fn terminate(log: &Path, e: &anyhow::Error) -> ExitCode {
    let reason = format!("{e:#}");
    let first = reason.lines().next().unwrap_or("").to_string();
    if let Err(write) = std::fs::write(log, format!("{first}\n")) {
        tracing::debug!(path = %log.display(), "no termination log: {write}");
    }
    eprintln!("balerix-agent sidecar: {reason}");
    ExitCode::FAILURE
}
