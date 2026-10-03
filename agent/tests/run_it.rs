#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §6.2 step 3: the agent container waits for the sidecar's
//! marker, starts the tmux server on the shared socket, and lives as long
//! as the server does.
mod support;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_balerix-agent");

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tmux(tools: &support::Tools, socket: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Command::new(&tools.tmux)
        .arg("-S")
        .arg(socket)
        .arg("-u")
        .args(args)
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn run_waits_for_the_marker_starts_the_server_and_ends_with_it() {
    let Some(tools) = support::tools() else {
        return;
    };
    let root = support::temp_root("run");
    let run_dir = root.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let socket = run_dir.join("tmux.sock");
    let _server = support::TmuxServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let child = Command::new(BIN)
        .args(["run", "--start-timeout-secs", "20", "--run-dir"])
        .arg(&run_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child = Kill(child);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "exited before the marker"
    );
    assert!(!socket.exists(), "no server before the marker");

    std::fs::write(run_dir.join("started"), "f/c\n").unwrap();
    support::wait_for("the socket", Duration::from_secs(10), || socket.exists());
    support::wait_for("the anchor window", Duration::from_secs(10), || {
        tmux(
            &tools,
            &socket,
            &["list-windows", "-t", "=f/c", "-F", "#{window_name}"],
        )
        .is_some_and(|w| w.trim() == "balerix")
    });
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "stays while the server lives"
    );

    tmux(&tools, &socket, &["kill-server"]);
    support::wait_for("run to exit", Duration::from_secs(5), || {
        child.0.try_wait().unwrap().is_some()
    });
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn run_gives_up_without_a_marker() {
    let Some(_tools) = support::tools() else {
        return;
    };
    let root = support::temp_root("run-nomarker");
    let run_dir = root.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let out = Command::new(BIN)
        .args(["run", "--start-timeout-secs", "1", "--run-dir"])
        .arg(&run_dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!(
        "no start marker at {} after 1 s",
        run_dir.join("started").display()
    )));
}

#[test]
fn a_sigterm_ends_the_server() {
    let Some(tools) = support::tools() else {
        return;
    };
    let root = support::temp_root("run-term");
    let run_dir = root.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("started"), "f/c\n").unwrap();
    let socket = run_dir.join("tmux.sock");
    let _server = support::TmuxServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let child = Command::new(BIN)
        .args(["run", "--run-dir"])
        .arg(&run_dir)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = Kill(child);
    support::wait_for("the server", Duration::from_secs(10), || {
        tmux(&tools, &socket, &["has-session", "-t", "=f/c"]).is_some()
    });
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    support::wait_for("run to exit", Duration::from_secs(5), || {
        child.0.try_wait().unwrap().is_some()
    });
    assert!(
        tmux(&tools, &socket, &["has-session", "-t", "=f/c"]).is_none(),
        "the server is gone"
    );
}

/// The helper every guard uses: it finds a live server under a root and
/// ends it, and one already gone is not an error.
#[test]
fn kill_tmux_server_leaves_no_server_under_the_root() {
    let Some(tools) = support::tools() else {
        return;
    };
    let root = support::temp_root("run-kill-server");
    let socket = root.join("deep/tmux.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    assert!(
        tmux(
            &tools,
            &socket,
            &[
                "new-session",
                "-d",
                "-s",
                "x",
                "--",
                "/bin/sh",
                "-c",
                "sleep 600"
            ]
        )
        .is_some()
    );
    assert_eq!(
        support::live_tmux_servers(&tools.tmux, &root),
        vec![socket.clone()]
    );
    support::kill_tmux_server(&tools.tmux, &socket);
    assert!(support::live_tmux_servers(&tools.tmux, &root).is_empty());
    support::kill_tmux_server(&tools.tmux, &socket);
}
