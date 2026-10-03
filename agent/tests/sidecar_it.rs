#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §15 "Sidecar and link": a Daemon and a sidecar as two processes,
//! no cluster, the real tools (git, mise, nono, tmux) and `dev
//! fake-claude` in place of `claude`. Skips without the tools or
//! `BALERIX_BIN`; fails instead under `BALERIX_REQUIRE_TOOLS=1`.
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use balerix_api::{AgentPhase, LinkOp, LinkRequest, LinkResult, SidecarFrame};
use serde_json::{Value, json};
use support::fake_daemon::{Conn, FakeDaemon};

const BIN: &str = env!("CARGO_BIN_EXE_balerix-agent");
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const ADMIN: &str = "fedcba9876543210fedcba9876543210";

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn chmod_tree(dir: &Path, dirs: u32, files: u32) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            chmod_tree(&p, dirs, files);
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(dirs)).unwrap();
        } else {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(files)).unwrap();
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(dirs)).unwrap();
}

/// The pod's three mounts under `root`, the crew slice filled the way the
/// Jobs fill it (§8.3): the cache's objects, read-only, and the daemon
/// pool with the embedded tool table (claude, gh) so the agent's own
/// `mise install` finds them and installs nothing.
struct Pod {
    root: PathBuf,
    origin: String,
    bundle: PathBuf,
    ca: PathBuf,
    run: Option<Kill>,
    sidecar: Option<Kill>,
    /// For the server `run` started: `Drop` ends it.
    tmux: PathBuf,
}

impl Pod {
    fn prepare(tools: &support::Tools, label: &str, ca: PathBuf) -> Pod {
        let root = support::temp_root(label);
        for d in ["agent", "shared", "run", "secret"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("f"), "one\n").unwrap();
        git(&origin, &["add", "f"]);
        git(&origin, &["commit", "-q", "-m", "one"]);
        let bare = root.join("cache-full");
        assert!(
            Command::new("git")
                .args(["clone", "-q", "--bare"])
                .arg(&origin)
                .arg(&bare)
                .status()
                .unwrap()
                .success()
        );
        let objects = root.join("shared/repo/.git/objects");
        std::fs::create_dir_all(objects.parent().unwrap()).unwrap();
        assert!(
            Command::new("cp")
                .arg("-r")
                .arg(bare.join("objects"))
                .arg(&objects)
                .status()
                .unwrap()
                .success()
        );
        chmod_tree(&objects, 0o555, 0o444);
        // the daemon pool, installed as the runtime installs a pool: its
        // own table as the global file, from `/` so that no project
        // `mise.toml` above the test root (the repository's) is read
        let pool = root.join("shared/daemon/mise");
        let table = root.join("pool-config/mise.toml");
        std::fs::create_dir_all(table.parent().unwrap()).unwrap();
        let mut toml = String::from("[tools]\n");
        for (tool, version) in balerix_runtime::embedded_system_tools() {
            toml.push_str(&format!("{tool} = \"{version}\"\n"));
        }
        std::fs::write(&table, toml).unwrap();
        let env = balerix_runtime::level_env(&table, &pool, &[]);
        for args in [
            vec!["trust".to_string(), table.display().to_string()],
            vec!["install".to_string()],
        ] {
            let out = Command::new(&tools.mise)
                .current_dir("/")
                .args(&args)
                .envs(&env)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "mise {args:?} for the daemon pool: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Pod {
            origin: format!("file://{}", origin.display()),
            bundle: root.join("secret/agent.json"),
            ca,
            root,
            run: None,
            sidecar: None,
            tmux: tools.tmux.clone(),
        }
    }

    fn write_bundle(&self, tools: &support::Tools, daemon_url: &str) {
        std::fs::write(
            &self.bundle,
            json!({
                "agent": "f/c/a",
                "repo": self.origin,
                "git_ref": "main",
                "git": { "push": false, "auth": "none" },
                "settings": {
                    "claude": { "binary": tools.balerix.display().to_string(), "args": ["dev", "fake-claude", "--verbose"] },
                    "sandbox": { "network": { "block": false } }
                },
                "daemon_url": daemon_url,
                "token": TOKEN,
                "credentials": {}
            })
            .to_string(),
        )
        .unwrap();
    }

