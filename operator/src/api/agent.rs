use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::open_object;

/// Generated: one agent of a Fleet, resolved (Spec O §4.4).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Agent",
    namespaced,
    status = "AgentStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Restarts","type":"integer","jsonPath":".status.restarts"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    pub daemon: String,
    pub fleet: String,
    pub crew: String,
    pub agent: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// The crew's `GitSettings`.
    #[schemars(schema_with = "open_object")]
    pub git: Value,
    /// The resolved `AgentSettings`.
    #[schemars(schema_with = "open_object")]
    pub settings: Value,
    /// Over everything the pod is made from; a pod annotated with another
    /// value is replaced (§5.4).
    pub spec_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// The Daemon's phase for the agent, as `balerix status` prints it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Plugin name to activation state.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, String>,
}
