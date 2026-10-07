#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `balerix agent-supervise` against real processes (Spec N amendment
//! §13.7). The wrapper is a child subreaper, so it is always its own
//! process here, never the test's.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use balerix_core::{AgentId, AgentRunner, LaunchPlan};
use balerix_runtime::TmuxRunner;
use balerix_runtime::testing::pid_alive;

const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

/// A `sleep` duration no other process on the host uses: the test
/// process's pid and a number per test. Every process a test starts
/// carries it on its command line.
fn marker(n: u32) -> String {
    format!("9999.{}{n}", std::process::id())
}

/// Live (not zombie) processes whose command line contains `needle`,
/// with their command lines.
fn with_cmdline(needle: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    // no unwrap: `Wrapper`'s drop calls this, maybe while unwinding
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmd.contains(needle) && pid_alive(pid) {
            out.push((pid, cmd));
        }
    }
    out
}

/// The `sleep <marker>` processes themselves, not the shells that name
/// them in a script.
fn sleepers(marker: &str) -> Vec<u32> {
    with_cmdline(marker)
        .into_iter()
        .filter(|(_, cmd)| cmd.starts_with("sleep "))
        .map(|(pid, _)| pid)
        .collect()
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How long a test waits for the wrapper to exit.
const WAIT_LIMIT: Duration = Duration::from_secs(30);

/// The wrapper under test, and the marker its processes carry. Dropped,
/// whether the test passed or an assertion failed, it kills the wrapper
/// and every marked process, so none of them outlives the test holding
/// nextest's stdout open (#121).
struct Wrapper {
    child: Child,
    marker: String,
}

impl Wrapper {
    fn signal(&self, sig: &str) {
        assert!(
            Command::new("kill")
                .args([sig, &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    }

    /// The wrapper's exit status, within `WAIT_LIMIT`.
    fn wait(&mut self) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                start.elapsed() < WAIT_LIMIT,
                "the wrapper is still running after {:?}",
                start.elapsed()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Wrapper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for (pid, _) in with_cmdline(&self.marker) {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
}

/// `script` under the wrapper; `m` is the marker its processes carry.
fn supervise(m: &str, script: &str) -> Wrapper {
    let child = Command::new(BALERIX)
        .args(["agent-supervise", "--", "/bin/sh", "-c", script])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    Wrapper {
        child,
        marker: m.to_string(),
    }
}

/// The guard itself: a wrapper dropped mid-test, as a failed assertion
/// drops it, leaves nothing running.
#[test]
fn a_wrapper_dropped_mid_test_leaves_nothing_running() {
    let m = marker(7);
    let w = supervise(&m, &format!("setsid sleep {m} & exec sleep {m}"));
    wait_for(|| sleepers(&m).len() == 2);
    drop(w);
    wait_for(|| with_cmdline(&m).is_empty());
}

/// Spec N §13.4: an interrupt (Ctrl-C by hand) is a stop, like a hangup.
#[test]
fn an_interrupt_first_ends_the_detached_sleepers_after_the_grace() {
    let m = marker(8);
    let mut w = supervise(&m, &format!("setsid sleep {m} & exec sleep {m}"));
    wait_for(|| sleepers(&m).len() == 2);
    w.signal("-INT");
    assert_eq!(w.wait().code(), Some(143));
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

/// #107: a `setsid` sleeper and a double-forked one survive the hangup,
/// so the wrapper waits out the grace and kills them.
#[test]
fn a_hangup_ends_the_detached_sleepers_after_the_grace() {
    let m = marker(1);
    let mut w = supervise(
        &m,
        &format!("setsid sleep {m} & ( setsid sh -c 'sleep {m} &' & ); exec sleep {m}"),
    );
    wait_for(|| sleepers(&m).len() == 3);
    let start = Instant::now();
    w.signal("-HUP");
    let status = w.wait();
    let took = start.elapsed();
    assert!(
        with_cmdline(&m).is_empty(),
        "nothing of the agent is alive when the wrapper returns: {:?}",
        with_cmdline(&m)
    );
    assert_eq!(status.code(), Some(143));
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(4),
        "the grace, then the kill: {took:?}"
    );
}

#[test]
fn a_child_that_exits_on_the_hangup_ends_the_wrapper_inside_the_grace() {
    let m = marker(2);
    let mut w = supervise(&m, &format!("exec sleep {m}"));
    wait_for(|| sleepers(&m).len() == 1);
    let start = Instant::now();
    w.signal("-HUP");
    let status = w.wait();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(status.code(), Some(143));
    assert!(with_cmdline(&m).is_empty());
}

/// NS-10: no grace when the main child exits by itself.
#[test]
fn the_main_childs_own_exit_kills_the_rest_and_its_status_is_the_wrappers() {
    let m = marker(3);
    let start = Instant::now();
    let mut w = supervise(&m, &format!("setsid sleep {m} & sleep 0.3; exit 7"));
    let status = w.wait();
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(status.code(), Some(7));
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

#[test]
fn a_main_child_killed_by_a_signal_reports_128_plus_the_signal() {
    let mut w = supervise(&marker(4), "kill -KILL $$");
    assert_eq!(w.wait().code(), Some(137));
}

/// A moving target: each generation detaches the next and exits.
#[test]
fn a_chain_that_respawns_itself_is_emptied() {
    let m = marker(5);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let chain = dir.path().join("chain.sh");
    std::fs::write(
        &chain,
        "sleep 0.01; setsid /bin/sh \"$0\" \"$1\" >/dev/null 2>&1 &\n",
    )
    .unwrap();
    let mut w = supervise(
        &m,
        &format!("setsid /bin/sh {} {m} & exec sleep {m}", chain.display()),
    );
    wait_for(|| sleepers(&m).len() == 1);
    std::thread::sleep(Duration::from_millis(300));
    w.signal("-HUP");
    assert_eq!(w.wait().code(), Some(143));
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
    std::thread::sleep(Duration::from_millis(200));
    assert!(with_cmdline(&m).is_empty(), "no generation escaped");
}

/// The main child ignores the hangup; a second signal during the grace
/// neither shortens it nor breaks the wrapper.
#[test]
fn a_main_child_that_ignores_the_hangup_is_killed_after_the_grace() {
    let m = marker(6);
    let mut w = supervise(
        &m,
        &format!("trap '' HUP TERM; while :; do sleep {m}; done"),
    );
    wait_for(|| sleepers(&m).len() == 1);
    let start = Instant::now();
    w.signal("-HUP");
    std::thread::sleep(Duration::from_millis(200));
    w.signal("-TERM");
    let status = w.wait();
    let took = start.elapsed();
    assert_eq!(status.code(), Some(143));
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(4),
        "{took:?}"
    );
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

#[test]
fn a_command_that_does_not_exist_fails_with_a_message() {
    let out = Command::new(BALERIX)
        .args(["agent-supervise", "--", "/no/such/binary"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("balerix agent-supervise:"), "{stderr}");
}

#[test]
fn no_command_is_a_usage_error() {
    let out = Command::new(BALERIX)
        .arg("agent-supervise")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// The agent's terminal is the wrapper's: nothing is piped or buffered.
#[test]
fn the_childs_stdin_and_stdout_pass_through() {
    assert_cmd::Command::new(BALERIX)
        .args(["agent-supervise", "--", "cat"])
        .write_stdin("through\n")
        .assert()
        .success()
        .stdout("through\n");
}

// -- with tmux: #107 end to end (Spec N amendment §13.7) --

fn tmux() -> Option<PathBuf> {
    let found = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("tmux"))
        .find(|p| p.is_file());
    if found.is_none() {
        if std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
            panic!("tmux is required (BALERIX_REQUIRE_TOOLS=1) but not available");
        }
        eprintln!("skip: tmux not available");
    }
    found
}

struct KillServer {
    tmux: PathBuf,
    socket: String,
}

impl Drop for KillServer {
    fn drop(&mut self) {
        let _ = Command::new(&self.tmux)
            .args(["-L", &self.socket, "kill-server"])
            .status();
    }
}

struct Supervised {
    r: TmuxRunner,
    id: AgentId,
    plan: LaunchPlan,
    _server: KillServer,
    _dir: tempfile::TempDir,
}

/// An agent window whose `launch.sh` is the wrapper in front of a script
/// that detaches one sleeper and stays in another.
fn supervised_agent(label: &str, m: &str) -> Option<Supervised> {
    let tmux = tmux()?;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::create_dir_all(dir.path().join("logs")).unwrap();
    let script = dir.path().join("launch.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec {BALERIX} agent-supervise -- /bin/sh -c 'setsid sleep {m} & exec sleep {m}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = format!("balerix-test-{label}-{}", std::process::id());
    let guard = KillServer {
        tmux: tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tmux, socket);
    let id: AgentId = "f/c/a".parse().unwrap();
    let plan = LaunchPlan {
        cwd: dir.path().to_path_buf(),
        env: BTreeMap::new(),
        argv: vec![],
        script,
    };
    r.ensure_crew(&id.crew_ref()).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| sleepers(m).len() == 2);
    Some(Supervised {
        r,
        id,
        plan,
        _server: guard,
        _dir: dir,
    })
}

/// #107's own case.
#[test]
fn stop_agent_leaves_no_process_of_an_agent_that_detached_a_sleeper() {
    let m = marker(11);
    let Some(a) = supervised_agent("sup-stop", &m) else {
        return;
    };
    a.r.stop_agent(&a.id).unwrap();
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

#[test]
fn stop_crew_leaves_no_process_of_an_agent_that_detached_a_sleeper() {
    let m = marker(12);
    let Some(a) = supervised_agent("sup-crew", &m) else {
        return;
    };
    a.r.stop_crew(&a.id.crew_ref()).unwrap();
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
}

#[test]
fn a_restart_leaves_no_process_of_the_old_agent() {
    let m = marker(13);
    let Some(a) = supervised_agent("sup-restart", &m) else {
        return;
    };
    let old = sleepers(&m);
    a.r.ensure_agent(&a.id, &a.plan).unwrap();
    assert!(
        old.iter().all(|pid| !pid_alive(*pid)),
        "a sleeper of the old agent outlived the restart"
    );
    wait_for(|| sleepers(&m).len() == 2);
    a.r.stop_crew(&a.id.crew_ref()).unwrap();
}
