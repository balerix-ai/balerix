//! Hook ingress wire type (architecture spec §8; Phase 3 spec §2). The
//! payload stays raw JSON on purpose: Spec B matches on JSON-pointer paths.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Timestamp;

/// Every Claude Code hook event balerix wires (Phase 2 spec §4.2). One
/// list so the settings writer, the daemon and plugin manifests agree.
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "SubagentStop",
    "PreCompact",
];

/// `#[non_exhaustive]`: outside this crate, build one with
/// [`HookEvent::new`] and [`HookEvent::with_session_id`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HookEvent {
    /// `fleet/crew/agent`.
    pub agent: String,
    /// Claude's `hook_event_name`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub received_at: Timestamp,
    pub payload: Value,
}

impl HookEvent {
    /// An event with no `session_id`.
    pub fn new(
        agent: impl Into<String>,
        name: impl Into<String>,
        received_at: Timestamp,
        payload: Value,
    ) -> Self {
        Self {
            agent: agent.into(),
            name: name.into(),
            session_id: None,
            received_at,
            payload,
        }
    }

    #[must_use]
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_with_and_without_a_session() {
        let e = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(5),
            payload: json!({ "tool_input": { "command": "ls" } }),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("session_id").is_none());
        let back: HookEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
        let built = HookEvent::new("f/c/a", "PreToolUse", Timestamp(5), e.payload.clone());
        assert_eq!(built, e);
        assert_eq!(
            built
                .with_session_id(Some("s".into()))
                .session_id
                .as_deref(),
            Some("s")
        );
    }
}
