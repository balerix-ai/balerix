#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Unix socket policy end to end: a sandboxed process cannot connect to
//! a tmux server outside its grants unless the policy is `Open`, and what
//! an agent runs (pipes, child processes, the daemon's git) still works.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use balerix_runtime::socket_policy::{apply_to_profile, probe_mediation, wrap_command};
use balerix_runtime::{Grants, SocketPolicy, ToolPaths, Workspace, render_profile};
use serde_json::json;

const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| v == "1")
}

/// nono, tmux and friends from PATH; otherwise skips (or panics under
/// `BALERIX_REQUIRE_TOOLS=1`).
fn tools() -> Option<ToolPaths> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    match ToolPaths::discover_in(&path, Path::new(BALERIX)) {
        Ok(t) => Some(t),
        Err(e) => {
            assert!(
                !required("BALERIX_REQUIRE_TOOLS"),
                "{e} (BALERIX_REQUIRE_TOOLS=1)"
            );
            eprintln!("skip: {e}");
            None
        }
    }
}

/// The directory a tool's binary sits in, resolved: a tool installed
/// outside the system prefixes (mise) is granted read on its own prefix.
fn prefix_of(bin: &Path) -> PathBuf {
    let real = std::fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    real.parent().unwrap().to_path_buf()
}

/// python3's installation prefix, or None (skipping, or panicking under
/// `BALERIX_REQUIRE_TOOLS=1`).
fn python_prefix() -> Option<PathBuf> {
    let out = Command::new("python3")
        .args(["-c", "import sys;print(sys.base_prefix)"])
        .output()
        .ok()
        .filter(|o| o.status.success());
    if out.is_none() {
        assert!(
            !required("BALERIX_REQUIRE_TOOLS"),
            "python3 is required (BALERIX_REQUIRE_TOOLS=1)"
        );
        eprintln!("skip: python3 not found");
    }
    out.map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

/// A private tmux server outside every grant, killed on drop.
struct Server {
    tmux: PathBuf,
    socket: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new(&self.tmux)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .status();
    }
}

/// One sandbox: a writable directory of its own, nono's home beside it,
/// and a tmux server it was never granted.
struct Fixture {
    tools: ToolPaths,
    own: PathBuf,
    home: PathBuf,
    root: PathBuf,
    port: std::net::TcpListener,
    server: Server,
    _dir: tempfile::TempDir,
}

/// Not under `/tmp`: nono's built-in groups grant it, which would make an
/// isolation assertion there pass vacuously.
fn fixture() -> Option<Fixture> {
    let tools = tools()?;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dir.path().to_path_buf();
    let (own, home, outside) = (root.join("own"), root.join("home"), root.join("server"));
    for d in [&own, &home, &outside] {
        std::fs::create_dir(d).unwrap();
    }
    let server = Server {
        tmux: tools.tmux.clone(),
        socket: outside.join("t.sock"),
    };
    let started = Command::new(&tools.tmux)
        .arg("-S")
        .arg(&server.socket)
        .args(["-f", "/dev/null", "new-session", "-d"])
        .status()
        .unwrap();
    assert!(started.success(), "the test's tmux server did not start");
    Some(Fixture {
        port: std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
        tools,
        own,
        home,
        root,
        server,
        _dir: dir,
    })
}

impl Fixture {
    /// The agent baseline under `policy`: system prefixes, the balerix
    /// binary, tmux's and `extra_read` prefixes read-only, `own` writable,
    /// one loopback port.
    fn profile(&self, policy: SocketPolicy, extra_read: &[PathBuf]) -> PathBuf {
        let mut read: Vec<PathBuf> = ["/usr", "/lib", "/lib64", "/bin", "/etc"]
            .iter()
            .map(PathBuf::from)
            .collect();
        read.push(PathBuf::from(BALERIX));
        read.push(prefix_of(&self.tools.tmux));
        read.extend(extra_read.iter().cloned());
        let grants = Grants {
            read,
            allow: vec![self.own.clone()],
        };
        let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
        let port = self.port.local_addr().unwrap().port();
        let base = render_profile(&id, &grants, port, &Default::default(), &json!({})).unwrap();
        let profile = apply_to_profile(
            base,
            policy,
            std::slice::from_ref(&self.own),
            Path::new(BALERIX),
        );
        let path = self.root.join(format!("profile-{policy:?}.json"));
        std::fs::write(&path, profile.to_string()).unwrap();
        path
    }

    /// `argv` inside nono under `policy`, wrapped as balerix wraps it.
    fn run(&self, policy: SocketPolicy, extra_read: &[PathBuf], argv: &[&str]) -> Output {
        let profile = self.profile(policy, extra_read);
        let argv = wrap_command(
            policy,
            Path::new(BALERIX),
            argv.iter().map(|s| (*s).to_string()).collect(),
        );
        Command::new(&self.tools.nono)
            .env_clear()
            .env("HOME", &self.home)
            .env("NONO_NO_UPDATE_CHECK", "1")
            .args(["-s", "run", "--no-audit", "--profile"])
            .arg(&profile)
            .arg("--")
            .args(argv)
            .current_dir(&self.own)
            .output()
            .unwrap()
    }

