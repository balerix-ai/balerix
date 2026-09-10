//! Daemon ↔ plugin bodies (plugins spec §4.2, §4.3) and the constants both
//! sides agree on. Serde DTOs only.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::HookEvent;

/// Observer batches are cut at this many events (spec §4.3).
pub const OBSERVER_BATCH: usize = 64;
/// Per-plugin observer queue depth; the oldest event is dropped on overflow.
pub const OBSERVER_QUEUE: usize = 1024;
/// The interceptor chain's shared budget inside Claude's 2 s hook timeout.
pub const CHAIN_BUDGET_MS: u64 = 1500;

/// What a verdict may ask the daemon to do (spec §3, §4.3, §8.1).
///
/// `Deserialize` is hand-written rather than derived: serde's
/// `deny_unknown_fields` does not reject extra fields on unit variants of
/// an internally tagged enum (a long-standing serde limitation, see
/// serde-rs/serde#1358), and `Restart`/`Stop` need to reject them like
/// `SendText` does. `Serialize` derives cleanly — the bug is deserialize-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PluginAction {
    SendText {
        text: String,
        #[serde(default)]
        submit: bool,
    },
    Restart,
    Stop,
}

impl<'de> Deserialize<'de> for PluginAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let value = Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| D::Error::custom("expected an object"))?;
        let action = obj
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::custom("missing field `action`"))?;

        let reject_extra = |allowed: &[&str]| -> Result<(), D::Error> {
            for key in obj.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(D::Error::custom(format!("unknown field `{key}`")));
                }
            }
            Ok(())
        };

        match action {
            "send_text" => {
                reject_extra(&["action", "text", "submit"])?;
                let text = obj
                    .get("text")
                    .cloned()
                    .ok_or_else(|| D::Error::custom("missing field `text`"))?;
                let text: String = serde_json::from_value(text).map_err(D::Error::custom)?;
                let submit = match obj.get("submit") {
                    Some(v) => serde_json::from_value(v.clone()).map_err(D::Error::custom)?,
                    None => false,
                };
                Ok(PluginAction::SendText { text, submit })
            }
            "restart" => {
                reject_extra(&["action"])?;
                Ok(PluginAction::Restart)
            }
            "stop" => {
                reject_extra(&["action"])?;
                Ok(PluginAction::Stop)
            }
            other => Err(D::Error::custom(format!("unknown variant `{other}`"))),
        }
    }
}

impl PluginAction {
    /// The wire tag; also the metrics label.
    pub fn label(&self) -> &'static str {
        match self {
            PluginAction::SendText { .. } => "send_text",
            PluginAction::Restart => "restart",
            PluginAction::Stop => "stop",
        }
    }
}

/// `POST /v1/activate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateRequest {
    /// `fleet/crew/agent`.
    pub agent: String,
    /// The agent's resolved config for this plugin.
    pub config: Value,
}

/// `POST /v1/deactivate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeactivateRequest {
    pub agent: String,
}

/// `POST /v1/events`: at most `OBSERVER_BATCH`, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventBatch {
    pub events: Vec<HookEvent>,
}

/// `POST /v1/intercept`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterceptRequest {
    pub event: HookEvent,
    /// The chain's response so far; `{}` for the first plugin.
    pub response_so_far: Value,
    /// What remains of the chain's budget for this call.
    pub deadline_ms: u64,
}

/// The verdict. `response` must be a JSON object; anything else is a
/// failure the daemon skips.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterceptResponse {
    pub response: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<PluginAction>,
}

/// `GET /v1/plugin-host/kv?prefix=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvKeys {
    pub keys: Vec<String>,
}

/// The one text frame an attach socket accepts, both directions of the
/// protocol (plugins spec §18.4): `{ "resize": { "cols", "rows" } }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResizeFrame {
    pub resize: Resize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resize {
    pub cols: u16,
    pub rows: u16,
}

impl ResizeFrame {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            resize: Resize { cols, rows },
        }
    }

    /// What a text frame on an attach socket is. A well-formed resize
    /// with a zero dimension is `ZeroSized`, to be ignored rather than
    /// refused: xterm.js's fit addon reports zeroes for a hidden
    /// container, and closing the terminal over a cosmetic frame would
    /// punish every client that does not guard against it.
    pub fn parse(text: &str) -> TextFrame {
        let Ok(frame) = serde_json::from_str::<Self>(text) else {
            return TextFrame::Malformed;
        };
        if frame.resize.cols >= 1 && frame.resize.rows >= 1 {
            TextFrame::Resize(frame)
        } else {
            TextFrame::ZeroSized
        }
    }
}

