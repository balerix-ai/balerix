//! The domain fleet: the wire `FleetSpec` after every name and repo has
//! been validated ("parse, don't validate").

use std::collections::BTreeMap;

use balerix_api::{AgentSettings, CrewSpec, FleetSpec, GitSettings};

use crate::name::{AgentName, CrewName, FleetName, NameError};
use crate::repo::{RepoError, RepoRef};
use crate::version::{exact_version_message, is_exact_version};

/// A validated fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    pub name: FleetName,
    /// Tools for the fleet pool, from `FleetSpec`; every version exact.
    pub tools: BTreeMap<String, String>,
    pub crews: BTreeMap<CrewName, Crew>,
}

/// A validated crew.
#[derive(Debug, Clone, PartialEq)]
pub struct Crew {
    pub repo: RepoRef,
    pub git_ref: String,
    pub git: GitSettings,
    /// Tools for this crew's pool, from `CrewSpec`; every version exact.
    pub tools: BTreeMap<String, String>,
    pub agents: BTreeMap<AgentName, AgentSettings>,
}

/// A spec that failed validation. The message always starts with the
/// config path of the offending value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FleetError {
    #[error("{path}: {source}")]
    InvalidName { path: String, source: NameError },
    #[error("{path}: {source}")]
    InvalidRepo { path: String, source: RepoError },
    #[error("{path}: must not be empty")]
    EmptyRef { path: String },
    #[error("{path}: reserved name (tmux anchor window)")]
    ReservedAgentName { path: String },
    #[error("{path}: must not be empty")]
    EmptyIdentity { path: String },
    /// Spec L §6: re-checked here so the admin `POST /v1/fleets`, which
    /// receives a resolved spec, never trusts its caller with a git argv.
    #[error("{path}: {reason}")]
    InvalidBranch { path: String, reason: String },
    /// #24: re-checked here for the same reason as `InvalidBranch`: the
    /// admin `POST` must not hand mise a fuzzy version.
    #[error("{path}: {}", exact_version_message(tool, version))]
    InexactVersion {
        path: String,
        tool: String,
        version: String,
    },
}

/// Refuses the first tool in `tools` whose version is not exact; `path`
/// is the table's config path (`tools`, `crews.c.tools`, …).
fn check_tools(path: &str, tools: &BTreeMap<String, String>) -> Result<(), FleetError> {
    match tools.iter().find(|(_, v)| !is_exact_version(v)) {
        Some((tool, version)) => Err(FleetError::InexactVersion {
            path: format!("{path}.{tool}"),
            tool: tool.clone(),
            version: version.clone(),
        }),
        None => Ok(()),
    }
}

/// The tmux anchor window that keeps a crew's session alive is named
/// `balerix` (spec §4.3); an agent may not claim that name.
pub const RESERVED_AGENT_NAME: &str = "balerix";

impl TryFrom<FleetSpec> for Fleet {
    type Error = FleetError;

    fn try_from(spec: FleetSpec) -> Result<Self, FleetError> {
        let name = FleetName::try_from(spec.name).map_err(|source| FleetError::InvalidName {
            path: "name".to_string(),
            source,
        })?;
        check_tools("tools", &spec.tools)?;
        let tools = spec.tools;
        let mut crews = BTreeMap::new();
        for (crew_name, crew) in spec.crews {
            let path = format!("crews.{crew_name}");
            let crew_name =
                CrewName::try_from(crew_name).map_err(|source| FleetError::InvalidName {
                    path: path.clone(),
                    source,
                })?;
            crews.insert(crew_name, convert_crew(&path, crew)?);
        }
        Ok(Self { name, tools, crews })
    }
}

fn convert_crew(path: &str, crew: CrewSpec) -> Result<Crew, FleetError> {
    let repo = RepoRef::parse(&crew.repo).map_err(|source| FleetError::InvalidRepo {
        path: format!("{path}.repo"),
        source,
    })?;
    if crew.git_ref.is_empty() {
        return Err(FleetError::EmptyRef {
            path: format!("{path}.ref"),
        });
    }
    if let Some(identity) = &crew.git.identity {
        for (field, value) in [("name", &identity.name), ("email", &identity.email)] {
            if value.trim().is_empty() {
                return Err(FleetError::EmptyIdentity {
                    path: format!("{path}.git.identity.{field}"),
                });
            }
        }
    }
    check_tools(&format!("{path}.tools"), &crew.tools)?;
    let mut agents = BTreeMap::new();
    for (agent_name, settings) in crew.agents {
        let agent_path = format!("{path}.agents.{agent_name}");
        if agent_name == RESERVED_AGENT_NAME {
            return Err(FleetError::ReservedAgentName { path: agent_path });
        }
        let agent_name =
            AgentName::try_from(agent_name).map_err(|source| FleetError::InvalidName {
                path: agent_path.clone(),
                source,
            })?;
        if let Some(branch) = &settings.branch {
            balerix_api::check_branch_name(branch).map_err(|reason| FleetError::InvalidBranch {
                path: format!("{agent_path}.branch"),
                reason,
            })?;
        }
        check_tools(&format!("{agent_path}.tools"), &settings.tools)?;
        agents.insert(agent_name, settings);
    }
    Ok(Crew {
        repo,
        git_ref: crew.git_ref,
        git: crew.git,
        tools: crew.tools,
        agents,
    })
}

