#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `balerix agent-supervise` against real processes (Spec N amendment
//! §13.7), and in front of an agent's and a plugin's tmux window (#119).
//! The wrapper is a child subreaper, so it is always its own process
//! here, never the test's.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use balerix_api::PluginManifest;
use balerix_core::{
    AgentId, AgentRunner, HookTarget, LaunchPlan, Materializer, ResolvedPlugin, plugin_id,
};
use balerix_runtime::testing::pid_alive;
use balerix_runtime::{Runtime, StateLayout, TmuxRunner, ToolPaths};

const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

/// A `sleep` duration no other process on the host uses: the test
/// process's pid and a number per test, both fixed width, so no marker is
/// a substring of another, in this process or another test binary's.
/// Every process a test starts carries it on its command line.
fn marker(n: u32) -> String {
    format!("9999.{:07}{n:02}", std::process::id())
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

/// SIGKILLs every process carrying `marker`, repeatedly: a process that
/// forks can outrun one pass. Called from the guards' drops, maybe while
/// unwinding, so no unwrap.
fn kill_marked(marker: &str) {
    let start = Instant::now();
    loop {
        let left = with_cmdline(marker);
        if left.is_empty() || start.elapsed() > Duration::from_secs(2) {
            break;
        }
        for (pid, _) in left {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for Wrapper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        kill_marked(&self.marker);
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

/// A marker must never match another test's: the guard SIGKILLs every
/// process whose command line contains its own.
#[test]
fn no_marker_contains_another() {
    let all: Vec<String> = (0..100).map(marker).collect();
    for (i, a) in all.iter().enumerate() {
        for (j, b) in all.iter().enumerate() {
            assert!(i == j || !a.contains(b.as_str()), "{a} contains {b}");
        }
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

// -- plugins under the wrapper (#119) --

/// Every process carrying the marker is killed on drop, whether the test
/// passed or an assertion failed.
struct Marked(String);

impl Drop for Marked {
    fn drop(&mut self) {
        kill_marked(&self.0);
    }
}

fn required() -> bool {
    std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1")
}

/// git, gh, mise, nono and tmux from PATH, and this crate's `balerix` in
/// the `balerix` slot, so the wrapper a plugin's `launch.sh` names is the
/// real one; otherwise skips (or panics under `BALERIX_REQUIRE_TOOLS=1`).
fn real_tools() -> Option<ToolPaths> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    match ToolPaths::discover_in(&path, Path::new(BALERIX)) {
        Ok(t) => Some(t),
        Err(e) => {
            assert!(!required(), "{e} (BALERIX_REQUIRE_TOOLS=1)");
            eprintln!("skip: {e}");
            None
        }
    }
}

/// Whether nono can sandbox here (Landlock); see
/// `balerix-runtime/tests/support/mod.rs::landlock_works`. The probe's
/// home sits beside its cwd, not inside: nono refuses its own deny rules
/// for the home under an allowed cwd.
fn landlock_works(nono: &Path, root: &Path) -> bool {
    let (cwd, home) = (root.join("nono-probe"), root.join("nono-probe-home"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let ok = Command::new(nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(&cwd)
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
    if !ok {
        assert!(
            !required(),
            "landlock is required (BALERIX_REQUIRE_TOOLS=1)"
        );
        eprintln!("skip: landlock not available");
    }
    ok
}

const PROBE_MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: probe\nversion: 0.0.1\nprotocol: 1\nstart: serve\n";

struct RunningPlugin {
    r: TmuxRunner,
    id: AgentId,
    rt: Runtime,
    plugin: ResolvedPlugin,
    _marked: Marked,
    _server: KillServer,
    _dir: tempfile::TempDir,
}

/// A real plugin package materialized by the runtime, its rendered
/// `launch.sh` started in a tmux window as the plugin host starts it
/// (`ensure_agent` on the plugin's id), its start task `task`. Returns
/// once `up` sleepers carrying `m` are running.
fn running_plugin(label: &str, m: &str, task: &str, up: usize) -> Option<RunningPlugin> {
    let tools = real_tools()?;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    if !landlock_works(&tools.nono, dir.path()) {
        return None;
    }
    let marked = Marked(m.to_string());
    let layout = StateLayout::xdg(
        dir.path().join("state"),
        dir.path().join("data"),
        dir.path().join("config"),
    );
    std::fs::create_dir_all(&layout.config_root).unwrap();
    let package = dir.path().join("pkg");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("balerix-plugin.yaml"), PROBE_MANIFEST).unwrap();
    std::fs::write(
        package.join("mise.toml"),
        format!("[tools]\n\n[tasks.serve]\nrun = '''{task}'''\n"),
    )
    .unwrap();
    let manifest: PluginManifest = serde_norway::from_str(PROBE_MANIFEST).unwrap();
    let plugin = ResolvedPlugin {
        name: "probe".parse().unwrap(),
        package,
        manifest,
        config: serde_json::json!({}),
        fleet_defaults: serde_json::json!({}),
        digest: None,
    };
    let host = HookTarget {
        url: "http://127.0.0.1:1".into(),
        secret: "tok-secret".into(),
    };
    let tmux = tools.tmux.clone();
    let rt = Runtime::new(layout, tools);
    let plan = rt.materialize_plugin(&plugin, &host).unwrap();
    assert_eq!(
        plan.argv[..3],
        [BALERIX, "agent-supervise", "--"].map(String::from),
        "a plugin starts under the wrapper"
    );
    let socket = format!("balerix-test-{label}-{}", std::process::id());
    let guard = KillServer {
        tmux: tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tmux, socket);
    let id = plugin_id(&plugin.name);
    r.ensure_crew(&id.crew_ref()).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| sleepers(m).len() == up);
    Some(RunningPlugin {
        r,
        id,
        rt,
        plugin,
        _marked: marked,
        _server: guard,
        _dir: dir,
    })
}

/// #119: a plugin process that ignores the hangup kept nono alive past
/// the stop. Under the wrapper it is killed after the grace, and the stop
/// returns only then.
#[test]
fn stop_agent_leaves_no_process_of_a_plugin_that_ignores_the_hangup() {
    let m = marker(21);
    let task = format!("trap '' HUP; sleep {m}");
    let Some(p) = running_plugin("plug-hup", &m, &task, 1) else {
        return;
    };
    let start = Instant::now();
    p.r.stop_agent(&p.id).unwrap();
    let took = start.elapsed();
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
    assert!(
        took >= Duration::from_secs(2),
        "the sleeper outlived the hangup, so the stop waited out the grace: {took:?}"
    );
    p.rt.purge_plugin(&p.plugin.name).unwrap();
}

/// #119: plugin processes that detach (`setsid`, a double fork) outlived
/// nono and the stop. Under the wrapper none of them survives it.
#[test]
fn stop_agent_leaves_no_process_of_a_plugin_that_detached_sleepers() {
    let m = marker(22);
    let task = format!("setsid sleep {m} & ( setsid sh -c 'sleep {m} &' & ); exec sleep {m}");
    let Some(p) = running_plugin("plug-setsid", &m, &task, 3) else {
        return;
    };
    p.r.stop_agent(&p.id).unwrap();
    assert!(with_cmdline(&m).is_empty(), "{:?}", with_cmdline(&m));
    p.rt.purge_plugin(&p.plugin.name).unwrap();
}
