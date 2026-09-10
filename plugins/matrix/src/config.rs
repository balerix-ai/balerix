//! The two config blocks (Spec G §4). `DaemonConfig` arrives in the hello
//! reply, `AgentConfig` at `activate`. No I/O: the daemon has already read
//! any file-backed secret (§5.1), so a password is a plain value here.

use std::collections::BTreeMap;
use std::fmt;

use balerix_api::HOOK_EVENTS;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The curated default event set (Spec G §4.2).
pub const DEFAULT_EVENTS: [&str; 4] = ["SessionStart", "Notification", "Stop", "SessionEnd"];
/// Always posted: these open and close the thread, so `events` cannot
/// suppress them.
pub const LIFECYCLE: [&str; 2] = ["SessionStart", "SessionEnd"];

/// A credential. Hand-written `Debug` printing `<redacted>`, per AGENTS.md.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DaemonConfig {
    pub homeserver: String,
    pub user_id: String,
    #[serde(default)]
    pub password: Option<Secret>,
    #[serde(default = "default_device")]
    pub device_id: String,
    #[serde(default = "default_device")]
    pub device_name: String,
    #[serde(default)]
    pub invite: Vec<String>,
    /// `fleet/crew` to room id (Spec G §7).
    #[serde(default)]
    pub rooms: BTreeMap<String, String>,
}

fn default_device() -> String {
    "balerix".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub events: Vec<String>,
    pub phases: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            events: DEFAULT_EVENTS.iter().map(|e| (*e).to_string()).collect(),
            phases: true,
        }
    }
}

impl AgentConfig {
    /// Lifecycle events are always posted; everything else is filtered by
    /// `events` (Spec G §4.2).
    pub fn wants(&self, event: &str) -> bool {
        LIFECYCLE.contains(&event) || self.events.iter().any(|e| e == event)
    }
}

/// One line, config path first; an empty path prints the message alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Serde's message trimmed to its first clause, with its path attached.
/// A non-map is rejected first: both structs are fully defaulted or have a
/// `default` on the struct, so serde would otherwise read a bare array as a
/// sequence of zero fields, exactly as `balerix-plugin-web` documents.
fn deserialize<T: serde::de::DeserializeOwned>(config: &Value) -> Result<T, ConfigError> {
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
        // `serde_path_to_error` only records a path for a field it visits; a
        // required field that is simply absent is detected after the map is
        // done, so the path comes back empty. Serde's own message still
        // names the field (`missing field `homeserver``), so pull it out of
        // the message and use it as the path rather than leave the error
        // pathless.
        let path = if path.is_empty() {
            message
                .strip_prefix("missing field `")
                .and_then(|rest| rest.strip_suffix('`'))
                .map(str::to_string)
                .unwrap_or(path)
        } else {
            path
        };
        ConfigError { path, message }
    })
}

pub fn parse_daemon(config: &Value) -> Result<DaemonConfig, ConfigError> {
    let c: DaemonConfig = deserialize(config)?;
    if !(c.homeserver.starts_with("https://") || c.homeserver.starts_with("http://")) {
        return Err(ConfigError {
            path: "homeserver".into(),
            message: "must start with https:// or http://".into(),
        });
    }
    if !c.user_id.starts_with('@') {
        return Err(ConfigError {
            path: "userId".into(),
            message: "must be a full Matrix id starting with @".into(),
        });
    }
    Ok(c)
}

