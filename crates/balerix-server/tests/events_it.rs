//! Plugins spec §11 "server integration", phase 2a: a plugin built on the
//! SDK, in-process, against the daemon over a real listener with the fakes
//! behind it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use balerix_api::{
    ActivationState, AgentPhase, AgentSettings, CrewSpec, FleetRequest, FleetSpec, GitSettings,
    HookEvent, InterceptResponse, PluginAction,
};
use balerix_core::plugin_id;
use balerix_plugin_sdk::{Env, Host, Plugin, bind, run};
use serde_json::{Value, json};
use support::{World, world};

#[derive(Default)]
struct FlowLike {
    activations: Mutex<Vec<(String, Value)>>,
    deactivations: Mutex<Vec<String>>,
    observed: Mutex<Vec<HookEvent>>,
    slow: Mutex<bool>,
}

impl Plugin for FlowLike {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        if config.get("reject").is_some() {
            return Err(format!("states: rejected for {agent}"));
        }
        self.activations
            .lock()
            .unwrap()
            .push((agent.to_string(), config));
        Ok(())
    }
    async fn deactivate(&self, agent: &str) {
        self.deactivations.lock().unwrap().push(agent.to_string());
    }
    async fn observe(&self, events: Vec<HookEvent>) {
        self.observed.lock().unwrap().extend(events);
    }
    async fn intercept(
        &self,
        event: HookEvent,
        mut so_far: Value,
        _deadline: u64,
    ) -> InterceptResponse {
        if *self.slow.lock().unwrap() {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        let mut actions = Vec::new();
        if event.name == "PreToolUse"
            && event.payload["tool_input"]["command"]
                .as_str()
                .is_some_and(|c| c.starts_with("rm -rf"))
        {
            so_far["decision"] = json!("block");
            so_far["reason"] = json!("no recursive deletes");
        }
        if event.name == "Stop" {
            actions.push(PluginAction::SendText {
                text: "Run the tests.".into(),
                submit: true,
            });
        }
        InterceptResponse {
            response: so_far,
            actions,
        }
    }
}

fn spec(agents: &[(&str, Value)]) -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: agents
                    .iter()
                    .map(|(n, cfg)| {
                        let mut s = AgentSettings::default();
                        if !cfg.is_null() {
                            s.plugins.insert("flow".into(), cfg.clone());
                        }
                        (n.to_string(), s)
                    })
                    .collect(),
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

async fn token(w: &World, plugin: &str) -> String {
    w.daemon
        .hook_secret(&plugin_id(&plugin.parse().unwrap()))
        .await
        .unwrap()
}

/// Starts the SDK plugin on a port and says hello with its real token.
async fn start_flow(w: &World) -> (Arc<FlowLike>, Host) {
    let plugin = Arc::new(FlowLike::default());
    let (listener, listen) = bind().await.unwrap();
    let env = Env {
        api_url: w.api.base.clone(),
        name: "flow".into(),
        token: token(w, "flow").await,
        scratch: w.dir.path().join("scratch"),
    };
    tokio::spawn({
        let plugin = plugin.clone();
        let token = env.token.clone();
        async move { run(listener, plugin, &token).await }
    });
    let host = Host::new(env).unwrap();
    host.hello("0.1.0", &listen).await.unwrap();
    (plugin, host)
}