    fn start(&mut self, tools: &support::Tools, plain_http: bool) {
        let run = Command::new(BIN)
            .args(["run", "--start-timeout-secs", "600", "--run-dir"])
            .arg(self.root.join("run"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(self.root.join("run.log")).unwrap(),
            ))
            .spawn()
            .unwrap();
        self.run = Some(Kill(run));
        let mut cmd = Command::new(BIN);
        cmd.args(["sidecar", "--hook-port", "0"])
            .arg("--bundle")
            .arg(&self.bundle)
            .arg("--ca")
            .arg(&self.ca)
            .arg("--agent-dir")
            .arg(self.root.join("agent"))
            .arg("--shared-dir")
            .arg(self.root.join("shared"))
            .arg("--run-dir")
            .arg(self.root.join("run"))
            .arg("--balerix")
            .arg(&tools.balerix)
            .arg("--termination-log")
            .arg(self.root.join("termination-log"))
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(self.root.join("sidecar.log")).unwrap(),
            ));
        if plain_http {
            cmd.arg("--allow-plain-http");
        }
        self.sidecar = Some(Kill(cmd.spawn().unwrap()));
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/tmux.sock")
    }
    fn home(&self) -> PathBuf {
        self.root.join("agent/home")
    }
    fn tmux(&self, tools: &support::Tools, args: &[&str]) -> String {
        let out = Command::new(&tools.tmux)
            .arg("-S")
            .arg(self.socket())
            .arg("-u")
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    /// Only the anchor window is left in the crew's session.
    fn agent_window_gone(&self, tools: &support::Tools) -> bool {
        self.tmux(
            tools,
            &["list-windows", "-t", "=f/c", "-F", "#{window_name}"],
        )
        .trim()
            == "balerix"
    }
    /// The port the profile granted: where Claude posts hooks.
    fn hook_port(&self) -> u16 {
        let profile: Value = serde_json::from_str(
            &std::fs::read_to_string(self.root.join("agent/nono-profile.json")).unwrap(),
        )
        .unwrap();
        profile["network"]["open_port"][0].as_u64().unwrap() as u16
    }
    fn still_running(&mut self) -> bool {
        self.sidecar
            .as_mut()
            .unwrap()
            .0
            .try_wait()
            .unwrap()
            .is_none()
    }
}

impl Drop for Pod {
    fn drop(&mut self) {
        self.sidecar.take();
        // `run` kills the server only on SIGTERM, and `Kill` is SIGKILL:
        // the daemonised server would outlive it, panes and all
        support::kill_tmux_server(&self.tmux, &self.socket());
        self.run.take();
        let objects = self.root.join("shared/repo/.git/objects");
        if objects.exists() {
            chmod_tree(&objects, 0o755, 0o644);
        }
        if std::thread::panicking() {
            eprintln!(
                "--- sidecar.log\n{}",
                std::fs::read_to_string(self.root.join("sidecar.log")).unwrap_or_default()
            );
            eprintln!(
                "--- termination-log\n{}",
                std::fs::read_to_string(self.root.join("termination-log")).unwrap_or_default()
            );
        }
    }
}

fn ca_file(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    std::fs::write(dir.join("ca.crt"), ca_cert.pem()).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = leaf.signed_by(&key, &issuer).unwrap();
    std::fs::write(dir.join("tls.crt"), cert.pem()).unwrap();
    std::fs::write(dir.join("tls.key"), key.serialize_pem()).unwrap();
    (dir.join("ca.crt"), dir.join("tls.crt"), dir.join("tls.key"))
}

async fn next_status(
    conn: &mut Conn,
    pred: impl Fn(&balerix_api::LinkStatus) -> bool,
    what: &str,
) -> balerix_api::LinkStatus {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, conn.from_sidecar.recv()).await {
            Ok(Some(SidecarFrame::Status(s))) if pred(&s) => return s,
            Ok(Some(_)) => {}
            Ok(None) => panic!("the link closed while waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

/// The next `name` the Daemon's events route received, skipping the
/// others (`dev fake-claude` posts more after `SessionStart`).
async fn next_event(daemon: &mut FakeDaemon, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, daemon.events.recv()).await {
            Ok(Some(n)) if n == name => return,
            Ok(Some(_)) => {}
            Ok(None) => panic!("the events route is gone"),
            Err(_) => panic!("timed out waiting for {name}"),
        }
    }
}

