//! The SDK surface (Spec M §3). Everything here validates and enqueues;
//! nothing here talks to GitHub. Starting the actor is a `Launcher`, so
//! this file is testable without a GitHub client.

use std::future::Future;
use std::sync::Arc;

use balerix_api::HookEvent;
use balerix_plugin_sdk::{Metrics, Plugin};
use serde_json::Value;

use crate::actor::{Command, Health, Queue};
use crate::config::{DaemonConfig, parse_agent, parse_daemon};

/// Proves the App and starts the actor, the webhook listener and the
/// ticker.
/// `Err(message)` fails `configure`, so `serve` returns and the process
/// exits 1 with the message in the plugin's log (Spec M §11).
pub trait Launcher: Send + Sync + 'static {
    fn launch(
        &self,
        config: DaemonConfig,
        queue: Arc<Queue>,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

pub struct GitHubPlugin<L: Launcher> {
    metrics: Metrics,
    health: Health,
    queue: Arc<Queue>,
    launcher: L,
}

impl<L: Launcher> std::fmt::Debug for GitHubPlugin<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubPlugin")
            .field("queued", &self.queue.len())
            .finish()
    }
}

impl<L: Launcher> GitHubPlugin<L> {
    /// The counters are deliberately not held here: `Metrics` is the
    /// prometheus registry, and a registered collector is kept alive by the
    /// registry's own handle to it, so the actor and the listener can own the
    /// only clones and this still renders every family.
    pub fn new(metrics: Metrics, health: Health, queue: Arc<Queue>, launcher: L) -> Self {
        Self {
            metrics,
            health,
            queue,
            launcher,
        }
    }

    pub fn queue(&self) -> Arc<Queue> {
        self.queue.clone()
    }
}

impl<L: Launcher> Plugin for GitHubPlugin<L> {
    /// Parse, prove the App, start the actor, then hand it the
    /// config. Nothing is queued unless all three succeed.
    async fn configure(&self, config: Value) -> Result<(), String> {
        let config = parse_daemon(&config).map_err(|e| e.to_string())?;
        self.launcher
            .launch(config.clone(), self.queue.clone())
            .await?;
        self.queue.push(Command::Configure(config));
        Ok(())
    }

