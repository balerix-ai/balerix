//! Config helpers every plugin re-implemented (Spec K §3.1): a redacted
//! `Secret`, a path-first `ConfigError`, the deserializer that attaches
//! serde's path, and the curated event filter of Spec G §4.2.

use std::fmt;

use balerix_api::{HOOK_EVENTS, MAX_KEY_DELAY_MS, MIN_KEY_DELAY_MS};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
pub fn deserialize<T: serde::de::DeserializeOwned>(config: &Value) -> Result<T, ConfigError> {
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

/// The curated default event set (Spec G §4.2).
pub const DEFAULT_EVENTS: [&str; 4] = ["SessionStart", "Notification", "Stop", "SessionEnd"];
/// Always wanted: these open and close a conversation, so a filter cannot
/// suppress them.
pub const LIFECYCLE: [&str; 2] = ["SessionStart", "SessionEnd"];

/// Which hook events a plugin shows (Spec G §4.2): the list replaces the
/// default, and the lifecycle pair is wanted whatever the list says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventFilter(pub Vec<String>);

impl Default for EventFilter {
    fn default() -> Self {
        Self(DEFAULT_EVENTS.iter().map(|e| (*e).to_string()).collect())
    }
}

impl EventFilter {
    pub fn wants(&self, event: &str) -> bool {
        LIFECYCLE.contains(&event) || self.0.iter().any(|e| e == event)
    }

    /// Every name must be one of `HOOK_EVENTS`; the error names the
    /// offending index under `events`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (i, name) in self.0.iter().enumerate() {
            if !HOOK_EVENTS.contains(&name.as_str()) {
                return Err(ConfigError {
                    path: format!("events[{i}]"),
                    message: format!("unknown event {name:?}"),
                });
            }
        }
        Ok(())
    }
}

/// Spec J §7.5: the pause between keys, `20..=500` ms, reported under
/// the camelCase key every plugin block uses.
pub fn validate_key_delay(ms: u64) -> Result<(), ConfigError> {
    if !(MIN_KEY_DELAY_MS..=MAX_KEY_DELAY_MS).contains(&ms) {
        return Err(ConfigError {
            path: "keyDelayMs".into(),
            message: format!(
                "expected {MIN_KEY_DELAY_MS} to {MAX_KEY_DELAY_MS} milliseconds, got {ms}"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_secret_never_prints_its_value() {
        let s = Secret::new("hunter2");
        assert_eq!(s.expose(), "hunter2");
        let text = format!("{s:?}");
        assert!(
            !text.contains("hunter2") && text.contains("<redacted>"),
            "{text}"
        );
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!("hunter2"),
            "transparent on the wire"
        );
    }

    #[test]
    fn a_config_error_prints_its_path_first_or_the_message_alone() {
        let e = ConfigError {
            path: "homeserver".into(),
            message: "missing".into(),
        };
        assert_eq!(e.to_string(), "homeserver: missing");
        let e = ConfigError {
            path: String::new(),
            message: "not a map".into(),
        };
        assert_eq!(e.to_string(), "not a map");
    }

    #[derive(Debug, serde::Deserialize, PartialEq)]
    #[serde(deny_unknown_fields, default)]
    struct Sample {
        a: u32,
        b: String,
    }
    impl Default for Sample {
        fn default() -> Self {
            Self {
                a: 1,
                b: "x".into(),
            }
        }
    }

    #[test]
    fn deserialize_rejects_a_non_map_and_attaches_the_path() {
        assert_eq!(
            deserialize::<Sample>(&json!({})).unwrap(),
            Sample {
                a: 1,
                b: "x".into()
            }
        );
        let e = deserialize::<Sample>(&json!([])).unwrap_err();
        assert_eq!(e.path, "");
        assert_eq!(e.message, "invalid type: array, expected a map");
        let e = deserialize::<Sample>(&json!({ "nope": 1 })).unwrap_err();
        assert_eq!(e.to_string(), "nope: unknown field `nope`");
        let e = deserialize::<Sample>(&json!({ "a": "no" })).unwrap_err();
        assert_eq!(e.path, "a");
    }

    #[derive(Debug, serde::Deserialize)]
    struct Needs {
        #[allow(dead_code)]
        homeserver: String,
    }

    #[test]
    fn a_missing_required_field_names_itself_in_the_path() {
        let e = deserialize::<Needs>(&json!({})).unwrap_err();
        assert_eq!(e.path, "homeserver");
        assert_eq!(e.to_string(), "homeserver: missing field `homeserver`");
    }

    #[test]
    fn the_event_filter_defaults_to_the_curated_set_and_always_wants_lifecycle() {
        let f = EventFilter::default();
        assert_eq!(f.0, DEFAULT_EVENTS.map(String::from).to_vec());
        assert!(f.wants("Notification"));
        assert!(!f.wants("PreToolUse"));
        let f = EventFilter(vec!["PreToolUse".into()]);
        assert!(f.wants("PreToolUse"));
        assert!(!f.wants("Notification"), "the list replaces the default");
        assert!(
            f.wants("SessionStart") && f.wants("SessionEnd"),
            "lifecycle is always wanted"
        );
        assert_eq!(f.validate(), Ok(()));
    }

    #[test]
    fn an_unknown_event_is_refused_with_its_index() {
        let f = EventFilter(vec!["Stop".into(), "Frobnicate".into()]);
        assert_eq!(
            f.validate().unwrap_err().to_string(),
            "events[1]: unknown event \"Frobnicate\""
        );
    }

    #[test]
    fn the_key_delay_is_bounded() {
        assert_eq!(validate_key_delay(100), Ok(()));
        for bad in [19, 501] {
            let e = validate_key_delay(bad).unwrap_err();
            assert_eq!(e.path, "keyDelayMs");
            assert!(e.message.contains("20 to 500"), "{e}");
        }
    }
}
