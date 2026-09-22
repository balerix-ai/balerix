//! The metric families every chat plugin registers (Spec G §10, Spec J
//! §8), through the SDK's prefixing registry so the names come out as
//! `balerix_plugin_<name>_…`.

use balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec};
use balerix_plugin_sdk::{Metrics, SdkError};

/// The counters shared by every chat plugin. Label values are the
/// plugin's own; Spec G §10 and Spec J §8 name the conventional ones.
#[derive(Debug, Clone)]
pub struct Shared {
    /// `events_dropped_total`: commands dropped because the queue was full.
    pub events_dropped: IntCounter,
    /// `messages_sent_total{kind}`: messages posted to the channel.
    pub messages_sent: IntCounterVec,
    /// `inbound_total{outcome}`: channel messages seen, by what became of them.
    pub inbound: IntCounterVec,
    /// `errors_total{kind}`: channel and daemon failures.
    pub errors: IntCounterVec,
    /// `answers_mismatched_total`: answers Claude recorded differently from
    /// what the channel chose (Spec J-6).
    pub answers_mismatched: IntCounter,
}

impl Shared {
    /// Registers every shared family through `metrics`, so their names come
    /// out prefixed `balerix_plugin_<name>_…`.
    pub fn new(metrics: &Metrics) -> Result<Self, SdkError> {
        Ok(Self {
            events_dropped: metrics.int_counter(
                "events_dropped_total",
                "Commands dropped because the queue was full",
            )?,
            messages_sent: metrics.int_counter_vec(
                "messages_sent_total",
                "Messages sent to the channel, by kind",
                &["kind"],
            )?,
            inbound: metrics.int_counter_vec(
                "inbound_total",
                "Channel messages seen, by what became of them",
                &["outcome"],
            )?,
            errors: metrics.int_counter_vec("errors_total", "Failures, by kind", &["kind"])?,
            answers_mismatched: metrics.int_counter(
                "answers_mismatched_total",
                "Answers Claude recorded differently from what was chosen",
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_carries_the_plugin_prefix() {
        let metrics = Metrics::new("chat");
        let s = Shared::new(&metrics).unwrap();
        s.events_dropped.inc();
        s.messages_sent.with_label_values(&["event"]).inc();
        s.inbound.with_label_values(&["routed"]).inc();
        s.errors.with_label_values(&["send"]).inc();
        s.answers_mismatched.inc();
        let text = metrics.render().unwrap();
        for family in [
            "events_dropped_total",
            "messages_sent_total",
            "inbound_total",
            "errors_total",
            "answers_mismatched_total",
        ] {
            assert!(
                text.contains(&format!("balerix_plugin_chat_{family}")),
                "{family}: {text}"
            );
        }
    }
}