async fn request(conn: &mut Conn, id: u64, op: LinkOp) -> LinkResult {
    conn.to_sidecar.send(LinkRequest { id, op }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, conn.from_sidecar.recv()).await {
            Ok(Some(SidecarFrame::Reply(r))) if r.id == id => return r.result,
            Ok(Some(_)) => {}
            _ => panic!("no reply to request {id}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sidecar_against_a_fake_daemon() {
    let Some(tools) = support::tools() else {
        return;
    };
    let scratch = support::temp_root("sidecar-fake-ca");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &scratch)) {
        return;
    }
    let (ca, _, _) = ca_file(&scratch);
    let mut daemon = FakeDaemon::start(None).await;
    let mut pod = Pod::prepare(&tools, "sidecar-fake", ca);
    pod.write_bundle(&tools, &format!("http://{}", daemon.addr));
    pod.start(&tools, true);

    // the link comes up with the token; SessionStart travels hook-relay →
    // sidecar → Daemon; the agent is Ready and the marker exists
    let mut conn = tokio::time::timeout(Duration::from_secs(180), daemon.links.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.headers["authorization"], format!("Bearer {TOKEN}"));
    let ready = next_status(&mut conn, |s| s.status.phase == AgentPhase::Ready, "Ready").await;
    assert!(ready.pid.is_some(), "the pane's pid rides the status frame");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), daemon.events.recv())
            .await
            .unwrap()
            .unwrap(),
        "SessionStart"
    );
    assert!(pod.root.join("run/ready").is_file());
    assert!(pod.home().join("fake-claude.argv").is_file());

    // send_text over the link reaches fake-claude's stdin
    assert_eq!(
        request(
            &mut conn,
            1,
            LinkOp::SendText {
                text: "hello".into(),
                submit: true
            }
        )
        .await,
        LinkResult::Ok
    );
    support::wait_for("hello on stdin", Duration::from_secs(10), || {
        std::fs::read_to_string(pod.home().join("fake-claude.stdin"))
            .is_ok_and(|s| s.contains("hello"))
    });

    // a workspace read runs in the sidecar, in the clone
    assert_eq!(
        request(&mut conn, 2, LinkOp::WorkspaceFile { path: "f".into() }).await,
        LinkResult::File {
            bytes: b"one\n".to_vec()
        }
    );
    assert!(matches!(
        request(
            &mut conn,
            3,
            LinkOp::WorkspaceFile {
                path: "nope".into()
            }
        )
        .await,
        LinkResult::Failed { .. }
    ));

    // attach: the second socket carries the pane
    conn.to_sidecar
        .send(LinkRequest {
            id: 4,
            op: LinkOp::Attach {
                session: "s1".into(),
            },
        })
        .unwrap();
    let mut att = tokio::time::timeout(Duration::from_secs(10), daemon.attaches.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(att.headers["authorization"], format!("Bearer {TOKEN}"));
    loop {
        match conn.from_sidecar.recv().await.unwrap() {
            SidecarFrame::Reply(r) if r.id == 4 => {
                assert_eq!(r.result, LinkResult::Ok);
                break;
            }
            _ => {}
        }
    }
    let first = tokio::time::timeout(Duration::from_secs(10), att.from_sidecar.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(first, axum::extract::ws::Message::Binary(b) if !b.is_empty()),
        "pane bytes"
    );
    drop(att);

    // stop holds; the window is gone; the marker is gone; restart brings
    // a second SessionStart
    assert_eq!(request(&mut conn, 5, LinkOp::Stop).await, LinkResult::Ok);
    next_status(
        &mut conn,
        |s| s.status.phase == AgentPhase::Stopped,
        "Stopped",
    )
    .await;
    support::wait_for("only the anchor window", Duration::from_secs(10), || {
        pod.agent_window_gone(&tools)
    });
    assert!(!pod.root.join("run/ready").exists());
    assert_eq!(request(&mut conn, 6, LinkOp::Restart).await, LinkResult::Ok);
    next_status(
        &mut conn,
        |s| s.status.phase == AgentPhase::Ready,
        "Ready again",
    )
    .await;
    next_event(&mut daemon, "SessionStart").await;

    // a dead Claude restarts with the planner's back-off: the supervisor
    // is told to stop (its tree ends, the pane dies), the sidecar notes
    // the exit (`restarts` counts one) and restarts it in a new pane
    let pid = pod
        .tmux(
            &tools,
            &[
                "list-windows",
                "-t",
                "=f/c",
                "-F",
                "#{window_name}\t#{pane_pid}",
            ],
        )
        .lines()
        .find_map(|l| l.strip_prefix("a\t").map(|p| p.trim().to_string()))
        .unwrap();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let killed: u32 = pid.parse().unwrap();
    next_status(&mut conn, |s| s.status.restarts == 1, "the exit noted").await;
    let after = next_status(
        &mut conn,
        |s| s.status.phase == AgentPhase::Ready && s.pid.is_some_and(|p| p != killed),
        "Ready in a new pane",
    )
    .await;
    assert_ne!(after.pid, ready.pid);
    assert!(pod.root.join("run/ready").is_file());

    // the Daemon goes away: hooks fail open inside the budget and are
    // counted; the link comes back when the Daemon does
    let addr = daemon.addr;
    daemon.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let port = pod.hook_port();
    let tls = balerix_agent::tls::client_config(&pod.ca).unwrap();
    let client = balerix_agent::tls::http_client(&tls, Duration::from_secs(10)).unwrap();
    let start = Instant::now();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/agents/f/c/a/events"))
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"hook_event_name":"Stop"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap(), "{}");
    assert!(
        start.elapsed() < Duration::from_millis(3500),
        "{:?}",
        start.elapsed()
    );
    // the route hands an event to the sidecar before forwarding it, so the
    // count reaches the status once the forward budget is spent; the
    // Daemon stays away past that, and its first frame must carry it
    tokio::time::sleep(balerix_agent::hooks::FORWARD_BUDGET + Duration::from_secs(1)).await;
    let mut daemon = FakeDaemon::start(Some(addr)).await;
    let mut conn = tokio::time::timeout(Duration::from_secs(60), daemon.links.recv())
        .await
        .unwrap()
        .unwrap();
    let s = next_status(&mut conn, |_| true, "a status after reconnect").await;
    assert!(s.hook_failures >= 1, "{s:?}");
    assert!(pod.still_running());
    assert_no_server_left(pod, &tools);
}

