//! The per-agent settings block (spec §5). Appears at fleet, crew and agent
//! level in the YAML file; after resolution every agent carries one complete
//! copy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Fully-resolved settings for one agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    #[serde(default)]
    pub claude: ClaudeSettings,
    /// Mirrors the nono profile schema; passthrough map.
    #[serde(default = "empty_object")]
    pub sandbox: Value,
    /// Tool → exact version, rendered into the agent's `mise.toml`.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    /// Extra environment variables appended after balerix's own.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub runner: RunnerSettings,
    /// Plugin name → that plugin's per-agent config (plugins spec §2.2);
    /// passthrough objects, merged like every other map. Reads `flow` too:
    /// that is what this block was called before Spec B, and a `fleet.json`
    /// stored by an older daemon must still load after the upgrade.
    #[serde(default, alias = "flow")]
    pub plugins: BTreeMap<String, Value>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            claude: ClaudeSettings::default(),
            sandbox: empty_object(),
            tools: BTreeMap::new(),
            env: BTreeMap::new(),
            runner: RunnerSettings::default(),
            plugins: BTreeMap::new(),
        }
    }
}

/// How Claude Code itself is configured and launched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSettings {
    /// Merged verbatim into the agent's `settings.json`; passthrough map.
    #[serde(default = "empty_object")]
    pub settings: Value,
    /// Extra CLI arguments passed to the claude binary.
    #[serde(default)]
    pub args: Vec<String>,
    /// Start with `--continue` when a preserved session exists.
    #[serde(default)]
    pub resume: bool,
    /// Binary to launch; overridable for tests and alternative builds.
    #[serde(default = "default_binary")]
    pub binary: String,
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        Self {
            settings: empty_object(),
            args: Vec::new(),
            resume: false,
            binary: default_binary(),
        }
    }
}

/// Which runner materializes the agent. Only tmux exists today (spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RunnerSettings {
    #[default]
    Tmux,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn default_binary() -> String {
    "claude".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_settings_are_tmux_with_claude_binary_and_empty_blocks() {
        let s = AgentSettings::default();
        assert_eq!(s.runner, RunnerSettings::Tmux);
        assert_eq!(s.claude.binary, "claude");
        assert!(!s.claude.resume);
        assert!(s.claude.args.is_empty());
        assert_eq!(s.claude.settings, json!({}));
        assert_eq!(s.sandbox, json!({}));
        assert!(s.plugins.is_empty());
        assert!(s.tools.is_empty());
        assert!(s.env.is_empty());
    }

    #[test]
    fn deserializes_a_full_block() {
        let v = json!({
            "claude": { "settings": { "model": "opus" }, "args": ["--verbose"], "resume": true, "binary": "/opt/claude" },
            "sandbox": { "network": { "block": false } },
            "tools": { "node": "22.11.0" },
            "env": { "RUST_LOG": "info" },
            "runner": { "type": "tmux" },
            "plugins": { "web": { "enabled": true } }
        });
        let s: AgentSettings = serde_json::from_value(v).unwrap();
        assert_eq!(s.claude.settings, json!({ "model": "opus" }));
        assert_eq!(s.claude.args, vec!["--verbose"]);
        assert!(s.claude.resume);
        assert_eq!(s.claude.binary, "/opt/claude");
        assert_eq!(s.tools["node"], "22.11.0");
        assert_eq!(s.env["RUST_LOG"], "info");
        assert_eq!(s.runner, RunnerSettings::Tmux);
        assert_eq!(s.plugins["web"]["enabled"], true);
    }

    #[test]
    fn plugins_is_a_map_of_passthrough_objects() {
        let s: AgentSettings = serde_json::from_value(
            json!({ "plugins": { "flow": { "initial": "working" }, "web": {} } }),
        )
        .unwrap();
        assert_eq!(s.plugins.len(), 2);
        assert_eq!(s.plugins["flow"]["initial"], "working");
    }

    /// A `fleet.json` written before Spec B carries the reserved `flow: {}`
    /// block; the daemon must still load its record after the upgrade.
    #[test]
    fn the_pre_spec_b_flow_block_is_read_as_plugins() {
        let s: AgentSettings = serde_json::from_value(json!({ "flow": {} })).unwrap();
        assert!(s.plugins.is_empty());
        assert_eq!(s, AgentSettings::default());
        let s: AgentSettings =
            serde_json::from_value(json!({ "flow": { "web": { "enabled": true } } })).unwrap();
        assert_eq!(s.plugins["web"]["enabled"], true);
        // still a map of objects, whichever name it arrives under
        assert!(serde_json::from_value::<AgentSettings>(json!({ "flow": "x" })).is_err());
        assert!(serde_json::from_value::<AgentSettings>(json!({ "plugins": "x" })).is_err());
    }

    #[test]
    fn missing_blocks_take_defaults() {
        let s: AgentSettings =
            serde_json::from_value(json!({ "tools": { "python": "3.12.8" } })).unwrap();
        assert_eq!(s.claude, ClaudeSettings::default());
        assert_eq!(s.runner, RunnerSettings::Tmux);
        assert_eq!(s.tools["python"], "3.12.8");
    }

    #[test]
    fn rejects_unknown_top_level_and_claude_fields() {
        assert!(serde_json::from_value::<AgentSettings>(json!({ "claud": {} })).is_err());
        assert!(
            serde_json::from_value::<AgentSettings>(json!({ "claude": { "model": "opus" } }))
                .is_err()
        );
    }

    #[test]
    fn rejects_unknown_runner_type() {
        assert!(
            serde_json::from_value::<AgentSettings>(json!({ "runner": { "type": "docker" } }))
                .is_err()
        );
    }

    #[test]
    fn round_trips_through_json() {
        let s = AgentSettings {
            tools: BTreeMap::from([("node".to_string(), "22.11.0".to_string())]),
            ..AgentSettings::default()
        };
        let back: AgentSettings =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
