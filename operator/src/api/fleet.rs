use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::{empty_object, open_object};

/// The fleet file, with `name` as `metadata.name`, `daemon` naming the
/// Daemon and `runner` a pod (Spec O §4.2).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Fleet",
    namespaced,
    status = "FleetStatus",
    printcolumn = r#"{"name":"Daemon","type":"string","jsonPath":".spec.daemon"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FleetSpec {
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
