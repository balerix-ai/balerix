//! One agent as the runtime sees it: every crew-level fact folded in
//! (Phase 2 spec §2.1). `ResolvedAgent::from_fleet` is the only way the
//! runtime reaches the fleet tree.

use std::fmt;
use std::str::FromStr;

use balerix_api::{AgentSettings, GitSettings, SpecHash};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::Fleet;
use crate::name::{AgentId, CrewName, FleetName, NameError};
use crate::repo::RepoRef;

/// A crew's identity, written `fleet/crew`. Also the tmux session name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CrewRef {
    pub fleet: FleetName,
    pub crew: CrewName,
}

impl fmt::Display for CrewRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.fleet, self.crew)
    }
}

impl FromStr for CrewRef {
    type Err = NameError;
    fn from_str(s: &str) -> Result<Self, NameError> {
        let Some((f, c)) = s.split_once('/') else {
            return Err(NameError {
                kind: "crew ref",
                value: s.to_string(),
                reason: "expected fleet/crew",
            });
        };
        if c.contains('/') {
            return Err(NameError {
                kind: "crew ref",
                value: s.to_string(),
                reason: "expected fleet/crew",
            });
        }
        Ok(Self {
            fleet: f.parse()?,
            crew: c.parse()?,
        })
    }
}

impl AgentId {
    pub fn crew_ref(&self) -> CrewRef {
        CrewRef {
            fleet: self.fleet.clone(),
            crew: self.crew.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAgent {
    pub id: AgentId,
    pub repo: RepoRef,
    pub git_ref: String,
    pub git: GitSettings,
    pub settings: AgentSettings,
}

/// Exactly the fields that, when changed, must restart the agent.
#[derive(Serialize)]
struct HashInput<'a> {
    repo: String,
    git_ref: &'a str,
    git: &'a GitSettings,
    settings: &'a AgentSettings,
}

impl ResolvedAgent {
    /// Every agent of the fleet, sorted by id.
    pub fn from_fleet(fleet: &Fleet) -> Vec<ResolvedAgent> {
        let mut out = Vec::new();
        for (crew_name, crew) in &fleet.crews {
            for (agent_name, settings) in &crew.agents {
                out.push(ResolvedAgent {
                    id: AgentId {
                        fleet: fleet.name.clone(),
                        crew: crew_name.clone(),
                        agent: agent_name.clone(),
                    },
                    repo: crew.repo.clone(),
                    git_ref: crew.git_ref.clone(),
                    git: crew.git.clone(),
                    settings: settings.clone(),
                });
            }
        }
        out
    }

    /// The worktree branch: `settings.branch` when set (an existing remote
    /// branch, Spec L §6), else the per-agent
    /// `balerix/<fleet>/<crew>/<agent>` (spec D3).
    pub fn branch(&self) -> String {
        match &self.settings.branch {
            Some(b) => b.clone(),
            None => format!("balerix/{}", self.id),
        }
    }

    /// The remote ref the worktree branch is created from when it does
    /// not exist locally: the branch itself when `settings.branch` is set,
    /// else the crew's `ref`. The workspace diff base is the crew's `ref`
    /// either way (`Daemon::base_ref`).
    pub fn start_ref(&self) -> &str {
        self.settings.branch.as_deref().unwrap_or(&self.git_ref)
    }

    /// SHA-256 over canonical JSON. `serde_json` writes maps in key order
    /// (every map here is a `BTreeMap` or a `serde_json::Map` without
    /// `preserve_order`), so equal inputs hash equal regardless of source order.
    pub fn hash(&self) -> SpecHash {
        let input = HashInput {
            repo: self.repo.clone_url(),
            git_ref: &self.git_ref,
            git: &self.git,
            settings: &self.settings,
        };
        // Serialization of these plain data types cannot fail.
        let bytes = serde_json::to_vec(&input).unwrap_or_default();
        SpecHash::new(hex::encode(Sha256::digest(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{CrewSpec, FleetSpec};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn fleet() -> Fleet {
        Fleet::try_from(FleetSpec {
            name: "payments".into(),
            crews: BTreeMap::from([(
                "backend".to_string(),
                CrewSpec {
                    repo: "acme/api".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: BTreeMap::from([
                        ("bob".to_string(), AgentSettings::default()),
                        ("alice".to_string(), AgentSettings::default()),
                    ]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn crew_ref_displays_and_parses() {
        let c: CrewRef = "payments/backend".parse().unwrap();
        assert_eq!(c.to_string(), "payments/backend");
        assert!("payments".parse::<CrewRef>().is_err());
        assert!("a/b/c".parse::<CrewRef>().is_err());
    }

    #[test]
    fn from_fleet_folds_crew_facts_in_and_sorts_by_id() {
        let agents = ResolvedAgent::from_fleet(&fleet());
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].id.to_string(), "payments/backend/alice");
        assert_eq!(agents[1].id.to_string(), "payments/backend/bob");
        assert_eq!(agents[0].git_ref, "main");
        assert_eq!(
            agents[0].repo.clone_url(),
            "https://github.com/acme/api.git"
        );
        assert_eq!(agents[0].branch(), "balerix/payments/backend/alice");
        assert_eq!(agents[0].id.crew_ref().to_string(), "payments/backend");
    }

    #[test]
    fn hash_ignores_json_key_order_and_changes_with_settings() {
        let mut a = ResolvedAgent::from_fleet(&fleet()).remove(0);
        a.settings.claude.settings = json!({ "model": "opus", "permissions": { "allow": ["x"] } });
        let mut b = a.clone();
        b.settings.claude.settings = json!({ "permissions": { "allow": ["x"] }, "model": "opus" });
        assert_eq!(a.hash(), b.hash());
        b.settings.claude.settings = json!({ "model": "sonnet" });
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.hash().as_str().len(), 64);
    }

    #[test]
    fn hash_changes_with_crew_level_facts() {
        let a = ResolvedAgent::from_fleet(&fleet()).remove(0);
        let mut b = a.clone();
        b.git_ref = "develop".into();
        assert_ne!(a.hash(), b.hash());
    }

    /// Spec L §6: `branch` names an existing remote branch; it is both the
    /// worktree branch and the start point. Without it, the per-agent
    /// branch starts from the crew's ref.
    #[test]
    fn branch_and_start_ref_follow_the_setting() {
        let mut a = ResolvedAgent::from_fleet(&fleet()).remove(0);
        assert_eq!(a.branch(), "balerix/payments/backend/alice");
        assert_eq!(a.start_ref(), "main");
        let before = a.hash();
        a.settings.branch = Some("feature/issue-12".into());
        assert_eq!(a.branch(), "feature/issue-12");
        assert_eq!(a.start_ref(), "feature/issue-12");
        assert_ne!(
            a.hash(),
            before,
            "a changed branch re-materializes the agent"
        );
    }
}
