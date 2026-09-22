//! `balerix-plugin-common` (Spec K): the channel-independent half of a
//! balerix chat plugin. Rendering, the `AskUserQuestion` answer flow,
//! config helpers, a drop-oldest command queue, shared metric families
//! and phase diffing — everything the matrix plugin needed that knew
//! nothing about Matrix. See README.md for how to build a plugin on it.

/// Config helpers: a redacted `Secret`, a path-first `ConfigError`, the
/// deserializer that attaches serde's path, and the curated event filter.
pub mod config;

/// The metric families every chat plugin registers.
pub mod metrics;

/// The bounded drop-oldest command queue and the health cell an actor
/// publishes through.
pub mod queue;

/// `AskUserQuestion` (Spec J §6): parse the dialog, match a thread reply
/// to its options, plan the keystrokes.
pub mod question;

/// The question each agent is waiting on, mirrored to KV (Spec J §7.1).
pub mod pending;

/// Agent phase changes derived from `fleets/watch`, and the loop that
/// feeds them to a sink for as long as the plugin runs.
pub mod phases;

/// Every message body the plugin sends (Spec G §8), as pure functions.
pub mod render;

/// A code review as one message for the agent (Spec C §4.4, Spec K-7).
pub mod review;

/// The answer flow of Spec J §7.2 and §7.3 as pure decisions (Spec K §4).
pub mod answer;
