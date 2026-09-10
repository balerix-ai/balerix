//! The web plugin (plugins spec §18.5): agents' terminals in a browser.
//! `config` parses the per-agent block, `state` is the cache `fleets/watch`
//! feeds, `routes` serves the pages and bridges the terminal, `plugin` is
//! the `Plugin` impl.

pub mod config;
pub mod plugin;
pub mod review;
pub mod routes;
pub mod state;

pub use config::{ConfigError, WebConfig, parse};
pub use plugin::{Shared, WebPlugin};
pub use review::{
    Comment, MAX_BODY_BYTES, MAX_COMMENTS, MAX_TEXT_BYTES, ReviewBody, Side, render_message,
    validate,
};
pub use state::{
    AgentRow, Cache, EVENT_BUFFER, Entry, Events, PAYLOAD_LIMIT, cut_payload, now, rows_of,
    summarize,
};