/// Dropping the pod leaves no tmux server under its root.
fn assert_no_server_left(pod: Pod, tools: &support::Tools) {
    let root = pod.root.clone();
    drop(pod);
    assert_eq!(
        support::live_tmux_servers(&tools.tmux, &root),
        Vec::<PathBuf>::new()
    );
}

/// `balerix serve --mode kubernetes` on `bind`, its state under `home`.
fn serve(tools: &support::Tools, scratch: &Path, home: &Path, bind: &str) -> Kill {
    Kill(
        Command::new(&tools.balerix)
            .args([
                "serve",
                "--mode",
                "kubernetes",
                "--bind",
                bind,
                "--tmux-socket",
                "unused",
            ])
            .arg("--tls-cert")
            .arg(scratch.join("tls.crt"))
            .arg("--tls-key")
            .arg(scratch.join("tls.key"))
            .arg("--admin-token-file")
            .arg(scratch.join("admin-token"))
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::options()
                    .create(true)
                    .append(true)
                    .open(scratch.join("daemon.log"))
                    .unwrap(),
            ))
            .spawn()
            .unwrap(),
    )
}

/// Prints the daemons' log when the test fails.
struct DaemonLog(PathBuf);
impl Drop for DaemonLog {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "--- daemon.log\n{}",
                std::fs::read_to_string(&self.0).unwrap_or_default()
            );
        }
    }
}

