//! The SDK against `docs/plugin-protocol/` (plugins spec §4, §10):
//! every daemon-to-plugin fixture through `router`, every plugin-to-daemon
//! fixture through `Host` against `FakeHost`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use balerix_api::{
    DownQuery, FleetRecord, HookEvent, InterceptResponse, PluginAction, WorkspaceDiff,
};
use balerix_plugin_sdk::testing::FakeHost;
use balerix_plugin_sdk::{Host, Plugin, bind, run};
use serde_json::{Value, json};

fn fixtures() -> BTreeMap<String, Value> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/plugin-protocol");
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let p: PathBuf = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "json") {
            let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
            out.insert(p.file_stem().unwrap().to_string_lossy().into_owned(), v);
        }
    }
    assert_eq!(
        out.len(),
        26,
        "every fixture accounted for: {:?}",
        out.keys()
    );
    out
}

fn b64(s: &str) -> Vec<u8> {
    // tiny base64 decoder: fixtures only carry short ASCII payloads
    let table: Vec<u8> = (b'A'..=b'Z')
        .chain(b'a'..=b'z')
        .chain(b'0'..=b'9')
        .chain(*b"+/")
        .collect();
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.bytes().filter(|c| *c != b'=') {
        bits = (bits << 6) | table.iter().position(|t| *t == c).unwrap() as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    out
}

/// The reference plugin the daemon-to-plugin fixtures describe.
struct Reference {
    metrics: balerix_plugin_sdk::Metrics,
}

impl Reference {
    fn new() -> Self {
        let metrics = balerix_plugin_sdk::Metrics::new("flow");
        metrics
            .int_gauge_vec(
                "state",
                "Current flow state (1 for the current state)",
                &["agent", "state"],
            )
            .unwrap()
            .with_label_values(&["bob", "working"])
            .set(1);
        Self { metrics }
    }
}

impl Plugin for Reference {
    async fn activate(&self, _agent: &str, config: Value) -> Result<(), String> {
        match config["initial"].as_str() {
            Some("working") => Ok(()),
            Some(other) => Err(format!("initial: unknown state {other:?}")),
            None => Err("initial: missing".into()),
        }
    }
    async fn intercept(
        &self,
        event: HookEvent,
        _so_far: Value,
        _deadline: u64,
    ) -> InterceptResponse {
        assert_eq!(event.payload["tool_input"]["command"], "rm -rf /");
        InterceptResponse {
            response: json!({ "decision": "block", "reason": "no recursive deletes" }),
            actions: vec![PluginAction::SendText {
                text: "Use trash instead.".into(),
                submit: true,
            }],
        }
    }
    fn metrics(&self) -> Option<&balerix_plugin_sdk::Metrics> {
        Some(&self.metrics)
    }
    fn routes(&self) -> Option<axum::Router> {
        Some(axum::Router::new().route("/", axum::routing::get(|| async { "hello from routes\n" })))
    }
}

#[tokio::test]
async fn the_router_answers_every_daemon_to_plugin_fixture() {
    let (listener, listen) = bind().await.unwrap();
    tokio::spawn(run(listener, Arc::new(Reference::new()), "tok"));
    let c = reqwest::Client::builder().no_proxy().build().unwrap();
    for (name, f) in fixtures()
        .iter()
        .filter(|(_, f)| f["direction"] == "daemon-to-plugin" && f.get("transport").is_none())
    {
        let (method, path) = f["route"].as_str().unwrap().split_once(' ').unwrap();
        let url = format!("http://{listen}{path}");
        let req = match method {
            "POST" => c.post(&url).json(&f["request"]),
            "GET" => c.get(&url),
            _ => unreachable!(),
        };
        let mut req = req;
        if let Some(headers) = f["headers"].as_object() {
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str().unwrap());
            }
        }
        let resp = req.send().await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            f["status"].as_u64().unwrap() as u16,
            "{name}"
        );
        let bytes = resp.bytes().await.unwrap();
        match f.get("raw") {
            Some(raw) => assert_eq!(bytes.to_vec(), b64(raw.as_str().unwrap()), "{name}"),
            None => assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                f["response"],
                "{name}"
            ),
        }
    }
}

