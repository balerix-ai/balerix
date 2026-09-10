//! The matrix plugin (Spec G): a room per crew, a thread per agent
//! session, and a thread reply back to that agent as `send_text`.

pub mod actor;
pub mod client;
pub mod config;
pub mod matrix;
pub mod plugin;
pub mod render;
pub mod routing;
pub mod session;

pub use client::MatrixLauncher;
pub use plugin::{Launcher, MatrixPlugin};
