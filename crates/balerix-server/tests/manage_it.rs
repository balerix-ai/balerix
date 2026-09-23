//! Spec L §3: `PUT`/`DELETE /v1/plugin-host/fleets/{name}` over HTTP —
//! the capability gate, the owner rule from both sides, the admin's
//! `force`, and the body shapes the route refuses.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{
    AgentSettings, CrewSpec, FleetPhase, FleetRecord, FleetRequest, FleetSpec, FleetSummary,
    GitSettings,
};
use balerix_core::plugin_id;
use balerix_plugin_sdk::{Env, Host, Plugin, bind, run};
use serde_json::{Value, json};
use support::{World, world_with};

struct Silent;
impl Plugin for Silent {}

async fn token(w: &World, plugin: &str) -> String {
    w.daemon
        .hook_secret(&plugin_id(&plugin.parse().unwrap()))
        .await
        .unwrap()
}

/// Starts a silent SDK plugin under `name` and says hello with its token.
async fn start_silent(w: &World, name: &str) -> Host {
    let env = Env {
        api_url: w.api.base.clone(),
        name: name.into(),
        token: token(w, name).await,
        scratch: w.dir.path().join("s"),
    };
    let (listener, listen) = bind().await.unwrap();
    let tok = env.token.clone();
    tokio::spawn(async move { run(listener, Arc::new(Silent), &tok).await });
    let host = Host::new(env).unwrap();
    host.hello("0.1.0", &listen).await.unwrap();
    host
}

fn spec(name: &str) -> FleetSpec {
    FleetSpec {
        name: name.into(),
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
    }
}

fn file(name: &str) -> Value {
    json!({
        "apiVersion": "balerix/v1", "kind": "Fleet", "name": name,
        "crews": { "c": { "repo": "acme/x", "agents": { "a": {} } } }
    })
}

async fn wait_for(w: &World, name: &str, pred: impl Fn(&FleetRecord) -> bool) -> FleetRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(r) = w.daemon.get(&name.parse().unwrap()).await
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

const DOWN: &str = "keep_repos=false&keep_sessions=false&purge=false&force=false";
const FORCED: &str = "keep_repos=false&keep_sessions=false&purge=false&force=true";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_with_manage_applies_and_downs_a_fleet_it_owns() {
    let w = world_with(&[("gh", "needs: [fleets, manage]\n")]).await;
    let _gh = start_silent(&w, "gh").await;
    let _web = start_silent(&w, "web").await;
    let gh_tok = token(&w, "gh").await;
    let web_tok = token(&w, "web").await;
    w.h.resolver.set(Ok(spec("f")));

    // apply: owned, resolved through the port, running
    let (s, v) = w.api.plugin(
        &gh_tok,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["owner"].as_str(), v["generation"].as_u64()),
        (Some("gh"), Some(1))
    );
    assert_eq!(w.h.resolver.calls(), vec![("f".to_string(), file("f"))]);
    wait_for(&w, "f", |r| r.status.observed_generation == 1).await;
    let (s, v) = w
        .api
        .plugin(&gh_tok, "GET", "/v1/plugin-host/fleets/f", None);
    assert_eq!((s, v["owner"].as_str()), (200, Some("gh")));
    let (_, v) = w.api.admin("GET", "/v1/fleets", None);
    let rows: Vec<FleetSummary> = serde_json::from_value(v).unwrap();
    assert_eq!(rows[0].managed_by.as_deref(), Some("gh"));

    // the capability gate
    let (s, v) = w.api.plugin(
        &web_tok,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!(s, 403, "{v}");
    assert_eq!(
        v["error"],
        "capability \"manage\" not declared in balerix-plugin.yaml"
    );
    let (s, _) = w.api.plugin(
        &web_tok,
        "DELETE",
        &format!("/v1/plugin-host/fleets/f?{DOWN}"),
        None,
    );
    assert_eq!(s, 403);
    let (s, _) = w.api.plugin(
        "nope",
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!(s, 401);

    // a second apply replaces in place
    let (s, v) = w.api.plugin(
        &gh_tok,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!((s, v["generation"].as_u64()), (200, Some(2)));

    // the admin routes refuse a managed fleet; `force` takes it down
    let req = json!(FleetRequest {
        spec: spec("f"),
        credentials: Default::default()
    });
    let (s, v) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("fleet f is managed by plugin gh"))
    );
    let (s, v) = w.api.admin("PUT", "/v1/fleets/f", Some(&req));
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("fleet f is managed by plugin gh"))
    );
    let (s, v) = w.api.admin("DELETE", &format!("/v1/fleets/f?{DOWN}"), None);
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("fleet f is managed by plugin gh"))
    );
    let (s, v) = w
        .api
        .admin("DELETE", &format!("/v1/fleets/f?{FORCED}"), None);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["owner"], "gh", "a forced down keeps the owner");
    wait_for(&w, "f", |r| r.status.phase == FleetPhase::Down).await;

    // the owner resumes it and downs it itself
    let (s, v) = w.api.plugin(
        &gh_tok,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!((s, v["generation"].as_u64()), (200, Some(3)), "{v}");
    let (s, v) = w.api.plugin(
        &gh_tok,
        "DELETE",
        &format!("/v1/plugin-host/fleets/f?{DOWN}"),
        None,
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["desired"]["state"].as_str(), v["owner"].as_str()),
        (Some("down"), Some("gh"))
    );

    // a fleet the CLI created is not the plugin's to apply or down
    let g = json!(FleetRequest {
        spec: spec("g"),
        credentials: Default::default()
    });
    let (s, _) = w.api.admin("POST", "/v1/fleets", Some(&g));
    assert_eq!(s, 200);
    let (s, v) = w.api.plugin(
        &gh_tok,
        "PUT",
        "/v1/plugin-host/fleets/g",
        Some(&json!({ "file": file("g") })),
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("fleet g is not managed by a plugin"))
    );
    let (s, v) = w.api.plugin(
        &gh_tok,
        "DELETE",
        &format!("/v1/plugin-host/fleets/g?{DOWN}"),
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("fleet g is not managed by a plugin"))
    );
    assert_eq!(
        w.h.resolver.calls().len(),
        3,
        "a foreign fleet was never resolved"
    );
}

