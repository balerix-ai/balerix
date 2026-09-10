//! Client-side checks on a resolved agent (spec §5 "Validation"). Names and
//! repos are validated by `balerix_core::Fleet`; this covers everything else.

use balerix_api::AgentSettings;
use balerix_core::is_exact_version;
use serde_json::Value;

use crate::ConfigError;

/// Environment variables balerix sets itself (spec §4 table); user `env`
/// may not override them. Entries ending in `_` are prefixes.
pub const RESERVED_ENV_PREFIXES: &[&str] = &[
    "HOME",
    "XDG_",
    "CLAUDE_CONFIG_DIR",
    "GH_CONFIG_DIR",
    "MISE_",
    "BALERIX_",
    "PATH",
    // claude refuses a temp dir it cannot reach; balerix points both at the
    // agent's own `home/tmp`.
    "TMPDIR",
    "CLAUDE_CODE_TMPDIR",
];

/// The message for a tool version `is_exact_version` refuses; shared by
/// `validate_agent` and `tools_layer` so the two stay identical by
/// construction rather than by two hand-kept copies.
fn exact_version_message(tool: &str, version: &str) -> String {
    format!("expected an exact version, got {version:?} (try: mise latest {tool}@{version})")
}

/// Validates one resolved settings block; `path` prefixes every message.
pub fn validate_agent(path: &str, settings: &AgentSettings) -> Result<(), ConfigError> {
    let invalid = |suffix: &str, message: String| ConfigError::Invalid {
        path: format!("{path}.{suffix}"),
        message,
    };

    for (tool, version) in &settings.tools {
        if !is_exact_version(version) {
            return Err(invalid(
                &format!("tools.{tool}"),
                exact_version_message(tool, version),
            ));
        }
    }

    let Value::Object(claude_settings) = &settings.claude.settings else {
        return Err(invalid("claude.settings", "expected a mapping".to_string()));
    };
    if claude_settings.contains_key("hooks") {
        return Err(invalid(
            "claude.settings.hooks",
            "balerix owns this key; configure hook behaviour via `plugins` instead".to_string(),
        ));
    }
    if !settings.sandbox.is_object() {
        return Err(invalid("sandbox", "expected a mapping".to_string()));
    }
    for (name, cfg) in &settings.plugins {
        if let Err(reason) = balerix_core::name::validate_name(name) {
            return Err(invalid(
                &format!("plugins.{name}"),
                format!("invalid plugin name: {reason}"),
            ));
        }
        if !cfg.is_object() {
            return Err(invalid(
                &format!("plugins.{name}"),
                "expected a mapping".to_string(),
            ));
        }
    }

    for key in settings.env.keys() {
        if key.is_empty() {
            return Err(invalid("env", "empty variable name".to_string()));
        }
        let reserved = RESERVED_ENV_PREFIXES.iter().any(|r| {
            if r.ends_with('_') {
                key.starts_with(r)
            } else {
                key == r
            }
        });
        if reserved {
            return Err(invalid(
                &format!("env.{key}"),
                "reserved; balerix sets this variable".to_string(),
            ));
        }
    }
    Ok(())
}

