//! The Phase 3 journey (spec §8): a real daemon over git, mise, nono and
//! tmux, with `balerix dev fake-claude` in place of `claude`. Skips without
//! the tools or Landlock; `BALERIX_REQUIRE_TOOLS=1` (CI) fails instead.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use balerix_api::{AgentPhase, FleetPhase};
use balerix_core::FleetRecord;
use balerix_runtime::testing::{
    TempRoot, processes_with_arg_pair, reap_dead_sessions, terminate, tmux_socket_dir,
};

const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

fn tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("{name} is required (BALERIX_REQUIRE_TOOLS=1) but not available");
    }
    eprintln!("skip: {name} not available");
    false
}

/// See `balerix-runtime/tests/support/mod.rs::landlock_works` for why the
/// probe home is a sibling of `root`.
fn landlock_works(nono: &Path, root: &Path) -> bool {
    let home = root.with_file_name(format!(
        "{}-nono-probe-home",
        root.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::create_dir_all(&home).unwrap();
    let ok = Command::new(nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = fs::remove_dir_all(&home);
    ok
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
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
}

struct World {
    home: PathBuf,
    socket: String,
    tmux: PathBuf,
}

impl World {
    fn balerix(&self) -> Command {
        let mut c = Command::new(BALERIX);
        c.env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("BALERIX_API_URL")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("GH_CONFIG_DIR");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        let out = self.balerix().args(args).output().unwrap();
        eprintln!(
            "$ balerix {}\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "balerix {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn state(&self) -> PathBuf {
        self.home.join(".local").join("state").join("balerix")
    }
    fn agent_dir(&self, agent: &str) -> PathBuf {
        self.state().join("fleets/e2e/crews/c/agents").join(agent)
    }
    fn status(&self) -> FleetRecord {
        serde_json::from_str(&self.ok(&["status", "e2e", "--json"])).unwrap()
    }
    fn status_of(&self, fleet: &str) -> FleetRecord {
        serde_json::from_str(&self.ok(&["status", fleet, "--json"])).unwrap()
    }
    fn window_pids(&self) -> String {
        let out = Command::new(&self.tmux)
            .args([
                "-L",
                &self.socket,
                "list-windows",
                "-t",
                "=e2e/c",
                "-F",
                "#{window_name} #{pane_pid}",
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn pid_of(&self, window: &str) -> String {
        self.window_pids()
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{window} ")).map(str::to_string))
            .unwrap_or_else(|| panic!("no window {window} in {:?}", self.window_pids()))
    }
}

impl Drop for World {
    /// Stops the daemon and the tmux server whether or not the pid file is
    /// still there: a daemon that removed it and then hung in shutdown is
    /// found by its `--tmux-socket` argument and killed like any other.
    fn drop(&mut self) {
        let mut pids = processes_with_arg_pair("--tmux-socket", &self.socket);
        let pid_file = self.state().join("server").join("balerix.pid");
        if let Some(pid) = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            pids.push(pid);
        }
        pids.sort_unstable();
        pids.dedup();
        terminate(&pids, Duration::from_secs(5));
        let _ = Command::new(&self.tmux)
            .args(["-L", &self.socket, "kill-server"])
            .status();
        // `kill-server` leaves the socket file behind; so would we.
        let _ = fs::remove_file(tmux_sockets().join(&self.socket));
    }
}

fn fleet_yaml(bare: &Path, bob_model: Option<&str>, plugins: Option<&str>) -> String {
    let bob = match bob_model {
        Some(m) => {
            format!("      bob: {{ claude: {{ resume: true, settings: {{ model: {m} }} }} }}\n")
        }
        None => "      bob: { claude: { resume: true } }\n".to_string(),
    };
    // only alice takes plugins; bob is the pass-through control
    let alice = match plugins {
        Some(p) => format!("      alice: {{ plugins: {p} }}\n"),
        None => "      alice: {}\n".to_string(),
    };
    format!(
        // The `sandbox:` block is deliberate: it is merged into the nono
        // profile verbatim, and the daemon's materialize runs a real
        // `nono profile validate`. Without a user block here that merge
        // reaches the real validator in no test at all — which is how
        // `network: { mode: allow }` shipped in the example.
        "apiVersion: balerix/v1\nkind: Fleet\nname: e2e\ndefaults:\n  claude:\n    binary: \"{BALERIX}\"\n    args: [dev, fake-claude, \"--verbose\"]\n    settings: {{ model: sonnet }}\n  sandbox:\n    network: {{ block: false }}\n  tools: {{}}\ncrews:\n  c:\n    repo: \"file://{}\"\n    ref: main\n    git: {{ push: false, auth: none }}\n    agents:\n{alice}{bob}",
        bare.display()
    )
}

/// The fleet a managed-fleet journey's plugin applies: `fleet_yaml`'s
/// shape as the JSON object `PUT fleets/{name}` takes, one agent, on
/// fake-claude, under a real nono profile.
fn managed_fleet_file(bare: &Path) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "balerix/v1", "kind": "Fleet", "name": "managed",
        "defaults": {
            "claude": {
                "binary": BALERIX,
                "args": ["dev", "fake-claude", "--verbose"],
                "settings": { "model": "sonnet" }
            },
            "sandbox": { "network": { "block": false } },
            "tools": {}
        },
        "crews": {
            "c": {
                "repo": format!("file://{}", bare.display()),
                "ref": "main",
                "git": { "push": false, "auth": "none" },
                "agents": { "alice": {} }
            }
        }
    })
}

fn hook_secrets(w: &World) -> Vec<String> {
    let mut out = Vec::new();
    for a in ["alice", "bob"] {
        let settings =
            fs::read_to_string(w.agent_dir(a).join("home/.claude/settings.json")).unwrap();
        for piece in settings.split("Bearer ").skip(1) {
            out.push(piece.split('"').next().unwrap().to_string());
        }
    }
    assert!(!out.is_empty());
    out
}

/// Polls for a file to exist and be non-empty, up to 30 s. The plugin
/// protocol writes these files after the call that triggered them has
/// already returned, so a single read races the writer.
fn wait_file(path: &Path) -> String {
    wait_file_until(path, |s| !s.trim().is_empty())
}

/// Polls (200 ms, 30 s cap) until the file's whole content satisfies
/// `pred`. A file the writer appends to line by line is read while it is
/// still being written: "non-empty" is not "complete", and an unbuffered
/// `writeln!` can even be caught mid-line, so the caller says what it
/// needs to see before the content is worth parsing.
fn wait_file_until(path: &Path, pred: impl Fn(&str) -> bool) -> String {
    let start = Instant::now();
    let mut last = String::new();
    loop {
        if let Ok(s) = fs::read_to_string(path) {
            if pred(&s) {
                return s;
            }
            last = s;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{} never arrived complete; last content:\n{last}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// What an interrupted earlier run left behind — a detached daemon, its
/// tmux server, its socket file, its root under `target/tmp` — is reaped
/// here, by the pid in each name being dead. A test process nextest or
/// Ctrl-C kills never runs `World::drop`; this is the net under it.
fn tmux_sockets() -> PathBuf {
    tmux_socket_dir(
        std::env::var_os("TMUX_TMPDIR")
            .map(PathBuf::from)
            .as_deref(),
    )
}

fn reap_earlier_runs() {
    let Some(tmux) = tool("tmux") else {
        return;
    };
    for prefix in ["balerix-e2e", "balerix-test"] {
        for name in reap_dead_sessions(&tmux, &tmux_sockets(), prefix) {
            eprintln!("reaped {name} from an earlier run");
        }
    }
}

#[test]
fn serve_up_update_down_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    reap_earlier_runs();
    let root = TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "e2e");
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    // empty system tool table: nothing to download, `mise exec` still resolves
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();

    // a repo with one commit and an untrusted mise.toml naming an uninstalled tool
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    fs::write(work.join("mise.toml"), "[tools]\nnode = \"0.0.1\"\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let v1 = root.join("fleet.yaml");
    let v2 = root.join("fleet2.yaml");
    fs::write(&v1, fleet_yaml(&bare, None, None)).unwrap();
    fs::write(&v2, fleet_yaml(&bare, Some("opus"), None)).unwrap();

    // serve -d
    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();

    // up → both Ready through the relay
    let out = w.ok(&[
        "up",
        &v1.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    let rec = w.status();
    assert_eq!(rec.status.phase, FleetPhase::Ready);
    for a in ["e2e/c/alice", "e2e/c/bob"] {
        assert_eq!(rec.status.agents[a].phase, AgentPhase::Ready, "{a}");
    }
    let alice_hash = rec.status.agents["e2e/c/alice"]
        .applied_hash
        .clone()
        .unwrap();
    let bob_hash = rec.status.agents["e2e/c/bob"].applied_hash.clone().unwrap();
    let alice_pid = w.pid_of("alice");
    // `list` pads NAME to the header's width, so the row is "e2e" + 1 pad + 2
    // separators; `status`'s headline joins with a literal two spaces.
    assert!(w.ok(&["list"]).contains("e2e   ready"));
    let argv = fs::read_to_string(w.agent_dir("alice").join("home/fake-claude.argv")).unwrap();
    assert!(
        argv.contains("--verbose") && !argv.contains("--continue"),
        "{argv}"
    );
    assert!(
        w.agent_dir("alice").join("workspace/README").exists(),
        "worktree checked out"
    );
    assert!(w.agent_dir("alice").join("home/.gitconfig").exists());

    // metrics saw the relay (SessionStart) and the HTTP hook (Notification).
    // `up` returns as soon as `Ready` is set by the SessionStart relay; the
    // fake posts `Notification` just after, so poll rather than scrape once.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let session_start = "balerix_hook_events_total{agent=\"alice\",crew=\"c\",event=\"SessionStart\",fleet=\"e2e\"} 1";
    let notification = "balerix_hook_events_total{agent=\"alice\",crew=\"c\",event=\"Notification\",fleet=\"e2e\"} 1";
    let start = Instant::now();
    let metrics = loop {
        let metrics = agent
            .get(format!("{url}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        if metrics.contains(session_start) && metrics.contains(notification) {
            break metrics;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "metrics never showed both hooks within 10s; last scrape:\n{metrics}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(metrics.contains(session_start), "{metrics}");
    assert!(metrics.contains(notification), "{metrics}");

    // update bob only
    let out = w.ok(&[
        "update",
        &v2.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    let rec = w.status();
    assert_eq!(rec.generation, 2);
    assert_ne!(
        rec.status.agents["e2e/c/bob"]
            .applied_hash
            .as_ref()
            .unwrap(),
        &bob_hash
    );
    assert_eq!(
        rec.status.agents["e2e/c/alice"]
            .applied_hash
            .as_ref()
            .unwrap(),
        &alice_hash
    );
    assert_eq!(
        rec.status.agents["e2e/c/bob"].restarts, 0,
        "a spec change is not a crash"
    );
    assert_eq!(w.pid_of("alice"), alice_pid, "alice untouched");

    // down --keep
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    assert!(w.state().join("fleets/e2e/crews/c/repo").exists());
    assert!(w.agent_dir("bob").join("home").exists());
    assert!(w.state().join("fleets/e2e/fleet.json").exists());
    assert!(w.window_pids().is_empty(), "session gone");

    // up again re-applies in place; bob resumes
    let out = w.ok(&[
        "up",
        &v2.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert_eq!(w.status().generation, 3);
    let bob_argv = fs::read_to_string(w.agent_dir("bob").join("home/fake-claude.argv")).unwrap();
    assert!(bob_argv.contains("--continue"), "{bob_argv}");
    let alice_argv =
        fs::read_to_string(w.agent_dir("alice").join("home/fake-claude.argv")).unwrap();
    assert!(!alice_argv.contains("--continue"), "{alice_argv}");

    // no secret leaks into the daemon log or launch scripts
    let secrets = hook_secrets(&w);
    let token = fs::read_to_string(w.state().join("server/token")).unwrap();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    for s in secrets
        .iter()
        .chain(std::iter::once(&token.trim().to_string()))
    {
        assert!(!log.contains(s), "secret in server.log");
        for a in ["alice", "bob"] {
            let launch = fs::read_to_string(w.agent_dir(a).join("launch.sh")).unwrap();
            assert!(!launch.contains(s), "secret in {a}'s launch.sh");
        }
    }

    // purge
    let out = w.ok(&["down", "e2e", "--purge", "--timeout", "60s"]);
    assert!(out.contains("e2e: purged"), "{out}");
    assert!(!w.state().join("fleets/e2e").exists());
    let out = w.run(&["status", "e2e"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    drop(w);
    assert!(
        !root
            .join("home/.local/state/balerix/server/endpoint")
            .exists(),
        "SIGTERM cleaned up"
    );
}

fn plugin_package(dir: &Path, manifest_extra: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("balerix-plugin.yaml"),
        format!(
            "apiVersion: balerix/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\nroutes: false\n{manifest_extra}"
        ),
    )
    .unwrap();
    // an empty tool table (nothing to download) and a task that runs this
    // very binary as the plugin
    fs::write(
        dir.join("mise.toml"),
        format!("[tools]\n\n[tasks.serve]\nrun = \"'{BALERIX}' dev fake-plugin\"\n"),
    )
    .unwrap();
}

/// Waits (up to 180 s, the toolchain install is real) for `plugin list` to
/// show `name` in phase `ready`, and returns that listing. On timeout it
/// prints the plugin's own logs — the sandbox is where a start fails.
fn wait_plugin_ready(w: &World, name: &str, plugin_dir: &Path) -> String {
    let start = Instant::now();
    loop {
        let list = w.ok(&["plugin", "list"]);
        if list
            .lines()
            .any(|l| l.starts_with(name) && l.contains("ready"))
        {
            return list;
        }
        assert!(
            start.elapsed() < Duration::from_secs(180),
            "plugin never became ready; last list:\n{list}\nnono.log:\n{}\nmise.toolchain.log:\n{}",
            fs::read_to_string(plugin_dir.join("logs/nono.log")).unwrap_or_default(),
            fs::read_to_string(plugin_dir.join("logs/mise.toolchain.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Plugins spec §13 item 1, "done when": a trivial SDK plugin reaches
/// `Ready` through a real `mise run` under nono, and the reserved fleet,
/// removal and purge behave.
#[test]
fn plugin_hello_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    reap_earlier_runs();
    let root = TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "e2e-plugins");
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-plugins-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    let pkg = root.join("hello-pkg");
    plugin_package(&pkg, "");
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: hello\n    source: \"{}\"\n    config: {{ greeting: hi }}\n",
            pkg.display()
        ),
    )
    .unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();
    let plugin_dir = w.state().join("plugins/hello");

    // Ready through hello, with a real loopback listen address
    let list = wait_plugin_ready(&w, "hello", &plugin_dir);
    assert!(list.contains("hello  0.1.0    ready"), "{list}");
    assert!(list.contains("127.0.0.1:"), "{list}");
    assert!(
        !plugin_dir.join("scratch/fake-plugin.bind-failed").exists(),
        "spec §11.1 row 1 FAILED: a sandboxed plugin could not bind a loopback listener; record the verdict and take the Unix-socket fallback in phase 2: {}",
        fs::read_to_string(plugin_dir.join("scratch/fake-plugin.bind-failed")).unwrap_or_default()
    );
    let hello = wait_file(&plugin_dir.join("scratch/fake-plugin.hello"));
    assert!(hello.contains("\"greeting\": \"hi\""), "{hello}");
    let rec: FleetRecord = serde_json::from_str(&w.ok(&["status", "balerix", "--json"])).unwrap();
    assert_eq!(
        rec.status.agents["balerix/plugins/hello"].phase,
        AgentPhase::Ready
    );
    assert_eq!(rec.status.phase, FleetPhase::Ready);
    assert!(
        w.ok(&["list"]).contains("no fleets"),
        "plugins are not a fleet row"
    );

    // the token is in the profile (0600) and nowhere else
    let profile_path = plugin_dir.join("nono-profile.json");
    assert_eq!(
        fs::metadata(&profile_path).unwrap().permissions().mode() & 0o777,
        0o600,
        "the profile carries the plugin token"
    );
    let profile = fs::read_to_string(&profile_path).unwrap();
    let token = profile
        .split("\"BALERIX_PLUGIN_TOKEN\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 64);
    assert!(
        !fs::read_to_string(plugin_dir.join("launch.sh"))
            .unwrap()
            .contains(&token)
    );
    assert!(
        !fs::read_to_string(w.state().join("server/server.log"))
            .unwrap()
            .contains(&token)
    );
    assert!(!pkg.join("escape").exists());

    // metrics
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let metrics = agent
        .get(format!("{url}/metrics"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    assert!(
        metrics.contains("balerix_agents{crew=\"plugins\",fleet=\"balerix\",phase=\"ready\"} 1"),
        "{metrics}"
    );

    // a user fleet may not take the reserved name
    let reserved = root.join("reserved.yaml");
    fs::write(
        &reserved,
        "apiVersion: balerix/v1\nkind: Fleet\nname: balerix\n",
    )
    .unwrap();
    let out = w.run(&[
        "up",
        &reserved.display().to_string(),
        "--no-host-defaults",
        "--no-wait",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("reserved for the daemon's plugins"));

    // remove: stopped, window gone, state kept
    let out = w.ok(&["plugin", "remove", "hello"]);
    assert!(out.contains("stopped: hello"), "{out}");
    assert_eq!(w.ok(&["plugin", "list"]), "no plugins\n");
    let start = Instant::now();
    loop {
        let windows = Command::new(&w.tmux)
            .args([
                "-L",
                &w.socket,
                "list-windows",
                "-t",
                "=balerix/plugins",
                "-F",
                "#{window_name}",
            ])
            .output()
            .unwrap();
        if !String::from_utf8_lossy(&windows.stdout).contains("hello") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "plugin window still present"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        plugin_dir.join("scratch").exists(),
        "state survives removal"
    );

    // Purge on an undeclared plugin deletes the state, first time. nono
    // flushes its audit ledger and session file into `plugins/hello/nono`
    // while it exits, which is after tmux has already dropped the window:
    // the daemon waits for the plugin to leave the record and retries
    // `remove_dir_all` on `Directory not empty`, so one attempt is enough.
    let out = w.ok(&["plugin", "remove", "hello", "--purge"]);
    assert!(out.contains("purged"), "{out}");
    assert!(!plugin_dir.exists(), "purge deletes plugins/hello");
    drop(w);
}

/// Plugins spec §13 item 2a, "done when": through a real daemon, nono and
/// tmux, the SDK plugin blocks a PreToolUse and sends text to fake-claude;
/// activation is pending until the plugin says hello, then active.
#[test]
fn plugin_protocol_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    reap_earlier_runs();
    let root = TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "e2e-protocol");
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-protocol-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    let pkg = root.join("fake-pkg");
    plugin_package(
        &pkg,
        "hooks:\n  intercept: [PreToolUse, Stop]\n  observe: [SessionStart, Notification, PreToolUse, Stop]\nneeds: [actions, kv]\n",
    );
    // the package's manifest names the plugin `fake`
    let manifest = fs::read_to_string(pkg.join("balerix-plugin.yaml"))
        .unwrap()
        .replace("name: hello", "name: fake");
    fs::write(pkg.join("balerix-plugin.yaml"), manifest).unwrap();
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: fake\n    source: \"{}\"\n",
            pkg.display()
        ),
    )
    .unwrap();

    // the same bare repo recipe as the first journey
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let fleet = root.join("fleet.yaml");
    fs::write(&fleet, fleet_yaml(&bare, None, Some("{ fake: {} }"))).unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let plugin_dir = w.state().join("plugins/fake");

    // The plugin has to be `Ready` before `up`: `Daemon::apply` activates
    // the pair inline only against a plugin that is already listening, and
    // fake-claude posts its PreToolUse moments after alice starts. A pair
    // left pending would activate at the next hello — too late to block.
    wait_plugin_ready(&w, "fake", &plugin_dir);

    // up waits for the plugin's activation too: the plugin must reach
    // hello (under nono, via mise run) for alice's row to turn active
    let out = w.ok(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert!(out.contains("fake=active"), "{out}");
    let rec = w.status();
    assert_eq!(
        rec.status.agents["e2e/c/alice"].plugins["fake"].state,
        balerix_api::ActivationState::Active
    );
    assert!(rec.status.agents["e2e/c/bob"].plugins.is_empty());
    let list = w.ok(&["plugin", "list"]);
    assert!(list.contains("fake  ") && list.contains("ready"), "{list}");
    assert!(
        list.lines()
            .nth(1)
            .is_some_and(|l| l.split_whitespace().nth(5) == Some("1")),
        "ACTIVE column: {list}"
    );
    let activations = fs::read_to_string(plugin_dir.join("scratch/activations.jsonl")).unwrap();
    assert!(
        activations.contains("\"agent\":\"e2e/c/alice\""),
        "{activations}"
    );

    // the PreToolUse block came back to fake-claude through the HTTP hook
    let reply = wait_file(
        &w.agent_dir("alice")
            .join("home/fake-claude.PreToolUse.reply"),
    );
    assert!(reply.contains("\"decision\":\"block\""), "{reply}");
    assert!(
        reply.contains("fake-plugin: no recursive deletes"),
        "{reply}"
    );
    let bob_reply = wait_file(&w.agent_dir("bob").join("home/fake-claude.PreToolUse.reply"));
    assert_eq!(bob_reply.trim(), "{}", "bob has no plugin: pass-through");

    // the Stop verdict's send_text reached alice's stdin through tmux
    let stdin = wait_file(&w.agent_dir("alice").join("home/fake-claude.stdin"));
    assert!(stdin.contains("fake-plugin says hi"), "{stdin}");

    // Observers saw alice's events, in order, and none of bob's. The
    // delivery task batches 100 ms after the *first* event is enqueued, so
    // `SessionStart` can be flushed on its own and Stop's intercept
    // (proven above) says nothing about its observe batch: wait until every
    // line parses and alice's `Stop` has landed, or a torn append and a
    // half-written file would panic in the parse below.
    let events = wait_file_until(&plugin_dir.join("scratch/events.jsonl"), |s| {
        let mut lines = s.lines().filter(|l| !l.trim().is_empty()).peekable();
        lines.peek().is_some()
            && lines
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<Result<Vec<_>, _>>()
                .is_ok_and(|vs| {
                    vs.iter()
                        .any(|v| v["agent"] == "e2e/c/alice" && v["name"] == "Stop")
                })
    });
    let names: Vec<String> = events
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .map(|v| {
            format!(
                "{} {}",
                v["agent"].as_str().unwrap(),
                v["name"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        names.iter().all(|n| n.starts_with("e2e/c/alice ")),
        "{names:?}"
    );
    let idx = |name: &str| {
        names
            .iter()
            .position(|n| n.ends_with(name))
            .unwrap_or_else(|| panic!("{name} missing in {names:?}"))
    };
    assert!(idx(" SessionStart") < idx(" PreToolUse") && idx(" PreToolUse") < idx(" Stop"));

    // metrics: the chain ran, the action ran, nothing failed
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();
    let metrics = agent
        .get(format!("{url}/metrics"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    assert!(
        metrics.contains(
            "balerix_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"fake\"} 1"
        ),
        "{metrics}"
    );
    assert!(
        metrics.contains("balerix_plugin_actions_total{action=\"send_text\",plugin=\"fake\"} 1"),
        "{metrics}"
    );
    assert!(
        !metrics.contains("balerix_plugin_intercept_failures_total{plugin=\"fake\""),
        "{metrics}"
    );

    // no token leaks (the plugin's, the agents', the admin's)
    let profile = fs::read_to_string(plugin_dir.join("nono-profile.json")).unwrap();
    let token = profile
        .split("\"BALERIX_PLUGIN_TOKEN\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    assert!(!log.contains(&token));
    for s in hook_secrets(&w) {
        assert!(!log.contains(&s), "hook secret in server.log");
    }

    // removing the plugin while the fleet runs: alice's row disappears with the plugin
    w.ok(&["plugin", "remove", "fake"]);
    let start = Instant::now();
    loop {
        let rec = w.status();
        if rec.status.agents["e2e/c/alice"].plugins.is_empty() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "row still there: {rec:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    w.ok(&["down", "e2e", "--purge", "--timeout", "60s"]);
    drop(w);
}

/// Spec L, end to end: a plugin with `manage` applies a fleet from its
/// config at hello; the fleet runs under the plugin's name; the CLI is
/// refused; removing the plugin takes the fleet down; `--force` purges it.
#[test]
fn plugin_manage_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    reap_earlier_runs();
    let root = TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "e2e-manage");
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-manage-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();

    // the same bare repo recipe as the first journey
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );

    // the package: `fake`, with `manage`, told which fleet to apply
    let pkg = root.join("fake-pkg");
    plugin_package(&pkg, "needs: [fleets, manage]\n");
    let manifest = fs::read_to_string(pkg.join("balerix-plugin.yaml"))
        .unwrap()
        .replace("name: hello", "name: fake");
    fs::write(pkg.join("balerix-plugin.yaml"), manifest).unwrap();
    let config = serde_json::json!({
        "manage": { "fleet": "managed", "file": managed_fleet_file(&bare) }
    });
    // JSON is YAML: the object rides on one line after `config:`
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: fake\n    source: \"{}\"\n    config: {}\n",
            pkg.display(),
            config
        ),
    )
    .unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let plugin_dir = w.state().join("plugins/fake");
    wait_plugin_ready(&w, "fake", &plugin_dir);

    // the plugin applied its fleet at hello, and owns it
    let outcome = wait_file(&plugin_dir.join("scratch/fake-plugin.manage"));
    let outcome: serde_json::Value = serde_json::from_str(&outcome).unwrap();
    assert!(outcome.get("error").is_none(), "{outcome}");
    assert_eq!(outcome["owner"], "fake", "{outcome}");
    assert_eq!(outcome["generation"], 1, "{outcome}");
    let status = w.ok(&["status", "managed"]);
    assert!(status.contains("  managed by fake"), "{status}");
    let list = w.ok(&["list"]);
    assert!(
        list.lines()
            .any(|l| l.starts_with("managed ") && l.ends_with("fake")),
        "{list}"
    );

    // it becomes ready like any fleet: the relay's SessionStart from
    // fake-claude, through a real nono profile
    let start = Instant::now();
    loop {
        let rec = w.status_of("managed");
        if rec.status.phase == balerix_api::FleetPhase::Ready {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(180),
            "managed fleet never ready: {rec:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        w.state()
            .join("fleets/managed/crews/c/agents/alice/workspace/README")
            .exists(),
        "the agent's worktree was created from the file's repo"
    );

    // the CLI may not take it over
    let fleet = root.join("managed.yaml");
    fs::write(
        &fleet,
        fleet_yaml(&bare, None, None).replace("name: e2e", "name: managed"),
    )
    .unwrap();
    let out = w.run(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--no-wait",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("fleet managed is managed by plugin fake"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = w.run(&["down", "managed", "--timeout", "30s"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("fleet managed is managed by plugin fake"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // removing the plugin takes its fleet down; --force purges what is left
    let out = w.ok(&["plugin", "remove", "fake"]);
    assert!(out.contains("stopped: fake"), "{out}");
    assert!(out.contains("fleet managed: down"), "{out}");
    let start = Instant::now();
    loop {
        let rec = w.status_of("managed");
        if rec.status.phase == balerix_api::FleetPhase::Down {
            assert_eq!(
                rec.owner.as_deref(),
                Some("fake"),
                "the owner survives the down"
            );
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "managed fleet never down: {rec:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let out = w.ok(&["down", "managed", "--force", "--purge", "--timeout", "60s"]);
    assert!(out.contains("managed: purged"), "{out}");
    assert_eq!(w.ok(&["list"]), "no fleets\n");
    drop(w);
}

/// Where `mise run package-plugins` left the flow package: next to this
/// binary's target dir. `None` when it has not been run.
fn flow_package() -> Option<PathBuf> {
    let dir = Path::new(BALERIX).parent()?.parent()?.join("plugins/flow");
    dir.join("bin/balerix-plugin-flow").exists().then_some(dir)
}

/// alice's `plugins:` block for the journey, in YAML flow style, with the
/// `rm -rf` pattern injectable so the rejection case can break it.
fn flow_block(pattern: &str) -> String {
    format!(
        "{{ flow: {{ initial: working, states: {{ \
           working: {{ on: [ \
             {{ event: PreToolUse, match: {{ /tool_input/command: \"{pattern}\" }}, \
                respond: {{ decision: block, reason: \"flow: no recursive deletes\" }} }}, \
             {{ event: Stop, goto: review, send: {{ text: \"flow says: run the tests\" }} }} ] }}, \
           review: {{ on: [ {{ event: Stop, goto: done }} ] }}, \
           done: {{}} }} }} }}"
    )
}

/// Plugins spec §13 item 2b, "done when" (§17.7): the packaged flow plugin
/// through a real daemon, nono and tmux — a block, a send_text, a
/// transition visible in /metrics and in KV, a rejected `update` that
/// leaves the fleet running, and a reset on `down`.
#[test]
fn flow_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let Some(pkg) = flow_package() else {
        assert!(!require_or_skip(
            "target/plugins/flow (run `mise run package-plugins`)",
            false
        ));
        return;
    };
    reap_earlier_runs();
    let root = TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "e2e-flow");
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-flow-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: flow\n    source: \"{}\"\n",
            pkg.display()
        ),
    )
    .unwrap();

    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let fleet = root.join("fleet.yaml");
    fs::write(
        &fleet,
        fleet_yaml(&bare, None, Some(&flow_block("rm -rf.*"))),
    )
    .unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();
    let plugin_dir = w.state().join("plugins/flow");
    // as in plugin_protocol_journey: the pair must activate inline, before
    // fake-claude's PreToolUse fires
    wait_plugin_ready(&w, "flow", &plugin_dir);

    let out = w.ok(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert!(out.contains("flow=active"), "{out}");

    // the block came back through the HTTP hook, with flow's reason; bob is pass-through
    let reply = wait_file(
        &w.agent_dir("alice")
            .join("home/fake-claude.PreToolUse.reply"),
    );
    assert!(reply.contains("\"decision\":\"block\""), "{reply}");
    assert!(reply.contains("flow: no recursive deletes"), "{reply}");
    let bob_reply = wait_file(&w.agent_dir("bob").join("home/fake-claude.PreToolUse.reply"));
    assert_eq!(bob_reply.trim(), "{}", "bob has no plugin: pass-through");

    // Stop → review carried the send_text to alice's stdin through tmux
    let stdin = wait_file(&w.agent_dir("alice").join("home/fake-claude.stdin"));
    assert!(stdin.contains("flow says: run the tests"), "{stdin}");

    // the transition is in the daemon's /metrics (re-exported from the
    // plugin's scrape) and in KV; poll: the KV put follows the verdict
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let state =
        "balerix_plugin_flow_state{agent=\"alice\",crew=\"c\",fleet=\"e2e\",state=\"review\"} 1";
    let transition = "balerix_plugin_flow_transitions_total{agent=\"alice\",crew=\"c\",fleet=\"e2e\",from=\"working\",to=\"review\"} 1";
    let start = Instant::now();
    let metrics = loop {
        let m = agent
            .get(format!("{url}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        if m.contains(state) && m.contains(transition) {
            break m;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "metrics never showed the transition within 10s; last scrape:\n{m}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        !metrics.contains("state=\"working\"} 1"),
        "the previous state's series is gone: {metrics}"
    );
    let kv = plugin_dir.join("kv/state/e2e/c/alice");
    let stored = wait_file_until(&kv, |s| s.contains("\"review\""));
    assert!(stored.contains("\"state\":\"review\""), "{stored}");

    // a bad regex is rejected with the full config path and the fleet keeps running
    fs::write(&fleet, fleet_yaml(&bare, None, Some(&flow_block("[")))).unwrap();
    let out = w.run(&[
        "update",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "60s",
    ]);
    assert!(!out.status.success(), "update with a bad regex must fail");
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        err.contains(
            "crews.c.agents.alice.plugins.flow: states.working.on[0].match./tool_input/command: "
        ),
        "{err}"
    );
    let rec = w.status();
    assert_eq!(
        rec.status.phase,
        FleetPhase::Ready,
        "the rejected update leaves the fleet phase unchanged"
    );
    assert_eq!(
        rec.status.agents["e2e/c/alice"].plugins["flow"].state,
        balerix_api::ActivationState::Active,
        "the rejected update leaves the activation row unchanged"
    );
    // and the flow state too: the changed pair was offered its new config by
    // an `activate` in place, no `deactivate` before it (§16.2), so the
    // rejection touched neither the plugin's entry nor the KV key
    let stored = fs::read_to_string(&kv).unwrap();
    assert!(
        stored.contains("\"state\":\"review\""),
        "a rejected update keeps the flow state: {stored}"
    );

    // down deactivates: the key is deleted
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    let start = Instant::now();
    while kv.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "kv/state/e2e/c/alice still present after down"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(w);
}

/// Where `mise run package-plugins` left the web package; `None` when it
/// has not been run.
fn web_package() -> Option<PathBuf> {
    let dir = Path::new(BALERIX).parent()?.parent()?.join("plugins/web");
    dir.join("bin/balerix-plugin-web").exists().then_some(dir)
}

/// `GET` with explicit headers, no redirects followed: status, headers, body.
/// The sha256 `plugins/web/assets/VENDOR.md` records for
/// `file`: the last backticked field of its table row.
fn vendored_sha256(file: &str) -> String {
    let vendor = include_str!("../../../plugins/web/assets/VENDOR.md");
    let row = vendor
        .lines()
        .find(|l| l.starts_with(&format!("| `{file}` |")))
        .unwrap_or_else(|| panic!("VENDOR.md has no row for {file}"));
    row.rsplit('`')
        .nth(1)
        .unwrap_or_else(|| panic!("no digest in {row:?}"))
        .to_string()
}

fn raw_get(url: &str, headers: &[(&str, &str)]) -> (u16, Vec<(String, String)>, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let text = resp.body_mut().read_to_string().unwrap();
    (status, headers, text)
}

/// `POST` with explicit headers and a body, redirects not followed.
fn raw_post(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<(String, String)>, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.send(body).unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let text = resp.body_mut().read_to_string().unwrap();
    (status, headers, text)
}

fn body_index_has_review_link(mount: &str, cookie: &str) -> bool {
    let (_, _, body) = raw_get(mount, &[("Cookie", cookie)]);
    body.contains("/v1/plugins/web/agents/e2e/c/alice/review\">review</a>")
}

/// Plugins spec §14 and §18.6, "done when": through a real daemon, nono
/// and tmux, `plugin open` mints a login URL, the browser's cookie opens
/// the mount from the daemon's origin only, the index lists alice from
/// the watch-fed cache, a WebSocket through the proxy shows fake-claude's
/// pane and types into it, and `down` empties the index.
#[test]
fn web_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let (Some(flow_pkg), Some(web_pkg)) = (flow_package(), web_package()) else {
        assert!(!require_or_skip(
            "target/plugins/{flow,web} (run `mise run package-plugins`)",
            false
        ));
        return;
    };
    reap_earlier_runs();
    let root = TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "e2e-web");
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-web-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: flow\n    source: \"{}\"\n  - name: web\n    source: \"{}\"\n",
            flow_pkg.display(),
            web_pkg.display()
        ),
    )
    .unwrap();

    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let fleet = root.join("fleet.yaml");
    fs::write(&fleet, fleet_yaml(&bare, None, Some("{ web: {} }"))).unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();
    wait_plugin_ready(&w, "flow", &w.state().join("plugins/flow"));
    let list = wait_plugin_ready(&w, "web", &w.state().join("plugins/web"));
    assert!(
        list.lines()
            .any(|l| l.starts_with("web") && l.contains("yes")),
        "ROUTES column: {list}"
    );

    let out = w.ok(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert!(out.contains("web=active"), "{out}");

    // plugin open → a login URL on the daemon's origin; the browser's GET
    // becomes a cookie, once
    let login = w.ok(&["plugin", "open", "web"]).trim().to_string();
    assert!(login.starts_with(&format!("{url}/v1/login/")), "{login}");
    assert!(login.ends_with("?to=/v1/plugins/web/"), "{login}");
    let (status, headers, _) = raw_get(&login, &[]);
    assert_eq!(status, 303);
    let cookie = headers
        .iter()
        .find(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.split(';').next().unwrap().to_string())
        .expect("a session cookie");
    assert!(cookie.starts_with("balerix_session="), "{cookie}");
    let (status, _, _) = raw_get(&login, &[]);
    assert_eq!(status, 404, "single use");

    // the mount: cookie from the daemon's origin only; no cookie, no entry
    let mount = format!("{url}/v1/plugins/web/");
    let (status, _, _) = raw_get(&mount, &[]);
    assert_eq!(status, 401);
    let (status, _, body) = raw_get(
        &mount,
        &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("e2e/c/alice"), "{body}");
    assert!(!body.contains("e2e/c/bob"), "bob has no web block: {body}");
    let (status, _, _) = raw_get(
        &mount,
        &[("Cookie", &cookie), ("Origin", "http://evil.example")],
    );
    assert_eq!(status, 403);
    // the index's rows come from fleets/watch: alice is ready there
    let start = Instant::now();
    let rows = loop {
        let (_, _, body) = raw_get(&format!("{mount}agents.json"), &[("Cookie", &cookie)]);
        let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
        if rows[0]["phase"] == "ready" {
            break rows;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "agents.json never showed alice ready: {body}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(rows[0]["id"], "e2e/c/alice");
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let (status, _, page) = raw_get(
        &format!("{mount}agents/e2e/c/alice"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 200);
    // assets are linked under a digest segment; the page says which
    let marker = "/v1/plugins/web/assets/";
    let at = page
        .find(marker)
        .unwrap_or_else(|| panic!("an asset link: {page}"))
        + marker.len();
    let digest = &page[at..at + 12];
    assert!(
        digest.chars().all(|c| c.is_ascii_hexdigit()),
        "a digest segment: {digest:?}"
    );
    assert!(
        page.contains(&format!("{marker}{digest}/xterm.js")),
        "{page}"
    );
    // the served bundle is the one VENDOR.md records, byte for byte
    let (status, _, js) = raw_get(
        &format!("{mount}assets/{digest}/xterm.js"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 200);
    assert_eq!(
        {
            use sha2::Digest as _;
            hex::encode(sha2::Sha256::digest(js.as_bytes()))
        },
        vendored_sha256("xterm.js"),
        "the served xterm.js is not the vendored one"
    );

    // the terminal: through the proxy, the plugin and the daemon's attach
    // to alice's tmux window — fake-claude's pane appears, typed bytes
    // reach its stdin
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let ws_url = format!(
            "{}/v1/plugins/web/agents/e2e/c/alice/ws",
            url.replacen("http://", "ws://", 1)
        );
        let mut req = ws_url.clone().into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        req.headers_mut().insert("origin", url.parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("the browser's socket through the proxy");
        ws.send(Message::Text(r#"{"resize":{"cols":120,"rows":40}}"#.into()))
            .await
            .unwrap();
        let mut seen = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ws.next().await.expect("open").unwrap() {
                    Message::Binary(b) => {
                        seen.extend_from_slice(&b);
                        if String::from_utf8_lossy(&seen).contains("fake-claude") {
                            return;
                        }
                    }
                    Message::Close(f) => panic!(
                        "closed early: {f:?}; saw {:?}",
                        String::from_utf8_lossy(&seen)
                    ),
                    _ => {}
                }
            }
        })
        .await
        .expect("fake-claude's pane over the terminal");
        ws.send(Message::Binary(b"hello-from-browser\n".to_vec().into()))
            .await
            .unwrap();
        let stdin = tokio::task::spawn_blocking({
            let path = w.agent_dir("alice").join("home/fake-claude.stdin");
            move || wait_file_until(&path, |s| s.contains("hello-from-browser"))
        })
        .await
        .unwrap();
        assert!(stdin.contains("hello-from-browser"), "{stdin}");
        ws.close(None).await.unwrap();
        // a cross-origin socket is refused at the handshake
        let mut req = ws_url.into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        req.headers_mut()
            .insert("origin", "http://evil.example".parse().unwrap());
        let e = tokio_tungstenite::connect_async(req).await.unwrap_err();
        assert!(
            matches!(e, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 403),
            "{e}"
        );
    });

    // the review page's data: alice's worktree through the workspace
    // routes, a review pasted into her stdin, the divider in her column
    let (status, _, page) = raw_get(
        &format!("{mount}agents/e2e/c/alice/review"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 200);
    assert!(
        page.contains(r#"const prefix = "/v1/plugins/web""#),
        "{page}"
    );
    assert!(body_index_has_review_link(&mount, &cookie));
    fs::write(
        w.agent_dir("alice").join("workspace/NOTES.md"),
        "agent notes\n",
    )
    .unwrap();
    let (status, _, body) = raw_get(
        &format!("{mount}agents/e2e/c/alice/diff.json"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 200, "{body}");
    let diff: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(diff["base_ref"], "origin/main");
    assert_eq!(diff["head"].as_str().unwrap().len(), 40);
    let notes = diff["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "NOTES.md")
        .unwrap_or_else(|| panic!("NOTES.md in {diff}"));
    assert_eq!(notes["status"], "added");
    assert_eq!(notes["uncommitted"], true);
    assert!(
        notes["patch"].as_str().unwrap().contains("+agent notes"),
        "{notes}"
    );
    let (status, _, text) = raw_get(
        &format!("{mount}agents/e2e/c/alice/file?path=NOTES.md"),
        &[("Cookie", &cookie)],
    );
    assert_eq!((status, text.as_str()), (200, "agent notes\n"));
    let (status, _, text) = raw_get(
        &format!("{mount}agents/e2e/c/alice/file?path=../home/.claude/settings.json"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("invalid path"), "{text}");
    // the column already holds fake-claude's startup events
    let (status, _, body) = raw_get(
        &format!("{mount}agents/e2e/c/alice/events.json"),
        &[("Cookie", &cookie)],
    );
    assert_eq!(status, 200, "{body}");
    let events: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "PreToolUse"),
        "{events}"
    );
    // the version rides on events.json and changes when the worktree does
    let fp0 = events["workspace"]["fingerprint"]
        .as_str()
        .unwrap_or_else(|| panic!("workspace in {events}"))
        .to_string();
    assert_eq!(fp0.len(), 64);
    assert_eq!(events["workspace"]["head"], diff["head"]);
    fs::write(
        w.agent_dir("alice").join("workspace/NOTES.md"),
        "agent notes\nmore\n",
    )
    .unwrap();
    let (_, _, body) = raw_get(
        &format!("{mount}agents/e2e/c/alice/events.json"),
        &[("Cookie", &cookie)],
    );
    let again: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_ne!(again["workspace"]["fingerprint"], fp0, "{again}");
    let (_, _, body) = raw_get(
        &format!("{mount}agents/e2e/c/alice/diff.json"),
        &[("Cookie", &cookie)],
    );
    let diff2: serde_json::Value = serde_json::from_str(&body).unwrap();
    let notes2 = diff2["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "NOTES.md")
        .unwrap();
    assert!(
        notes2["patch"].as_str().unwrap().contains("+more"),
        "{notes2}"
    );
    let review = serde_json::json!({
        "head": diff["head"],
        "base_ref": diff["base_ref"],
        "summary": "Looks fine.",
        "comments": [{ "path": "NOTES.md", "side": "new", "line": 1, "text": "+agent notes",
                       "body": "Please expand these notes." }]
    });
    let (status, _, body) = raw_post(
        &format!("{mount}agents/e2e/c/alice/review"),
        &[
            ("Cookie", &cookie),
            ("Sec-Fetch-Site", "same-origin"),
            ("Content-Type", "application/json"),
        ],
        review.to_string().as_bytes(),
    );
    assert_eq!((status, body.as_str()), (200, "{}"));
    // the paste is pumped line by line: wait for the last line, not a middle one
    let stdin = wait_file_until(&w.agent_dir("alice").join("home/fake-claude.stdin"), |s| {
        s.contains("Looks fine.")
    });
    assert!(stdin.contains("Review against origin/main at "), "{stdin}");
    assert!(
        stdin.contains("NOTES.md line 1 (new):\n> +agent notes\nPlease expand these notes."),
        "{stdin}"
    );
    assert!(stdin.contains("Overall:\nLooks fine."), "{stdin}");
    let start = Instant::now();
    loop {
        let (_, _, body) = raw_get(
            &format!("{mount}agents/e2e/c/alice/events.json"),
            &[("Cookie", &cookie)],
        );
        if body.contains("\"review_sent\"") && body.contains("review sent (1 comment)") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "no divider: {body}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // a cross-origin POST is refused by the daemon, not the plugin
    let (status, _, _) = raw_post(
        &format!("{mount}agents/e2e/c/alice/review"),
        &[
            ("Cookie", &cookie),
            ("Origin", "http://evil.example"),
            ("Content-Type", "application/json"),
        ],
        review.to_string().as_bytes(),
    );
    assert_eq!(status, 403);

    // metrics: the proxy counted, the plugin's gauge rose and fell
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let start = Instant::now();
    loop {
        let m = agent
            .get(format!("{url}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        if m.contains("balerix_plugin_proxy_requests_total{plugin=\"web\",status=\"200\"}")
            && m.contains("balerix_plugin_web_terminals_total 1")
            && m.contains("balerix_plugin_web_terminals_open 0")
            && m.contains("balerix_plugin_web_reviews_total{outcome=\"sent\"} 1")
            && m.contains("balerix_plugin_web_diff_refreshes_total 2")
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "metrics never settled:\n{m}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // no attach session survives the socket; the crew session does
    let sessions = String::from_utf8_lossy(
        &Command::new(&w.tmux)
            .args(["-L", &w.socket, "list-sessions", "-F", "#{session_name}"])
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned();
    assert!(!sessions.contains("balerix-attach-"), "{sessions}");
    assert!(sessions.contains("e2e/c"), "{sessions}");

    // no secret leaks: the admin token, the session cookie and the web token
    let token = fs::read_to_string(w.state().join("server/token")).unwrap();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    assert!(!log.contains(token.trim()));
    assert!(!log.contains(cookie.trim_start_matches("balerix_session=")));
    let profile = fs::read_to_string(w.state().join("plugins/web/nono-profile.json")).unwrap();
    let web_token = profile
        .split("\"BALERIX_PLUGIN_TOKEN\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert!(!log.contains(&web_token));
    // no workspace read went near the agent's home
    assert!(
        !log.contains(&w.agent_dir("alice").join("home").display().to_string()),
        "server.log names the agent's home"
    );

    // down: alice is deactivated and leaves the index; the tmux group is gone
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    let start = Instant::now();
    loop {
        let (_, _, body) = raw_get(&format!("{mount}agents.json"), &[("Cookie", &cookie)]);
        if body.trim() == "[]" {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "alice still listed: {body}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    drop(w);
}
