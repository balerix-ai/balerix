//! Host credentials the client sends alongside a spec (spec §5, §7). The
//! daemon encrypts them at rest; this type never prints their contents.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Credentials discovered on the client host.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CredentialBundle {
    /// Contents of `~/.claude/.credentials.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_credentials: Option<Value>,
    /// Account fields lifted from `~/.claude.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_account: Option<Value>,
    /// `gh` OAuth token for the crew repos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_token: Option<String>,
}

impl fmt::Debug for CredentialBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialBundle")
            .field(
                "claude_credentials",
                &Redacted(self.claude_credentials.is_some()),
            )
            .field("claude_account", &Redacted(self.claude_account.is_some()))
            .field("gh_token", &Redacted(self.gh_token.is_some()))
            .finish()
    }
}

/// Prints `Some(<redacted>)` or `None` without touching the value.
struct Redacted(bool);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 {
            f.write_str("Some(<redacted>)")
        } else {
            f.write_str("None")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn debug_never_prints_secret_material() {
        let b = CredentialBundle {
            claude_credentials: Some(
                json!({ "claudeAiOauth": { "accessToken": "sk-ant-SECRET" } }),
            ),
            claude_account: Some(json!({ "oauthAccount": { "emailAddress": "a@b.c" } })),
            gh_token: Some("gho_SECRET".into()),
        };
        let dbg = format!("{b:?}");
        assert!(!dbg.contains("SECRET"), "debug leaked a secret: {dbg}");
        assert!(!dbg.contains("a@b.c"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn debug_shows_which_parts_are_present() {
        let b = CredentialBundle {
            claude_credentials: None,
            claude_account: None,
            gh_token: Some("x".into()),
        };
        let dbg = format!("{b:?}");
        assert!(dbg.contains("claude_credentials: None"));
        assert!(dbg.contains("gh_token: Some(<redacted>)"));
    }

    #[test]
    fn serializes_fully_for_the_wire() {
        let b = CredentialBundle {
            claude_credentials: None,
            claude_account: None,
            gh_token: Some("gho_x".into()),
        };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["gh_token"], "gho_x");
    }
}
