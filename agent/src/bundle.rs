//! The operator's bundle, turned into what the planner wants: one fleet
//! with one crew and one agent (Spec O §5.4, §6.2).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, ensure};
use balerix_api::{AgentBundle, CrewSpec, FleetSpec};
use balerix_core::{AgentId, Fleet, ResolvedAgent};

pub struct Loaded {
    pub bundle: AgentBundle,
    pub id: AgentId,
    pub fleet: Fleet,
    pub agent: ResolvedAgent,
}

/// The bundle carries the token and the credentials: only the id and the
/// bundle's own redacting `Debug` are shown.
impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("id", &self.id)
            .field("bundle", &self.bundle)
            .finish_non_exhaustive()
    }
}

pub fn load(path: &Path) -> Result<Loaded> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the agent bundle at {}", path.display()))?;
    let bundle: AgentBundle = serde_json::from_str(&text).map_err(|e| {
        anyhow!(
            "{}: not an agent bundle ({} at line {} column {})",
            path.display(),
            category(&e),
            e.line(),
            e.column()
        )
    })?;
    let id: AgentId = bundle
        .agent
        .parse()
        .map_err(|e| anyhow!("{}: agent: {e}", path.display()))?;
    ensure!(
        bundle.token.len() >= 32,
        "{}: the token is at least 32 characters",
        path.display()
    );
    let spec = FleetSpec {
        name: id.fleet.to_string(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            id.crew.to_string(),
            CrewSpec {
                repo: bundle.repo.clone(),
                git_ref: bundle.git_ref.clone(),
                git: bundle.git.clone(),
                tools: BTreeMap::new(),
                agents: BTreeMap::from([(id.agent.to_string(), bundle.settings.clone())]),
            },
        )]),
    };
    let fleet = Fleet::try_from(spec).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let agent = ResolvedAgent::from_fleet(&fleet)
        .into_iter()
        .find(|a| a.id == id)
        .context("the bundle's agent is not in its own fleet")?;
    Ok(Loaded {
        bundle,
        id,
        fleet,
        agent,
    })
}

/// What kind of parse failure, without serde's message: that quotes the
/// offending value, and the bundle holds the token and the credentials.
fn category(e: &serde_json::Error) -> &'static str {
    match e.classify() {
        serde_json::error::Category::Io => "a read error",
        serde_json::error::Category::Syntax => "a syntax error",
        serde_json::error::Category::Data => "a data error",
        serde_json::error::Category::Eof => "an unexpected end",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_bundle_becomes_one_fleet_with_one_agent() {
        let dir = crate::test_dir();
        let path = dir.path().join("agent.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "agent": "payments/backend/alice",
                "repo": "acme/payments-api",
                "git_ref": "main",
                "settings": { "claude": { "settings": { "model": "sonnet" } } },
                "daemon_url": "https://d:7643",
                "token": "0123456789abcdef0123456789abcdef"
            })
            .to_string(),
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.id.to_string(), "payments/backend/alice");
        assert_eq!(loaded.fleet.name.as_str(), "payments");
        assert_eq!(loaded.agent.git_ref, "main");
        assert_eq!(loaded.agent.branch(), "balerix/payments/backend/alice");
        let shown = format!("{loaded:?}");
        assert!(!shown.contains("0123456789abcdef"), "{shown}");
    }

    /// The termination log is readable by whoever can describe the pod:
    /// serde's own message quotes the offending value, which can be the
    /// token or a credential.
    #[test]
    fn a_malformed_bundle_names_the_place_not_the_value() {
        let dir = crate::test_dir();
        let path = dir.path().join("agent.json");
        for (field, value) in [
            ("token", json!(918_273_645_546_372_819_u64)),
            ("credentials", json!("sk-ant-very-secret-value")),
        ] {
            let mut bundle = json!({
                "agent": "payments/backend/alice",
                "repo": "acme/payments-api",
                "git_ref": "main",
                "settings": {},
                "daemon_url": "https://d:7643",
                "token": "0123456789abcdef0123456789abcdef"
            });
            bundle[field] = value.clone();
            std::fs::write(&path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();
            let shown = format!("{:#}", load(&path).unwrap_err());
            let raw = match &value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            assert!(!shown.contains(&raw), "{field}: {shown}");
            assert!(
                shown.starts_with(&format!("{}: not an agent bundle", path.display())),
                "{shown}"
            );
            assert!(shown.contains("data error at line"), "{shown}");
        }
        std::fs::write(&path, "{").unwrap();
        let shown = format!("{:#}", load(&path).unwrap_err());
        assert!(
            shown.contains("unexpected end at line 1 column 1"),
            "{shown}"
        );
    }

    #[test]
    fn errors_name_the_file() {
        let e = load(std::path::Path::new("/nonexistent/agent.json")).unwrap_err();
        assert!(
            format!("{e:#}").starts_with("cannot read the agent bundle at /nonexistent/agent.json"),
            "{e:#}"
        );
    }
}
