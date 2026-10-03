use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::{ClaimSpec, SecretRef, empty_object, open_object};

/// A `balerix serve` instance in Kubernetes mode and what it shares with
/// its agents (Spec O §4.1).
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "balerix.ai",
    version = "v1alpha1",
    kind = "Daemon",
    namespaced,
    status = "DaemonStatus",
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Endpoint","type":"string","jsonPath":".status.endpoint"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSpec {
    /// The daemon and agent images' version; must equal the operator's
    /// in `v1alpha1`, and is the operator's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub storage: DaemonStorage,
    #[serde(default)]
    pub credentials: Credentials,
    /// The merge layer the host's `settings.json` is on one machine.
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub defaults: Value,
    /// Plugin names; list order is interceptor order.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// The daemon container's `ResourceRequirements`.
    #[serde(default = "empty_object")]
    #[schemars(schema_with = "open_object")]
    pub resources: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStorage {
    /// Plugin KV, vault, records.
    pub state: ClaimSpec,
    /// ReadWriteMany: crew caches and tool pools.
    pub shared: ClaimSpec,
    /// The default per-agent claim.
    pub agent: ClaimSpec,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    /// Key `credentials.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<SecretRef>,
    /// Key `token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<SecretRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// The Daemon's in-cluster URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}
