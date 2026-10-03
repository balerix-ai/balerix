//! What the operator mounts for one sidecar (Spec O §5.4): the resolved
//! agent, the credential bundle and the Daemon token, as one JSON file.
//! Never printed: `Debug` redacts the token and the credentials.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{AgentSettings, CredentialBundle, GitSettings};

/// `/balerix/secret/agent.json`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBundle {
    /// `fleet/crew/agent`.
    pub agent: String,
    /// The crew's repository, as the fleet file wrote it.
    pub repo: String,
    /// The crew's base ref.
    pub git_ref: String,
    #[serde(default)]
    pub git: GitSettings,
    pub settings: AgentSettings,
    /// `https://<daemon service>:<port>`.
    pub daemon_url: String,
    /// The agent's token: its hook secret and its link credential.
    pub token: String,
    #[serde(default)]
    pub credentials: CredentialBundle,
}

impl fmt::Debug for AgentBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentBundle")
            .field("agent", &self.agent)
            .field("repo", &self.repo)
            .field("git_ref", &self.git_ref)
            .field("daemon_url", &self.daemon_url)
            .field("token", &"<redacted>")
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bundle_json() -> serde_json::Value {
        json!({
            "agent": "payments/backend/alice",
            "repo": "acme/payments-api",
            "git_ref": "main",
            "settings": { "claude": { "settings": { "model": "sonnet" } } },
            "daemon_url": "https://balerix-default.team-a.svc:7643",
            "token": "tok-SECRET-0123456789abcdef0123456789abcdef",
            "credentials": { "gh_token": "gho_SECRET" }
        })
    }

    fn bundle() -> AgentBundle {
        serde_json::from_value(bundle_json()).unwrap()
    }

    #[test]
    fn debug_redacts_the_token_and_the_credentials() {
        let dbg = format!("{:?}", bundle());
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert!(dbg.contains("payments/backend/alice"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn round_trips_and_refuses_unknown_fields() {
        let b = bundle();
        let back: AgentBundle = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back, b);
        // otherwise valid, so only the unknown field can make it fail
        let mut with_extra = bundle_json();
        with_extra["x"] = json!(1);
        assert!(serde_json::from_value::<AgentBundle>(with_extra).is_err());
    }
}
