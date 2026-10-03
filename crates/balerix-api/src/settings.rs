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
    /// An existing remote branch this agent works on (Spec L §6). When
    /// set, the worktree branch is this name, created from
    /// `origin/<branch>`; the crew `ref` remains the base the workspace
    /// diff is taken against. Absent: the per-agent branch
    /// `balerix/<fleet>/<crew>/<agent>` from `origin/<ref>`. Validated by
    /// `check_branch_name` in the resolver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
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
            branch: None,
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

/// Which runner materializes the agent: a tmux window on one machine
/// (spec §6), or a pod (Spec O §4.2). The pod's Kubernetes shapes are
/// opaque here; `balerix-operator` gives them their types, so
/// `k8s-openapi` stays out of this crate (Spec O §20.3).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum RunnerSettings {
    #[default]
    Tmux,
    #[serde(rename_all = "camelCase")]
    Pod {
        /// A `ResourceRequirements` for the agent container.
        #[serde(default = "empty_object")]
        resources: Value,
        /// `{ size }`: overrides the Daemon's agent claim size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage: Option<Value>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        node_selector: BTreeMap<String, String>,
        /// A list of `Toleration`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tolerations: Vec<Value>,
    },
}

/// `RunnerSettings` without its payload: what a resolver or a daemon is
/// willing to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunnerKind {
    #[default]
    Tmux,
    Pod,
}

impl RunnerSettings {
    pub fn kind(&self) -> RunnerKind {
        match self {
            Self::Tmux => RunnerKind::Tmux,
            Self::Pod { .. } => RunnerKind::Pod,
        }
    }
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
    fn a_pod_runner_keeps_its_kubernetes_shapes_as_json() {
        let s: AgentSettings = serde_json::from_value(json!({
            "runner": {
                "type": "pod",
                "resources": { "requests": { "cpu": "1", "memory": "2Gi" } },
                "storage": { "size": "40Gi" },
                "nodeSelector": { "pool": "agents" },
                "tolerations": [{ "key": "agents", "operator": "Exists" }]
            }
        }))
        .unwrap();
        assert_eq!(s.runner.kind(), RunnerKind::Pod);
        let RunnerSettings::Pod {
            resources,
            storage,
            node_selector,
            tolerations,
        } = &s.runner
        else {
            panic!("{:?}", s.runner)
        };
        assert_eq!(resources["requests"]["memory"], "2Gi");
        assert_eq!(storage.as_ref().unwrap()["size"], "40Gi");
        assert_eq!(node_selector["pool"], "agents");
        assert_eq!(tolerations.len(), 1);
        // round trip, camelCase on the wire
        let back = serde_json::to_value(&s.runner).unwrap();
        assert_eq!(back["nodeSelector"]["pool"], "agents");
        assert_eq!(
            serde_json::from_value::<RunnerSettings>(back).unwrap(),
            s.runner
        );
    }

    #[test]
    fn a_bare_pod_runner_has_empty_shapes_and_an_unknown_key_is_refused() {
        let r: RunnerSettings = serde_json::from_value(json!({ "type": "pod" })).unwrap();
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "type": "pod", "resources": {} })
        );
        let e = serde_json::from_value::<RunnerSettings>(json!({ "type": "pod", "image": "x" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown field `image`"), "{e}");
        assert_eq!(RunnerSettings::default().kind(), RunnerKind::Tmux);
    }

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

    /// Spec L §6: an optional existing remote branch. Absent and `null`
    /// are both "no branch"; the wire form omits it when unset.
    #[test]
    fn branch_is_optional_and_omitted_when_unset() {
        let s = AgentSettings::default();
        assert_eq!(s.branch, None);
        assert!(serde_json::to_value(&s).unwrap().get("branch").is_none());
        let s: AgentSettings =
            serde_json::from_value(json!({ "branch": "feature/issue-12" })).unwrap();
        assert_eq!(s.branch.as_deref(), Some("feature/issue-12"));
        assert_eq!(
            serde_json::to_value(&s).unwrap()["branch"],
            "feature/issue-12"
        );
        let s: AgentSettings = serde_json::from_value(json!({ "branch": null })).unwrap();
        assert_eq!(s.branch, None);
        assert!(serde_json::from_value::<AgentSettings>(json!({ "branch": 3 })).is_err());
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
