use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::{SecretKeyRef, empty_object, open_object};

/// A plugin a Daemon in its namespace may list (Spec O §4.5). Defined
/// here so the five kinds are reviewed once; its controller is
/// sub-project 4 (§23).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Plugin",
    namespaced,
    status = "PluginStatus",
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct PluginSpec {
    pub image: String,
    /// The grant.
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub config: Value,
    /// Config key to the Secret key injected there.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretKeyRef>,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub fleet_defaults: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose: Option<Expose>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<Scratch>,
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub resources: Value,
}

/// A Service for the plugin's own listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Expose {
    pub port: i32,
}

/// A claim for the plugin's scratch directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Scratch {
    pub size: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}
