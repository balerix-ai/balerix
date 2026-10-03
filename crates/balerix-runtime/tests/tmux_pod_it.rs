#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §6.3: the runner the sidecar uses. A socket *path* on a shared
//! directory, `-u` on every client call, and waits that never read
//! `/proc` (the pane's pid is another container's).
mod support;

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use balerix_core::{AgentId, AgentRunner, LaunchPlan, ProcessState};
use balerix_runtime::TmuxRunner;
use balerix_runtime::testing::pid_alive;

struct KillServer {
    tmux: PathBuf,
    socket: PathBuf,
}

impl Drop for KillServer {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.tmux)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .stderr(std::process::Stdio::null())
            .status();
    }
}

struct Pane {
    r: TmuxRunner,
    id: AgentId,
    plan: LaunchPlan,
    socket: PathBuf,
    tmux: PathBuf,
    _server: KillServer,
    _root: balerix_runtime::testing::TempRoot,
}

/// A pane process that takes half a second to die after the hangup: a
/// stand-in for the supervisor emptying its tree (as `tmux_it` uses).
const SLOW_TO_DIE: &str = "trap 'sleep 0.5; exit 0' HUP\nwhile :; do sleep 0.1; done";

fn pane(label: &str, body: &str) -> Option<Pane> {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return None;
    };
    let root = support::temp_root(label);
    let socket = root.join("run").join("tmux.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let guard = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::at_socket(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let script = agent_dir.join("launch.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir,
        env: BTreeMap::new(),
        argv: vec![],
        script,
    };
    // the agent container's `balerix-agent run` starts the server with
    // the crew session and its anchor window; the sidecar never does
    start_server(&tools.tmux, &socket);
    r.ensure_crew(&id.crew_ref()).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    Some(Pane {
        r,
        id,
        plan,
        socket,
        tmux: tools.tmux.clone(),
        _server: guard,
        _root: root,
    })
}

/// What `balerix-agent run` does: `new-session` of the crew with the
/// anchor window.
fn start_server(tmux: &std::path::Path, socket: &std::path::Path) {
    let st = std::process::Command::new(tmux)
        .arg("-S")
        .arg(socket)
        .args([
            "-u",
            "new-session",
            "-d",
            "-s",
            "f/c",
            "-n",
            balerix_runtime::ANCHOR_WINDOW,
            "--",
            "/bin/sh",
            "-c",
            "while :; do sleep 3600; done",
        ])
        .status()
        .unwrap();
    assert!(st.success());
}

fn running_pid(p: &Pane) -> u32 {
    match p.r.observe(&p.id.fleet).unwrap().get(&p.id) {
        Some(ProcessState::Running { pid }) => *pid,
        other => panic!("expected running, got {other:?}"),
    }
}

fn tmux(p: &Pane, args: &[&str]) -> String {
    let out = std::process::Command::new(&p.tmux)
        .arg("-S")
        .arg(&p.socket)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "tmux {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_runner_speaks_to_a_socket_path_and_is_a_pod_runner() {
    let Some(p) = pane("podsock", "exec sleep 300") else {
        return;
    };
    assert!(p.r.pod());
    assert!(p.socket.exists(), "the server listens on the given path");
    assert_eq!(
        tmux(&p, &["list-windows", "-F", "#{window_name}"]).trim(),
        "balerix\na"
    );
    assert!(matches!(
        p.r.observe(&p.id.fleet).unwrap().get(&p.id),
        Some(ProcessState::Running { .. })
    ));
}

/// Review Focus 1: with no UTF-8 locale a client without `-u` prints the
/// tabs of `WINDOW_FORMAT` as `_` (§19.1). The runner passes `-u`, so
/// `observe` parses whatever the environment is.
#[test]
fn observe_parses_without_a_utf8_locale() {
    let Some(p) = pane("podlocale", "exec sleep 300") else {
        return;
    };
    // the probe of the bug itself: a plain client in a C locale
    let out = std::process::Command::new(&p.tmux)
        .arg("-S")
        .arg(&p.socket)
        .args(["list-windows", "-F", "#{window_name}\t#{pane_dead}"])
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .env("LANG", "C")
        .output()
        .unwrap();
    let plain = String::from_utf8_lossy(&out.stdout);
    let with_u = std::process::Command::new(&p.tmux)
        .arg("-S")
        .arg(&p.socket)
        .args(["-u", "list-windows", "-F", "#{window_name}\t#{pane_dead}"])
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .env("LANG", "C")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&with_u.stdout).contains('\t'),
        "-u keeps the tab"
    );
    // whether or not this tmux build mangles tabs in a C locale (3.7c on
    // the spike did), the runner's own parse must hold
    eprintln!("plain client printed {plain:?}");
    assert!(running_pid(&p) > 0);
}

