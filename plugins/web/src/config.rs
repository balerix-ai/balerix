//! The `plugins.web` block (plugins spec §18.5): `{ enabled: bool }`,
//! `true` by default, nothing else. Validated at `activate`, with the
//! config path in the error like every balerix config error.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebConfig {
    pub enabled: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// One line, config path first; an empty path prints the message alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Serde's message trimmed to its first clause, as the flow plugin does.
/// `WebConfig`'s one field is fully defaulted, so serde's derived struct
/// deserializer would happily read a non-map (e.g. a bare array) as a seq
/// of zero elements and hand back the default instead of an error; a
/// per-agent config block is always a map, so that shape is rejected here
/// before deserializing.
pub fn parse(config: &Value) -> Result<WebConfig, ConfigError> {
    if !config.is_object() {
        let kind = match config {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        return Err(ConfigError {
            path: String::new(),
            message: format!("invalid type: {kind}, expected a map"),
        });
    }
    serde_path_to_error::deserialize(config.clone()).map_err(|e| {
        let path = match e.path().to_string() {
            p if p == "." => String::new(),
            p => p,
        };
        let inner = e.into_inner().to_string();
        let message = inner
            .split(", expected one of")
            .next()
            .unwrap_or(&inner)
            .split(", expected `")
            .next()
            .unwrap_or(&inner)
            .to_string();
        ConfigError { path, message }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enabled_defaults_true_and_unknown_fields_carry_their_path() {
        assert_eq!(parse(&json!({})).unwrap(), WebConfig { enabled: true });
        assert_eq!(
            parse(&json!({ "enabled": false })).unwrap(),
            WebConfig { enabled: false }
        );
        assert_eq!(
            parse(&json!({ "nope": 1 })).unwrap_err().to_string(),
            "nope: unknown field `nope`"
        );
        let e = parse(&json!({ "enabled": "yes" })).unwrap_err();
        assert_eq!(e.path, "enabled");
        assert!(e.message.starts_with("invalid type"), "{e}");
        assert!(parse(&json!([])).unwrap_err().path.is_empty());
    }
}
