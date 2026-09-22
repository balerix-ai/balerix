//! `balerix-plugin-common` (Spec K): the channel-independent half of a
//! balerix chat plugin. Rendering, the `AskUserQuestion` answer flow,
//! config helpers, a drop-oldest command queue, shared metric families
//! and phase diffing — everything the matrix plugin needed that knew
//! nothing about Matrix. See README.md for how to build a plugin on it.

/// Config helpers: a redacted `Secret`, a path-first `ConfigError`, the
/// deserializer that attaches serde's path, and the curated event filter.
pub mod config;

/// The bounded drop-oldest command queue and the health cell an actor
/// publishes through.
pub mod queue;