pub fn parse_agent(config: &Value) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = deserialize(config)?;
    for (i, name) in c.events.iter().enumerate() {
        if !HOOK_EVENTS.contains(&name.as_str()) {
            return Err(ConfigError {
                path: format!("events[{i}]"),
                message: format!("unknown event {name:?}"),
            });
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn daemon() -> Value {
        json!({
            "homeserver": "https://matrix.example.org",
            "userId": "@balerix:example.org",
            "password": "hunter2"
        })
    }

    #[test]
    fn daemon_config_defaults_the_device_and_keeps_the_secret_out_of_debug() {
        let c = parse_daemon(&daemon()).unwrap();
        assert_eq!(c.homeserver, "https://matrix.example.org");
        assert_eq!(c.user_id, "@balerix:example.org");
        assert_eq!(c.device_id, "balerix");
        assert_eq!(c.device_name, "balerix");
        assert!(c.invite.is_empty() && c.rooms.is_empty());
        assert_eq!(c.password.as_ref().map(Secret::expose), Some("hunter2"));
        let text = format!("{c:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn daemon_config_reads_camel_case_and_rejects_unknown_keys() {
        let mut v = daemon();
        v["deviceId"] = json!("laptop");
        v["deviceName"] = json!("balerix on laptop");
        v["invite"] = json!(["@rahul:example.org"]);
        v["rooms"] = json!({ "payments/backend": "!abc:example.org" });
        let c = parse_daemon(&v).unwrap();
        assert_eq!(c.device_id, "laptop");
        assert_eq!(c.device_name, "balerix on laptop");
        assert_eq!(c.invite, vec!["@rahul:example.org".to_string()]);
        assert_eq!(
            c.rooms.get("payments/backend").map(String::as_str),
            Some("!abc:example.org")
        );

        let mut v = daemon();
        v["nope"] = json!(1);
        assert_eq!(
            parse_daemon(&v).unwrap_err().to_string(),
            "nope: unknown field `nope`"
        );
    }

    #[test]
    fn daemon_config_rejects_a_bad_homeserver_or_user_id() {
        let mut v = daemon();
        v["homeserver"] = json!("matrix.example.org");
        assert_eq!(
            parse_daemon(&v).unwrap_err().to_string(),
            "homeserver: must start with https:// or http://"
        );
        let mut v = daemon();
        v["userId"] = json!("balerix:example.org");
        assert_eq!(
            parse_daemon(&v).unwrap_err().to_string(),
            "userId: must be a full Matrix id starting with @"
        );
        let v = json!({ "userId": "@a:b" });
        assert_eq!(
            parse_daemon(&v).unwrap_err().to_string(),
            "homeserver: missing field `homeserver`"
        );
    }

    #[test]
    fn a_missing_required_field_names_itself_in_the_path() {
        // `serde_path_to_error` never visits an absent field, so it would
        // otherwise report an empty path; the message names the field
        // regardless, and that name must become the path so every
        // `ConfigError` starts with a config path (AGENTS.md).
        let err = parse_daemon(&json!({ "userId": "@a:b" })).unwrap_err();
        assert_eq!(err.path, "homeserver");
        assert_eq!(err.to_string(), "homeserver: missing field `homeserver`");

        let err = parse_daemon(&json!({ "homeserver": "https://example.org" })).unwrap_err();
        assert_eq!(err.path, "userId");
        assert_eq!(err.to_string(), "userId: missing field `userId`");
    }

    #[test]
    fn agent_config_defaults_to_the_curated_set_and_always_wants_lifecycle() {
        let c = parse_agent(&json!({})).unwrap();
        assert!(c.enabled);
        assert!(c.phases);
        assert_eq!(c.events, DEFAULT_EVENTS.map(String::from).to_vec());
        assert!(c.wants("Notification"));
        assert!(!c.wants("PreToolUse"));

        let c = parse_agent(&json!({ "events": ["PreToolUse"], "phases": false })).unwrap();
        assert!(!c.phases);
        assert!(c.wants("PreToolUse"));
        assert!(!c.wants("Notification"), "the list replaces the default");
        assert!(
            c.wants("SessionStart") && c.wants("SessionEnd"),
            "lifecycle is always posted"
        );
    }

    #[test]
    fn agent_config_rejects_unknown_keys_and_unknown_events_with_their_index() {
        assert_eq!(
            parse_agent(&json!({ "enable": true }))
                .unwrap_err()
                .to_string(),
            "enable: unknown field `enable`"
        );
        assert_eq!(
            parse_agent(&json!({ "events": ["Stop", "Frobnicate"] }))
                .unwrap_err()
                .to_string(),
            "events[1]: unknown event \"Frobnicate\""
        );
        assert!(parse_agent(&json!([])).unwrap_err().path.is_empty());
    }
}
