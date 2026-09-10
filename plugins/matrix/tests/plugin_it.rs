//! The whole plugin over the real §4.2 wire format, with a fake Matrix
//! port: `Harness` drives it exactly as the daemon would.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::PluginAction;
use balerix_plugin_matrix::MatrixPlugin;
use balerix_plugin_matrix::actor::{Actor, Command, Counters, Health, Queue};
use balerix_plugin_matrix::config::DaemonConfig;
use balerix_plugin_matrix::matrix::Inbound;
use balerix_plugin_matrix::matrix::fake::{Call, FakePort};
use balerix_plugin_matrix::plugin::Launcher;
use balerix_plugin_sdk::testing::{FakeHost, Harness, event};
use balerix_plugin_sdk::{Host, Metrics};
use serde_json::json;

/// Starts a real actor over a fake port, which is what the `matrix-sdk`
/// launcher does over a real one.
struct TestLauncher {
    host: Host,
    port: FakePort,
    counters: Counters,
    health: Health,
}

impl Launcher for TestLauncher {
    async fn launch(&self, _config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String> {
        let mut actor = Actor::new(
            self.host.clone(),
            self.port.clone(),
            self.counters.clone(),
            self.health.clone(),
        );
        actor.load().await;
        tokio::spawn(actor.run(queue));
        Ok(())
    }
}

async fn eventually(label: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {label}");
}

fn sends(calls: &[Call]) -> Vec<(Option<String>, String)> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::Send {
                thread_root, body, ..
            } => Some((thread_root.clone(), body.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_session_gets_a_thread_and_a_reply_in_it_reaches_the_agent() {
    let config = json!({
        "homeserver": "https://h",
        "userId": "@balerix:h",
        "password": "pw",
        "invite": ["@rahul:h"]
    });
    let fake = FakeHost::start("tok", config, Vec::new()).await;
    let env = fake.env("matrix", std::path::Path::new("scratch"));
    let host = Host::new(env.clone()).unwrap();

    let metrics = Metrics::new("matrix");
    let counters = Counters::new(&metrics).unwrap();
    let health = Health::new();
    let queue = Queue::new(counters.events_dropped.clone());
    let port = FakePort::new("@balerix:h");
    let plugin = MatrixPlugin::new(
        metrics,
        health,
        queue.clone(),
        TestLauncher {
            host,
            port: port.clone(),
            counters,
            health: Health::new(),
        },
    );

    let h = Harness::start(&env, plugin).await;
    h.activate("payments/backend/alice", json!({}))
        .await
        .unwrap();

    let mut start = event(
        "payments/backend/alice",
        "SessionStart",
        json!({ "source": "startup" }),
    );
    start.session_id = Some("s1".into());
    let mut note = event(
        "payments/backend/alice",
        "Notification",
        json!({ "message": "Claude needs your permission to use Bash" }),
    );
    note.session_id = Some("s1".into());
    h.observe(vec![start, note]).await;

    let p = port.clone();
    eventually("the room and two messages", move || {
        let calls = p.calls();
        calls.iter().any(|c| matches!(c, Call::CreateRoom { .. })) && sends(&calls).len() == 2
    })
    .await;

    let calls = port.calls();
    let s = sends(&calls);
    assert_eq!(s[0].0, None, "the root is not a thread reply");
    assert!(s[1].0.is_some(), "the notification is in the thread");
    assert!(s[1].1.contains("permission to use Bash"), "{}", s[1].1);

    let (room, root) = match (&calls[0], &calls[1]) {
        (Call::CreateRoom { .. }, Call::Send { room, .. }) => {
            (room.clone(), "$evt2:fake".to_string())
        }
        other => panic!("unexpected calls: {other:?}"),
    };

    queue.push(Command::Inbound(Inbound {
        room,
        event_id: "$reply:fake".into(),
        sender: "@rahul:h".into(),
        thread_root: Some(root),
        body: "run the tests".into(),
    }));

    let f = &fake;
    eventually("the send_text", || {
        !f.actions_for("payments/backend/alice").is_empty()
    })
    .await;
    assert_eq!(
        fake.actions_for("payments/backend/alice"),
        vec![PluginAction::SendText {
            text: "run the tests".into(),
            submit: true,
        }]
    );

    assert!(
        h.metrics()
            .await
            .contains("balerix_plugin_matrix_inbound_total"),
        "the outcome counter is published"
    );
}
