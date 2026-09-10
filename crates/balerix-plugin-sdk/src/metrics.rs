//! A Prometheus registry that prefixes every family with
//! `balerix_plugin_<name>_` (plugins spec §9, §17.4): the daemon drops a
//! whole scrape when one family lacks the prefix, and a hand-formatted
//! body is how a plugin author trips that rule. Register through this
//! type and the rule cannot be broken.

use std::fmt;

pub use prometheus::{IntCounter, IntCounterVec, IntGauge, IntGaugeVec};
use prometheus::{Opts, Registry, TextEncoder};

use crate::SdkError;

/// A registry whose every family is `balerix_plugin_<name>_<short>`.
pub struct Metrics {
    plugin_name: String,
    registry: Registry,
}

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metrics")
            .field("plugin_name", &self.plugin_name)
            .finish()
    }
}

impl Metrics {
    pub fn new(plugin_name: &str) -> Self {
        Self {
            plugin_name: plugin_name.to_string(),
            registry: Registry::new(),
        }
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// The full family name for a short one: `state` →
    /// `balerix_plugin_flow_state`.
    pub fn family(&self, short: &str) -> String {
        format!("balerix_plugin_{}_{short}", self.plugin_name)
    }

    fn opts(&self, short: &str, help: &str) -> Result<Opts, SdkError> {
        if short.is_empty() {
            return Err(SdkError::Metrics("family name is empty".into()));
        }
        Ok(Opts::new(self.family(short), help))
    }

    fn register<C: prometheus::core::Collector + Clone + 'static>(
        &self,
        c: C,
    ) -> Result<C, SdkError> {
        self.registry
            .register(Box::new(c.clone()))
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        Ok(c)
    }

    pub fn int_counter(&self, short: &str, help: &str) -> Result<IntCounter, SdkError> {
        let c = IntCounter::with_opts(self.opts(short, help)?)
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        self.register(c)
    }

    pub fn int_counter_vec(
        &self,
        short: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<IntCounterVec, SdkError> {
        let c = IntCounterVec::new(self.opts(short, help)?, labels)
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        self.register(c)
    }

    pub fn int_gauge(&self, short: &str, help: &str) -> Result<IntGauge, SdkError> {
        let g = IntGauge::with_opts(self.opts(short, help)?)
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        self.register(g)
    }

    pub fn int_gauge_vec(
        &self,
        short: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<IntGaugeVec, SdkError> {
        let g = IntGaugeVec::new(self.opts(short, help)?, labels)
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        self.register(g)
    }

    /// Prometheus text (`text/plain; version=0.0.4`), every family
    /// prefixed. Empty for an empty registry.
    pub fn render(&self) -> Result<String, SdkError> {
        TextEncoder::new()
            .encode_to_string(&self.registry.gather())
            .map_err(|e| SdkError::Metrics(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_is_prefixed_with_the_plugin_name() {
        let m = Metrics::new("flow");
        assert_eq!(m.plugin_name(), "flow");
        assert_eq!(m.family("state"), "balerix_plugin_flow_state");
        let g = m
            .int_gauge_vec("state", "current state", &["agent", "state"])
            .unwrap();
        g.with_label_values(&["a", "working"]).set(1);
        let c = m
            .int_counter_vec("transitions_total", "transitions", &["from", "to"])
            .unwrap();
        c.with_label_values(&["working", "review"]).inc();
        let up = m.int_gauge("up", "1 while serving").unwrap();
        up.set(1);
        let calls = m.int_counter("calls_total", "calls").unwrap();
        calls.inc();
        let text = m.render().unwrap();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let family = match line.strip_prefix('#') {
                Some(rest) => rest.split_whitespace().nth(1).unwrap(),
                None => line.split(['{', ' ']).next().unwrap(),
            };
            assert!(
                family.starts_with("balerix_plugin_flow_"),
                "unprefixed family in: {line}"
            );
        }
        assert!(text.contains("# HELP balerix_plugin_flow_state current state\n"));
        assert!(text.contains("# TYPE balerix_plugin_flow_state gauge\n"));
        assert!(text.contains("balerix_plugin_flow_state{agent=\"a\",state=\"working\"} 1\n"));
        assert!(
            text.contains(
                "balerix_plugin_flow_transitions_total{from=\"working\",to=\"review\"} 1\n"
            )
        );
        assert!(text.contains("balerix_plugin_flow_up 1\n"));
        assert!(text.contains("balerix_plugin_flow_calls_total 1\n"));
    }

    #[test]
    fn an_empty_registry_renders_nothing_and_duplicates_are_errors() {
        let m = Metrics::new("web");
        assert_eq!(m.render().unwrap(), "");
        m.int_gauge("up", "x").unwrap();
        let err = m.int_gauge("up", "x").unwrap_err();
        assert!(err.to_string().starts_with("metrics: "), "{err}");
        let err = m.int_gauge("", "x").unwrap_err();
        assert_eq!(err.to_string(), "metrics: family name is empty");
        let err = m.int_gauge("Bad-Name", "x").unwrap_err();
        assert!(err.to_string().starts_with("metrics: "), "{err}");
    }

    #[test]
    fn debug_shows_the_name_only() {
        let m = Metrics::new("flow");
        assert_eq!(format!("{m:?}"), "Metrics { plugin_name: \"flow\" }");
    }
}