/// A text frame on an attach socket, parsed (`ResizeFrame::parse`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextFrame {
    /// A resize with both dimensions at least 1.
    Resize(ResizeFrame),
    /// A resize with a zero dimension: ignored.
    ZeroSized,
    /// Not a resize: the peer closes 1003.
    Malformed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Timestamp;
    use serde_json::json;

    #[test]
    fn actions_are_tagged_by_kind_and_labelled() {
        let a: PluginAction =
            serde_json::from_value(json!({ "action": "send_text", "text": "hi", "submit": true }))
                .unwrap();
        assert_eq!(
            a,
            PluginAction::SendText {
                text: "hi".into(),
                submit: true
            }
        );
        assert_eq!(a.label(), "send_text");
        let r: PluginAction = serde_json::from_value(json!({ "action": "restart" })).unwrap();
        assert_eq!((r.clone(), r.label()), (PluginAction::Restart, "restart"));
        assert_eq!(
            serde_json::to_value(PluginAction::Stop).unwrap(),
            json!({ "action": "stop" })
        );
        let no_submit: PluginAction =
            serde_json::from_value(json!({ "action": "send_text", "text": "x" })).unwrap();
        assert_eq!(
            no_submit,
            PluginAction::SendText {
                text: "x".into(),
                submit: false
            },
            "submit defaults to false"
        );
        assert!(serde_json::from_value::<PluginAction>(json!({ "action": "reboot" })).is_err());
        assert!(
            serde_json::from_value::<PluginAction>(json!({ "action": "stop", "x": 1 })).is_err(),
            "unknown fields rejected"
        );
    }

    #[test]
    fn intercept_bodies_round_trip_and_actions_default_empty() {
        let event = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(5),
            payload: json!({ "tool_name": "Bash" }),
        };
        let req = InterceptRequest {
            event: event.clone(),
            response_so_far: json!({}),
            deadline_ms: 1200,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["deadline_ms"], 1200);
        assert_eq!(v["event"]["name"], "PreToolUse");
        let back: InterceptRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
        let resp: InterceptResponse =
            serde_json::from_value(json!({ "response": { "decision": "block" } })).unwrap();
        assert_eq!(resp.response["decision"], "block");
        assert!(resp.actions.is_empty());
        let batch = EventBatch {
            events: vec![event],
        };
        let back: EventBatch =
            serde_json::from_str(&serde_json::to_string(&batch).unwrap()).unwrap();
        assert_eq!(back, batch);
        let act: ActivateRequest =
            serde_json::from_value(json!({ "agent": "f/c/a", "config": { "k": 1 } })).unwrap();
        assert_eq!(
            (act.agent.as_str(), act.config["k"].as_i64()),
            ("f/c/a", Some(1))
        );
        let de = DeactivateRequest {
            agent: "f/c/a".into(),
        };
        assert_eq!(
            serde_json::to_value(&de).unwrap(),
            json!({ "agent": "f/c/a" })
        );
        let keys: KvKeys = serde_json::from_value(json!({ "keys": ["a", "b/c"] })).unwrap();
        assert_eq!(keys.keys, vec!["a", "b/c"]);
        assert_eq!(
            (OBSERVER_BATCH, OBSERVER_QUEUE, CHAIN_BUDGET_MS),
            (64, 1024, 1500)
        );
    }

    #[test]
    fn resize_frames_round_trip_and_reject_the_malformed() {
        let f = ResizeFrame::new(120, 40);
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#"{"resize":{"cols":120,"rows":40}}"#
        );
        assert_eq!(
            ResizeFrame::parse(r#"{"resize":{"cols":120,"rows":40}}"#),
            TextFrame::Resize(f)
        );
        for zero in [
            r#"{"resize":{"cols":0,"rows":40}}"#,
            r#"{"resize":{"cols":80,"rows":0}}"#,
        ] {
            assert_eq!(ResizeFrame::parse(zero), TextFrame::ZeroSized, "{zero}");
        }
        for bad in [
            "junk",
            r#"{"resize":{"cols":80}}"#,
            r#"{"resize":{"cols":80,"rows":24},"x":1}"#,
            r#"{"cols":80,"rows":24}"#,
        ] {
            assert_eq!(ResizeFrame::parse(bad), TextFrame::Malformed, "{bad}");
        }
    }
}
