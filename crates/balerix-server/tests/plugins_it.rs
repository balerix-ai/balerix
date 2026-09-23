//! The plugin host over the fakes through a real listener (plugins spec
//! §11 "server integration", phase 1 slice): sync, hello, list, the
//! reserved fleet, removal and purge.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{AgentPhase, AgentSettings, CrewSpec, FleetRequest, FleetSpec, GitSettings};
use balerix_core::{AgentId, FleetRecord, PassThrough, plugin_id};
use balerix_server::testing::{Harness, plugin_config_in};
use balerix_server::{Daemon, Metrics, router, serve};
use serde_json::{Value, json};

const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\nroutes: true\n";
const MISE: &str = "[tools]\n[tasks.serve]\nrun = \"true\"\n";

fn write_package(dir: &Path, manifest: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("balerix-plugin.yaml"), manifest).unwrap();
    fs::write(dir.join("mise.toml"), MISE).unwrap();
}

struct Api {
    base: String,
    agent: ureq::Agent,
}

impl Api {
    fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => self.agent.get(&url).force_send_body(),
            "POST" => self.agent.post(&url),
            "DELETE" => self.agent.delete(&url).force_send_body(),
            _ => unreachable!(),
        };
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let mut resp = match body {
            Some(b) => req.send_json(b).unwrap(),
            None => req.send_empty().unwrap(),
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap();
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, v)
    }
}

