//! Group `balerix.ai`, version `v1alpha1`: all five kinds namespaced,
//! every status with `observedGeneration` and `conditions`, the operator
//! their only writer (Spec O §4).

pub mod agent;
pub mod common;
pub mod crew;
pub mod daemon;
pub mod fleet;
pub mod plugin;

pub use agent::{Agent, AgentSpec, AgentStatus};
pub use common::{ClaimSpec, SecretKeyRef, SecretRef};
pub use crew::{Crew, CrewSpec, CrewStatus};
pub use daemon::{Credentials, Daemon, DaemonSpec, DaemonStatus, DaemonStorage};
pub use fleet::{Fleet, FleetCrew, FleetSpec, FleetStatus, Retain};
pub use plugin::{Plugin, PluginSpec, PluginStatus};

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt;

pub const GROUP: &str = "balerix.ai";
pub const VERSION: &str = "v1alpha1";

/// In the order `crds` prints them: what a Fleet needs comes first.
pub fn crds() -> Vec<CustomResourceDefinition> {
    vec![
        Daemon::crd(),
        Fleet::crd(),
        Crew::crd(),
        Agent::crd(),
        Plugin::crd(),
    ]
}

/// `<plural>.balerix.ai.yaml` and its content, one per kind.
pub fn crd_files() -> Result<Vec<(String, String)>, serde_norway::Error> {
    crds()
        .iter()
        .map(|crd| {
            let name = crd.metadata.name.clone().unwrap_or_default();
            Ok((format!("{name}.yaml"), serde_norway::to_string(crd)?))
        })
        .collect()
}