async fn wait_for(
    w: &World,
    pred: impl Fn(&balerix_api::FleetRecord) -> bool,
) -> balerix_api::FleetRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(r) = w.daemon.get(&"f".parse().unwrap()).await
                && pred(&r)
            {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_pending_then_active_rejection_fails_up_and_down_deactivates() {
    let w = world().await;
    // pending: the plugin has not said hello
    let req = json!(FleetRequest {
        spec: spec(&[("a", json!({ "v": 1 }))]),
        credentials: Default::default()
    });
    let (s, v) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(s, 200, "{v}");
    let rec = wait_for(&w, |r| r.status.agents.contains_key("f/c/a")).await;
    assert_eq!(
        rec.status.agents["f/c/a"].plugins["flow"].state,
        ActivationState::Pending
    );
    let (_, rows) = w.api.admin("GET", "/v1/plugins", None);
    assert_eq!(rows[0]["active_agents"], 0);

    let (plugin, _host) = start_flow(&w).await;
    let rec = wait_for(&w, |r| {
        r.status.agents["f/c/a"].plugins["flow"].state == ActivationState::Active
    })
    .await;
    assert_eq!(rec.status.agents["f/c/a"].plugins["flow"].message, "");
    assert_eq!(
        plugin.activations.lock().unwrap()[0],
        ("f/c/a".to_string(), json!({ "v": 1 }))
    );
    let (_, rows) = w.api.admin("GET", "/v1/plugins", None);
    assert_eq!(rows[0]["active_agents"], 1);

    // a rejected activation fails the update with the config path
    let bad = json!(FleetRequest {
        spec: spec(&[("a", json!({ "v": 1 })), ("b", json!({ "reject": true }))]),
        credentials: Default::default()
    });
    let (s, v) = w.api.admin("PUT", "/v1/fleets/f", Some(&bad));
    assert_eq!(s, 400);
    assert_eq!(
        v["error"],
        "crews.c.agents.b.plugins.flow: states: rejected for f/c/b"
    );
    assert_eq!(
        w.daemon
            .get(&"f".parse().unwrap())
            .await
            .unwrap()
            .generation,
        1,
        "nothing landed"
    );

    // an unknown plugin is refused before anything
    let mut s2 = spec(&[("a", Value::Null)]);
    s2.crews
        .get_mut("c")
        .unwrap()
        .agents
        .get_mut("a")
        .unwrap()
        .plugins
        .insert("nope".into(), json!({}));
    let (s, v) = w.api.admin(
        "PUT",
        "/v1/fleets/f",
        Some(&json!(FleetRequest {
            spec: s2,
            credentials: Default::default()
        })),
    );
    assert_eq!(
        (s, v["error"].as_str().unwrap()),
        (
            400,
            "crews.c.agents.a.plugins.nope: no plugin \"nope\" is installed"
        )
    );

    // down deactivates
    let (s, _) = w.api.admin(
        "DELETE",
        "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false",
        None,
    );
    assert_eq!(s, 200);
    tokio::time::timeout(Duration::from_secs(2), async {
        while plugin.deactivations.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(plugin.deactivations.lock().unwrap()[0], "f/c/a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chain_blocks_observers_see_and_actions_reach_the_runner() {
    let w = world().await;
    let (plugin, _host) = start_flow(&w).await;
    let req = json!(FleetRequest {
        spec: spec(&[("a", json!({}))]),
        credentials: Default::default()
    });
    let (s, _) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(s, 200);
    wait_for(&w, |r| r.status.observed_generation == 1).await;
    let secret = w
        .daemon
        .hook_secret(&"f/c/a".parse().unwrap())
        .await
        .unwrap();
    let hook = |name: &str, extra: Value| {
        let mut body = json!({ "hook_event_name": name, "session_id": "s1" });
        if let (Some(dst), Some(src)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        body
    };
    let (s, v) = w.api.call(
        "POST",
        "/v1/agents/f/c/a/events",
        Some(&secret),
        Some(&hook(
            "PreToolUse",
            json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf /" } }),
        )),
    );
    assert_eq!(
        (s, v),
        (
            200,
            json!({ "decision": "block", "reason": "no recursive deletes" })
        )
    );
    let (s, v) = w.api.call(
        "POST",
        "/v1/agents/f/c/a/events",
        Some(&secret),
        Some(&hook(
            "PreToolUse",
            json!({ "tool_input": { "command": "ls" } }),
        )),
    );
    assert_eq!((s, v), (200, json!({})));
    let (s, v) = w.api.call(
        "POST",
        "/v1/agents/f/c/a/events",
        Some(&secret),
        Some(&hook("Stop", json!({}))),
    );
    assert_eq!(
        (s, v),
        (200, json!({})),
        "the response is written before the action runs"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !w
            .h
            .runner
            .calls()
            .contains(&"send_text f/c/a \"Run the tests.\" submit=true".to_string())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("send_text reached the runner");
    tokio::time::timeout(Duration::from_secs(2), async {
        while plugin.observed.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the observer batch arrived");
    assert_eq!(plugin.observed.lock().unwrap()[0].name, "Stop");
    // timeout fail-open
    *plugin.slow.lock().unwrap() = true;
    let started = std::time::Instant::now();
    let (s, v) = w.api.call(
        "POST",
        "/v1/agents/f/c/a/events",
        Some(&secret),
        Some(&hook(
            "PreToolUse",
            json!({ "tool_input": { "command": "rm -rf /" } }),
        )),
    );
    assert_eq!((s, v), (200, json!({})), "slow plugin: allowed");
    assert!(started.elapsed() < Duration::from_secs(2));
    let (_, m) = w.api.call("GET", "/metrics", None, None);
    let m = m.as_str().unwrap();
    assert!(
        m.contains("balerix_plugin_intercept_failures_total{plugin=\"flow\",reason=\"timeout\"} 1"),
        "{m}"
    );
    assert!(m.contains(
        "balerix_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"flow\"} 3"
    ));
    assert!(m.contains(
        "balerix_hook_actions_total{action=\"send_text\",agent=\"a\",crew=\"c\",fleet=\"f\"} 1"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_routes_are_gated_by_needs_and_kv_and_actions_work() {
    let w = world().await;
    let (_plugin, host) = start_flow(&w).await;
    let (s, _) = w.api.admin(
        "POST",
        "/v1/fleets",
        Some(&json!(FleetRequest {
            spec: spec(&[("a", json!({}))]),
            credentials: Default::default()
        })),
    );
    assert_eq!(s, 200);
    wait_for(&w, |r| r.status.observed_generation == 1).await;
    // flow has kv + actions, not fleets
    let e = host.fleets().await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "daemon: HTTP 403: capability \"fleets\" not declared in balerix-plugin.yaml"
    );
    host.kv_put("state/f/c/a", b"working", false).await.unwrap();
    host.kv_put("tok", b"s3cret", true).await.unwrap();
    assert_eq!(
        host.kv_get("state/f/c/a").await.unwrap().as_deref(),
        Some(&b"working"[..])
    );
    assert_eq!(
        host.kv_get("tok").await.unwrap().as_deref(),
        Some(&b"s3cret"[..])
    );
    assert_eq!(
        host.kv_list("state/").await.unwrap(),
        vec!["state/f/c/a".to_string()]
    );
    let on_disk = std::fs::read(w.h.kv_dir.path().join("plugins/flow/kv/tok")).unwrap();
    assert!(
        !on_disk.windows(6).any(|x| x == b"s3cret"),
        "sealed on disk"
    );
    let e = host.kv_get("../x").await.unwrap_err();
    assert!(
        e.to_string()
            .starts_with("daemon: HTTP 400: kv: invalid key"),
        "{e}"
    );
    host.kv_delete("tok").await.unwrap();
    assert_eq!(host.kv_get("tok").await.unwrap(), None);
    host.action("f/c/a", &PluginAction::Stop).await.unwrap();
    let rec = wait_for(&w, |r| {
        r.status.agents["f/c/a"].phase == AgentPhase::Stopped
    })
    .await;
    assert!(rec.stopped.contains("f/c/a"));
    host.action("f/c/a", &PluginAction::Restart).await.unwrap();
    wait_for(&w, |r| {
        r.stopped.is_empty() && r.status.agents["f/c/a"].phase == AgentPhase::Starting
    })
    .await;
    let e = host.action("f/c/b", &PluginAction::Stop).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "daemon: HTTP 404: plugin is not active for agent f/c/b"
    );
    // web has fleets, and sees the overlay
    let web_token = token(&w, "web").await;
    let (s, v) = w
        .api
        .plugin(&web_token, "GET", "/v1/plugin-host/fleets", None);
    assert_eq!(s, 200);
    assert_eq!(
        v[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"],
        "active"
    );
    let (s, _) = w
        .api
        .plugin(&web_token, "GET", "/v1/plugin-host/fleets/balerix", None);
    assert_eq!(s, 404);
    let (s, _) = w
        .api
        .plugin(&web_token, "GET", "/v1/plugin-host/kv?prefix=", None);
    assert_eq!(s, 403);
    let (s, _) = w.api.plugin("nope", "GET", "/v1/plugin-host/fleets", None);
    assert_eq!(s, 401);
    let (_, m) = w.api.call("GET", "/metrics", None, None);
    assert!(
        m.as_str()
            .unwrap()
            .contains("balerix_plugin_actions_total{action=\"stop\",plugin=\"flow\"} 1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_metrics_are_re_exported_under_the_prefix_rule() {
    let w = world().await;
    struct Good(balerix_plugin_sdk::Metrics);
    impl Plugin for Good {
        fn metrics(&self) -> Option<&balerix_plugin_sdk::Metrics> {
            Some(&self.0)
        }
    }
    let good = balerix_plugin_sdk::Metrics::new("flow");
    good.int_gauge_vec("state", "s", &["agent"])
        .unwrap()
        .with_label_values(&["a"])
        .set(1);
    let (l1, listen1) = bind().await.unwrap();
    let flow_token = token(&w, "flow").await;
    tokio::spawn(async move { run(l1, Arc::new(Good(good)), &flow_token).await });
    // A plugin outside the SDK answering an unprefixed family: the daemon
    // must drop the body whole.
    let bad = axum::Router::new().route(
        "/v1/metrics",
        axum::routing::get(|| async { "balerix_agents{fleet=\"spoof\"} 9\n" }),
    );
    let (l2, listen2) = bind().await.unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l2, bad).await;
    });
    for (name, listen) in [("flow", listen1), ("web", listen2)] {
        let env = Env {
            api_url: w.api.base.clone(),
            name: name.into(),
            token: token(&w, name).await,
            scratch: w.dir.path().join("s"),
        };
        Host::new(env)
            .unwrap()
            .hello("0.1.0", &listen)
            .await
            .unwrap();
    }
    let (_, m) = w.api.call("GET", "/metrics", None, None);
    let m = m.as_str().unwrap();
    assert!(
        m.contains("balerix_plugin_flow_state{agent=\"a\"} 1"),
        "{m}"
    );
    assert!(!m.contains("spoof"));
    let (_, m) = w.api.call("GET", "/metrics", None, None);
    assert!(
        m.as_str()
            .unwrap()
            .contains("balerix_plugin_metrics_scrape_failures_total{plugin=\"web\"} 1")
    );
}
