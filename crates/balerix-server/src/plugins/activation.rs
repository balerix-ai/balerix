//! Which `(agent, plugin)` pairs a spec wants, and what changes between
//! two specs (plugins spec §16.2). Pure.

use balerix_api::FleetSpec;
use balerix_core::{AgentId, AgentName, FleetName};
use serde_json::Value;

use super::PluginError;

#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub agent: AgentId,
    pub plugin: AgentName,
    pub config: Value,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ActivationDiff {
    /// New pairs, and pairs whose config changed.
    pub activate: Vec<Pair>,
    /// Dropped pairs only: a changed pair is replaced by its `activate`.
    pub deactivate: Vec<(AgentId, AgentName)>,
}

/// `crews.<crew>.agents.<agent>.plugins.<plugin>`: the config path every
/// activation error starts with.
pub fn config_path(agent: &AgentId, plugin: &AgentName) -> String {
    format!(
        "crews.{}.agents.{}.plugins.{}",
        agent.crew, agent.agent, plugin
    )
}

/// Every `(agent, plugin, config)` of the spec, sorted by agent then
/// plugin. The spec was resolved client-side, but plugin names are
/// validated here again: the daemon builds ids from them.
pub fn pairs(fleet: &FleetName, spec: &FleetSpec) -> Result<Vec<Pair>, PluginError> {
    let mut out = Vec::new();
    for (crew, c) in &spec.crews {
        for (agent, settings) in &c.agents {
            let id: AgentId = format!("{fleet}/{crew}/{agent}").parse().map_err(
                |e: balerix_core::NameError| PluginError::Activation {
                    path: format!("crews.{crew}.agents.{agent}"),
                    message: e.to_string(),
                },
            )?;
            for (name, config) in &settings.plugins {
                if let Err(reason) = balerix_core::name::validate_name(name) {
                    return Err(PluginError::Activation {
                        path: format!("crews.{crew}.agents.{agent}.plugins.{name}"),
                        message: format!("invalid plugin name: {reason}"),
                    });
                }
                let plugin: AgentName =
                    name.parse()
                        .map_err(|e: balerix_core::NameError| PluginError::Activation {
                            path: format!("crews.{crew}.agents.{agent}.plugins.{name}"),
                            message: e.to_string(),
                        })?;
                out.push(Pair {
                    agent: id.clone(),
                    plugin,
                    config: config.clone(),
                });
            }
        }
    }
    out.sort_by(|x, y| (&x.agent, &x.plugin).cmp(&(&y.agent, &y.plugin)));
    Ok(out)
}

pub fn diff(old: &[Pair], new: &[Pair]) -> ActivationDiff {
    let find = |set: &[Pair], p: &Pair| {
        set.iter()
            .find(|q| q.agent == p.agent && q.plugin == p.plugin)
            .cloned()
    };
    let mut d = ActivationDiff::default();
    for p in new {
        match find(old, p) {
            Some(prev) if prev.config == p.config => {}
            // changed: the new config goes by `activate` alone and the
            // plugin replaces the one it holds (§16.2, §17.9)
            Some(_) | None => d.activate.push(p.clone()),
        }
    }
    for p in old {
        if find(new, p).is_none() {
            d.deactivate.push((p.agent.clone(), p.plugin.clone()));
        }
    }
    d.deactivate.sort();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, CrewSpec, GitSettings};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn spec(agents: &[(&str, &[(&str, serde_json::Value)])]) -> FleetSpec {
        FleetSpec {
            name: "f".into(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/x".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: agents
                        .iter()
                        .map(|(n, plugins)| {
                            let s = AgentSettings {
                                plugins: plugins
                                    .iter()
                                    .map(|(p, c)| (p.to_string(), c.clone()))
                                    .collect(),
                                ..Default::default()
                            };
                            (n.to_string(), s)
                        })
                        .collect(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    fn render(pairs: &[Pair]) -> Vec<String> {
        pairs
            .iter()
            .map(|p| format!("{} {} {}", p.agent, p.plugin, p.config))
            .collect()
    }

    #[test]
    fn pairs_are_sorted_and_named_by_config_path() {
        let fleet: FleetName = "f".parse().unwrap();
        let s = spec(&[
            ("b", &[("web", json!({ "enabled": true }))]),
            (
                "a",
                &[("web", json!({})), ("flow", json!({ "initial": "x" }))],
            ),
        ]);
        assert_eq!(
            render(&pairs(&fleet, &s).unwrap()),
            vec![
                "f/c/a flow {\"initial\":\"x\"}",
                "f/c/a web {}",
                "f/c/b web {\"enabled\":true}"
            ]
        );
        let bad = spec(&[("a", &[("Bad Name", json!({}))])]);
        assert_eq!(
            pairs(&fleet, &bad).unwrap_err().to_string(),
            "crews.c.agents.a.plugins.Bad Name: invalid plugin name: contains characters other than a-z, 0-9 and '-'"
        );
        assert_eq!(
            config_path(&"f/c/a".parse().unwrap(), &"flow".parse().unwrap()),
            "crews.c.agents.a.plugins.flow"
        );
    }

    #[test]
    fn diff_activates_new_and_changed_and_deactivates_dropped_only() {
        let fleet: FleetName = "f".parse().unwrap();
        let old = pairs(
            &fleet,
            &spec(&[
                ("a", &[("flow", json!({ "v": 1 })), ("web", json!({}))]),
                ("b", &[("web", json!({}))]),
            ]),
        )
        .unwrap();
        let new = pairs(
            &fleet,
            &spec(&[
                ("a", &[("flow", json!({ "v": 2 })), ("web", json!({}))]),
                ("c", &[("web", json!({}))]),
            ]),
        )
        .unwrap();
        let d = diff(&old, &new);
        assert_eq!(
            render(&d.activate),
            vec!["f/c/a flow {\"v\":2}", "f/c/c web {}"]
        );
        assert_eq!(
            d.deactivate
                .iter()
                .map(|(a, p)| format!("{a} {p}"))
                .collect::<Vec<_>>(),
            vec!["f/c/b web"],
            "a changed pair is re-activated in place, never deactivated"
        );
        let none = diff(&new, &new);
        assert!(none.activate.is_empty() && none.deactivate.is_empty());
        let fresh = diff(&[], &new);
        assert_eq!(fresh.activate.len(), 3);
        assert!(fresh.deactivate.is_empty());
    }
}
