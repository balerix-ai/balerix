//! Kubernetes mode (Spec O §7): the sidecar link and the two ports over
//! it. The operator's apply, the idle ports and TLS serving join this
//! module in later tasks.

pub mod link;

pub use link::{LINK_CALL_TIMEOUT, LinkError, LinkHub};

/// `FleetRecord.owner` of a fleet the operator applied (§7.3).
pub const KUBERNETES_OWNER: &str = "kubernetes";
