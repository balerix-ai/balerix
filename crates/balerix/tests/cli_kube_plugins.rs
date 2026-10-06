#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §23.6 (4a): `balerix serve --mode kubernetes` and `balerix dev
//! fake-plugin` as two processes, TLS both ways under one authority.

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use balerix_api::{AgentSettings, CrewSpec, FleetSpec};
use serde_json::{Value, json};
use support::{Kill, balerix, fake_tools, tls_call, tls_files, wait_for_file};

const ADMIN: &str = "0123456789abcdef0123456789abcdef";
const FAKE_TOKEN: &str = "fake-token-0123456789abcdef01234567";
const GRANT: &[&str] = &["actions", "fleets", "kv", "manage"];

struct Kube {
    home: tempfile::TempDir,
    addr: String,
    ca: PathBuf,
    cert: PathBuf,
    key: PathBuf,
    daemon: Kill,
}

/// The Daemon in Kubernetes mode with `--tls-ca`, on a state dir that a
/// second start can reuse (`home`).
fn kube_daemon(home: tempfile::TempDir, port: Option<u16>) -> Kube {
    let tools = home.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    fake_tools(&tools);
    let tls_dir = home.path().join("tls");
    std::fs::create_dir_all(&tls_dir).unwrap();
    // one authority across restarts: reuse the files when they exist
    let (ca, cert, key) = if tls_dir.join("ca.crt").exists() {
        (
            tls_dir.join("ca.crt"),
            tls_dir.join("tls.crt"),
            tls_dir.join("tls.key"),
        )
    } else {
        tls_files(&tls_dir)
    };
    let token_file = home.path().join("admin-token");
    std::fs::write(&token_file, format!("{ADMIN}\n")).unwrap();
    let endpoint = home.path().join(".local/state/balerix/server/endpoint");
    let _ = std::fs::remove_file(&endpoint);
    let child = balerix(home.path(), &tools)
        .args([
            "serve",
            "--mode",
            "kubernetes",
            "--tmux-socket",
            "unused",
            "--bind",
        ])
        .arg(format!("127.0.0.1:{}", port.unwrap_or(0)))
        .arg("--tls-cert")
        .arg(&cert)
        .arg("--tls-key")
        .arg(&key)
        .arg("--tls-ca")
        .arg(&ca)
        .arg("--admin-token-file")
        .arg(&token_file)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let daemon = Kill(child);
    let url = wait_for_file(&endpoint);
    Kube {
        addr: url.trim_start_matches("https://").to_string(),
        home,
        ca,
        cert,
        key,
        daemon,
    }
}

/// Stops the Daemon and starts it again on the same port, state and
/// authority, so the running fake's `BALERIX_API_URL` stays valid.
fn restart(k: Kube) -> Kube {
    let Kube {
        home, addr, daemon, ..
    } = k;
    drop(daemon);
    let port = addr.rsplit_once(':').unwrap().1.parse().unwrap();
    kube_daemon(home, Some(port))
}

/// A free loopback port: bound then released. The fake binds it again.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `balerix dev fake-plugin` as a pod would run it: every input a file or
/// a variable, TLS both ways.
fn fake_plugin(k: &Kube, port: u16, scratch: &Path) -> Kill {
    let token = scratch.join("token");
    std::fs::write(&token, FAKE_TOKEN).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_balerix"))
        .args(["dev", "fake-plugin"])
        .env_clear()
        .env("BALERIX_API_URL", format!("https://{}", k.addr))
        .env("BALERIX_PLUGIN_NAME", "fake")
        .env("BALERIX_PLUGIN_TOKEN_FILE", &token)
        .env("BALERIX_PLUGIN_SCRATCH", scratch)
        .env("BALERIX_CA_FILE", &k.ca)
        .env("BALERIX_PLUGIN_TLS_CERT", &k.cert)
        .env("BALERIX_PLUGIN_TLS_KEY", &k.key)
        .env("BALERIX_PLUGIN_LISTEN", format!("127.0.0.1:{port}"))
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    Kill(child)
}

fn declare(k: &Kube, port: u16, grant: &[&str], config: Value) {
    let (s, v) = tls_call(
        &k.addr,
        &k.ca,
        "PUT",
        "/v1/plugins",
        Some(ADMIN),
        Some(&json!({
            "plugins": [{ "name": "fake", "grant": grant, "config": config,
                          "token": FAKE_TOKEN, "url": format!("https://127.0.0.1:{port}") }]
        })),
    );
    assert_eq!(s, 204, "{v}");
}