impl From<Fleet> for FleetSpec {
    fn from(fleet: Fleet) -> Self {
        Self {
            name: fleet.name.into(),
            tools: fleet.tools,
            crews: fleet
                .crews
                .into_iter()
                .map(|(name, crew)| {
                    (
                        name.into(),
                        CrewSpec {
                            repo: crew.repo.clone_url(),
                            git_ref: crew.git_ref,
                            git: crew.git,
                            tools: crew.tools,
                            agents: crew
                                .agents
                                .into_iter()
                                .map(|(n, s)| (n.into(), s))
                                .collect(),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentSettings, CrewSpec, FleetSpec, GitSettings};
    use std::collections::BTreeMap;

    fn spec(fleet: &str, crew: &str, repo: &str, git_ref: &str, agents: &[&str]) -> FleetSpec {
        FleetSpec {
            name: fleet.into(),
            crews: BTreeMap::from([(
                crew.to_string(),
                CrewSpec {
                    repo: repo.into(),
                    git_ref: git_ref.into(),
                    git: GitSettings::default(),
                    agents: agents
                        .iter()
                        .map(|a| (a.to_string(), AgentSettings::default()))
                        .collect(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn converts_a_valid_spec() {
        let f = Fleet::try_from(spec(
            "payments",
            "backend",
            "acme/api",
            "main",
            &["alice", "bob"],
        ))
        .unwrap();
        assert_eq!(f.name.as_str(), "payments");
        let crew = &f.crews[&CrewName::try_from("backend").unwrap()];
        assert_eq!(
            crew.repo,
            RepoRef::GitHub {
                owner: "acme".into(),
                name: "api".into()
            }
        );
        assert_eq!(crew.git_ref, "main");
        assert_eq!(crew.agents.len(), 2);
    }

    #[test]
    fn invalid_fleet_name_reports_path_name() {
        let err =
            Fleet::try_from(spec("Payments", "backend", "acme/api", "main", &[])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "name: invalid fleet name \"Payments\": contains characters other than a-z, 0-9 and '-'"
        );
    }

    #[test]
    fn invalid_crew_and_agent_names_report_their_paths() {
        let err = Fleet::try_from(spec("payments", "Back", "acme/api", "main", &[])).unwrap_err();
        assert!(
            err.to_string().starts_with("crews.Back: invalid crew name"),
            "{err}"
        );

        let err =
            Fleet::try_from(spec("payments", "backend", "acme/api", "main", &["Bob"])).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("crews.backend.agents.Bob: invalid agent name"),
            "{err}"
        );
    }

    /// M7: a resolved spec that reaches the daemon without the config
    /// crate (the admin `POST`) still has its `branch` checked.
    #[test]
    fn an_invalid_branch_reports_its_path() {
        let mut s = spec("payments", "backend", "acme/api", "main", &["alice"]);
        let crew = s.crews.get_mut("backend").unwrap();
        crew.agents.get_mut("alice").unwrap().branch = Some("-x".into());
        assert_eq!(
            Fleet::try_from(s.clone()).unwrap_err().to_string(),
            "crews.backend.agents.alice.branch: starts with '-'"
        );
        let crew = s.crews.get_mut("backend").unwrap();
        crew.agents.get_mut("alice").unwrap().branch = Some("feature/x".into());
        assert!(Fleet::try_from(s).is_ok());
    }

    #[test]
    fn the_anchor_window_name_is_reserved() {
        let err = Fleet::try_from(spec(
            "payments",
            "backend",
            "acme/api",
            "main",
            &["balerix"],
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.backend.agents.balerix: reserved name (tmux anchor window)"
        );
    }

    #[test]
    fn invalid_repo_and_empty_ref_report_their_paths() {
        let err = Fleet::try_from(spec("payments", "backend", "nope", "main", &[])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "crews.backend.repo: invalid repo \"nope\": expected owner/name or a clone URL"
        );

        let err = Fleet::try_from(spec("payments", "backend", "acme/api", "", &[])).unwrap_err();
        assert_eq!(err.to_string(), "crews.backend.ref: must not be empty");
    }

    #[test]
    fn round_trips_back_to_the_wire_type() {
        let original = spec("payments", "backend", "acme/api", "main", &["alice"]);
        let back: FleetSpec = Fleet::try_from(original.clone()).unwrap().into();
        assert_eq!(back.name, original.name);
        // repo is normalized to the https clone URL on the way back
        assert_eq!(
            back.crews["backend"].repo,
            "https://github.com/acme/api.git"
        );
        assert_eq!(
            back.crews["backend"].agents,
            original.crews["backend"].agents
        );
    }

    /// #25: the reverse conversion carries both pool tables, so nothing
    /// that round-trips a fleet through the wire type drops them.
    #[test]
    fn round_trips_the_fleet_and_crew_tool_tables() {
        let mut original = spec("payments", "backend", "acme/api", "main", &["alice"]);
        original.tools = BTreeMap::from([("node".to_string(), "22.11.0".to_string())]);
        original.crews.get_mut("backend").unwrap().tools =
            BTreeMap::from([("python".to_string(), "3.12.8".to_string())]);
        let back: FleetSpec = Fleet::try_from(original.clone()).unwrap().into();
        assert_eq!(back.tools, original.tools);
        assert_eq!(back.crews["backend"].tools, original.crews["backend"].tools);
    }

    #[test]
    fn an_empty_identity_field_reports_its_path() {
        let mut s = spec("f", "c", "acme/x", "main", &["a"]);
        s.crews.get_mut("c").unwrap().git.identity = Some(balerix_api::GitIdentity {
            name: "".into(),
            email: "a@b.c".into(),
        });
        assert_eq!(
            Fleet::try_from(s.clone()).unwrap_err().to_string(),
            "crews.c.git.identity.name: must not be empty"
        );
        s.crews.get_mut("c").unwrap().git.identity = Some(balerix_api::GitIdentity {
            name: "A".into(),
            email: " ".into(),
        });
        assert_eq!(
            Fleet::try_from(s).unwrap_err().to_string(),
            "crews.c.git.identity.email: must not be empty"
        );
    }

    /// #24: the admin `POST /v1/fleets` receives a resolved spec; every
    /// tool table is re-checked here so the daemon never hands mise a
    /// fuzzy version, whichever client wrote the spec.
    #[test]
    fn an_inexact_fleet_tool_reports_its_path() {
        let mut s = spec("f", "c", "acme/x", "main", &["a"]);
        s.tools.insert("node".into(), "latest".into());
        assert_eq!(
            Fleet::try_from(s).unwrap_err(),
            FleetError::InexactVersion {
                path: "tools.node".into(),
                tool: "node".into(),
                version: "latest".into(),
            }
        );
    }

    #[test]
    fn an_inexact_crew_tool_reports_its_path() {
        let mut s = spec("f", "c", "acme/x", "main", &["a"]);
        let crew = s.crews.get_mut("c").unwrap();
        crew.tools.insert("python".into(), "3.12".into());
        assert_eq!(
            Fleet::try_from(s).unwrap_err().to_string(),
            "crews.c.tools.python: expected an exact version, got \"3.12\" \
             (try: mise latest python@3.12)"
        );
    }

    #[test]
    fn an_inexact_agent_tool_reports_its_path() {
        let mut s = spec("f", "c", "acme/x", "main", &["a"]);
        let crew = s.crews.get_mut("c").unwrap();
        let agent = crew.agents.get_mut("a").unwrap();
        agent.tools.insert("node".into(), "22.11.0".into());
        agent.tools.insert("ripgrep".into(), "14.x".into());
        assert_eq!(
            Fleet::try_from(s.clone()).unwrap_err().to_string(),
            "crews.c.agents.a.tools.ripgrep: expected an exact version, got \"14.x\" \
             (try: mise latest ripgrep@14.x)"
        );
        let agent = s.crews.get_mut("c").unwrap().agents.get_mut("a").unwrap();
        agent.tools.insert("ripgrep".into(), "14.1.1".into());
        assert!(Fleet::try_from(s).is_ok());
    }

    #[test]
    fn conversion_carries_the_fleet_and_crew_tool_tables() {
        let spec = FleetSpec {
            name: "f".into(),
            tools: BTreeMap::from([("node".to_string(), "22.11.0".to_string())]),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "o/r".into(),
                    git_ref: "main".into(),
                    tools: BTreeMap::from([("python".to_string(), "3.12.8".to_string())]),
                    ..Default::default()
                },
            )]),
        };
        let fleet = Fleet::try_from(spec).unwrap();
        assert_eq!(fleet.tools["node"], "22.11.0");
        assert_eq!(fleet.crews[&"c".parse().unwrap()].tools["python"], "3.12.8");
    }
}
