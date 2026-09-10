//! The web plugin through the SDK harness (plugins spec §18.5): activation
//! validates, the index follows `fleets/watch`, the bridge echoes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use balerix_api::{AgentPhase, FleetRecord, FleetSpec, PluginAction};
use balerix_plugin_sdk::testing::{FakeHost, Harness, event, metric};
use balerix_plugin_sdk::{Env, Host};
use balerix_plugin_web::WebPlugin;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ALICE: &str = "e2e/c/alice";
const BOB: &str = "e2e/c/bob";

fn fleet(agents: &[(&str, AgentPhase)]) -> FleetRecord {
    let mut r = FleetRecord::new(FleetSpec {
        name: "e2e".into(),
        crews: BTreeMap::new(),
        ..Default::default()
    });
    for (id, phase) in agents {
        r.status.entry(id).phase = *phase;
    }
    r
}

async fn world() -> (FakeHost, Env, Harness, tokio::task::JoinHandle<()>) {
    let fake = FakeHost::start("tok", json!({}), vec![]).await;
    let env = fake.env("web", std::path::Path::new("/s"));
    let plugin = WebPlugin::new(Host::new(env.clone()).unwrap()).unwrap();
    let watch = plugin.start_watch();
    let h = Harness::start(&env, plugin).await;
    (fake, env, h, watch)
}

async fn rows(h: &Harness) -> Vec<Value> {
    let (status, _, body) = h.get_route("/agents.json", "/v1/plugins/web").await;
    assert_eq!(status, 200);
    serde_json::from_slice(&body).unwrap()
}

async fn wait_rows(h: &Harness, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let r = rows(h).await;
            if pred(&r) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the rows to settle")
}

#[tokio::test]
async fn activation_validates_and_the_index_follows_the_watch() {
    let (fake, _, h, _watch) = world().await;
    assert_eq!(h.activate(ALICE, json!({})).await, Ok(()));
    assert_eq!(h.activate(BOB, json!({ "enabled": false })).await, Ok(()));
    assert_eq!(
        h.activate("e2e/c/carol", json!({ "nope": 1 })).await,
        Err("nope: unknown field `nope`".into())
    );
    let r = wait_rows(&h, |r| r.len() == 1).await;
    assert_eq!(
        r[0],
        json!({ "id": ALICE, "phase": "pending", "message": "" }),
        "no frame yet"
    );

    fake.set_fleets(vec![fleet(&[
        (ALICE, AgentPhase::Ready),
        (BOB, AgentPhase::Ready),
    ])]);
    let r = wait_rows(&h, |r| r[0]["phase"] == "ready").await;
    assert_eq!(r.len(), 1, "bob is disabled");

    // enabling bob in place, and a config change back
    assert_eq!(h.activate(BOB, json!({ "enabled": true })).await, Ok(()));
    wait_rows(&h, |r| r.len() == 2 && r[1]["id"] == BOB).await;
    h.deactivate(ALICE).await;
    let r = wait_rows(&h, |r| r.len() == 1).await;
    assert_eq!(r[0]["id"], BOB);
    let _ = h.token();
}