fn wait(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "condition not reached"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_sync_hello_list_and_purge() {
    let dir = tempfile::tempdir().unwrap();
    let plugins_yaml = dir.path().join("plugins.yaml");
    let package = dir.path().join("hello-pkg");
    write_package(&package, MANIFEST);
    fs::write(
        &plugins_yaml,
        "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n",
    )
    .unwrap();

    let h = Harness::new(Duration::from_secs(3600));
    let daemon = h.daemon(Arc::new(PassThrough), dir.path());
    let report = daemon.sync_plugins().await.unwrap();
    assert_eq!(report.installed, vec!["hello"]);
    assert!(report.stopped.is_empty() && report.unchanged.is_empty());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = stop_rx.await;
    }));
    let api = Api {
        base,
        agent: ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into(),
    };
    let admin = Some("admin-tok");
    let id: AgentId = plugin_id(&"hello".parse().unwrap());

    // the actor's pass materialized and started the synthetic agent
    {
        let m = h.materializer.clone();
        let r = h.runner.clone();
        tokio::task::spawn_blocking(move || {
            wait(|| {
                m.calls()
                    .contains(&"materialize_plugin balerix/plugins/hello".to_string())
                    && r.calls()
                        .contains(&"ensure_agent balerix/plugins/hello".to_string())
            })
        })
        .await
        .unwrap();
    }
    let (s, rows) = api.call("GET", "/v1/plugins", admin, None);
    assert_eq!(s, 200);
    assert_eq!(rows[0]["name"], "hello");
    assert_eq!(rows[0]["version"], "0.1.0");
    assert_eq!(rows[0]["routes"], true);
    assert!(rows[0]["listen"].is_null());
    assert_eq!(rows[0]["phase"], "starting");
    let (s, _) = api.call("GET", "/v1/plugins", None, None);
    assert_eq!(s, 401, "admin token required");

    // hello: token, protocol, listen, then Ready with the config
    let token = daemon.hook_secret(&id).await.unwrap();
    let hello = |name: &str, protocol: u32, listen: &str| json!({ "name": name, "version": "0.1.0", "protocol": protocol, "listen": listen });
    let (s, _) = api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some("wrong"),
        Some(&hello("hello", 1, "127.0.0.1:4000")),
    );
    assert_eq!(s, 401);
    let (s, _) = api.call(
        "POST",
        "/v1/plugin-host/hello",
        None,
        Some(&hello("hello", 1, "127.0.0.1:4000")),
    );
    assert_eq!(s, 401);
    let (s, body) = api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some(&token),
        Some(&hello("hello", 2, "127.0.0.1:4000")),
    );
    assert_eq!(s, 400);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("hello.protocol"),
        "{body}"
    );
    let (s, body) = api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some(&token),
        Some(&hello("hello", 1, "0.0.0.0:4000")),
    );
    assert_eq!(
        (s, body["error"].as_str().unwrap()),
        (400, "hello.listen: must be a loopback address")
    );
    let (s, _body) = api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some(&token),
        Some(&hello("other", 1, "127.0.0.1:4000")),
    );
    assert_eq!(
        s, 401,
        "the body's name must be the token's plugin; unknown plugin answers like a bad token"
    );
    let (s, body) = api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some(&token),
        Some(&hello("hello", 1, "127.0.0.1:4000")),
    );
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["config"]["greeting"], "hi");
    {
        let d = daemon.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let rows = d.plugins().list().await;
                if rows[0].phase == AgentPhase::Ready {
                    assert_eq!(rows[0].listen.as_deref(), Some("127.0.0.1:4000"));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    // the reserved fleet: readable, not writable, not listed
    let (s, rec) = api.call("GET", "/v1/fleets/balerix", admin, None);
    assert_eq!(s, 200);
    assert_eq!(
        rec["status"]["agents"]["balerix/plugins/hello"]["phase"],
        "ready"
    );
    let (s, rows) = api.call("GET", "/v1/fleets", admin, None);
    assert_eq!(
        (s, rows.as_array().unwrap().len()),
        (200, 0),
        "plugins are not a fleet row"
    );
    let spec = FleetSpec {
        name: "balerix".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let req = serde_json::to_value(FleetRequest {
        spec,
        credentials: Default::default(),
    })
    .unwrap();
    let (s, body) = api.call("POST", "/v1/fleets", admin, Some(&req));
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "name: \"balerix\" is reserved for the daemon's plugins"
    );
    let mut watch = req.clone();
    watch["spec"]["name"] = "watch".into();
    let (s, body) = api.call("POST", "/v1/fleets", admin, Some(&watch));
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "name: \"watch\" is reserved: it would shadow the plugin host's fleets/watch route"
    );
    let (s, _) = api.call(
        "DELETE",
        "/v1/fleets/balerix?keep_repos=false&keep_sessions=false&purge=false",
        admin,
        None,
    );
    assert_eq!(s, 400);

    // metrics carry the plugin fleet's gauges
    let (_, metrics) = api.call("GET", "/metrics", None, None);
    assert!(
        metrics
            .as_str()
            .unwrap()
            .contains("balerix_agents{crew=\"plugins\",fleet=\"balerix\",phase=\"ready\"} 1"),
        "{metrics}"
    );

    // a secret colliding with an inline config key fails the sync, changes
    // nothing, and the error composes one `plugins.yaml:` prefix with the
    // entry index and the full `secrets.<key>` path — never the file's
    // contents.
    {
        use std::os::unix::fs::PermissionsExt;
        let secret = dir.path().join("secret-pw");
        fs::write(&secret, "s3cr3t\n").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            &plugins_yaml,
            "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n    secrets:\n      greeting: ./secret-pw\n",
        )
        .unwrap();
        let (s, body) = api.call("POST", "/v1/plugins/sync", admin, None);
        assert_eq!(s, 400);
        assert_eq!(
            body["error"],
            "plugins.yaml: plugins[0].secrets.greeting: collides with config.greeting"
        );
        assert!(
            !body["error"].as_str().unwrap().contains("s3cr3t"),
            "{body}"
        );
        // restore the file the "broken manifest" case below builds on.
        fs::write(
            &plugins_yaml,
            "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n",
        )
        .unwrap();
    }

    // a broken manifest fails the sync and changes nothing
    let bad = dir.path().join("bad-pkg");
    write_package(
        &bad,
        &MANIFEST
            .replace("name: hello", "name: bad")
            .replace("protocol: 1", "protocol: 7"),
    );
    fs::write(
        &plugins_yaml,
        "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n  - name: bad\n    source: ./bad-pkg\n",
    )
    .unwrap();
    let (s, body) = api.call("POST", "/v1/plugins/sync", admin, None);
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "plugins.yaml: plugins[1]: balerix-plugin.yaml: protocol: this daemon speaks protocol 1, got 7"
    );
    assert_eq!(daemon.plugins().list().await.len(), 1, "previous set kept");

    // purge refuses while declared; removal stops; purge then deletes
    let (s, body) = api.call("DELETE", "/v1/plugins/hello", admin, None);
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "plugin \"hello\" is still declared in plugins.yaml; remove it first"
    );
    fs::write(&plugins_yaml, "plugins: []\n").unwrap();
    let (s, report) = api.call("POST", "/v1/plugins/sync", admin, None);
    assert_eq!(s, 200);
    assert_eq!(report["stopped"], json!(["hello"]));
    assert!(daemon.plugins().list().await.is_empty());
    assert!(
        daemon.hook_secret(&id).await.is_none(),
        "a removed plugin's token is revoked"
    );
    // The actor answers `Apply` before the pass that stops the plugin, so
    // the sync above can return while the plugin is still running: purge
    // must wait for the pass rather than delete under it. Nothing here
    // waits for `stop_agent` — the DELETE does.
    let (s, _) = api.call("DELETE", "/v1/plugins/hello", admin, None);
    assert_eq!(s, 200);
    assert!(
        h.runner
            .calls()
            .contains(&"stop_agent balerix/plugins/hello".to_string()),
        "purge waited until the plugin was stopped"
    );
    assert!(
        !daemon
            .plugins()
            .record()
            .status
            .agents
            .contains_key("balerix/plugins/hello"),
        "and until the actor took it out of the record"
    );
    assert!(
        h.materializer
            .calls()
            .contains(&"purge_plugin hello".to_string())
    );
    let (s, _) = api.call("DELETE", "/v1/plugins/Nope", admin, None);
    assert_eq!(s, 400);

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
}