/// The `tools` table of one settings layer, as the pool for that level
/// wants it: absent or null is empty, a `null` value is the file's delete
/// marker and is dropped, and every surviving version must be exact.
/// `path` is the layer's config path, e.g. `defaults` or
/// `crews.web.defaults`.
pub fn tools_layer(
    path: &str,
    layer: &Value,
) -> Result<std::collections::BTreeMap<String, String>, ConfigError> {
    let invalid = |suffix: &str, message: String| ConfigError::Invalid {
        path: format!("{path}.tools{suffix}"),
        message,
    };
    let mut out = std::collections::BTreeMap::new();
    let tools = match layer.get("tools") {
        None | Some(Value::Null) => return Ok(out),
        Some(Value::Object(t)) => t,
        Some(_) => return Err(invalid("", "expected a mapping".to_string())),
    };
    for (tool, value) in tools {
        let version = match value {
            Value::Null => continue,
            Value::String(s) => s,
            _ => {
                return Err(invalid(
                    &format!(".{tool}"),
                    "expected a version string".to_string(),
                ));
            }
        };
        if !is_exact_version(version) {
            return Err(invalid(
                &format!(".{tool}"),
                exact_version_message(tool, version),
            ));
        }
        out.insert(tool.clone(), version.clone());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, ClaudeSettings};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn with_tools(tools: &[(&str, &str)]) -> AgentSettings {
        AgentSettings {
            tools: tools
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..AgentSettings::default()
        }
    }

    #[test]
    fn happy_path_passes() {
        let s = AgentSettings {
            claude: ClaudeSettings {
                settings: json!({"model": "opus"}),
                ..ClaudeSettings::default()
            },
            env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
            ..with_tools(&[("node", "22.11.0")])
        };
        validate_agent("crews.backend.agents.bob", &s).unwrap();
    }

    #[test]
    fn fuzzy_tool_version_error_names_the_path_and_hints_mise_latest() {
        let err =
            validate_agent("crews.backend.agents.bob", &with_tools(&[("node", "22")])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.backend.agents.bob.tools.node: expected an exact version, got \"22\" (try: mise latest node@22)"
        );
    }

    #[test]
    fn hooks_in_claude_settings_are_rejected() {
        let s = AgentSettings {
            claude: ClaudeSettings {
                settings: json!({"hooks": {}}),
                ..ClaudeSettings::default()
            },
            ..AgentSettings::default()
        };
        let err = validate_agent("crews.c.agents.a", &s).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.c.agents.a.claude.settings.hooks: balerix owns this key; configure hook behaviour via `plugins` instead"
        );
    }

    #[test]
    fn passthrough_blocks_must_be_maps() {
        let s = AgentSettings {
            sandbox: json!([1]),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.sandbox: expected a mapping"
        );
        let s = AgentSettings {
            plugins: BTreeMap::from([("web".to_string(), json!("x"))]),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.plugins.web: expected a mapping"
        );
        let s = AgentSettings {
            plugins: BTreeMap::from([("Web".to_string(), json!({}))]),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.plugins.Web: invalid plugin name: contains characters other than a-z, 0-9 and '-'"
        );
        let s = AgentSettings {
            claude: ClaudeSettings {
                settings: json!(3),
                ..ClaudeSettings::default()
            },
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.claude.settings: expected a mapping"
        );
    }

    #[test]
    fn tools_layer_treats_a_whole_table_null_as_empty() {
        // A crew layer that removes every fleet tool by writing `tools:
        // null` outright, rather than nulling each key individually: the
        // one arm no other test reaches.
        assert_eq!(
            tools_layer("crews.web.defaults", &json!({"tools": null})).unwrap(),
            BTreeMap::new()
        );
    }

    #[test]
    fn reserved_env_keys_are_rejected() {
        for key in [
            "HOME",
            "XDG_DATA_HOME",
            "CLAUDE_CONFIG_DIR",
            "GH_CONFIG_DIR",
            "MISE_DATA_DIR",
            "BALERIX_FLEET",
            "PATH",
            "TMPDIR",
            "CLAUDE_CODE_TMPDIR",
        ] {
            let s = AgentSettings {
                env: BTreeMap::from([(key.to_string(), "x".to_string())]),
                ..AgentSettings::default()
            };
            let err = validate_agent("p", &s).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("p.env.{key}: reserved; balerix sets this variable"),
                "for {key}"
            );
        }
        let s = AgentSettings {
            env: BTreeMap::from([(String::new(), "x".to_string())]),
            ..AgentSettings::default()
        };
        assert_eq!(
            validate_agent("p", &s).unwrap_err().to_string(),
            "p.env: empty variable name"
        );
    }
}