/// Review focus 3 and 5: what the route refuses before the daemon sees a file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manage_routes_refuse_bad_names_bodies_and_flags() {
    let w = world_with(&[("gh", "needs: [manage]\n")]).await;
    let _gh = start_silent(&w, "gh").await;
    let gh_tok = token(&w, "gh").await;
    let put = |path: &str, body: Value| w.api.plugin(&gh_tok, "PUT", path, Some(&body));

    // `watch` is a route, never a fleet: axum answers the static route's
    // method set, and the daemon's reserved-name rule stands behind it
    let (s, _) = put(
        "/v1/plugin-host/fleets/watch",
        json!({ "file": file("watch") }),
    );
    assert_eq!(s, 405);
    let (s, v) = put(
        "/v1/plugin-host/fleets/balerix",
        json!({ "file": file("balerix") }),
    );
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().starts_with("name:"), "{v}");
    let (s, v) = put(
        "/v1/plugin-host/fleets/Not-Valid",
        json!({ "file": file("f") }),
    );
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().starts_with("name:"), "{v}");
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({}));
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("file"), "{v}");
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({ "file": 3 }));
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("file: expected a mapping"))
    );
    let (s, v) = put(
        "/v1/plugin-host/fleets/f",
        json!({ "file": file("f"), "x": 1 }),
    );
    assert_eq!(s, 400, "{v}");
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({ "file": file("g") }));
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("name: \"g\" does not match the fleet f"))
    );
    assert!(
        w.h.resolver.calls().is_empty(),
        "nothing above reached the resolver"
    );

    // the resolver's refusal, verbatim, config path first
    let bad = "crews.c.agents.a.tools.node: expected an exact version, got \"22\" (try: mise latest node@22)";
    w.h.resolver.set(Err(bad.into()));
    let (s, v) = put("/v1/plugin-host/fleets/f", json!({ "file": file("f") }));
    assert_eq!((s, v["error"].as_str()), (400, Some(bad)));
    assert!(w.daemon.get(&"f".parse().unwrap()).await.is_none());

    // the down flags' rule, and a fleet that does not exist
    let (s, v) = w.api.plugin(
        &gh_tok,
        "DELETE",
        "/v1/plugin-host/fleets/f?keep_repos=true&keep_sessions=false&purge=true&force=false",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("purge cannot be combined with keep flags"))
    );
    let (s, v) = w.api.plugin(
        &gh_tok,
        "DELETE",
        &format!("/v1/plugin-host/fleets/f?{DOWN}"),
        None,
    );
    assert_eq!((s, v["error"].as_str()), (404, Some("fleet not found")));
}