/// A `fleet.json` under the reserved name — written before the name was
/// reserved, or by hand — must not get an actor: the plugin host already
/// owns `balerix`, its tmux session and its state root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_fleet_under_the_reserved_name_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("plugins.yaml"), "plugins: []\n").unwrap();

    let h = Harness::new(Duration::from_secs(3600));
    let mut record = FleetRecord::new(FleetSpec {
        name: "balerix".into(),
        crews: BTreeMap::from([(
            "plugins".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: BTreeMap::from([("hello".to_string(), AgentSettings::default())]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    });
    record.status.entry("balerix/plugins/hello");
    let ports = balerix_server::Ports {
        materializer: h.materializer.clone(),
        runner: h.runner.clone(),
        clock: h.clock.clone(),
        store: h.store.clone(),
        workspace: h.workspace.clone(),
        resolver: h.resolver.clone(),
        credentials: h.credentials.clone(),
        policy: Default::default(),
        hook_url: "http://127.0.0.1:1".into(),
        resync: Duration::from_secs(3600),
    };
    // Not `Harness::daemon`: this test needs a *stored* record, which only
    // `Daemon::start` takes.
    let daemon = Daemon::start(
        ports,
        Arc::new(PassThrough),
        Metrics::new().unwrap(),
        "admin-tok".into(),
        vec![(record, Default::default())],
        plugin_config_in(dir.path()),
        h.registry.clone(),
        h.client.clone(),
        h.kv.clone(),
        balerix_server::testing::ready_toolchain(),
    );
    daemon.sync_plugins().await.unwrap();

    let name: balerix_core::FleetName = "balerix".parse().unwrap();
    let rec = daemon.get(&name).await.unwrap();
    assert!(
        rec.status.agents.is_empty(),
        "the plugin host's own record answers, not the stored one: {:?}",
        rec.status.agents
    );
    assert!(daemon.list().await.is_empty(), "and it is not a fleet row");
    // no actor ran the stored spec: its agent was never materialized or
    // started. The plugin host's own actor may have observed `balerix` by
    // now — `plugins.yaml` is empty, so it touches nothing else.
    let r = h.runner.clone();
    let m = h.materializer.clone();
    tokio::task::spawn_blocking(move || {
        let mut calls = r.calls();
        calls.extend(m.calls());
        assert!(
            calls.iter().all(|c| !c.contains("hello")),
            "the stored record's agent was acted on: {calls:?}"
        );
    })
    .await
    .unwrap();
}
