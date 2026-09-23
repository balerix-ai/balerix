#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use balerix_api::AgentPhase;
use balerix_core::FleetRecord;
use balerix_core::{AgentId, PassThrough};
use balerix_server::testing::Harness;
use balerix_server::{ApplyMode, Caller, Daemon, router, serve};
use predicates::prelude::*;

// Not `examples/payments.yaml`: the example fleet names the `flow` plugin,
// which this stub daemon does not install, so `up`/`update` would be
// rejected before any of the CLI mechanics below ran. Same shape (fleet
// `payments`, crew `backend`, agents `alice` and `bob`) minus the plugin.
const FLEET_YAML: &str = r#"apiVersion: balerix/v1
kind: Fleet
name: payments
defaults:
  claude:
    settings: { model: sonnet, permissions: { allow: ["Bash(git *)"] } }
    args: ["--verbose"]
    resume: true
  sandbox:
    network: { block: false }
  tools: { node: "22.11.0" }
  env: { RUST_LOG: info }
  runner: { type: tmux }
  plugins: {}
crews:
  backend:
    repo: acme/payments-api
    ref: main
    git: { push: true, auth: gh }
    defaults:
      tools: { python: "3.12.8" }
    agents:
      alice: {}
      bob: { claude: { settings: { model: opus } } }
"#;

/// Writes `FLEET_YAML` under `dir` and returns its path.
fn fleet_file(dir: &Path) -> PathBuf {
    let path = dir.join("payments.yaml");
    fs::write(&path, FLEET_YAML).unwrap();
    path
}

/// A daemon on port 0 over the fakes; the binary finds it through the
/// endpoint and token files under this HOME.
struct Stub {
    home: tempfile::TempDir,
    url: String,
    daemon: Arc<Daemon>,
    _stop: tokio::sync::oneshot::Sender<()>,
    _rt: tokio::runtime::Runtime,
    /// The plugin host's (empty) config and install roots; dropped with the stub.
    _plugin_dir: tempfile::TempDir,
}

fn stub() -> Stub {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let h = Harness::new(Duration::from_secs(3600));
    let plugin_dir = tempfile::tempdir().unwrap();
    let (daemon, url, stop) = rt.block_on(async {
        let daemon = h.daemon_with_token(Arc::new(PassThrough), plugin_dir.path(), "tok");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(serve(listener, router(daemon.clone()), async {
            let _ = rx.await;
        }));
        (daemon, url, tx)
    });
    let home = tempfile::tempdir().unwrap();
    let server = home.path().join(".local/state/balerix/server");
    fs::create_dir_all(&server).unwrap();
    fs::write(server.join("endpoint"), &url).unwrap();
    fs::write(server.join("token"), "tok").unwrap();
    Stub {
        home,
        url,
        daemon,
        _stop: stop,
        _rt: rt,
        _plugin_dir: plugin_dir,
    }
}

fn balerix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("BALERIX_API_URL")
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