#[tokio::test]
async fn pages_link_through_the_forwarded_prefix_and_assets_are_served() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_fleets(vec![fleet(&[(ALICE, AgentPhase::Ready)])]);
    wait_rows(&h, |r| r[0]["phase"] == "ready").await;

    let (status, headers, body) = h.get_route("", "/v1/plugins/web").await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.starts_with("text/html"))
    );
    assert!(
        body.contains(r#"href="/v1/plugins/web/agents/e2e/c/alice""#),
        "{body}"
    );
    assert!(
        body.contains(r#"fetch("/v1/plugins/web/agents.json")"#),
        "{body}"
    );
    assert!(
        !body.contains("bob"),
        "disabled agents are not listed: {body}"
    );

    let (status, _, body) = h.get_route("/agents/e2e/c/alice", "/v1/plugins/web").await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, 200);
    // assets live under a digest of their bytes, so `immutable` caching
    // cannot serve a stale bundle after a vendor bump
    let digest = asset_digest(&body);
    assert!(
        body.contains(&format!(
            r#"src="/v1/plugins/web/assets/{digest}/xterm.js""#
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            r#"src="/v1/plugins/web/assets/{digest}/addon-fit.js""#
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            r#"href="/v1/plugins/web/assets/{digest}/xterm.css""#
        )),
        "{body}"
    );
    assert!(
        body.contains("/v1/plugins/web/agents/e2e/c/alice/ws"),
        "{body}"
    );
    assert!(body.contains("e2e/c/alice"), "{body}");
    let (status, _, _) = h.get_route("/agents/e2e/c/bob", "/v1/plugins/web").await;
    assert_eq!(status, 404, "hidden agents have no page");
    let (status, _, _) = h.get_route("/agents/e2e/c/bob/ws", "/v1/plugins/web").await;
    assert_ne!(status, 200, "and no bridge");

    // the bytes are the committed files, whatever their size this week
    for (file, kind, len) in [
        (
            "xterm.js",
            "text/javascript",
            include_bytes!("../assets/xterm.js").len(),
        ),
        (
            "xterm.css",
            "text/css",
            include_bytes!("../assets/xterm.css").len(),
        ),
        (
            "addon-fit.js",
            "text/javascript",
            include_bytes!("../assets/addon-fit.js").len(),
        ),
    ] {
        let (status, headers, body) = h
            .get_route(&format!("/assets/{digest}/{file}"), "/v1/plugins/web")
            .await;
        assert_eq!(status, 200, "{file}");
        assert_eq!(body.len(), len, "{file}");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "content-type" && v.starts_with(kind)),
            "{file}: {headers:?}"
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "cache-control" && v.contains("immutable")),
            "{file}"
        );
    }
    let (status, _, _) = h
        .get_route(&format!("/assets/{digest}/nope.js"), "/v1/plugins/web")
        .await;
    assert_eq!(status, 404);
    let (status, _, _) = h.get_route("/assets/xterm.js", "/v1/plugins/web").await;
    assert_eq!(status, 404, "the unversioned path is gone");
    let (status, _, _) = h
        .get_route("/assets/000000000000/xterm.js", "/v1/plugins/web")
        .await;
    assert_eq!(
        status, 404,
        "another digest is another bundle, not this one"
    );

    // a forwarded prefix that is not a path is not trusted into the pages
    let (status, _, body) = h.get_route("", "http://evil.example").await;
    assert_eq!(status, 200);
    let body = String::from_utf8(body).unwrap();
    assert!(!body.contains("evil.example"), "{body}");
    assert!(body.contains(r#"fetch("/agents.json")"#), "{body}");
}

/// The twelve hex digits the page links its assets under.
fn asset_digest(page: &str) -> String {
    let marker = "/v1/plugins/web/assets/";
    let start = page.find(marker).expect("an asset link") + marker.len();
    let digest = page[start..start + 12].to_string();
    assert!(
        digest.chars().all(|c| c.is_ascii_hexdigit()),
        "a digest segment: {digest:?}"
    );
    digest
}

#[tokio::test]
async fn the_bridge_relays_bytes_and_resizes_to_the_daemon_attach() {
    let (fake, env, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    let url = format!("ws://{}/v1/routes/agents/e2e/c/alice/ws", h.listen());
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", env.token).parse().unwrap(),
    );
    let (mut ws, _) = connect_async(req).await.unwrap();
    ws.send(Message::Binary(b"ls\n".to_vec().into()))
        .await
        .unwrap();
    let echo = ws.next().await.unwrap().unwrap();
    assert_eq!(
        echo.into_data().as_ref(),
        b"ls\n",
        "browser → plugin → daemon(fake) → back"
    );
    ws.send(Message::Text(r#"{"resize":{"cols":100,"rows":30}}"#.into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while fake.resizes().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fake.resizes()[0],
        (
            ALICE.to_string(),
            json!({ "resize": { "cols": 100, "rows": 30 } })
        )
    );
    assert_eq!(fake.attaches(), vec![ALICE.to_string()]);
    let text = h.metrics().await;
    assert_eq!(
        metric(&text, "balerix_plugin_web_terminals_open", &[]),
        Some(1.0)
    );
    assert_eq!(
        metric(&text, "balerix_plugin_web_terminals_total", &[]),
        Some(1.0)
    );
    // a text frame that is not a resize is ignored, not forwarded and
    // not fatal: the next bytes still cross
    ws.send(Message::Text("junk".into())).await.unwrap();
    ws.send(Message::Binary(b"more".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().into_data().as_ref(),
        b"more"
    );
    assert_eq!(fake.resizes().len(), 1, "junk was not a resize");
    // the daemon's side ends: the browser gets the daemon's code and
    // reason, not a fixed 1000
    fake.close_attaches(1011, "the terminal's writer failed");
    let close = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Message::Close(f) = ws.next().await.expect("a frame").unwrap() {
                return f;
            }
        }
    })
    .await
    .expect("the daemon's close reached the browser")
    .expect("with a frame");
    assert_eq!(
        (u16::from(close.code), close.reason.as_str()),
        (1011, "the terminal's writer failed")
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while metric(&h.metrics().await, "balerix_plugin_web_terminals_open", &[]) != Some(0.0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the gauge falls when the daemon ends the terminal");
    let _ = ws.close(None).await;
    // a bridge for a hidden agent is refused before any attach
    let mut req = format!("ws://{}/v1/routes/agents/e2e/c/bob/ws", h.listen())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", env.token).parse().unwrap(),
    );
    assert!(connect_async(req).await.is_err());
    assert_eq!(fake.attaches().len(), 1);
}

#[tokio::test]
async fn observed_events_feed_the_activity_column_of_enabled_agents() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_fleets(vec![fleet(&[(ALICE, AgentPhase::Ready)])]);
    h.observe(vec![
        event(
            ALICE,
            "PreToolUse",
            json!({ "tool_name": "Bash", "tool_input": { "command": "cargo test" } }),
        ),
        event(BOB, "Stop", json!({})),
        event(
            ALICE,
            "Notification",
            json!({ "message": "x".repeat(6000) }),
        ),
        event(ALICE, "Stop", json!({})),
    ])
    .await;
    let (status, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["phase"], "ready");
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 3, "{v}");
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["summary"], "Bash: cargo test");
    assert_eq!(events[1]["payload_truncated"], true);
    assert_eq!(events[1]["payload"]["truncated"], true);
    assert_eq!(events[2]["summary"], "turn ended");
    let (status, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json?after=2", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["events"].as_array().unwrap().len(), 1);
    assert_eq!(v["events"][0]["seq"], 3);
    let (status, _, _) = h
        .get_route("/agents/e2e/c/bob/events.json", "/v1/plugins/web")
        .await;
    assert_eq!(
        status, 404,
        "hidden agents have no column and their events were dropped"
    );
    let (status, _, _) = h
        .get_route("/agents/e2e/c/alice/events.json?after=x", "/v1/plugins/web")
        .await;
    assert_eq!(status, 400);
    let text = h.metrics().await;
    assert_eq!(
        metric(&text, "balerix_plugin_web_events_buffered_total", &[]),
        Some(3.0)
    );
}