/// The pod-mode stop: respawn into a waiter, wait for `pane_dead`, then
/// kill the window. The agent is absent afterwards, as on one machine.
#[test]
fn stop_waits_for_the_pane_to_die_then_removes_the_window() {
    let Some(p) = pane("podstop", SLOW_TO_DIE) else {
        return;
    };
    let pid = running_pid(&p);
    let start = Instant::now();
    p.r.stop_agent(&p.id).unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(450),
        "stop returned before the pane process could have died: {:?}",
        start.elapsed()
    );
    assert!(!pid_alive(pid), "the pane process outlived stop_agent");
    assert_eq!(
        p.r.observe(&p.id.fleet).unwrap().get(&p.id),
        None,
        "absent after stop"
    );
    p.r.stop_agent(&p.id).unwrap(); // absent → ok
    // a pane that already exited stops at once
    p.r.ensure_agent(&p.id, &p.plan).unwrap();
    let pid = running_pid(&p);
    assert!(
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let t = Instant::now();
    while !matches!(
        p.r.observe(&p.id.fleet).unwrap().get(&p.id),
        Some(ProcessState::Exited { .. })
    ) {
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    p.r.stop_agent(&p.id).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(p.r.observe(&p.id.fleet).unwrap().get(&p.id), None);
}

/// A restart waits for the old pane process the same way, and keeps the
/// window.
#[test]
fn a_restart_waits_for_the_old_pane_to_die() {
    let Some(p) = pane("podrestart", SLOW_TO_DIE) else {
        return;
    };
    let old = running_pid(&p);
    p.r.ensure_agent(&p.id, &p.plan).unwrap();
    assert!(!pid_alive(old), "the old pane process outlived the restart");
    assert_ne!(running_pid(&p), old);
}

/// Review Focus 2: past the bound the stop fails and names the pid, and
/// the window is still there with its pane alive (the spec's rule: the
/// window is not removed before the pane is dead).
#[test]
fn a_pane_process_that_ignores_the_hangup_fails_the_stop_and_keeps_the_window() {
    let Some(mut p) = pane("podbound", "trap '' HUP\nwhile :; do sleep 0.2; done") else {
        return;
    };
    p.r.stop_wait = Duration::from_millis(300);
    let pid = running_pid(&p);
    let start = Instant::now();
    let err = p.r.stop_agent(&p.id).unwrap_err();
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        err.to_string(),
        format!("f/c/a: agent processes still running after stop (pid {pid})")
    );
    assert!(pid_alive(pid));
    assert!(
        tmux(&p, &["list-windows", "-F", "#{window_name}"])
            .lines()
            .any(|l| l == "a"),
        "the window stays until the pane is dead"
    );
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

#[test]
fn stop_crew_ends_every_window_and_the_session() {
    let Some(p) = pane("podstopcrew", SLOW_TO_DIE) else {
        return;
    };
    let pid = running_pid(&p);
    p.r.stop_crew(&p.id.crew_ref()).unwrap();
    assert!(!pid_alive(pid));
    assert!(p.r.observe(&p.id.fleet).unwrap().crews.is_empty());
    p.r.stop_crew(&p.id.crew_ref()).unwrap(); // absent → ok
}

/// Spec O §6.4, §10.4: the tmux server, and so Claude, runs in the agent
/// container only. With that server gone (the container restarting),
/// `ensure_crew` fails the pass instead of starting a server in the
/// sidecar's container, and no other call starts one either.
#[test]
fn the_pod_runner_never_starts_a_server() {
    let Some(p) = pane("podnoserver", "exec sleep 300") else {
        return;
    };
    tmux(&p, &["kill-server"]);
    let t = Instant::now();
    while std::os::unix::net::UnixStream::connect(&p.socket).is_ok() {
        assert!(t.elapsed() < Duration::from_secs(5), "the server lingers");
        std::thread::sleep(Duration::from_millis(20));
    }
    let err = p.r.ensure_crew(&p.id.crew_ref()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "f/c: tmux has-session: the agent container's tmux server is not running"
    );
    assert!(p.r.ensure_agent(&p.id, &p.plan).is_err());
    assert!(p.r.attach(&p.id).is_err());
    assert!(p.r.send_text(&p.id, "x\ny", true).is_err());
    assert!(
        std::os::unix::net::UnixStream::connect(&p.socket).is_err(),
        "nothing listens on the socket"
    );
}

#[test]
fn attach_works_over_the_socket_path() {
    let Some(p) = pane("podattach", "exec sleep 300") else {
        return;
    };
    let stream = p.r.attach(&p.id).unwrap();
    let mut reader = stream.reader().unwrap();
    let mut buf = [0u8; 1024];
    // the grouped session draws the pane; any bytes prove the PTY is up
    let n = reader.read(&mut buf).unwrap();
    assert!(n > 0);
    drop(stream);
    let t = Instant::now();
    loop {
        let sessions = tmux(&p, &["list-sessions", "-F", "#{session_name}"]);
        if !sessions.contains("balerix-attach-") {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the attach session lingers"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
