#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `balerix agent-supervise` against real processes (Spec N amendment
//! §13.7). The wrapper is a child subreaper, so it is always its own
//! process here, never the test's.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
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

fn supervise(script: &str) -> Child {
    Command::new(BALERIX)
        .args(["agent-supervise", "--", "/bin/sh", "-c", script])
        .stdin(Stdio::null())
        .spawn()
        .unwrap()
}

fn signal(child: &Child, sig: &str) {
    assert!(
        Command::new("kill")
            .args([sig, &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}

/// #107: a `setsid` sleeper and a double-forked one survive the hangup,
/// so the wrapper waits out the grace and kills them.
#[test]
fn a_hangup_ends_the_detached_sleepers_after_the_grace() {
    let m = marker(1);
    let mut w = supervise(&format!(
        "setsid sleep {m} & ( setsid sh -c 'sleep {m} &' & ); exec sleep {m}"
    ));
    wait_for(|| sleepers(&m).len() == 3);
    let start = Instant::now();
    signal(&w, "-HUP");
    let status = w.wait().unwrap();
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
    let mut w = supervise(&format!("exec sleep {m}"));
    wait_for(|| sleepers(&m).len() == 1);
    let start = Instant::now();
    signal(&w, "-HUP");
    let status = w.wait().unwrap();
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
    let mut w = supervise(&format!("setsid sleep {m} & sleep 0.3; exit 7"));
    let status = w.wait().unwrap();
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
    let mut w = supervise("kill -KILL $$");
    assert_eq!(w.wait().unwrap().code(), Some(137));
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
    let mut w = supervise(&format!(
        "setsid /bin/sh {} {m} & exec sleep {m}",
        chain.display()
    ));
    wait_for(|| sleepers(&m).len() == 1);
    std::thread::sleep(Duration::from_millis(300));
    signal(&w, "-HUP");
    assert_eq!(w.wait().unwrap().code(), Some(143));
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
    std::thread::sleep(Duration::from_millis(200));
    assert!(with_cmdline(&m).is_empty(), "no generation escaped");
}

/// The main child ignores the hangup; a second signal during the grace
/// neither shortens it nor breaks the wrapper.
#[test]
fn a_main_child_that_ignores_the_hangup_is_killed_after_the_grace() {
    let m = marker(6);
    let mut w = supervise(&format!("trap '' HUP TERM; while :; do sleep {m}; done"));
    wait_for(|| sleepers(&m).len() == 1);
    let start = Instant::now();
    signal(&w, "-HUP");
    std::thread::sleep(Duration::from_millis(200));
    signal(&w, "-TERM");
    let status = w.wait().unwrap();
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