fn sample_diff() -> balerix_api::WorkspaceDiff {
    use balerix_api::{FileDiff, FileStatus, WorkspaceDiff};
    WorkspaceDiff {
        base_ref: "origin/main".into(),
        merge_base: "m".repeat(40),
        head: "3f9c2a1".to_string() + &"0".repeat(33),
        files: vec![FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            uncommitted: true,
            binary: false,
            patch: "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn b() { c() }\n".into(),
            truncated: false,
        }],
        truncated: false,
    }
}

#[tokio::test]
async fn the_review_page_and_its_data_routes_pass_the_workspace_through() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_workspace(
        ALICE,
        sample_diff(),
        BTreeMap::from([(
            "src/lib.rs".to_string(),
            b"fn a() {}\nfn b() { c() }\n".to_vec(),
        )]),
    );
    let (status, headers, body) = h
        .get_route("/agents/e2e/c/alice/review", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.starts_with("text/html"))
    );
    let page = String::from_utf8(body).unwrap();
    assert!(page.contains(r#"const prefix = "/v1/plugins/web""#));
    assert!(page.contains("e2e/c/alice"));

    let (status, _, body) = h
        .get_route("/agents/e2e/c/alice/diff.json", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v, serde_json::to_value(sample_diff()).unwrap(), "unchanged");

    let (status, headers, body) = h
        .get_route(
            "/agents/e2e/c/alice/file?path=src/lib.rs",
            "/v1/plugins/web",
        )
        .await;
    assert_eq!(status, 200);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "text/plain; charset=utf-8")
    );
    assert_eq!(body, b"fn a() {}\nfn b() { c() }\n");
    let (status, _, body) = h
        .get_route("/agents/e2e/c/alice/file?path=nope", "/v1/plugins/web")
        .await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (404, "no such path")
    );

    // the daemon's refusals cross as they are: bob is hidden here, carol
    // has no workspace at the fake
    for path in ["review", "diff.json", "file?path=x", "events.json"] {
        let (status, _, _) = h
            .get_route(&format!("/agents/e2e/c/bob/{path}"), "/v1/plugins/web")
            .await;
        assert_eq!(status, 404, "{path}");
    }
    h.activate("e2e/c/carol", json!({})).await.unwrap();
    let (status, _, body) = h
        .get_route("/agents/e2e/c/carol/diff.json", "/v1/plugins/web")
        .await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (404, "no workspace for agent e2e/c/carol")
    );
}