fn wait_until(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "the condition never held"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The usual bound for a phase to come; the post-restart wait has its own
/// (the first health poll is 10 s after the Daemon starts).
const PHASE_WAIT: Duration = Duration::from_secs(15);

/// The fake's row in `GET /v1/plugins` (the only plugin listed).
fn plugin_row(k: &Kube) -> Value {
    let (_, rows) = tls_call(&k.addr, &k.ca, "GET", "/v1/plugins", Some(ADMIN), None);
    rows[0].clone()
}

fn wait_for_phase(k: &Kube, phase: &str, within: Duration) -> Value {
    let start = Instant::now();
    loop {
        let (_, rows) = tls_call(&k.addr, &k.ca, "GET", "/v1/plugins", Some(ADMIN), None);
        if rows[0]["phase"] == phase {
            return rows[0].clone();
        }
        assert!(start.elapsed() < within, "phase {phase} never came: {rows}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The operator's request for fleet `name`: crew `c` on `acme/api`, agent
/// `a` with `plugin` enabled, and the agents' tokens.
fn fleet_request(name: &str, plugin: &str, tokens: Value) -> Value {
    let spec = FleetSpec {
        name: name.into(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..CrewSpec::default()
            },
        )]),
    };
    let mut req = json!({ "spec": serde_json::to_value(spec).unwrap(), "agent_tokens": tokens });
    req["spec"]["crews"]["c"]["agents"]["a"]["plugins"] = json!({ plugin: {} });
    req
}

#[test]
fn a_fake_plugin_in_its_own_process_says_hello_over_tls_and_intercepts() {
    let k = kube_daemon(tempfile::tempdir().unwrap(), None);
    let port = free_port();
    let scratch = tempfile::tempdir().unwrap();
    declare(&k, port, GRANT, json!({ "greeting": "hi" }));
    let _fake = fake_plugin(&k, port, scratch.path());
    wait_for_phase(&k, "ready", PHASE_WAIT);
    assert!(
        wait_for_file(&scratch.path().join("fake-plugin.hello")).contains("\"greeting\": \"hi\"")
    );
    // the operator's fleet with the fake enabled for its agent; a hook
    // event from the agent's sidecar is intercepted over TLS
    let secret = "a".repeat(32);
    let tokens = json!({ "f/c/a": secret });
    let (s, v) = tls_call(
        &k.addr,
        &k.ca,
        "PUT",
        "/v1/fleets/f",
        Some(ADMIN),
        Some(&fleet_request("f", "fake", tokens)),
    );
    assert_eq!(s, 200, "{v}");
    // activation is async: wait until the pair is active
    wait_until(|| {
        let (_, rec) = tls_call(&k.addr, &k.ca, "GET", "/v1/fleets/f", Some(ADMIN), None);
        rec["status"]["agents"]["f/c/a"]["plugins"]["fake"]["state"] == "active"
    });
    let (s, verdict) = tls_call(
        &k.addr,
        &k.ca,
        "POST",
        "/v1/agents/f/c/a/events",
        Some(&secret),
        Some(
            &json!({ "hook_event_name": "PreToolUse", "session_id": "s1",
                      "tool_name": "Bash", "tool_input": { "command": "rm -rf /" } }),
        ),
    );
    assert_eq!(
        (s, verdict["decision"].as_str()),
        (200, Some("block")),
        "{verdict}"
    );
}

#[test]
fn a_grant_short_of_the_manifest_refuses_the_hello_and_the_row_says_which() {
    let k = kube_daemon(tempfile::tempdir().unwrap(), None);
    let port = free_port();
    let scratch = tempfile::tempdir().unwrap();
    declare(&k, port, &["actions", "fleets", "kv"], json!({}));
    let _fake = fake_plugin(&k, port, scratch.path());
    let row = wait_for_phase(&k, "failed", PHASE_WAIT);
    assert_eq!(
        row["message"],
        "hello.manifest.needs: manage is not granted"
    );
}

#[test]
fn a_managed_fleet_from_the_plugin_is_listed_for_the_operator() {
    let k = kube_daemon(tempfile::tempdir().unwrap(), None);
    let port = free_port();
    let scratch = tempfile::tempdir().unwrap();
    // a real fleet file: the binary resolves it with the real resolver
    let file = json!({ "apiVersion": "balerix/v1", "kind": "Fleet",
                       "crews": { "c": { "repo": "acme/api", "agents": { "a": {} } } } });
    declare(
        &k,
        port,
        GRANT,
        json!({ "manage": { "fleet": "gh-1", "file": file } }),
    );
    let _fake = fake_plugin(&k, port, scratch.path());
    let outcome = wait_for_file(&scratch.path().join("fake-plugin.manage"));
    assert!(outcome.contains("\"owner\": \"fake\""), "{outcome}");
    let (_, rows) = tls_call(
        &k.addr,
        &k.ca,
        "GET",
        "/v1/managed-fleets",
        Some(ADMIN),
        None,
    );
    assert_eq!(rows[0]["name"], "gh-1");
    assert_eq!(rows[0]["plugin"], "fake");
}

#[test]
fn a_daemon_restart_keeps_a_running_plugin_ready_without_a_new_hello() {
    let k = kube_daemon(tempfile::tempdir().unwrap(), None);
    let port = free_port();
    let scratch = tempfile::tempdir().unwrap();
    declare(&k, port, GRANT, json!({}));
    let _fake = fake_plugin(&k, port, scratch.path());
    let before = wait_for_phase(&k, "ready", PHASE_WAIT);
    let version = before["version"].as_str().unwrap().to_string();
    assert!(!version.is_empty(), "{before}");
    let hello = scratch.path().join("fake-plugin.hello");
    let hello_at = std::fs::metadata(&hello).unwrap().modified().unwrap();
    // restart the Daemon on the same state (the fake keeps running), then
    // re-send the list as the operator does after a restart
    let k = restart(k);
    declare(&k, port, GRANT, json!({}));
    // The restore itself: the persisted hello is back (the hello'd
    // version, where an entry still waiting for a hello lists none) but
    // not ready. The health loop sleeps HEALTH_INTERVAL before its first
    // poll, so nothing can have made it ready this soon after the start.
    let restored = plugin_row(&k);
    assert_eq!(
        (restored["phase"].as_str(), restored["version"].as_str()),
        (Some("starting"), Some(version.as_str())),
        "{restored}"
    );
    // Ready from the first health poll (10 s after the start). Only a
    // hello'd entry is polled, so health alone could not have done this
    // for an entry without one.
    let after = wait_for_phase(&k, "ready", Duration::from_secs(30));
    assert_eq!(after["version"], before["version"], "{after}");
    // Secondary: the fake says hello only at start-up, so an unchanged
    // file shows it was not restarted or re-configured; it cannot show the
    // Daemon restored the hello (the assertions above do that).
    let again = std::fs::metadata(&hello).unwrap().modified().unwrap();
    assert_eq!(hello_at, again, "no second hello");
}