#[tokio::test]
async fn the_host_sends_every_plugin_to_daemon_fixture_and_reads_the_answer() {
    let fx = fixtures();
    let record: FleetRecord = serde_json::from_value(fx["fleets"]["response"][0].clone()).unwrap();
    let fake = FakeHost::start("tok", json!({ "greeting": "hi" }), vec![record]).await;
    let host = Host::new(fake.env("flow", Path::new("/s"))).unwrap();
    // A raw client for asserting a fixture's exact recorded status and body
    // beyond what the typed `Host` API surfaces (e.g. `Result<(), _>` calls
    // collapse a 200 and its `{}` body to `Ok(())`; `SdkError::Status` keeps
    // only the message, not the numeric code as read from the fixture).
    let c = reqwest::Client::builder().no_proxy().build().unwrap();

    let r = host.hello("0.1.0", "127.0.0.1:4000").await.unwrap();
    assert_eq!(serde_json::to_value(&r).unwrap(), fx["hello"]["response"]);
    assert_eq!(
        serde_json::to_value(&fake.hellos()[0]).unwrap(),
        fx["hello"]["request"]
    );
    let mut env = fake.env("flow", Path::new("/s"));
    env.token = fx["hello-bad-token"]["token"].as_str().unwrap().into();
    let e = Host::new(env)
        .unwrap()
        .hello("0.1.0", "127.0.0.1:4000")
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        format!(
            "daemon: HTTP 401: {}",
            fx["hello-bad-token"]["response"]["error"].as_str().unwrap()
        )
    );
    // `Display` above only proves the message; check the fixture's exact
    // recorded status and body on the wire.
    let resp = c
        .post(format!("{}/v1/plugin-host/hello", fake.url))
        .bearer_auth(fx["hello-bad-token"]["token"].as_str().unwrap())
        .json(&fx["hello-bad-token"]["request"])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        fx["hello-bad-token"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        fx["hello-bad-token"]["response"]
    );

    let fleets = host.fleets().await.unwrap();
    assert_eq!(
        serde_json::to_value(&fleets).unwrap(),
        fx["fleets"]["response"]
    );
    assert_eq!(host.fleet("nope").await.unwrap(), None);
    // `Host::fleet` collapses every non-2xx to `None`; check the fixture's
    // exact recorded status and body on the wire.
    let resp = c
        .get(format!("{}/v1/plugin-host/fleets/nope", fake.url))
        .bearer_auth("tok")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        fx["fleet-missing"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        fx["fleet-missing"]["response"]
    );

    let action: PluginAction = serde_json::from_value(fx["action"]["request"].clone()).unwrap();
    host.action("payments/backend/bob", &action).await.unwrap();
    assert_eq!(
        fake.actions()[0],
        ("payments/backend/bob".to_string(), action)
    );

    let keys: PluginAction =
        serde_json::from_value(fx["action-send-keys"]["request"].clone()).unwrap();
    assert_eq!(keys.validate(), Ok(()));
    host.action("payments/backend/bob", &keys).await.unwrap();
    assert_eq!(
        fake.actions()[1],
        ("payments/backend/bob".to_string(), keys)
    );

    let bytes = b64(fx["kv-put"]["raw"].as_str().unwrap());
    host.kv_put("state/payments/backend/bob", &bytes, false)
        .await
        .unwrap();
    assert_eq!(
        host.kv_get("state/payments/backend/bob").await.unwrap(),
        Some(b64(fx["kv-get"]["raw"].as_str().unwrap()))
    );
    assert_eq!(
        serde_json::to_value(host.kv_list("state/").await.unwrap()).unwrap(),
        fx["kv-list"]["response"]["keys"]
    );

    let diff: WorkspaceDiff =
        serde_json::from_value(fx["workspace-diff"]["response"].clone()).unwrap();
    let file = b64(fx["workspace-file"]["raw"].as_str().unwrap());
    fake.set_workspace(
        "payments/backend/bob",
        diff.clone(),
        BTreeMap::from([("src/lib.rs".to_string(), file.clone())]),
    );
    assert_eq!(
        serde_json::to_value(host.workspace_diff("payments/backend/bob").await.unwrap()).unwrap(),
        fx["workspace-diff"]["response"]
    );
    assert_eq!(
        host.workspace_file("payments/backend/bob", "src/lib.rs")
            .await
            .unwrap(),
        Some(file)
    );
    assert_eq!(
        host.workspace_file("payments/backend/bob", "nope")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        serde_json::to_value(
            host.workspace_tree("payments/backend/bob", "src")
                .await
                .unwrap()
        )
        .unwrap(),
        fx["workspace-tree"]["response"]
    );
    assert_eq!(
        serde_json::to_value(
            host.workspace_version("payments/backend/bob")
                .await
                .unwrap()
        )
        .unwrap(),
        fx["workspace-version"]["response"]
    );
    let e = host
        .workspace_version("payments/backend/nobody")
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "daemon: HTTP 404: no workspace for agent payments/backend/nobody"
    );
    // an agent with no workspace is the other 404, surfaced as an error
    let e = host
        .workspace_diff("payments/backend/nobody")
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "daemon: HTTP 404: no workspace for agent payments/backend/nobody"
    );
    let e = host
        .workspace_file("payments/backend/nobody", "x")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no workspace"), "{e}");

    // Spec L: the manage routes. The fake answers the record it holds
    // under the name, so `set_fleets` shapes the answer to the fixture.
    let put_record: FleetRecord =
        serde_json::from_value(fx["fleet-put"]["response"].clone()).unwrap();
    fake.set_fleets(vec![put_record]);
    let file = fx["fleet-put"]["request"]["file"].clone();
    assert_eq!(
        serde_json::to_value(host.apply_fleet("gh-acme-api", &file).await.unwrap()).unwrap(),
        fx["fleet-put"]["response"]
    );
    assert_eq!(
        fake.applied_fleets(),
        vec![("gh-acme-api".to_string(), file.clone())]
    );
    let rejected = fx["fleet-put-rejected"]["response"]["error"]
        .as_str()
        .unwrap();
    fake.fail_manage(Some((400, rejected)));
    let e = host
        .apply_fleet("gh-acme-api", &fx["fleet-put-rejected"]["request"]["file"])
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), format!("daemon: HTTP 400: {rejected}"));
    let resp = c
        .put(format!("{}/v1/plugin-host/fleets/gh-acme-api", fake.url))
        .bearer_auth("tok")
        .json(&fx["fleet-put-rejected"]["request"])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        fx["fleet-put-rejected"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        fx["fleet-put-rejected"]["response"]
    );
    fake.fail_manage(None);
    let q = DownQuery::default();
    assert_eq!(
        fx["fleet-delete"]["route"],
        format!(
            "DELETE /v1/plugin-host/fleets/gh-acme-api?{}",
            q.to_query_string()
        ),
        "the fixture's route carries every flag, as the SDK sends them"
    );
    assert_eq!(
        serde_json::to_value(host.down_fleet("gh-acme-api", &q).await.unwrap()).unwrap(),
        fx["fleet-delete"]["response"]
    );
    assert_eq!(fake.downed_fleets(), vec![("gh-acme-api".to_string(), q)]);
    let e = host.down_fleet("nope", &q).await.unwrap_err();
    assert_eq!(e.to_string(), "daemon: HTTP 404: fleet not found");
}

