//! Validated identifiers (spec §5: names match `[a-z0-9-]+`, DNS-label
//! style so they survive as tmux session names, branch names and, later,
//! Kubernetes labels).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why a string is not a valid name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} name {value:?}: {reason}")]
pub struct NameError {
    pub kind: &'static str,
    pub value: String,
    pub reason: &'static str,
}

/// Checks the shared naming rule. Returns the human-readable reason on failure.
pub fn validate_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("empty");
    }
    if s.len() > 63 {
        return Err("longer than 63 characters");
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("contains characters other than a-z, 0-9 and '-'");
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err("starts or ends with '-'");
    }
    Ok(())
}

macro_rules! name_type {
    ($(#[$doc:meta])* $t:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $t(String);

        impl $t {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $t {
            type Error = NameError;
            fn try_from(value: String) -> Result<Self, NameError> {
                validate_name(&value)
                    .map(|()| Self(value.clone()))
                    .map_err(|reason| NameError { kind: $kind, value, reason })
            }
        }

        impl TryFrom<&str> for $t {
            type Error = NameError;
            fn try_from(value: &str) -> Result<Self, NameError> {
                Self::try_from(value.to_string())
            }
        }

        impl FromStr for $t {
            type Err = NameError;
            fn from_str(s: &str) -> Result<Self, NameError> {
                Self::try_from(s)
            }
        }

        impl From<$t> for String {
            fn from(n: $t) -> String {
                n.0
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

name_type!(
    /// Name of a fleet.
    FleetName, "fleet"
);
name_type!(
    /// Name of a crew within a fleet.
    CrewName, "crew"
);
name_type!(
    /// Name of an agent within a crew.
    AgentName, "agent"
);

/// Fully-qualified agent identity, written `fleet/crew/agent`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentId {
    pub fleet: FleetName,
    pub crew: CrewName,
    pub agent: AgentName,
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.fleet, self.crew, self.agent)
    }
}

impl FromStr for AgentId {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, NameError> {
        let mut parts = s.splitn(4, '/');
        let (Some(f), Some(c), Some(a), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(NameError {
                kind: "agent id",
                value: s.to_string(),
                reason: "expected exactly fleet/crew/agent",
            });
        };
        Ok(Self {
            fleet: f.parse()?,
            crew: c.parse()?,
            agent: a.parse()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_dns_label_like_names() {
        let longest = "x".repeat(63);
        for ok in [
            "a",
            "payments",
            "backend-1",
            "0abc",
            "a-b-c",
            longest.as_str(),
        ] {
            assert!(FleetName::try_from(ok).is_ok(), "{ok:?} should be valid");
        }
    }

    #[test]
    fn rejects_bad_names_with_a_reason() {
        let too_long = "x".repeat(64);
        let cases = [
            ("", "empty"),
            (too_long.as_str(), "longer than 63 characters"),
            (
                "Payments",
                "contains characters other than a-z, 0-9 and '-'",
            ),
            (
                "back_end",
                "contains characters other than a-z, 0-9 and '-'",
            ),
            ("-lead", "starts or ends with '-'"),
            ("trail-", "starts or ends with '-'"),
            ("a/b", "contains characters other than a-z, 0-9 and '-'"),
        ];
        for (bad, reason) in cases {
            let err = CrewName::try_from(bad).unwrap_err();
            assert_eq!(err.kind, "crew");
            assert_eq!(err.value, bad);
            assert_eq!(err.reason, reason, "for {bad:?}");
        }
    }

    #[test]
    fn error_message_names_the_kind_value_and_reason() {
        let err = AgentName::try_from("Bob").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid agent name \"Bob\": contains characters other than a-z, 0-9 and '-'"
        );
    }

    #[test]
    fn serde_deserialization_validates() {
        assert!(serde_json::from_str::<FleetName>("\"ok-name\"").is_ok());
        assert!(serde_json::from_str::<FleetName>("\"Not Ok\"").is_err());
        assert_eq!(
            serde_json::to_string(&FleetName::try_from("f").unwrap()).unwrap(),
            "\"f\""
        );
    }

    #[test]
    fn agent_id_displays_and_parses_as_three_segments() {
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        assert_eq!(id.fleet.as_str(), "payments");
        assert_eq!(id.crew.as_str(), "backend");
        assert_eq!(id.agent.as_str(), "alice");
        assert_eq!(id.to_string(), "payments/backend/alice");
        assert!("payments/backend".parse::<AgentId>().is_err());
        assert!("a/b/c/d".parse::<AgentId>().is_err());
        assert!("a/B/c".parse::<AgentId>().is_err());
    }

    proptest! {
        #[test]
        fn every_generated_valid_name_parses(s in "[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?") {
            prop_assert!(FleetName::try_from(s.as_str()).is_ok());
        }

        #[test]
        fn parse_then_display_is_identity(s in "[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?") {
            let n = AgentName::try_from(s.as_str()).unwrap();
            prop_assert_eq!(n.to_string(), s);
        }
    }
}
