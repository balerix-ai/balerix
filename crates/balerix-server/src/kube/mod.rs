//! Kubernetes mode (Spec O §7): the sidecar link and the two ports over
//! it, and the idle ports. TLS serving joins this module in a later task.

pub mod idle;
pub mod link;

pub use idle::{NoFiles, NoPool};
pub use link::{LINK_CALL_TIMEOUT, LinkError, LinkHub};

/// `FleetRecord.owner` of a fleet the operator applied (§7.3).
pub const KUBERNETES_OWNER: &str = "kubernetes";
