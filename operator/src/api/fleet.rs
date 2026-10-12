use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::{empty_object, open_object};

/// The fleet file, with `name` as `metadata.name`, `daemon` naming the
/// Daemon and `runner` a pod (Spec O §4.2).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, KubeSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Fleet",
    namespaced,
    status = "FleetStatus",
    printcolumn = r#"{"name":"Daemon","type":"string","jsonPath":".spec.daemon"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    // the same names resolution refuses (§4.2), refused at admission too,
    // on create only: a Fleet made before the rules can still have its
    // status written, its finalizer removed and be deleted
    validation = kube::core::Rule::new("oldSelf.hasValue() || self.metadata.name != 'balerix'").optional_old_self(true).message("the name balerix is reserved"),
    validation = kube::core::Rule::new("oldSelf.hasValue() || !self.metadata.name.startsWith('balerix-')").optional_old_self(true).message("the balerix- prefix is reserved for the operator's own objects")
)]
#[serde(rename_all = "camelCase")]
pub struct FleetSpec {
    /// The Daemon the fleet runs on. Immutable: to move the Fleet to
    /// another Daemon, delete it and create it again.
    #[x_kube(validation = Rule::new("self == oldSelf").message("spec.daemon is immutable: delete the Fleet and create it again to move it to another Daemon"))]
    pub daemon: String,
    #[serde(default)]
    pub retain: Retain,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub defaults: Value,
    #[serde(default)]
    pub crews: BTreeMap<String, FleetCrew>,
}

/// `Branches` is `down --keep-repos`, `None` is plain `down` (O-16).
/// Either keeps the fleet's record on the Daemon. The annotation
/// `balerix.ai/purge: "true"` on the Fleet counts only once the Fleet is
/// deleted: it then skips every agent's harvest and purges that record
/// after the down and any cleanup. An agent dropped while the Fleet lives
/// is still harvested. With `Branches` the crew cache, and the branches
/// harvested into it before, is kept (Spec O §25).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Retain {
    #[default]
    None,
    Branches,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetCrew {
    pub repo: String,
    /// `balerix-config`'s default when absent.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "open_object")]
    pub git: Option<Value>,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub defaults: Value,
    /// Agent name to its settings layer.
    #[serde(default)]
    #[schemars(schema_with = "open_object")]
    pub agents: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}
