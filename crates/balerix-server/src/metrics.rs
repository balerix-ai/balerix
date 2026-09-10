//! Prometheus series (architecture spec §8 table). Gauges are recomputed
//! from snapshots on every `/metrics` scrape; counters and histograms are
//! bumped where the event happens.

use std::sync::Arc;

use balerix_core::{AgentId, FleetRecord};
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};

use crate::plugins::wire_label as label;

#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Registry,
    fleets: IntGaugeVec,
    agents: IntGaugeVec,
    reconcile_duration: HistogramVec,
    reconcile_errors: IntCounterVec,
    agent_restarts: IntCounterVec,
    hook_events: IntCounterVec,
    hook_handle_duration: HistogramVec,
    hook_actions: IntCounterVec,
    plugin_events: IntCounterVec,
    plugin_intercept_duration: HistogramVec,
    plugin_intercept_failures: IntCounterVec,
    plugin_events_dropped: IntCounterVec,
    plugin_actions: IntCounterVec,
    plugin_metrics_scrape_failures: IntCounterVec,
    proxy_requests: IntCounterVec,
}

impl Metrics {
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();
        let fleets = IntGaugeVec::new(Opts::new("balerix_fleets", "Fleets by phase"), &["phase"])?;
        let agents = IntGaugeVec::new(
            Opts::new("balerix_agents", "Agents by fleet, crew and phase"),
            &["fleet", "crew", "phase"],
        )?;
        let reconcile_duration = HistogramVec::new(
            HistogramOpts::new(
                "balerix_reconcile_duration_seconds",
                "Reconcile pass duration",
            ),
            &["fleet"],
        )?;
        let reconcile_errors = IntCounterVec::new(
            Opts::new(
                "balerix_reconcile_errors_total",
                "Passes with a failed step or observe",
            ),
            &["fleet"],
        )?;
        let agent_restarts = IntCounterVec::new(
            Opts::new(
                "balerix_agent_restarts_total",
                "Agent exits noted by the reconciler",
            ),
            &["fleet", "crew", "agent"],
        )?;
        let hook_events = IntCounterVec::new(
            Opts::new("balerix_hook_events_total", "Hook events accepted"),
            &["fleet", "crew", "agent", "event"],
        )?;
        let hook_handle_duration = HistogramVec::new(
            HistogramOpts::new("balerix_hook_handle_duration_seconds", "Handler latency"),
            &["event"],
        )?;
        let hook_actions = IntCounterVec::new(
            Opts::new(
                "balerix_hook_actions_total",
                "Actions executed for hook events",
            ),
            &["fleet", "crew", "agent", "action"],
        )?;
        let plugin_events = IntCounterVec::new(
            Opts::new(
                "balerix_plugin_events_total",
                "Hook events handed to plugins",
            ),
            &["plugin", "event", "mode"],
        )?;
        let plugin_intercept_duration = HistogramVec::new(
            HistogramOpts::new(
                "balerix_plugin_intercept_duration_seconds",
                "Interceptor call latency",
            ),
            &["plugin", "event"],
        )?;
        let plugin_intercept_failures = IntCounterVec::new(
            Opts::new(
                "balerix_plugin_intercept_failures_total",
                "Interceptor calls skipped",
            ),
            &["plugin", "reason"],
        )?;
        let plugin_events_dropped = IntCounterVec::new(
            Opts::new(
                "balerix_plugin_events_dropped_total",
                "Observer events dropped on overflow",
            ),
            &["plugin"],
        )?;
        let plugin_actions = IntCounterVec::new(
            Opts::new(
                "balerix_plugin_actions_total",
                "Actions requested by plugins",
            ),
            &["plugin", "action"],
        )?;
        let plugin_metrics_scrape_failures = IntCounterVec::new(
            Opts::new(
                "balerix_plugin_metrics_scrape_failures_total",
                "Plugin /v1/metrics bodies dropped",
            ),
            &["plugin"],
        )?;
        let proxy_requests = IntCounterVec::new(
            Opts::new(
                "balerix_plugin_proxy_requests_total",
                "Requests proxied to plugin routes, by response status",
            ),
            &["plugin", "status"],
        )?;
        for c in [
            Box::new(fleets.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(agents.clone()),
            Box::new(reconcile_duration.clone()),
            Box::new(reconcile_errors.clone()),
            Box::new(agent_restarts.clone()),
            Box::new(hook_events.clone()),
            Box::new(hook_handle_duration.clone()),
            Box::new(hook_actions.clone()),
            Box::new(plugin_events.clone()),
            Box::new(plugin_intercept_duration.clone()),
            Box::new(plugin_intercept_failures.clone()),
            Box::new(plugin_events_dropped.clone()),
            Box::new(plugin_actions.clone()),
            Box::new(plugin_metrics_scrape_failures.clone()),
            Box::new(proxy_requests.clone()),
        ] {
            registry.register(c)?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                registry,
                fleets,
                agents,
                reconcile_duration,
                reconcile_errors,
                agent_restarts,
                hook_events,
                hook_handle_duration,
                hook_actions,
                plugin_events,
                plugin_intercept_duration,
                plugin_intercept_failures,
                plugin_events_dropped,
                plugin_actions,
                plugin_metrics_scrape_failures,
                proxy_requests,
            }),
        })
    }

    pub fn encode(&self) -> String {
        let mut buf = Vec::new();
        let _ = TextEncoder::new().encode(&self.inner.registry.gather(), &mut buf);
        String::from_utf8(buf).unwrap_or_default()
    }

    pub fn set_gauges(&self, records: &[FleetRecord]) {
        self.inner.fleets.reset();
        self.inner.agents.reset();
        for r in records {
            self.inner
                .fleets
                .with_label_values(&[&label(r.status.phase)])
                .inc();
            for (id, a) in &r.status.agents {
                let Ok(id) = id.parse::<AgentId>() else {
                    continue;
                };
                self.inner
                    .agents
                    .with_label_values(&[id.fleet.as_str(), id.crew.as_str(), &label(a.phase)])
                    .inc();
            }
        }
    }

    pub fn reconcile(&self, fleet: &str, secs: f64, ok: bool) {
        self.inner
            .reconcile_duration
            .with_label_values(&[fleet])
            .observe(secs);
        if !ok {
            self.inner
                .reconcile_errors
                .with_label_values(&[fleet])
                .inc();
        }
    }

    pub fn restart(&self, id: &AgentId) {
        self.inner
            .agent_restarts
            .with_label_values(&[id.fleet.as_str(), id.crew.as_str(), id.agent.as_str()])
            .inc();
    }

    pub fn hook_event(&self, id: &AgentId, event: &str, secs: f64) {
        self.inner
            .hook_events
            .with_label_values(&[
                id.fleet.as_str(),
                id.crew.as_str(),
                id.agent.as_str(),
                event,
            ])
            .inc();
        self.inner
            .hook_handle_duration
            .with_label_values(&[event])
            .observe(secs);
    }

    pub fn plugin_event(&self, plugin: &str, event: &str, mode: &str) {
        self.inner
            .plugin_events
            .with_label_values(&[plugin, event, mode])
            .inc();
    }

    /// One interceptor call; `failure` is the `reason` label when it failed.
    pub fn intercept(&self, plugin: &str, event: &str, secs: f64, failure: Option<&str>) {
        self.inner
            .plugin_intercept_duration
            .with_label_values(&[plugin, event])
            .observe(secs);
        if let Some(reason) = failure {
            self.inner
                .plugin_intercept_failures
                .with_label_values(&[plugin, reason])
                .inc();
        }
    }

    pub fn events_dropped(&self, plugin: &str, n: u64) {
        self.inner
            .plugin_events_dropped
            .with_label_values(&[plugin])
            .inc_by(n);
    }

    pub fn plugin_action(&self, plugin: &str, action: &str) {
        self.inner
            .plugin_actions
            .with_label_values(&[plugin, action])
            .inc();
    }

    pub fn hook_action(&self, id: &AgentId, action: &str) {
        self.inner
            .hook_actions
            .with_label_values(&[
                id.fleet.as_str(),
                id.crew.as_str(),
                id.agent.as_str(),
                action,
            ])
            .inc();
    }

    /// Bumped only after `encode()` already rendered this scrape's body
    /// (§9), so a failure here is reflected in the *next* `/metrics`
    /// response, not this one.
    pub fn scrape_failure(&self, plugin: &str) {
        self.inner
            .plugin_metrics_scrape_failures
            .with_label_values(&[plugin])
            .inc();
    }

    /// One request through the plugin mount (§18.2), by answered status.
    pub fn proxy_request(&self, plugin: &str, status: u16) {
        self.inner
            .proxy_requests
            .with_label_values(&[plugin, &status.to_string()])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentPhase, FleetPhase, FleetSpec};
    use balerix_core::FleetRecord;
    use std::collections::BTreeMap;

    fn record(name: &str, phase: FleetPhase, agents: &[(&str, AgentPhase)]) -> FleetRecord {
        let mut r = FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
            ..Default::default()
        });
        r.status.phase = phase;
        for (id, p) in agents {
            r.status.entry(id).phase = *p;
        }
        r
    }

    #[test]
    fn gauges_follow_the_snapshots_and_counters_accumulate() {
        let m = Metrics::new().unwrap();
        m.set_gauges(&[
            record(
                "f",
                FleetPhase::Ready,
                &[("f/c/a", AgentPhase::Ready), ("f/c/b", AgentPhase::Ready)],
            ),
            record("g", FleetPhase::Degraded, &[("g/d/x", AgentPhase::Dead)]),
        ]);
        let text = m.encode();
        assert!(text.contains("balerix_fleets{phase=\"ready\"} 1"), "{text}");
        assert!(text.contains("balerix_fleets{phase=\"degraded\"} 1"));
        assert!(text.contains("balerix_agents{crew=\"c\",fleet=\"f\",phase=\"ready\"} 2"));
        assert!(text.contains("balerix_agents{crew=\"d\",fleet=\"g\",phase=\"dead\"} 1"));
        // a fleet that disappears takes its gauges with it
        m.set_gauges(&[record(
            "f",
            FleetPhase::Ready,
            &[("f/c/a", AgentPhase::Ready)],
        )]);
        let text = m.encode();
        assert!(!text.contains("fleet=\"g\""));
        assert!(text.contains("balerix_agents{crew=\"c\",fleet=\"f\",phase=\"ready\"} 1"));

        let id: AgentId = "f/c/a".parse().unwrap();
        m.reconcile("f", 0.25, true);
        m.reconcile("f", 0.5, false);
        m.restart(&id);
        m.hook_event(&id, "Notification", 0.001);
        m.hook_event(&id, "Notification", 0.002);
        let text = m.encode();
        assert!(text.contains("balerix_reconcile_errors_total{fleet=\"f\"} 1"));
        assert!(text.contains("balerix_reconcile_duration_seconds_count{fleet=\"f\"} 2"));
        assert!(
            text.contains("balerix_agent_restarts_total{agent=\"a\",crew=\"c\",fleet=\"f\"} 1")
        );
        assert!(text.contains(
            "balerix_hook_events_total{agent=\"a\",crew=\"c\",event=\"Notification\",fleet=\"f\"} 2"
        ));
        assert!(
            text.contains("balerix_hook_handle_duration_seconds_count{event=\"Notification\"} 2")
        );
        m.plugin_event("flow", "PreToolUse", "intercept");
        m.intercept("flow", "PreToolUse", 0.01, None);
        m.intercept("flow", "PreToolUse", 1.5, Some("timeout"));
        m.events_dropped("web", 3);
        m.plugin_action("flow", "send_text");
        m.hook_action(&id, "send_text");
        m.scrape_failure("web");
        m.proxy_request("web", 200);
        let text = m.encode();
        assert!(text.contains(
            "balerix_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"flow\"} 1"
        ));
        assert!(text.contains(
            "balerix_plugin_intercept_duration_seconds_count{event=\"PreToolUse\",plugin=\"flow\"} 2"
        ));
        assert!(text.contains(
            "balerix_plugin_intercept_failures_total{plugin=\"flow\",reason=\"timeout\"} 1"
        ));
        assert!(text.contains("balerix_plugin_events_dropped_total{plugin=\"web\"} 3"));
        assert!(
            text.contains("balerix_plugin_actions_total{action=\"send_text\",plugin=\"flow\"} 1")
        );
        assert!(text.contains(
            "balerix_hook_actions_total{action=\"send_text\",agent=\"a\",crew=\"c\",fleet=\"f\"} 1"
        ));
        assert!(text.contains("balerix_plugin_metrics_scrape_failures_total{plugin=\"web\"} 1"));
        assert!(
            text.contains("balerix_plugin_proxy_requests_total{plugin=\"web\",status=\"200\"} 1")
        );
        for name in [
            "balerix_fleets",
            "balerix_agents",
            "balerix_reconcile_duration_seconds",
            "balerix_reconcile_errors_total",
            "balerix_agent_restarts_total",
            "balerix_hook_events_total",
            "balerix_hook_handle_duration_seconds",
            "balerix_hook_actions_total",
            "balerix_plugin_events_total",
            "balerix_plugin_intercept_duration_seconds",
            "balerix_plugin_intercept_failures_total",
            "balerix_plugin_events_dropped_total",
            "balerix_plugin_actions_total",
            "balerix_plugin_metrics_scrape_failures_total",
            "balerix_plugin_proxy_requests_total",
        ] {
            assert!(text.contains(&format!("# TYPE {name} ")), "{name} missing");
        }
    }
}
