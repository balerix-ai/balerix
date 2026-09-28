//! State (Spec M §5): one KV row per agent under `session/<agent>`,
//! mirrored like matrix's maps (store first, then memory), with the
//! reverse map `(repo, number) → agent` derived at load; and
//! `repo/<fleet>`, which repository a sanitised fleet name stands for.

use std::collections::HashMap;

use balerix_plugin_sdk::{Host, SdkError};
use serde::{Deserialize, Serialize};

use crate::config::Kind;
use crate::repo_config::Live;

pub const SESSION_PREFIX: &str = "session/";
pub const REPO_PREFIX: &str = "repo/";

pub fn session_key(agent: &str) -> String {
    format!("{SESSION_PREFIX}{agent}")
}
pub fn repo_key(fleet: &str) -> String {
    format!("{REPO_PREFIX}{fleet}")
}
/// `fleet/crew/agent` → `fleet`.
pub fn fleet_of(agent: &str) -> &str {
    agent.split('/').next().unwrap_or(agent)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub repo: String,
    pub installation: u64,
    pub kind: Kind,
    pub number: u64,
    #[serde(default)]
    pub head: Option<String>,
    /// The PR's base ref (what the head merges into); `None` for an issue.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub status_comment: Option<u64>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub last_activity: u64,
    #[serde(default)]
    pub closed: bool,
}

#[derive(Debug, Default)]
pub struct Sessions {
    rows: HashMap<String, Session>,
    by_number: HashMap<(String, u64), String>,
}

impl Sessions {
    pub async fn load(host: &Host) -> Result<Self, SdkError> {
        let mut s = Self::default();
        for key in host.kv_list(SESSION_PREFIX).await? {
            let Some(agent) = key.strip_prefix(SESSION_PREFIX) else {
                continue;
            };
            let Some(bytes) = host.kv_get(&key).await? else {
                continue;
            };
            match serde_json::from_slice::<Session>(&bytes) {
                Ok(row) => s.insert(agent.to_string(), row),
                Err(e) => tracing::warn!("github: bad session record {key}: {e}"),
            }
        }
        Ok(s)
    }

    fn insert(&mut self, agent: String, row: Session) {
        if let Some(old) = self.rows.get(&agent) {
            self.by_number.remove(&(old.repo.clone(), old.number));
        }
        self.by_number
            .insert((row.repo.clone(), row.number), agent.clone());
        self.rows.insert(agent, row);
    }

    pub fn get(&self, agent: &str) -> Option<&Session> {
        self.rows.get(agent)
    }
    pub fn get_mut(&mut self, agent: &str) -> Option<&mut Session> {
        self.rows.get_mut(agent)
    }
    pub fn by_number(&self, repo: &str, number: u64) -> Option<&str> {
        self.by_number
            .get(&(repo.to_string(), number))
            .map(String::as_str)
    }

    /// Store first, then memory (J-8's rule): a row the store refused is
    /// a row a restart would not know.
    pub async fn set(&mut self, host: &Host, agent: &str, row: Session) -> Result<(), SdkError> {
        let bytes = serde_json::to_vec(&row)
            .map_err(|e| SdkError::Transport(format!("encode session: {e}")))?;
        host.kv_put(&session_key(agent), &bytes, false).await?;
        self.insert(agent.to_string(), row);
        Ok(())
    }

    pub async fn remove(&mut self, host: &Host, agent: &str) -> Result<(), SdkError> {
        host.kv_delete(&session_key(agent)).await?;
        if let Some(old) = self.rows.remove(agent) {
            self.by_number.remove(&(old.repo, old.number));
        }
        Ok(())
    }

    /// The agents the fleet file must carry: every open row in `fleet`.
    pub fn live_in(&self, fleet: &str) -> Vec<Live> {
        let mut v: Vec<(&String, &Session)> = self
            .rows
            .iter()
            .filter(|(a, r)| fleet_of(a) == fleet && !r.closed)
            .collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v.into_iter()
            .map(|(_, r)| Live {
                kind: r.kind,
                number: r.number,
                head: r.head.clone(),
            })
            .collect()
    }

    pub fn agents_in(&self, fleet: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .rows
            .keys()
            .filter(|a| fleet_of(a) == fleet)
            .cloned()
            .collect();
        v.sort();
        v
    }

    pub fn open_count(&self) -> usize {
        self.rows.values().filter(|r| !r.closed).count()
    }

    pub fn agents(&self) -> Vec<String> {
        self.rows.keys().cloned().collect()
    }
}

pub async fn repo_of_fleet(host: &Host, fleet: &str) -> Result<Option<String>, SdkError> {
    Ok(host
        .kv_get(&repo_key(fleet))
        .await?
        .map(|b| String::from_utf8_lossy(&b).into_owned()))
}

pub async fn set_repo_of_fleet(host: &Host, fleet: &str, repo: &str) -> Result<(), SdkError> {
    host.kv_put(&repo_key(fleet), repo.as_bytes(), false).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::FakeHost;
    use serde_json::json;

    fn row(repo: &str, number: u64) -> Session {
        Session {
            repo: repo.into(),
            installation: 7,
            kind: Kind::Issue,
            number,
            head: None,
            base: None,
            status_comment: None,
            session_id: None,
            last_activity: 0,
            closed: false,
        }
    }

    #[tokio::test]
    async fn rows_are_mirrored_and_the_reverse_map_survives_a_reload() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("github", std::path::Path::new("scratch"))).unwrap();
        let mut s = Sessions::default();
        s.set(&host, "gh-acme-api/repo/issue-12", row("acme/api", 12))
            .await
            .unwrap();
        s.set(
            &host,
            "gh-acme-api/repo/pr-3",
            Session {
                kind: Kind::Pr,
                head: Some("f".into()),
                ..row("acme/api", 3)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fake.kv_json("session/gh-acme-api/repo/issue-12").unwrap()["number"],
            12
        );
        let again = Sessions::load(&host).await.unwrap();
        assert_eq!(
            again.by_number("acme/api", 12),
            Some("gh-acme-api/repo/issue-12")
        );
        assert_eq!(again.live_in("gh-acme-api").len(), 2);
        assert_eq!(again.live_in("gh-acme-api")[1].head.as_deref(), Some("f"));
        assert_eq!(again.agents_in("other"), Vec::<String>::new());
        s.remove(&host, "gh-acme-api/repo/issue-12").await.unwrap();
        assert!(fake.kv_json("session/gh-acme-api/repo/issue-12").is_none());
        assert_eq!(s.by_number("acme/api", 12), None);
        set_repo_of_fleet(&host, "gh-acme-api", "acme/api")
            .await
            .unwrap();
        assert_eq!(
            repo_of_fleet(&host, "gh-acme-api")
                .await
                .unwrap()
                .as_deref(),
            Some("acme/api")
        );
        assert_eq!(repo_of_fleet(&host, "gh-none").await.unwrap(), None);
    }

    #[test]
    fn an_older_row_without_the_optional_fields_still_loads() {
        let r: Session = serde_json::from_value(
            json!({ "repo": "a/b", "installation": 1, "kind": "pr", "number": 2 }),
        )
        .unwrap();
        assert!(
            !r.closed && r.status_comment.is_none() && r.last_activity == 0 && r.base.is_none()
        );
        assert_eq!(fleet_of("gh-a-b/repo/pr-2"), "gh-a-b");
    }
}
