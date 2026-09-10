//! What the daemon persists per fleet, and the port it persists through
//! (Phase 3 spec §2). The registry in memory is authoritative while the
//! daemon runs; the store is how it survives a restart.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use balerix_api::CredentialBundle;
use serde::{Deserialize, Serialize};

use crate::name::FleetName;

pub use balerix_api::{Desired, FleetRecord};

/// Everything secret about a fleet: the credential bundle and one hook
/// secret per agent id. Encrypted at rest by the store.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetSecrets {
    #[serde(default)]
    pub credentials: CredentialBundle,
    /// `fleet/crew/agent` → bearer secret its hooks present.
    #[serde(default)]
    pub hook_secrets: BTreeMap<String, String>,
}

impl fmt::Debug for FleetSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FleetSecrets {{ credentials: {:?}, hook_secrets: {} <redacted> }}",
            self.credentials,
            self.hook_secrets.len()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Corrupt { path: PathBuf, message: String },
}

/// Persists fleets. `put` is atomic per fleet; `purge` removes the fleet's
/// whole directory, kept repos and homes included (`down --purge`). There is
/// no narrower delete: a downed fleet keeps its record until purged (P3-5).
pub trait FleetStore: Send + Sync {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError>;
    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError>;
    fn purge(&self, name: &FleetName) -> Result<(), StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_debug_is_redacted_and_round_trips() {
        let s = FleetSecrets {
            credentials: CredentialBundle {
                gh_token: Some("gho_SECRET".into()),
                ..CredentialBundle::default()
            },
            hook_secrets: BTreeMap::from([("f/c/a".to_string(), "hook-SECRET".to_string())]),
        };
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert!(dbg.contains("hook_secrets: 1 <redacted>"));
        let back: FleetSecrets = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let empty: FleetSecrets = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, FleetSecrets::default());
    }

    #[test]
    fn store_errors_start_with_the_path() {
        let e = StoreError::Corrupt {
            path: "/x/fleet.json".into(),
            message: "expected value at line 1".into(),
        };
        assert_eq!(e.to_string(), "/x/fleet.json: expected value at line 1");
    }
}