    /// Validated here rather than in the actor, so a bad block fails the
    /// operator's `up` instead of failing silently later.
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let config = parse_agent(&config).map_err(|e| e.to_string())?;
        self.queue.push(Command::Activate {
            agent: agent.to_string(),
            config,
        });
        Ok(())
    }

    async fn deactivate(&self, agent: &str) {
        self.queue.push(Command::Deactivate {
            agent: agent.to_string(),
        });
    }

    /// Enqueue and return: this is a daemon-to-plugin call and must never
    /// wait on GitHub (Spec M §3).
    async fn observe(&self, events: Vec<HookEvent>) {
        self.queue.push(Command::Events(events));
    }

    async fn health(&self) -> Result<(), String> {
        self.health.get()
    }

    fn metrics(&self) -> Option<&Metrics> {
        Some(&self.metrics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::Counters;
    use balerix_plugin_sdk::testing::{FakeHost, Harness, event};
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeLauncher {
        launched: Arc<Mutex<Vec<DaemonConfig>>>,
        reject: Option<String>,
    }

    impl Launcher for FakeLauncher {
        async fn launch(&self, config: DaemonConfig, _queue: Arc<Queue>) -> Result<(), String> {
            self.launched
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(config);
            match &self.reject {
                Some(m) => Err(m.clone()),
                None => Ok(()),
            }
        }
    }

    fn daemon_json() -> serde_json::Value {
        json!({
            "appId": 1,
            "privateKey": "k",
            "webhookSecret": "s"
        })
    }

    fn plugin(launcher: FakeLauncher) -> (GitHubPlugin<FakeLauncher>, Arc<Queue>, Health) {
        let metrics = Metrics::new("github");
        let counters = Counters::new(&metrics).unwrap();
        let health = Health::new();
        let queue = Queue::new(counters.events_dropped.clone());
        let p = GitHubPlugin::new(metrics, health.clone(), queue.clone(), launcher);
        (p, queue, health)
    }

    #[tokio::test]
    async fn configure_launches_once_and_queues_the_config() {
        let launched = Arc::new(Mutex::new(Vec::new()));
        let (p, queue, _health) = plugin(FakeLauncher {
            launched: launched.clone(),
            reject: None,
        });
        p.configure(daemon_json()).await.unwrap();
        assert_eq!(launched.lock().unwrap().len(), 1);
        assert!(matches!(queue.pop().await, Command::Configure(_)));
    }

    #[tokio::test]
    async fn a_bad_daemon_config_or_a_failed_launch_rejects_configure() {
        let (p, queue, _health) = plugin(FakeLauncher::default());
        let err = p.configure(json!({ "appId": 1 })).await.unwrap_err();
        assert!(err.starts_with("privateKey: "), "{err}");
        assert!(queue.is_empty(), "nothing is queued on a bad config");

        let (p, queue, _health) = plugin(FakeLauncher {
            launched: Arc::new(Mutex::new(Vec::new())),
            reject: Some("proving the App: auth: 401".into()),
        });
        assert_eq!(
            p.configure(daemon_json()).await.unwrap_err(),
            "proving the App: auth: 401"
        );
        assert!(queue.is_empty(), "nothing is queued on a failed launch");
    }

    #[tokio::test]
    async fn activate_validates_and_enqueues_and_a_bad_block_is_rejected() {
        let (p, queue, _health) = plugin(FakeLauncher::default());
        p.activate("f/c/issue-12", json!({ "kind": "issue", "number": 12 }))
            .await
            .unwrap();
        match queue.pop().await {
            Command::Activate { agent, config } => {
                assert_eq!(agent, "f/c/issue-12");
                assert_eq!(config.number, Some(12));
                assert!(config.wants("Stop"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            p.activate(
                "f/c/issue-12",
                json!({ "kind": "issue", "number": 12, "events": ["Nope"] })
            )
            .await
            .unwrap_err(),
            "events[0]: unknown event \"Nope\""
        );
        assert!(queue.is_empty(), "a rejected activate queues nothing");
    }

    #[tokio::test]
    async fn observe_and_deactivate_enqueue_and_health_follows_the_cell() {
        let (p, queue, health) = plugin(FakeLauncher::default());
        p.observe(vec![event("f/c/issue-12", "Stop", json!({}))])
            .await;
        assert!(matches!(queue.pop().await, Command::Events(e) if e.len() == 1));
        p.deactivate("f/c/issue-12").await;
        assert!(matches!(queue.pop().await, Command::Deactivate { .. }));

        assert_eq!(p.health().await, Ok(()));
        health.fail("webhook listener: address in use".into());
        assert_eq!(
            p.health().await,
            Err("webhook listener: address in use".into())
        );
    }

    /// The whole surface over the real §4.2 wire format.
    #[tokio::test]
    async fn the_wire_surface_works_end_to_end() {
        let fake = FakeHost::start("tok", daemon_json(), Vec::new()).await;
        let env = fake.env("github", std::path::Path::new("scratch"));
        let (p, queue, _health) = plugin(FakeLauncher::default());
        let h = Harness::start(&env, p).await;

        assert!(
            matches!(queue.pop().await, Command::Configure(_)),
            "hello configured it"
        );
        h.activate("f/c/issue-12", json!({ "kind": "issue", "number": 12 }))
            .await
            .unwrap();
        assert!(matches!(queue.pop().await, Command::Activate { .. }));
        assert_eq!(
            h.activate("f/c/issue-12", json!({})).await.unwrap_err(),
            "kind: missing"
        );
        assert_eq!(
            h.activate("f/c/issue-12", json!({ "kind": "issue" }))
                .await
                .unwrap_err(),
            "number: missing"
        );
        h.observe(vec![event("f/c/issue-12", "Stop", json!({}))])
            .await;
        assert!(matches!(queue.pop().await, Command::Events(_)));
        assert!(h.health().await.is_ok());
        assert!(
            h.metrics().await.contains("balerix_plugin_github_"),
            "the registry is served"
        );
    }
}
