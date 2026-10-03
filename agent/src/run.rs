//! `balerix-agent run`: the agent container's entrypoint (Spec O §6.2
//! step 3). Waits for the sidecar's start marker, starts the tmux server
//! on the shared socket with the crew's anchor window (what the runner's
//! `ensure_crew` would create), and lives as long as the server does. The
//! sidecar then creates the agent's window through the socket exactly as
//! the daemon does on one machine.
//!
//! It is the container's pid 1. The server daemonises away from it, so it
//! polls `has-session`; SIGTERM (the pod ending) becomes `kill-server`,
//! and the supervisor in the pane ends the agent's tree on the hangup
//! that follows (Spec N amendment §13).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use balerix_runtime::ANCHOR_WINDOW;

use crate::cli::RunArgs;

const POLL: Duration = Duration::from_millis(200);
const IDLE: &str = "while :; do sleep 3600; done";

pub fn run(args: &RunArgs) -> Result<ExitCode> {
    let marker = args.run_dir.join("started");
    let socket = args.run_dir.join("tmux.sock");
    let session = wait_marker(&marker, Duration::from_secs(args.start_timeout_secs))?;
    let tmux = on_path("tmux").context("tmux is not on PATH")?;
    let status = Command::new(&tmux)
        .arg("-S")
        .arg(&socket)
        .args([
            "-u",
            "new-session",
            "-d",
            "-s",
            &session,
            "-n",
            ANCHOR_WINDOW,
            "--",
            "/bin/sh",
            "-c",
            IDLE,
        ])
        .status()
        .context("cannot start tmux")?;
    ensure!(status.success(), "tmux new-session exited {status}");
    tracing::info!(%session, socket = %socket.display(), "tmux server up");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if !has_session(&tmux, &socket, &session) {
                        tracing::info!("tmux server gone; exiting");
                        return Ok::<(), anyhow::Error>(());
                    }
                }
                _ = term.recv() => break,
                _ = int.recv() => break,
            }
        }
        tracing::info!("signal received; killing the tmux server");
        let _ = Command::new(&tmux)
            .arg("-S")
            .arg(&socket)
            .args(["-u", "kill-server"])
            .status();
        Ok(())
    })?;
    Ok(ExitCode::SUCCESS)
}

/// The marker's trimmed content: the crew's session name.
fn wait_marker(marker: &Path, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = std::fs::read_to_string(marker)
            && !s.trim().is_empty()
        {
            return Ok(s.trim().to_string());
        }
        if Instant::now() >= deadline {
            bail!(
                "no start marker at {} after {} s",
                marker.display(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(POLL);
    }
}

fn has_session(tmux: &Path, socket: &Path, session: &str) -> bool {
    Command::new(tmux)
        .arg("-S")
        .arg(socket)
        .args(["-u", "has-session", "-t", &format!("={session}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The first executable `name` on PATH.
pub fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}
