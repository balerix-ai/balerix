//! The matrix plugin (Spec G): a room per crew, a thread per agent
//! session, and a thread reply back to that agent as `send_text` — or, while
//! Claude has an `AskUserQuestion` dialog open, as the paced `send_keys`
//! that answers it (Spec J).

pub mod actor;
pub mod client;
pub mod config;
pub mod matrix;
pub mod plugin;
pub mod routing;
pub mod session;

pub use balerix_plugin_common::{pending, phases, question, render};
pub use client::MatrixLauncher;
pub use plugin::{Launcher, MatrixPlugin};
