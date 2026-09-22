//! The two config blocks (Spec G §4). `DaemonConfig` arrives in the hello
//! reply, `AgentConfig` at `activate`. No I/O: the daemon has already read
//! any file-backed secret (§5.1), so a password is a plain value here.

use std::collections::BTreeMap;

use balerix_api::DEFAULT_KEY_DELAY_MS;
pub use balerix_plugin_common::config::{
    ConfigError, DEFAULT_EVENTS, EventFilter, LIFECYCLE, Secret, deserialize, validate_key_delay,
};
use serde::Deserialize;
use serde_json::Value;

/// How many messages one body may be split across before the remainder
/// is dropped (Spec G §8). Ten 4000-character parts is far past any real
/// assistant turn, and keeps a runaway output from flooding the room.
pub const DEFAULT_MAX_PARTS: usize = 10;

/// One message holds this much (Spec G §8): comfortably under the 64 KiB
/// event limit a homeserver enforces, and the limit OpenClaw defaults to.
/// A longer body is split across messages by `split`, never cut — the
/// ceiling is readability on a phone, not the protocol's.
pub const BODY_LIMIT: usize = 4000;

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
    /// Most messages one body may be split across.
    #[serde(default = "default_max_parts")]
    pub max_parts: usize,
}

fn default_max_parts() -> usize {
    DEFAULT_MAX_PARTS
}

fn default_device() -> String {
    "balerix".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub events: EventFilter,
    pub phases: bool,
    /// The pause after each key when answering a question (Spec J §7.5).
    #[serde(rename = "keyDelayMs")]
    pub key_delay_ms: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            events: EventFilter::default(),
            phases: true,
            key_delay_ms: DEFAULT_KEY_DELAY_MS,
        }
    }
}

impl AgentConfig {
    /// Lifecycle events are always posted; everything else is filtered by
    /// `events` (Spec G §4.2).
    pub fn wants(&self, event: &str) -> bool {
        self.events.wants(event)
    }
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
    if c.max_parts == 0 {
        return Err(ConfigError {
            path: "maxParts".into(),
            message: "must be at least 1".into(),
        });
    }
    Ok(c)
}

pub fn parse_agent(config: &Value) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = deserialize(config)?;
    c.events.validate()?;
    validate_key_delay(c.key_delay_ms)?;
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
    fn max_parts_defaults_and_is_configurable() {
        let mut v = json!({
            "homeserver": "https://matrix.example.org",
            "userId": "@bot:example.org"
        });
        assert_eq!(parse_daemon(&v).unwrap().max_parts, DEFAULT_MAX_PARTS);
        v["maxParts"] = json!(3);
        assert_eq!(parse_daemon(&v).unwrap().max_parts, 3);
    }

    #[test]
    fn a_zero_max_parts_is_rejected() {
        let v = json!({
            "homeserver": "https://matrix.example.org",
            "userId": "@bot:example.org",
            "maxParts": 0
        });
        let err = parse_daemon(&v).unwrap_err();
        assert_eq!(err.path, "maxParts");
        assert_eq!(err.to_string(), "maxParts: must be at least 1");
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
        assert_eq!(c.events.0, DEFAULT_EVENTS.map(String::from).to_vec());
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

    #[test]
    fn key_delay_defaults_to_100_and_is_bounded() {
        assert_eq!(parse_agent(&json!({})).unwrap().key_delay_ms, 100);
        assert_eq!(
            parse_agent(&json!({ "keyDelayMs": 250 }))
                .unwrap()
                .key_delay_ms,
            250
        );
        for bad in [19, 501] {
            let e = parse_agent(&json!({ "keyDelayMs": bad })).unwrap_err();
            assert_eq!(e.path, "keyDelayMs");
            assert!(e.message.contains("20 to 500"), "{e}");
        }
        assert!(
            parse_agent(&json!({ "key_delay_ms": 100 })).is_err(),
            "camelCase only"
        );
    }
}
