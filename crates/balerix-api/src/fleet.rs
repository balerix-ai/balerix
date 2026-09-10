//! The resolved fleet specification — the wire format for `up`/`update`
//! and what the daemon stores (spec §5, §7).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::settings::AgentSettings;

/// A fully-resolved fleet: every agent carries a complete settings block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetSpec {
    pub name: String,
    /// Tools declared in the fleet file's top-level `defaults`. Installed
    /// once into the fleet pool and shared read-only with every agent in
    /// the fleet (Spec E §3).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub crews: BTreeMap<String, CrewSpec>,
}

/// One crew: a shared repository plus its agents.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrewSpec {
    /// GitHub `owner/name` shorthand or a full clone URL.
    pub repo: String,
    /// Base ref for per-agent branches.
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default)]
    pub git: GitSettings,
    /// This crew's own `defaults.tools` layer, never merged with the
    /// fleet's: the fleet pool is already a parent of the crew pool.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSettings>,
}

/// Git permissions for a crew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSettings {
    #[serde(default = "default_true")]
    pub push: bool,
    #[serde(default)]
    pub auth: GitAuth,
    /// Commit identity written to the agent's `.gitconfig` (Phase 3 spec
    /// §6.3). Absent → derived from the agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<GitIdentity>,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self {
            push: true,
            auth: GitAuth::default(),
            identity: None,
        }
    }
}

/// `user.name` / `user.email` for commits made inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

/// Where the crew's git credentials come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitAuth {
    /// Token from the host's `gh` config, sent in the credential bundle.
    #[default]
    Gh,
    /// No credentials; public repos only.
    None,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn git_settings_default_to_push_with_gh_auth() {
        let g = GitSettings::default();
        assert!(g.push);
        assert_eq!(g.auth, GitAuth::Gh);
    }

    #[test]
    fn crew_spec_uses_ref_as_the_wire_name() {
        let c: CrewSpec = serde_json::from_value(json!({
            "repo": "acme/payments-api", "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "alice": {} }
        }))
        .unwrap();
        assert_eq!(c.git_ref, "main");
        assert!(!c.git.push);
        assert_eq!(c.git.auth, GitAuth::None);
        assert!(c.agents.contains_key("alice"));
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back["ref"], "main");
        assert_eq!(back["git"]["auth"], "none");
    }

    #[test]
    fn fleet_spec_round_trips() {
        let spec = FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::from([(
                "backend".to_string(),
                CrewSpec {
                    repo: "acme/payments-api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: BTreeMap::from([("alice".to_string(), AgentSettings::default())]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let back: FleetSpec = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn identity_is_optional_and_round_trips() {
        let g: GitSettings =
            serde_json::from_value(json!({ "push": true, "auth": "none" })).unwrap();
        assert_eq!(g.identity, None);
        let back = serde_json::to_value(&g).unwrap();
        assert!(
            back.get("identity").is_none(),
            "absent identity is not serialized"
        );
        let g: GitSettings = serde_json::from_value(
            json!({ "identity": { "name": "Alice Bot", "email": "alice@example.com" } }),
        )
        .unwrap();
        assert_eq!(
            g.identity,
            Some(GitIdentity {
                name: "Alice Bot".into(),
                email: "alice@example.com".into()
            })
        );
        assert!(
            serde_json::from_value::<GitSettings>(
                json!({ "identity": { "name": "x", "nope": 1 } })
            )
            .is_err()
        );
    }

    #[test]
    fn tools_default_to_empty_and_are_omitted_when_empty() {
        let json = r#"{"name":"f","crews":{"c":{"repo":"o/r","ref":"main"}}}"#;
        let spec: FleetSpec = serde_json::from_str(json).unwrap();
        assert!(spec.tools.is_empty(), "an older fleet.json still loads");
        assert!(spec.crews["c"].tools.is_empty());
        let back = serde_json::to_string(&spec).unwrap();
        assert!(
            !back.contains("tools"),
            "an empty table stays out of the wire format: {back}"
        );
    }

    #[test]
    fn tools_round_trip_at_both_levels() {
        let json = r#"{"name":"f","tools":{"node":"22.11.0"},
            "crews":{"c":{"repo":"o/r","ref":"main","tools":{"python":"3.12.8"}}}}"#;
        let spec: FleetSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.tools["node"], "22.11.0");
        assert_eq!(spec.crews["c"].tools["python"], "3.12.8");
    }
}
