//! Kubernetes mode (Spec O §7): the sidecar link and the two ports over
//! it, the idle ports, and TLS serving.

pub mod idle;
pub mod link;
pub mod pty;
pub mod tls;

pub use idle::{NoFiles, NoPool};
pub use link::{ATTACH_WAIT, LINK_CALL_TIMEOUT, LinkError, LinkHub};
pub use tls::{TlsServer, client_config, no_roots, serve_tls};

/// `FleetRecord.owner` of a fleet the operator applied (§7.3).
pub const KUBERNETES_OWNER: &str = "kubernetes";
