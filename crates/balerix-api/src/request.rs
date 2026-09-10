//! Request bodies for the fleet endpoints (spec §7).

use serde::{Deserialize, Serialize};

use crate::{CredentialBundle, FleetSpec};

/// Body of `POST /v1/fleets` (up) and `PUT /v1/fleets/{name}` (update).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetRequest {
    pub spec: FleetSpec,
    #[serde(default)]
    pub credentials: CredentialBundle,
}

/// Query flags of `DELETE /v1/fleets/{name}` (spec D6). Every flag is sent
/// as `key=true|false`: axum's `Query` rejects a bare key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownQuery {
    pub keep_repos: bool,
    pub keep_sessions: bool,
    pub purge: bool,
}

impl DownQuery {
    pub fn to_query_string(&self) -> String {
        format!(
            "keep_repos={}&keep_sessions={}&purge={}",
            self.keep_repos, self.keep_sessions, self.purge
        )
    }
}

/// Body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// Body of `POST /v1/sessions` (plugins spec §18.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionRequest {
    /// Where the login redirects: a path under `/v1/plugins/`; the mount
    /// root when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResponse {
    /// `http://127.0.0.1:<port>/v1/login/<code>?to=<path>`, single use.
    pub login_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn request_round_trips() {
        let r = FleetRequest {
            spec: FleetSpec {
                name: "f".into(),
                crews: BTreeMap::new(),
                ..Default::default()
            },
            credentials: CredentialBundle::default(),
        };
        let back: FleetRequest = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.spec, r.spec);
        assert_eq!(back.credentials.gh_token, None);
    }

    #[test]
    fn down_query_defaults_to_false_and_renders_every_flag() {
        let q: DownQuery = serde_json::from_value(serde_json::json!({ "purge": true })).unwrap();
        assert!(q.purge && !q.keep_repos && !q.keep_sessions);
        assert_eq!(
            DownQuery {
                keep_repos: true,
                keep_sessions: false,
                purge: false
            }
            .to_query_string(),
            "keep_repos=true&keep_sessions=false&purge=false"
        );
        assert!(serde_json::from_value::<DownQuery>(serde_json::json!({ "x": 1 })).is_err());
        let e: ErrorBody = serde_json::from_str(r#"{"error":"fleet exists"}"#).unwrap();
        assert_eq!(e.error, "fleet exists");
    }

    #[test]
    fn session_request_round_trips() {
        assert_eq!(
            serde_json::to_string(&SessionRequest { to: None }).unwrap(),
            "{}"
        );
        let r: SessionRequest =
            serde_json::from_value(serde_json::json!({ "to": "/v1/plugins/web/" })).unwrap();
        assert_eq!(r.to.as_deref(), Some("/v1/plugins/web/"));
        assert!(serde_json::from_value::<SessionRequest>(serde_json::json!({ "x": 1 })).is_err());
    }
}
