//! What every `desired` function shares: the hardened pod settings, the
//! labels, the conditions, and `typed`, which turns a manifest written as
//! JSON into the `k8s-openapi` type the function returns.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::Resource;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The server-side-apply field manager (Spec O §5).
pub const MANAGER: &str = "balerix-operator";
/// Every container's uid and gid, and the pods' `fsGroup` (§6.1).
pub const UID: i64 = 10001;
/// The Daemon's port, as the sidecar's hook port is on one machine.
pub const DAEMON_PORT: i32 = 7643;
/// On a Job: the hash of what it was made from. A Job whose hash is not
/// the wanted one is stale and is replaced.
pub const HASH_ANNOTATION: &str = "balerix.ai/input-hash";

#[derive(Debug, thiserror::Error)]
pub enum DesiredError {
    /// A manifest did not fit its type, or a user-supplied shape (a
    /// `resources` block, a toleration) is not what Kubernetes takes.
    #[error("{0}")]
    Shape(#[from] serde_json::Error),
    /// An object read from the cluster lacks what the API server always
    /// sets, or a Daemon lacks what a Fleet needs of it.
    #[error("{0} has no {1}")]
    Missing(&'static str, &'static str),
}

pub fn typed<T: DeserializeOwned>(manifest: Value) -> Result<T, DesiredError> {
    Ok(serde_json::from_value(manifest)?)
}

/// The operator's own settings: what it is, and the images it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorConfig {
    /// `CARGO_PKG_VERSION`: the one version a Daemon may ask for (§4.1).
    pub version: String,
    pub images: Images,
    /// The operator's own namespace, for the Daemon's NetworkPolicy.
    pub namespace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Images {
    pub daemon: String,
    pub agent: String,
}

impl Images {
    pub fn for_version(version: &str) -> Self {
        Self {
            daemon: format!("ghcr.io/balerix-ai/balerix:{version}"),
            agent: format!("ghcr.io/balerix-ai/balerix-agent:{version}"),
        }
    }
}

/// `component` is `daemon`, `agent`, `pool`, `sync` or `harvest`.
pub fn labels(daemon: &str, component: &str, extra: &[(&str, &str)]) -> Value {
    let mut all = json!({
        "app.kubernetes.io/managed-by": MANAGER,
        "balerix.ai/daemon": daemon,
        "balerix.ai/component": component,
    });
    for (k, v) in extra {
        all[*k] = json!(v);
    }
    all
}

pub fn pod_security() -> Value {
    json!({
        "runAsUser": UID,
        "runAsGroup": UID,
        "fsGroup": UID,
        "runAsNonRoot": true,
        "seccompProfile": { "type": "RuntimeDefault" },
    })
}

pub fn container_security() -> Value {
    json!({
        "allowPrivilegeEscalation": false,
        "readOnlyRootFilesystem": true,
        "runAsNonRoot": true,
        "capabilities": { "drop": ["ALL"] },
        "seccompProfile": { "type": "RuntimeDefault" },
    })
}

/// The controller owner reference to `object`, as manifest JSON.
pub fn owner_of<K: Resource<DynamicType = ()>>(object: &K) -> Result<Value, DesiredError> {
    let reference = object
        .controller_owner_ref(&())
        .ok_or(DesiredError::Missing("the owner", "metadata.name and uid"))?;
    Ok(serde_json::to_value(reference)?)
}

/// sha256 over the value's JSON. `serde_json` keeps object keys sorted
/// (no `preserve_order` here), so equal values hash equal. Takes a
/// `Value`, not a `Serialize`: rendering a `Value` to text cannot fail,
/// so there is no error to hide.
pub fn hash(value: &Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

/// One condition as a function decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cond {
    pub type_: &'static str,
    /// `None` is `Unknown`.
    pub status: Option<bool>,
    pub reason: String,
    pub message: String,
}

impl Cond {
    pub fn yes(type_: &'static str, reason: &str, message: &str) -> Self {
        Self::new(type_, Some(true), reason, message)
    }
    pub fn no(type_: &'static str, reason: &str, message: &str) -> Self {
        Self::new(type_, Some(false), reason, message)
    }
    pub fn unknown(type_: &'static str, reason: &str, message: &str) -> Self {
        Self::new(type_, None, reason, message)
    }
    fn new(type_: &'static str, status: Option<bool>, reason: &str, message: &str) -> Self {
        Self {
            type_,
            status,
            reason: reason.to_string(),
            message: message.to_string(),
        }
    }
}

/// `new` as status conditions. A condition whose status is what `old`
/// had keeps its transition time; `now` is passed in, so this is pure.
pub fn conditions(
    old: &[Condition],
    new: &[Cond],
    generation: Option<i64>,
    now: &Time,
) -> Vec<Condition> {
    new.iter()
        .map(|c| {
            let status = match c.status {
                Some(true) => "True",
                Some(false) => "False",
                None => "Unknown",
            };
            let since = old
                .iter()
                .find(|o| o.type_ == c.type_ && o.status == status)
                .map(|o| o.last_transition_time.clone())
                .unwrap_or_else(|| now.clone());
            Condition {
                type_: c.type_.to_string(),
                status: status.to_string(),
                reason: c.reason.clone(),
                message: c.message.clone(),
                observed_generation: generation,
                last_transition_time: since,
            }
        })
        .collect()
}

/// What a Job came to, read from the Job and its pods (`jobs::job_outcome`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// No Job by that name.
    Absent,
    /// A Job made from other input: delete it and make the wanted one.
    Stale,
    Running,
    /// With the container's termination message.
    Succeeded(String),
    Failed(String),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use serde_json::json;