#[tokio::test]
async fn events_json_carries_the_workspace_version_when_there_is_one() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate("e2e/c/carol", json!({})).await.unwrap();
    fake.set_workspace(
        ALICE,
        sample_diff(),
        BTreeMap::from([("src/lib.rs".to_string(), b"fn a() {}\n".to_vec())]),
    );
    let (status, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["workspace"]["head"], sample_diff().head);
    let fp = v["workspace"]["fingerprint"].as_str().unwrap().to_string();
    assert_eq!(fp.len(), 64);
    fake.set_workspace(
        ALICE,
        sample_diff(),
        BTreeMap::from([("src/lib.rs".to_string(), b"fn a() {}\nfn b() {}\n".to_vec())]),
    );
    let (_, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json", "/v1/plugins/web")
        .await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_ne!(
        v["workspace"]["fingerprint"], fp,
        "different bytes, different fingerprint"
    );
    // carol has no workspace at the fake: null, and the column still flows
    let (status, _, body) = h
        .get_route("/agents/e2e/c/carol/events.json", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(v["workspace"].is_null(), "{v}");
    assert_eq!(v["phase"], "pending");
    // one diff.json fetch is one refresh
    let (status, _, _) = h
        .get_route("/agents/e2e/c/alice/diff.json", "/v1/plugins/web")
        .await;
    assert_eq!(status, 200);
    let text = h.metrics().await;
    assert_eq!(
        metric(&text, "balerix_plugin_web_diff_refreshes_total", &[]),
        Some(1.0)
    );
    assert_eq!(
        metric(&text, "balerix_plugin_web_version_failures_total", &[]),
        Some(1.0)
    );
}

#[tokio::test]
async fn a_review_is_one_send_text_and_a_divider_in_the_column() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_workspace(ALICE, sample_diff(), BTreeMap::new());
    let review = json!({
        "head": sample_diff().head,
        "base_ref": "origin/main",
        "summary": "Looks fine.",
        "comments": [
            { "path": "src/lib.rs", "side": "new", "line": 2, "text": "+fn b() { c() }", "body": "Name this." }
        ]
    });
    let (status, body) = h
        .post_route("/agents/e2e/c/alice/review", "/v1/plugins/web", &review)
        .await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (200, "{}")
    );
    let expected = "Review against origin/main at 3f9c2a1 (1 comment)\n\nsrc/lib.rs line 2 (new):\n> +fn b() { c() }\nName this.\n\nOverall:\nLooks fine.";
    assert_eq!(
        fake.actions_for(ALICE),
        vec![PluginAction::SendText {
            text: expected.to_string(),
            submit: true
        }]
    );
    // the divider
    let (_, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json", "/v1/plugins/web")
        .await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    let last = v["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["name"], "review_sent");
    assert_eq!(last["summary"], "review sent (1 comment)");
    assert_eq!(last["payload"]["message"], expected);
    assert!(last["at"].as_u64().unwrap() > 1_700_000_000);
    // refusals: not enabled, a bad body, the caps, nothing to send
    let (status, _) = h
        .post_route("/agents/e2e/c/bob/review", "/v1/plugins/web", &review)
        .await;
    assert_eq!(status, 404);
    let (status, body) = h
        .post_route(
            "/agents/e2e/c/alice/review",
            "/v1/plugins/web",
            &json!({ "comments": [{ "path": "x" }] }),
        )
        .await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let (status, body) = h
        .post_route("/agents/e2e/c/alice/review", "/v1/plugins/web", &json!({}))
        .await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (400, "nothing to send")
    );
    let many: Vec<Value> = (0..201)
        .map(|i| json!({ "path": "a", "side": "new", "line": i, "text": "", "body": "b" }))
        .collect();
    let (status, body) = h
        .post_route(
            "/agents/e2e/c/alice/review",
            "/v1/plugins/web",
            &json!({ "comments": many }),
        )
        .await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (400, "comments: more than 200")
    );
    // Every field within its cap and the whole body under the 1 MiB
    // proxy limit, but 200 × (4095-byte path + 300-byte text) renders
    // past the message cap.
    let wide: Vec<Value> = (0..200)
        .map(|i| {
            json!({ "path": "d/".repeat(2047) + "f", "side": "new", "line": i,
                    "text": "+".repeat(300), "body": "b" })
        })
        .collect();
    let (status, body) = h
        .post_route(
            "/agents/e2e/c/alice/review",
            "/v1/plugins/web",
            &json!({ "comments": wide }),
        )
        .await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (400, "message: longer than 262144 bytes")
    );
    assert_eq!(
        fake.actions_for(ALICE).len(),
        1,
        "no refused review was sent"
    );
    // the daemon refusing the action: 502 with its message, nothing recorded
    fake.fail_actions(Some("f/c/a: tmux send-keys: no window"));
    let (status, body) = h
        .post_route("/agents/e2e/c/alice/review", "/v1/plugins/web", &review)
        .await;
    assert_eq!(status, 502);
    assert!(
        String::from_utf8_lossy(&body).contains("no window"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    fake.fail_actions(None);
    let text = h.metrics().await;
    assert_eq!(
        metric(
            &text,
            "balerix_plugin_web_reviews_total",
            &[("outcome", "sent")]
        ),
        Some(1.0)
    );
    assert_eq!(
        metric(
            &text,
            "balerix_plugin_web_reviews_total",
            &[("outcome", "failed")]
        ),
        Some(1.0)
    );
    assert_eq!(
        metric(&text, "balerix_plugin_web_review_comments_total", &[]),
        Some(1.0)
    );
    let (_, _, body) = h
        .get_route("/agents/e2e/c/alice/events.json?after=1", "/v1/plugins/web")
        .await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v["events"].as_array().unwrap().is_empty(),
        "a failed send adds no divider"
    );
}
