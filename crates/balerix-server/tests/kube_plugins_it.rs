#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §23.2: the plugin list, hello against the grant, the 409s.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{AgentSettings, CrewSpec, FleetSpec};
use balerix_core::PassThrough;
use balerix_server::plugins::PluginClient;
use balerix_server::testing::{Harness, StubScript, stub_plugin};
use balerix_server::{Daemon, router, serve};
use serde_json::{Value, json};

const ADMIN: &str = "0123456789abcdef0123456789abcdef";

fn spec() -> FleetSpec {
    FleetSpec {
        name: "f".into(),
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
    }
}

struct World {
    _daemon: Arc<Daemon>,
    base: String,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

impl World {
    /// A blocking request off the runtime, as `kube_api_it` makes them.
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let token = token.map(str::to_string);
        let method = method.to_string();
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                .http_status_as_error(false)
                .build()
                .into();
            let mut req = match method.as_str() {
                "GET" => agent.get(&url).force_send_body(),
                "POST" => agent.post(&url),
                "PUT" => agent.put(&url),
                "DELETE" => agent.delete(&url).force_send_body(),
                _ => unreachable!(),
            };
            if let Some(t) = token {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = match body {
                Some(b) => req.send_json(&b).unwrap(),
                None => req.send_empty().unwrap(),
            };
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap();
            (
                status,
                serde_json::from_str(&text).unwrap_or(Value::String(text)),
            )
        })
        .await
        .unwrap()
    }
}

async fn serve_world(daemon: Arc<Daemon>, dir: tempfile::TempDir) -> World {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = rx.await;
    }));
    World {
        _daemon: daemon,
        base: format!("http://127.0.0.1:{port}"),
        _dir: dir,
        stop: Some(stop),
    }
}

/// A Kubernetes-mode daemon over `h` hosting the operator's list, calling
/// plugins with `client`.
async fn world_with(h: &Harness, client: PluginClient) -> World {
    let dir = tempfile::tempdir().unwrap();
    let daemon = h.daemon_declared(Arc::new(PassThrough), dir.path(), ADMIN, client);
    serve_world(daemon, dir).await
}

/// A tmux-mode daemon: `plugins.yaml` and packages.
async fn tmux_world(h: &Harness) -> World {
    let dir = tempfile::tempdir().unwrap();
    let daemon = h.daemon_with_token(Arc::new(PassThrough), dir.path(), ADMIN);
    serve_world(daemon, dir).await
}

fn request(tokens: Option<Value>) -> Value {
    let mut v = json!({ "spec": serde_json::to_value(spec()).unwrap() });
    if let Some(t) = tokens {
        v["agent_tokens"] = t;
    }
    v
}

