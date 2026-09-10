//! The router over the fakes, through a real listener and ureq (Phase 3
//! spec §8 "API integration").
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{
    AgentPhase, AgentSettings, CrewSpec, ErrorBody, FleetPhase, FleetRequest, FleetSpec,
    FleetSummary, GitSettings,
};
use balerix_core::{AgentId, FleetName, FleetRecord, PassThrough};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, router, serve};
use serde_json::{Value, json};

fn spec(agents: &[&str]) -> FleetSpec {
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
                    .map(|a| (a.to_string(), AgentSettings::default()))
                    .collect(),
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

struct Api {
    base: String,
    token: String,
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
            "PUT" => self.agent.put(&url),
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
    fn admin(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        self.call(method, path, Some(&self.token.clone()), body)
    }
}

async fn wait_for(daemon: &Daemon, pred: impl Fn(Option<&FleetRecord>) -> bool) {
    let name: FleetName = "f".parse().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if pred(daemon.get(&name).await.as_ref()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fleet_api_and_hook_ingress_end_to_end() {
    let h = Harness::new(Duration::from_secs(3600));
    // The plugin host wants a config even when no plugin is declared; the
    // directory stays empty and is dropped with the test.
    let plugin_dir = tempfile::tempdir().unwrap();
    let daemon = h.daemon(Arc::new(PassThrough), plugin_dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = stop_rx.await;
    }));
    let api = Arc::new(Api {
        base: format!("http://127.0.0.1:{port}"),
        token: "admin-tok".into(),
        agent: ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into(),
    });
    let call = |api: Arc<Api>,
                m: &'static str,
                p: String,
                tok: Option<String>,
                body: Option<Value>| async move {
        tokio::task::spawn_blocking(move || api.call(m, &p, tok.as_deref(), body.as_ref()))
            .await
            .unwrap()
    };
    let admin = |api: Arc<Api>, m: &'static str, p: String, body: Option<Value>| async move {
        tokio::task::spawn_blocking(move || api.admin(m, &p, body.as_ref()))
            .await
            .unwrap()
    };

    // health and auth
    let (st, v) = call(api.clone(), "GET", "/healthz".into(), None, None).await;
    assert_eq!((st, v.as_str()), (200, Some("ok")));
    let (st, v) = call(api.clone(), "GET", "/v1/fleets".into(), None, None).await;
    assert_eq!(st, 401);
    let e: ErrorBody = serde_json::from_value(v).unwrap();
    assert!(e.error.contains("admin token"));
    let (st, _) = call(
        api.clone(),
        "GET",
        "/v1/fleets".into(),
        Some("wrong".into()),
        None,
    )
    .await;
    assert_eq!(st, 401);

    // create, conflict, get, list
    let req = json!(FleetRequest {
        spec: spec(&["a", "b"]),
        credentials: Default::default()
    });
    let (st, v) = admin(api.clone(), "POST", "/v1/fleets".into(), Some(req.clone())).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["generation"], 1);
    let (st, _) = admin(api.clone(), "POST", "/v1/fleets".into(), Some(req.clone())).await;
    assert_eq!(st, 409);
    wait_for(&daemon, |r| {
        r.is_some_and(|r| r.status.observed_generation == 1)
    })
    .await;
    let (st, v) = admin(api.clone(), "GET", "/v1/fleets/f".into(), None).await;
    let rec: FleetRecord = serde_json::from_value(v).unwrap();
    assert_eq!(
        (st, rec.status.agents["f/c/a"].phase),
        (200, AgentPhase::Starting)
    );
    let (_, v) = admin(api.clone(), "GET", "/v1/fleets".into(), None).await;
    let rows: Vec<FleetSummary> = serde_json::from_value(v).unwrap();
    assert_eq!((rows.len(), rows[0].agents), (1, 2));
    let (st, _) = admin(api.clone(), "GET", "/v1/fleets/nope".into(), None).await;
    assert_eq!(st, 404);
    let (st, _) = admin(api.clone(), "GET", "/v1/fleets/Not-Valid!".into(), None).await;
    assert_eq!(st, 400);

    // update: name mismatch, then a real change
    let (st, v) = admin(api.clone(), "PUT", "/v1/fleets/g".into(), Some(req.clone())).await;
    assert_eq!(st, 400, "{v}");
    let mut changed = spec(&["a", "b"]);
    changed
        .crews
        .get_mut("c")
        .unwrap()
        .agents
        .get_mut("a")
        .unwrap()
        .env
        .insert("V".into(), "1".into());
    let req2 = json!(FleetRequest {
        spec: changed,
        credentials: Default::default()
    });
    let (st, v) = admin(api.clone(), "PUT", "/v1/fleets/f".into(), Some(req2)).await;
    assert_eq!((st, v["generation"].as_u64()), (200, Some(2)));
    let (st, v) = admin(
        api.clone(),
        "POST",
        "/v1/fleets".into(),
        Some(json!({ "spec": { "name": "f", "bogus": 1 } })),
    )
    .await;
    assert_eq!(st, 400, "{v}");
    // a request without a JSON content type is a 415, not a 400 — the
    // status carries information the client needs (fix it by asking for
    // `application/json`), unlike a malformed or badly-shaped body.
    let (st, v) = tokio::task::spawn_blocking({
        let api = api.clone();
        move || {
            let url = format!("{}/v1/fleets", api.base);
            let mut resp = api
                .agent
                .post(&url)
                .header("Authorization", format!("Bearer {}", api.token))
                .header("Content-Type", "text/plain")
                .send("{}")
                .unwrap();
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap();
            let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
            (status, v)
        }
    })
    .await
    .unwrap();
    assert_eq!(st, 415);
    assert!(v["error"].is_string(), "{v}");

    // hooks
    let a: AgentId = "f/c/a".parse().unwrap();
    let secret = daemon.hook_secret(&a).await.unwrap();
    let ev = json!({ "hook_event_name": "SessionStart", "session_id": "s1" });
    let events = "/v1/agents/f/c/a/events".to_string();
    let (st, _) = call(
        api.clone(),
        "POST",
        events.clone(),
        Some("wrong".into()),
        Some(ev.clone()),
    )
    .await;
    assert_eq!(st, 401);
    let (st, _) = call(
        api.clone(),
        "POST",
        "/v1/agents/f/c/zzz/events".into(),
        Some(secret.clone()),
        Some(ev.clone()),
    )
    .await;
    assert_eq!(st, 401, "unknown agent looks like a bad secret");
    let (st, _) = call(api.clone(), "POST", events.clone(), None, Some(ev.clone())).await;
    assert_eq!(st, 401);
    // The secret is checked *before* the rate limiter (spec §3.5): a burst
    // of wrong-secret requests — well past the burst size of 50 — must
    // never trip the limiter (429), and must never spend the bucket that a
    // correctly authenticated caller relies on.
    for _ in 0..60 {
        let (st, _) = call(
            api.clone(),
            "POST",
            events.clone(),
            Some("wrong".into()),
            Some(ev.clone()),
        )
        .await;
        assert_eq!(st, 401, "a bad secret must never be rate-limited");
    }
    let (st, v) = call(
        api.clone(),
        "POST",
        events.clone(),
        Some(secret.clone()),
        Some(ev.clone()),
    )
    .await;
    assert_eq!(
        (st, v),
        (200, json!({})),
        "the unauthenticated burst must not have spent the bucket"
    );
    wait_for(&daemon, |r| {
        r.is_some_and(|r| r.status.agents["f/c/a"].phase == AgentPhase::Ready)
    })
    .await;
    let (st, v) = call(
        api.clone(),
        "POST",
        events.clone(),
        Some(secret.clone()),
        Some(json!([1])),
    )
    .await;
    assert_eq!(st, 400);
    assert_eq!(v["error"], "body must be a JSON object");
    let big = json!({ "hook_event_name": "PreToolUse", "blob": "x".repeat(2 << 20) });
    let (st, v) = call(
        api.clone(),
        "POST",
        events.clone(),
        Some(secret.clone()),
        Some(big),
    )
    .await;
    assert_eq!(st, 413);
    // axum's own body-limit rejection is plain text; the handler must
    // render it as `ApiError`'s `{ "error": "<message>" }` JSON instead.
    let _: ErrorBody = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}: {v}"));
    assert!(v["error"].is_string(), "{v}");
    // The bucket refills at 20/s; issued one at a time, a slow test runner
    // can drain the burst as fast as it refills and never see a 429. Fire
    // all 80 requests concurrently so the burst actually lands together,
    // regardless of scheduling delay between requests.
    let mut burst = tokio::task::JoinSet::new();
    for _ in 0..80 {
        let api = api.clone();
        let events = events.clone();
        let secret = secret.clone();
        burst.spawn_blocking(move || {
            api.call(
                "POST",
                &events,
                Some(&secret),
                Some(&json!({ "hook_event_name": "PreToolUse" })),
            )
        });
    }
    let mut saw_429 = false;
    while let Some(res) = burst.join_next().await {
        let (st, _) = res.unwrap();
        if st == 429 {
            saw_429 = true;
        }
    }
    assert!(
        saw_429,
        "burst of 80 concurrent requests must trip the limiter"
    );

    // metrics
    let (st, v) = call(api.clone(), "GET", "/metrics".into(), None, None).await;
    let text = v.as_str().unwrap();
    assert_eq!(st, 200);
    assert!(
        text.contains(
            "balerix_hook_events_total{agent=\"a\",crew=\"c\",event=\"SessionStart\",fleet=\"f\"} 1"
        ),
        "{text}"
    );
    assert!(text.contains("balerix_agents{crew=\"c\",fleet=\"f\",phase=\"ready\"} 1"));
    assert!(text.contains("balerix_fleets{phase=\"reconciling\"} 1"));

    // down: bad flags, keep, re-up, purge
    let (st, v) = admin(
        api.clone(),
        "DELETE",
        "/v1/fleets/f?purge=true&keep_repos=true".into(),
        None,
    )
    .await;
    assert_eq!(st, 400, "{v}");
    let (st, v) = admin(
        api.clone(),
        "DELETE",
        "/v1/fleets/f?keep_repos=true&keep_sessions=true&purge=false".into(),
        None,
    )
    .await;
    assert_eq!(st, 200, "{v}");
    wait_for(&daemon, |r| {
        r.is_some_and(|r| r.status.phase == FleetPhase::Down)
    })
    .await;
    assert!(
        h.materializer
            .calls()
            .contains(&"remove_crew f/c repos=true sessions=true".to_string())
    );
    let (st, v) = admin(api.clone(), "POST", "/v1/fleets".into(), Some(req.clone())).await;
    assert_eq!(
        (st, v["generation"].as_u64()),
        (200, Some(3)),
        "a downed fleet re-applies in place"
    );
    wait_for(&daemon, |r| {
        r.is_some_and(|r| r.status.observed_generation == 3)
    })
    .await;
    let (st, _) = admin(
        api.clone(),
        "DELETE",
        "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=true".into(),
        None,
    )
    .await;
    assert_eq!(st, 200);
    wait_for(&daemon, |r| r.is_none()).await;
    let (st, _) = admin(api.clone(), "GET", "/v1/fleets/f".into(), None).await;
    assert_eq!(st, 404);
    let (_, v) = admin(api.clone(), "GET", "/v1/fleets".into(), None).await;
    assert_eq!(v, json!([]));
    assert!(h.store.names().is_empty());
    let (st, _) = admin(
        api.clone(),
        "DELETE",
        "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false".into(),
        None,
    )
    .await;
    assert_eq!(st, 404);

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
}