    /// A read-only tmux client command against the server, from inside the
    /// sandbox: it has to connect to the server's socket to answer.
    fn has_session(&self, policy: SocketPolicy) -> Output {
        let tmux = self.tools.tmux.display().to_string();
        let socket = self.server.socket.display().to_string();
        self.run(policy, &[], &[&tmux, "-S", &socket, "has-session"])
    }

    /// `balerix sandbox-probe` from inside the sandbox, with the server's
    /// socket as the outside path; its JSON line.
    fn probe(&self, policy: SocketPolicy) -> (serde_json::Value, Output) {
        let port = self.port.local_addr().unwrap().port().to_string();
        let own = self.own.display().to_string();
        let socket = self.server.socket.display().to_string();
        let out = self.run(
            policy,
            &[],
            &[
                BALERIX,
                "sandbox-probe",
                "--tcp",
                &port,
                "--inside",
                &own,
                "--outside",
                &socket,
            ],
        );
        let line = String::from_utf8_lossy(&out.stdout).into_owned();
        let v =
            serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("{e}: {line:?} {out:?}"));
        (v, out)
    }
}

/// The control: without a socket policy the test's sandbox can connect to
/// the server's socket. If this fails, the fixture is wrong and the tests
/// below prove nothing.
#[test]
fn open_policy_control_can_connect_to_the_tmux_server() {
    let Some(f) = fixture() else {
        return;
    };
    let (v, out) = f.probe(SocketPolicy::Open);
    assert_eq!(
        v["outside"], "connected",
        "the control did not connect: {out:?}"
    );
}

#[test]
fn deny_policy_cannot_connect_to_a_tmux_server_outside_its_grants() {
    let Some(f) = fixture() else {
        return;
    };
    let out = f.has_session(SocketPolicy::Deny);
    assert!(!out.status.success(), "{out:?}");
    // refused by the filter (EAFNOSUPPORT), not by some other failure
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Address family not supported"),
        "{out:?}"
    );
}

#[test]
fn mediate_policy_own_sockets_work_and_cannot_connect_to_a_tmux_server_outside_its_grants() {
    let Some(f) = fixture() else {
        return;
    };
    let scratch = f.root.join("probe");
    if let Err(e) = probe_mediation(&f.tools, &scratch) {
        assert!(
            !required("BALERIX_REQUIRE_MEDIATION"),
            "BALERIX_REQUIRE_MEDIATION=1 but pathname mediation does not work here: {e}"
        );
        eprintln!("skip: pathname mediation does not work here: {e}");
        return;
    }
    let out = f.has_session(SocketPolicy::Mediate);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        !out.stderr.is_empty(),
        "the client failed silently: {out:?}"
    );

    let (v, out) = f.probe(SocketPolicy::Mediate);
    assert_eq!(
        v,
        json!({"tcp": "ok", "inside": "ok", "outside": "refused"}),
        "{out:?}"
    );
}

/// What a Claude session does all the time: pipes and child processes.
#[test]
fn deny_policy_claude_like_child_processes_work() {
    let Some(f) = fixture() else {
        return;
    };
    // nono's profile leaves no PATH; the shell sets one
    let out = f.run(
        SocketPolicy::Deny,
        &[],
        &["/bin/sh", "-c", "PATH=/usr/bin:/bin; printf x | cat"],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "x");

    let Some(prefix) = python_prefix() else {
        return;
    };
    let python = prefix.join("bin/python3").display().to_string();
    let out = f.run(
        SocketPolicy::Deny,
        &[prefix],
        &[
            &python,
            "-c",
            "import subprocess;subprocess.run(['/bin/true'],check=True)",
        ],
    );
    assert!(out.status.success(), "{out:?}");
}

// -- the daemon's sandboxed git under Deny --

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "maintenance.auto=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        // a pre-commit hook's environment would point git at the real repo
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The git profile's read grant on the balerix binary is enough for
/// `sandbox-exec` to start, and git works behind it: a harvest, which
/// runs the canary and the probes in the clone under the git profile.
#[test]
fn deny_policy_the_daemons_sandboxed_git_runs_behind_the_wrapper() {
    let Some(tools) = tools() else {
        return;
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dir.path();
    let work = root.join("upstream-work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("upstream.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let repo = balerix_core::RepoRef::parse(&format!("file://{}", bare.display())).unwrap();

    let layout = balerix_runtime::StateLayout::xdg(
        root.join("state"),
        root.join("data"),
        root.join("config"),
    );
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
        cache_is_read_only: false,
        git_read: &[],
        socket_policy: SocketPolicy::Deny,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);

    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();

    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha,
        "the harvest works behind the wrapper"
    );
    let log = std::fs::read_to_string(crew.root.join("logs/git.log")).unwrap();
    let wrapped = format!("{BALERIX} sandbox-exec -- ");
    assert!(
        log.lines()
            .any(|l| l.contains(" version") && l.contains(&wrapped)),
        "the canary did not run behind the wrapper: {log}"
    );
}
