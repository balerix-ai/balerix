//! The flow plugin (plugins spec §8.1, §17): a per-agent state machine over
//! hook events. `config` parses and compiles an agent's `plugins.flow`
//! block, `machine` is the pure step, `plugin` is the `Plugin` impl that
//! owns the agents, the KV-backed state and the metrics.

pub mod config;
pub mod machine;
pub mod plugin;

pub use config::{Compiled, ConfigError, FlowConfig, compile};
pub use machine::{Step, step};
pub use plugin::FlowPlugin;
