//! The `Plugin` impl (plugins spec §18.5): a per-agent `enabled` flag, the
//! cache `fleets/watch` feeds, and the two terminal metrics. The routes
//! (`crate::routes`) share `Shared` with it.

use std::sync::Arc;

use balerix_api::HookEvent;
use balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge};
use balerix_plugin_sdk::{Host, Metrics, Plugin, SdkError};
use serde_json::Value;

use crate::config::parse;
use crate::state::Cache;

/// What the plugin and its routes both hold.
pub struct Shared {
    pub host: Host,
    pub cache: Cache,
    pub terminals_open: IntGauge,
    pub terminals_total: IntCounter,
    pub reviews_total: IntCounterVec,
    pub review_comments_total: IntCounter,
    pub events_buffered_total: IntCounter,
    pub diff_refreshes_total: IntCounter,
    pub version_failures_total: IntCounter,
}

pub struct WebPlugin {
    shared: Arc<Shared>,
    metrics: Metrics,
}

impl std::fmt::Debug for WebPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebPlugin")
            .field("host", &self.shared.host)
            .finish()
    }
}

impl WebPlugin {
    pub fn new(host: Host) -> Result<Self, SdkError> {
        let metrics = Metrics::new(&host.env().name);
        let terminals_open = metrics.int_gauge("terminals_open", "Browser terminals open now")?;
        let terminals_total =
            metrics.int_counter("terminals_total", "Browser terminals opened since start")?;
        let reviews_total = metrics.int_counter_vec(
            "reviews_total",
            "Reviews submitted, by outcome",
            &["outcome"],
        )?;
        let review_comments_total =
            metrics.int_counter("review_comments_total", "Line comments sent in reviews")?;
        let events_buffered_total = metrics.int_counter(
            "events_buffered_total",
            "Hook events appended to an agent's activity buffer",
        )?;
        let diff_refreshes_total = metrics.int_counter(
            "diff_refreshes_total",
            "diff.json fetches, one per refresh of a review page's diff",
        )?;
        let version_failures_total = metrics.int_counter(
            "version_failures_total",
            "workspace_version calls that failed during an events.json poll",
        )?;
        Ok(Self {
            shared: Arc::new(Shared {
                host,
                cache: Cache::new(),
                terminals_open,
                terminals_total,
                reviews_total,
                review_comments_total,
                events_buffered_total,
                diff_refreshes_total,
                version_failures_total,
            }),
            metrics,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Feeds the cache from `fleets/watch` for as long as the task lives.
    /// Started before `serve`: the route needs the token and the `fleets`
    /// capability, not readiness, and the first frame is the current list.
    pub fn start_watch(&self) -> tokio::task::JoinHandle<()> {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let mut watch = shared.host.watch_fleets();
            loop {
                shared.cache.set_fleets(watch.next().await);
            }
        })
    }
}

impl Plugin for WebPlugin {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let cfg = parse(&config).map_err(|e| e.to_string())?;
        self.shared.cache.set_enabled(agent, cfg.enabled);
        eprintln!(
            "web: {agent} {}",
            if cfg.enabled { "listed" } else { "hidden" }
        );
        Ok(())
    }

    async fn deactivate(&self, agent: &str) {
        self.shared.cache.remove(agent);
    }

    /// Every observed event of an enabled agent joins its activity buffer
    /// (Spec C §4.1); events of hidden agents are dropped.
    async fn observe(&self, events: Vec<HookEvent>) {
        for e in events {
            let summary = crate::state::summarize(&e.name, &e.payload);
            if self
                .shared
                .cache
                .push_event(&e.agent, e.received_at, &e.name, summary, e.payload)
                .is_some()
            {
                self.shared.events_buffered_total.inc();
            }
        }
    }

    fn metrics(&self) -> Option<&Metrics> {
        Some(&self.metrics)
    }

    fn routes(&self) -> Option<axum::Router> {
        Some(crate::routes::router(self.shared.clone()))
    }
}