    use super::*;

    fn at(secs: i64) -> Time {
        Time(k8s_openapi::jiff::Timestamp::from_second(secs).unwrap())
    }

    #[test]
    fn a_condition_keeps_its_transition_time_while_its_status_holds() {
        let first = conditions(
            &[],
            &[Cond::no("Ready", "ClaimPending", "x")],
            Some(1),
            &at(100),
        );
        assert_eq!(first[0].status, "False");
        assert_eq!(first[0].last_transition_time, at(100));
        let same = conditions(
            &first,
            &[Cond::no("Ready", "PoolSyncRunning", "y")],
            Some(2),
            &at(200),
        );
        assert_eq!(same[0].last_transition_time, at(100), "still False");
        assert_eq!(same[0].reason, "PoolSyncRunning");
        assert_eq!(same[0].observed_generation, Some(2));
        let flipped = conditions(&same, &[Cond::yes("Ready", "Ready", "")], Some(2), &at(300));
        assert_eq!(flipped[0].status, "True");
        assert_eq!(flipped[0].last_transition_time, at(300));
        let unknown = conditions(
            &flipped,
            &[Cond::unknown("Ready", "DaemonUnavailable", "z")],
            Some(2),
            &at(400),
        );
        assert_eq!(unknown[0].status, "Unknown");
    }

    #[test]
    fn typed_names_a_wrong_shape_and_hash_is_stable() {
        let ok: ResourceRequirements = typed(json!({ "requests": { "cpu": "1" } })).unwrap();
        assert!(ok.requests.is_some());
        let e = typed::<ResourceRequirements>(json!({ "requests": 3 })).unwrap_err();
        assert!(e.to_string().contains("invalid type"), "{e}");
        assert_eq!(
            hash(&json!({ "a": 1, "b": 2 })),
            hash(&json!({ "b": 2, "a": 1 }))
        );
        assert_ne!(hash(&json!({ "a": 1 })), hash(&json!({ "a": 2 })));
        assert_eq!(hash(&json!({})).len(), 64);
    }

    #[test]
    fn names_are_the_documented_ones() {
        use crate::desired::names;
        assert_eq!(names::daemon("default"), "balerix-default");
        assert_eq!(names::state_claim("default"), "balerix-default-state");
        assert_eq!(names::shared_claim("default"), "balerix-default-shared");
        assert_eq!(names::authority("default"), "balerix-default-ca");
        assert_eq!(names::serving("default"), "balerix-default-tls");
        assert_eq!(names::admin("default"), "balerix-default-admin");
        assert_eq!(names::daemon_pool_job("default"), "balerix-default-pool");
        assert_eq!(
            names::endpoint("team-a", "default"),
            "https://balerix-default.team-a.svc:7643"
        );
        assert_eq!(names::fleet_pool_job("payments"), "payments-pool");
        assert_eq!(names::crew("payments", "backend"), "payments-backend");
        assert_eq!(
            names::sync_job("payments", "backend"),
            "payments-backend-sync"
        );
        assert_eq!(
            names::agent("payments", "backend", "alice"),
            "payments-backend-alice"
        );
        assert_eq!(
            names::bundle("payments-backend-alice"),
            "payments-backend-alice-bundle"
        );
        assert_eq!(
            names::token("payments-backend-alice"),
            "payments-backend-alice-token"
        );
        assert_eq!(
            names::harvest_job("payments-backend-alice"),
            "payments-backend-alice-harvest"
        );
        assert_eq!(names::vol_daemon_pool(), "pools/daemon");
        assert_eq!(names::vol_fleet_pool("payments"), "fleets/payments/pool");
        assert_eq!(
            names::vol_crew_pool("payments", "backend"),
            "fleets/payments/crews/backend/pool"
        );
        assert_eq!(
            names::vol_crew_repo("payments", "backend"),
            "fleets/payments/crews/backend/repo"
        );
    }
}