/// The two WebSocket fixtures: one frame each, asserted against `FakeHost`
/// (the daemon's `PluginClient` has no stream calls to replay them through).
#[tokio::test]
async fn the_host_streams_match_their_fixtures() {
    let fx = fixtures();
    let record: FleetRecord =
        serde_json::from_value(fx["fleets-watch"]["frame"][0].clone()).unwrap();
    let fake = FakeHost::start("tok", json!({}), vec![record]).await;
    let host = Host::new(fake.env("web", Path::new("/s"))).unwrap();
    let mut watch = host.watch_fleets();
    assert_eq!(
        serde_json::to_value(watch.next().await).unwrap(),
        fx["fleets-watch"]["frame"]
    );
    let mut a = host.attach("payments/backend/bob").await.unwrap();
    a.resize(120, 40).await.unwrap();
    a.write(b"ls\n").await.unwrap();
    assert_eq!(a.read().await.as_deref(), Some(&b"ls\n"[..]));
    assert_eq!(fake.resizes()[0].1, fx["attach-resize"]["frame"]);
    assert_eq!(
        fx["attach-resize"]["route"].as_str().unwrap(),
        "GET /v1/plugin-host/agents/payments/backend/bob/attach"
    );
    assert_eq!(fake.attaches(), vec!["payments/backend/bob".to_string()]);
}