fn entry(name: &str, url: &str, grant: &[&str]) -> Value {
    json!({ "name": name, "grant": grant, "config": { "greeting": "hi" },
            "token": format!("{name}-tok-0123456789abcdef0123456789ab"), "url": url })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_without_an_authority_refuses_the_list() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world_with(&h, PluginClient::new(None).unwrap()).await;
    let (s, v) = w
        .call(
            "PUT",
            "/v1/plugins",
            Some(ADMIN),
            Some(json!({ "plugins": [entry("flow", "https://127.0.0.1:1", &["kv"])] })),
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (
            409,
            Some("this daemon was started without --tls-ca; it cannot call plugins")
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_list_is_declared_hello_is_checked_and_the_row_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let (ca, cert, key) = balerix_server::testing::test_authority(dir.path());
    let stub = stub_plugin(StubScript {
        health_ok: true,
        tls: Some((cert, key)),
        ..Default::default()
    })
    .await;
    let h = Harness::kube(Duration::from_secs(3600));
    let tls = balerix_server::kube::tls::client_config(&ca).unwrap();
    let w = world_with(&h, PluginClient::new(Some(tls)).unwrap()).await;

    // a plain-http url is refused whole (Review Focus 5's sibling)
    let (s, v) = w
        .call(
            "PUT",
            "/v1/plugins",
            Some(ADMIN),
            Some(json!({ "plugins": [entry("flow", "http://x:1", &["kv"])] })),
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("plugins[0].url: must be https://"))
    );

    let (s, _) = w
        .call(
            "PUT",
            "/v1/plugins",
            Some(ADMIN),
            Some(json!({ "plugins": [entry("flow", &stub.listen, &["kv"])] })),
        )
        .await;
    assert_eq!(s, 204);
    let (_, rows) = w.call("GET", "/v1/plugins", Some(ADMIN), None).await;
    assert_eq!(rows[0]["phase"], "starting");

    let token = entry("flow", "", &[])["token"]
        .as_str()
        .unwrap()
        .to_string();
    let manifest = |needs: &str| {
        json!({ "apiVersion": "balerix/v1", "kind": "Plugin", "name": "flow",
        "version": "1.0.0", "protocol": 1, "start": "serve", "needs": needs.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>() })
    };
    let hello = |needs: &str| {
        json!({ "name": "flow", "version": "1.0.0", "protocol": 1,
        "listen": "0.0.0.0:7644", "manifest": manifest(needs) })
    };
    let (s, v) = w
        .call(
            "POST",
            "/v1/plugin-host/hello",
            Some(&token),
            Some(hello("kv,workspace")),
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("hello.manifest.needs: workspace is not granted"))
    );
    let (_, rows) = w.call("GET", "/v1/plugins", Some(ADMIN), None).await;
    assert_eq!(
        (rows[0]["phase"].as_str(), rows[0]["message"].as_str()),
        (
            Some("failed"),
            Some("hello.manifest.needs: workspace is not granted")
        )
    );

    let (s, v) = w
        .call(
            "POST",
            "/v1/plugin-host/hello",
            Some(&token),
            Some(hello("kv")),
        )
        .await;
    assert_eq!((s, v["config"]["greeting"].as_str()), (200, Some("hi")));
    let (_, rows) = w.call("GET", "/v1/plugins", Some(ADMIN), None).await;
    assert_eq!(rows[0]["phase"], "ready");
    // the token works on the host routes its manifest allows
    let (s, _) = w
        .call("GET", "/v1/plugin-host/kv", Some(&token), None)
        .await;
    assert_eq!(s, 200);
    let (s, _) = w
        .call("GET", "/v1/plugin-host/fleets", Some(&token), None)
        .await;
    assert_eq!(s, 403, "fleets is neither needed nor granted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kubernetes_mode_refuses_sync_and_purge_and_tmux_mode_refuses_the_list() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world_with(&h, PluginClient::new(None).unwrap()).await;
    let msg =
        "this daemon is in kubernetes mode; change its plugins through the Daemon's spec.plugins";
    let (s, v) = w.call("POST", "/v1/plugins/sync", Some(ADMIN), None).await;
    assert_eq!((s, v["error"].as_str()), (409, Some(msg)));
    let (s, v) = w
        .call("DELETE", "/v1/plugins/flow", Some(ADMIN), None)
        .await;
    assert_eq!((s, v["error"].as_str()), (409, Some(msg)));
    let tmux = Harness::new(Duration::from_secs(3600));
    let w = tmux_world(&tmux).await;
    let (s, v) = w
        .call(
            "PUT",
            "/v1/plugins",
            Some(ADMIN),
            Some(json!({ "plugins": [] })),
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("this daemon reads plugins.yaml"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fleet_naming_a_declared_plugin_applies_with_its_pair_pending() {
    let dir = tempfile::tempdir().unwrap();
    let (ca, _, _) = balerix_server::testing::test_authority(dir.path());
    let h = Harness::kube(Duration::from_secs(3600));
    let tls = balerix_server::kube::tls::client_config(&ca).unwrap();
    let w = world_with(&h, PluginClient::new(Some(tls)).unwrap()).await;
    let (s, _) = w
        .call(
            "PUT",
            "/v1/plugins",
            Some(ADMIN),
            Some(json!({ "plugins": [entry("flow", "https://127.0.0.1:1", &["kv"])] })),
        )
        .await;
    assert_eq!(s, 204);
    // no hello yet: the operator's fleet names the plugin all the same
    let mut req = request(Some(json!({ "f/c/a": "a".repeat(32) })));
    req["spec"]["crews"]["c"]["agents"]["a"]["plugins"] = json!({ "flow": {} });
    let (s, rec) = w.call("PUT", "/v1/fleets/f", Some(ADMIN), Some(req)).await;
    assert_eq!(s, 200, "not `no plugin \"flow\" is installed`: {rec}");
    assert_eq!(
        rec["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"],
        "pending"
    );
}
