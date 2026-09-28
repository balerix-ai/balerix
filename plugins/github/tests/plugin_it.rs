//! The real `Plugin` over the wire: `Harness` drives `hello`/`activate`/
//! `observe`, a signed webhook hits the listener, `FakeHost` records the
//! apply and the `send_text`, `FakePort` the comments and reactions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use balerix_api::PluginAction;
use balerix_plugin_github::actor::{Actor, Command, Counters, Health, Queue};
use balerix_plugin_github::config::DaemonConfig;
use balerix_plugin_github::github::Permission;
use balerix_plugin_github::github::fake::{Call, FakePort};
use balerix_plugin_github::webhook::Listener;
use balerix_plugin_github::{GitHubPlugin, Launcher};
use balerix_plugin_sdk::testing::{FakeHost, Harness, event};
use balerix_plugin_sdk::{Host, Metrics};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;

struct TestLauncher {
    host: Host,
    port: FakePort,
    counters: Counters,
    health: Health,
    listen: Arc<std::sync::Mutex<Option<String>>>,
}

impl Launcher for TestLauncher {
    async fn launch(&self, config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String> {
        let mut actor = Actor::new(
            self.host.clone(),
            self.port.clone(),
            self.counters.clone(),
            self.health.clone(),
            "balerix".into(),
        );
        actor.load().await;
        tokio::spawn(actor.run(queue.clone()));
        let listener = tokio::net::TcpListener::bind(config.listen)
            .await
            .map_err(|e| e.to_string())?;
        *self.listen.lock().unwrap() = Some(listener.local_addr().unwrap().to_string());
        let q = queue.clone();
        let l = Listener::new(
            config.webhook_secret.clone(),
            move |ev| q.push(Command::Webhook(ev)),
            self.counters.webhooks.clone(),
        );
        tokio::spawn(balerix_plugin_github::webhook::serve(listener, l.router()));
        Ok(())
    }
}

async fn eventually(label: &str, mut done: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for {label}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!(
        "sha256={}",
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mention_starts_a_session_and_the_first_prompt_reaches_the_agent() {
    let config = json!({ "appId": 1, "privateKey": "k", "webhookSecret": "s3cret", "listen": "127.0.0.1:0" });
    let fake = Arc::new(FakeHost::start("tok", config, Vec::new()).await);
    let env = fake.env("github", std::path::Path::new("scratch"));
    let host = Host::new(env.clone()).unwrap();
    let metrics = Metrics::new("github");
    let counters = Counters::new(&metrics).unwrap();
    let health = Health::new();
    let queue = Queue::new(counters.events_dropped.clone());
    let port = FakePort::new("balerix");
    port.set_permission("acme/api", "alice", Permission::Write);
    port.set_file(
        "acme/api",
        "main",
        ".balerix.yaml",
        "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo:\n    repo: acme/api\n",
    );
    let listen_cell = Arc::new(std::sync::Mutex::new(None));
    let launcher = TestLauncher {
        host,
        port: port.clone(),
        counters: counters.clone(),
        health: health.clone(),
        listen: listen_cell.clone(),
    };
    let plugin = GitHubPlugin::new(metrics, health, queue.clone(), launcher);
    let h = Harness::start(&env, plugin).await;
    let listen = listen_cell.lock().unwrap().clone().unwrap();

    let payload = json!({ "action": "created", "repository": { "full_name": "acme/api" }, "installation": { "id": 7 }, "issue": { "number": 12 }, "comment": { "id": 5, "body": "@balerix take a look", "user": { "login": "alice", "type": "User" } } });
    let body = serde_json::to_vec(&payload).unwrap();
    let resp = reqwest::Client::new()
        .post(format!("http://{listen}/webhook"))
        .header("X-Hub-Signature-256", sign("s3cret", &body))
        .header("X-GitHub-Delivery", "d1")
        .header("X-GitHub-Event", "issue_comment")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let f = fake.clone();
    eventually("the apply", || !f.applied_fleets().is_empty()).await;
    assert_eq!(fake.applied_fleets()[0].0, "gh-acme-api");
    let p = port.clone();
    eventually("the status comment", || {
        p.calls().iter().any(|c| matches!(c, Call::Comment { .. }))
    })
    .await;

    h.activate(
        "gh-acme-api/repo/issue-12",
        json!({ "kind": "issue", "number": 12 }),
    )
    .await
    .unwrap();
    let mut start = event(
        "gh-acme-api/repo/issue-12",
        "SessionStart",
        json!({ "source": "startup" }),
    );
    start.session_id = Some("s1".into());
    h.observe(vec![start]).await;
    let f = fake.clone();
    eventually("the first prompt", || {
        !f.actions_for("gh-acme-api/repo/issue-12").is_empty()
    })
    .await;
    assert!(matches!(
        fake.actions_for("gh-acme-api/repo/issue-12")[0],
        PluginAction::SendText { submit: true, .. }
    ));
    assert!(h.health().await.is_ok());
    assert!(
        h.metrics()
            .await
            .contains("balerix_plugin_github_webhooks_total")
    );
}
