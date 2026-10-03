//! The balerix agent pod (Spec O §6, §7). `sidecar` materialises one agent
//! on the claim with `balerix-runtime`, drives it over a tmux socket the
//! `agent` container's server listens on, forwards Claude's hooks to the
//! Daemon and holds one outbound link to it. `run` is the agent
//! container's entrypoint: it starts that tmux server once the sidecar
//! says the agent is ready to launch.

pub mod attach;
pub mod bundle;
pub mod cli;
pub mod hooks;
pub mod link;
pub mod run;
pub mod sidecar;
pub mod tls;
