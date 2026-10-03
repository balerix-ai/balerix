use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A settings layer, validated by `balerix-config` exactly as the fleet
/// file's is, not by the schema (Spec O §4.2). Also used for maps of
/// such layers and for the Kubernetes shapes kept opaque.
pub fn open_object(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "type": "object", "x-kubernetes-preserve-unknown-fields": true })
}

pub fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// One claim's class and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    pub size: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    pub secret_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    pub secret_name: String,
    pub key: String,
}
