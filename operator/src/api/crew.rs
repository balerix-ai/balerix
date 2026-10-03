use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::open_object;

/// Generated: one crew of a Fleet, resolved (Spec O §4.3). A hand edit
/// is reverted on the next reconcile.
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Crew",
    namespaced,
    status = "CrewStatus",
    printcolumn = r#"{"name":"Cache","type":"string","jsonPath":".status.conditions[?(@.type==\"CacheReady\")].status"}"#,
    printcolumn = r#"{"name":"Tools","type":"string","jsonPath":".status.conditions[?(@.type==\"ToolsReady\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct CrewSpec {
    pub daemon: String,
    pub fleet: String,
    pub crew: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// The crew's `GitSettings`.
    #[schemars(schema_with = "open_object")]
    pub git: Value,
    /// The fleet's `defaults.tools`: the fleet pool's table.
    #[serde(default)]
    pub fleet_tools: BTreeMap<String, String>,
    /// This crew's own table.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CrewStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// The commit last fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ref: Option<String>,
}