#[test]
fn up_status_list_update_and_down_through_the_binary() {
    let s = stub();
    let home = s.home.path();
    let fleet = fleet_file(home);
    let fleet = fleet.to_str().unwrap();

    balerix(home)
        .args(["up", fleet, "--no-host-defaults", "--no-wait"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "payments  pending  generation 1 (observed 0)",
        ));
    balerix(home)
        .args(["up", fleet, "--no-host-defaults", "--no-wait"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("fleet exists"));

    // the fakes start every window but nothing sends SessionStart: up times out
    balerix(home)
        .args(["update", fleet, "--no-host-defaults", "--timeout", "2s"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "timed out after 2s waiting for ready",
        ))
        .stderr(predicate::str::contains("payments/backend/alice  starting"));

    let out = balerix(home)
        .args(["status", "payments", "--json"])
        .assert()
        .success();
    let rec: FleetRecord = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(rec.generation, 2);
    assert_eq!(
        rec.status.agents["payments/backend/bob"].phase,
        AgentPhase::Starting
    );

    balerix(home)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "NAME      PHASE        GEN  OBSERVED  AGENTS",
        ))
        .stdout(predicate::str::contains(
            "payments  reconciling  2    2         2",
        ));

    // ready both agents by hand, then `up`'s wait loop sees Ready
    for a in ["payments/backend/alice", "payments/backend/bob"] {
        let id: AgentId = a.parse().unwrap();
        let secret = s._rt.block_on(s.daemon.hook_secret(&id)).unwrap();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let resp = agent
            .post(&format!("{}/v1/agents/{a}/events", s.url))
            .header("Authorization", &format!("Bearer {secret}"))
            .send_json(serde_json::json!({ "hook_event_name": "SessionStart" }))
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }
    balerix(home)
        .args(["update", fleet, "--no-host-defaults", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "payments  ready  generation 3 (observed 3)",
        ))
        .stderr(predicate::str::contains("payments/backend/alice: ready"));

    balerix(home)
        .args(["down", "payments", "--purge", "--keep"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--purge cannot be combined"));
    balerix(home)
        .args(["down", "payments", "--keep", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "payments  down  generation 3 (observed 3)",
        ));
    balerix(home)
        .args(["down", "payments", "--purge", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains("payments: purged"));
    balerix(home)
        .args(["status", "payments"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("fleet payments not found"));
    balerix(home)
        .args(["list"])
        .assert()
        .success()
        .stdout("no fleets\n");
}

/// Spec L §5: a fleet a plugin applied shows its owner, refuses the
/// CLI's `up`, `update` and `down`, and goes down with `--force`.
#[test]
fn a_managed_fleet_shows_its_owner_and_needs_force_to_go_down() {
    let s = stub();
    let home = s.home.path();
    let fleet = fleet_file(home);
    let fleet = fleet.to_str().unwrap();
    // seeded as the daemon's manage path would leave it: owned by `gh`
    let spec = balerix_api::FleetSpec {
        name: "payments".into(),
        crews: BTreeMap::from([(
            "backend".to_string(),
            balerix_api::CrewSpec {
                repo: "acme/payments-api".into(),
                git_ref: "main".into(),
                git: balerix_api::GitSettings::default(),
                agents: BTreeMap::from([(
                    "alice".to_string(),
                    balerix_api::AgentSettings::default(),
                )]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    s._rt
        .block_on(s.daemon.apply_as(
            &"payments".parse().unwrap(),
            spec,
            Default::default(),
            ApplyMode::Create,
            &Caller::Plugin("gh".parse().unwrap()),
        ))
        .unwrap();

    balerix(home)
        .args(["status", "payments"])
        .assert()
        .success()
        .stdout(predicate::str::contains("  managed by gh\n"));
    balerix(home)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("MANAGED BY"))
        .stdout(predicate::str::contains("  gh\n"));
    let out = balerix(home)
        .args(["status", "payments", "--json"])
        .assert()
        .success();
    let rec: FleetRecord = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(rec.owner.as_deref(), Some("gh"));

    for cmd in [["up", fleet], ["update", fleet]] {
        balerix(home)
            .args(cmd)
            .args(["--no-host-defaults", "--no-wait"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "fleet payments is managed by plugin gh (HTTP 409)",
            ));
    }
    balerix(home)
        .args(["down", "payments", "--timeout", "30s"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "fleet payments is managed by plugin gh (HTTP 409)",
        ));
    balerix(home)
        .args(["down", "payments", "--force", "--timeout", "30s"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "payments  down  generation 1 (observed 1)  managed by gh",
        ));
}

/// Spec L-6 (F3): `plugin remove --purge` purges every fleet the plugin
/// owns — the one the sync downs and the one that was already down —
/// and leaves the CLI's fleets alone.
#[test]
fn plugin_remove_purge_purges_every_fleet_the_plugin_owns() {
    let s = stub();
    let home = s.home.path();
    let spec = |name: &str| balerix_api::FleetSpec {
        name: name.into(),
        crews: BTreeMap::from([(
            "backend".to_string(),
            balerix_api::CrewSpec {
                repo: "acme/payments-api".into(),
                git_ref: "main".into(),
                git: balerix_api::GitSettings::default(),
                agents: BTreeMap::from([(
                    "alice".to_string(),
                    balerix_api::AgentSettings::default(),
                )]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let gh = Caller::Plugin("gh".parse().unwrap());
    s._rt.block_on(async {
        for (name, caller) in [
            ("up", &gh),
            ("down", &gh),
            ("mine", &Caller::Admin { force: false }),
        ] {
            s.daemon
                .apply_as(
                    &name.parse().unwrap(),
                    spec(name),
                    Default::default(),
                    ApplyMode::Create,
                    caller,
                )
                .await
                .unwrap();
        }
        s.daemon
            .down_as(&"down".parse().unwrap(), Default::default(), false, &gh)
            .await
            .unwrap();
    });
    // declared on the CLI's side; the stub daemon's own plugins.yaml is
    // empty, so its sync reads `gh` as removed either way
    let config = home.join(".config/balerix");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("plugins.yaml"),
        "plugins:\n  - name: gh\n    source: /nowhere\n",
    )
    .unwrap();

    balerix(home)
        .args(["plugin", "remove", "gh", "--purge"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fleet up: down\n"))
        .stdout(predicate::str::contains("fleet up: purged\n"))
        .stdout(predicate::str::contains("fleet down: purged\n"))
        .stdout(predicate::str::contains(
            "removed gh and purged its state\n",
        ))
        .stdout(predicate::str::contains("fleet down: down").not());
    let names: Vec<String> = s
        ._rt
        .block_on(s.daemon.list())
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["mine"], "both owned fleets purged, the CLI's kept");
}

#[test]
fn without_a_daemon_the_client_says_how_to_start_one() {
    let home = tempfile::tempdir().unwrap();
    balerix(home.path())
        .args(["list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "daemon not running; run `balerix serve -d`",
        ));
    let fleet = fleet_file(home.path());
    balerix(home.path())
        .args([
            "up",
            fleet.to_str().unwrap(),
            "--no-host-defaults",
            "--timeout",
            "zz",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid duration"));
}