/// SIGTERM, and the exit.
fn shut_down(daemon: &mut Kill) {
    assert!(
        Command::new("kill")
            .args(["-TERM", &daemon.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    support::wait_for("the daemon to exit", Duration::from_secs(20), || {
        daemon.0.try_wait().unwrap().is_some()
    });
}

/// The URL the daemon just published, once it is not `previous`.
fn endpoint_url(endpoint: &Path, previous: Option<&str>) -> String {
    let read = || {
        std::fs::read_to_string(endpoint)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| s.starts_with("https://") && Some(s.as_str()) != previous)
    };
    support::wait_for("the endpoint", Duration::from_secs(20), || read().is_some());
    read().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sidecar_against_a_real_daemon_over_tls() {
    let Some(tools) = support::tools() else {
        return;
    };
    let scratch = support::temp_root("sidecar-real-ca");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &scratch)) {
        return;
    }
    let (ca, _, _) = ca_file(&scratch);
    std::fs::write(scratch.join("admin-token"), format!("{ADMIN}\n")).unwrap();
    let _log = DaemonLog(scratch.join("daemon.log"));
    let daemon_home = scratch.join("daemon-home");
    std::fs::create_dir_all(&daemon_home).unwrap();
    let endpoint = daemon_home.join(".local/state/balerix/server/endpoint");
    let mut daemon = serve(&tools, &scratch, &daemon_home, "127.0.0.1:0");
    let url = endpoint_url(&endpoint, None);
    let bind = url.trim_start_matches("https://").to_string();

    let tls = balerix_agent::tls::client_config(&ca).unwrap();
    let http = balerix_agent::tls::http_client(&tls, Duration::from_secs(10)).unwrap();
    let mut pod = Pod::prepare(&tools, "sidecar-real", ca.clone());
    pod.write_bundle(&tools, &url);
    let spec = json!({
        "spec": { "name": "f", "crews": { "c": {
            "repo": pod.origin, "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "a": {
                "claude": { "binary": tools.balerix.display().to_string(), "args": ["dev", "fake-claude", "--verbose"] },
                "sandbox": { "network": { "block": false } }
            } }
        } } },
        "agent_tokens": { "f/c/a": TOKEN }
    });
    let put = |url: &str| {
        let (http, url, spec) = (http.clone(), url.to_string(), spec.clone());
        async move {
            http.put(format!("{url}/v1/fleets/f"))
                .bearer_auth(ADMIN)
                .json(&spec)
                .send()
                .await
                .unwrap()
        }
    };
    let down = |url: &str| {
        let (http, url) = (http.clone(), url.to_string());
        async move {
            http.delete(format!(
                "{url}/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false&force=true"
            ))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap()
        }
    };
    let agent = |url: &str| {
        let (http, url) = (http.clone(), url.to_string());
        async move {
            let v: Value = http
                .get(format!("{url}/v1/fleets/f"))
                .bearer_auth(ADMIN)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["status"]["agents"]["f/c/a"].clone()
        }
    };
    let resp = put(&url).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );

    pod.start(&tools, false);
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let a = agent(&url).await;
        if a["phase"] == "ready" {
            break;
        }
        assert!(Instant::now() < deadline, "never ready: {a}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // a down recorded while the link is down: the Daemon goes, and the
    // down is made by another Daemon on the same state that the sidecar
    // cannot reach (another port). When the Daemon is back on its port,
    // the sidecar's first status says it is running, and the answer is a
    // stop.
    shut_down(&mut daemon);
    let mut elsewhere = serve(&tools, &scratch, &daemon_home, "127.0.0.1:0");
    let other = endpoint_url(&endpoint, Some(&url));
    assert_eq!(down(&other).await.status().as_u16(), 200);
    shut_down(&mut elsewhere);
    daemon = serve(&tools, &scratch, &daemon_home, &bind);
    assert_eq!(endpoint_url(&endpoint, Some(&other)), url);
    support::wait_for(
        "the stop that answers the reconnect",
        Duration::from_secs(90),
        || pod.agent_window_gone(&tools),
    );
    assert!(pod.still_running());

    // up again over the live link: the apply that takes the fleet from
    // Down to Up sends the sidecar a restart (nothing else would start
    // the agent; the sidecar holds the stop)
    let resp = put(&url).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let a = agent(&url).await;
        if a["phase"] == "ready" {
            break;
        }
        assert!(Instant::now() < deadline, "not back: {a}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(!pod.agent_window_gone(&tools));

    // a forced down from the operator's side travels the live link as a stop
    assert_eq!(down(&url).await.status().as_u16(), 200);
    support::wait_for("the agent window to go", Duration::from_secs(30), || {
        pod.agent_window_gone(&tools)
    });
    assert!(pod.still_running());
    drop(daemon);
    assert_no_server_left(pod, &tools);
}
