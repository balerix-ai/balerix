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
/// `send_keys`: the pause after each step when the action names none.
pub const DEFAULT_KEY_DELAY_MS: u64 = 100;
/// The floor is the point of the action (Spec J §2): keys sent with no
/// pause are dropped at a dialog transition.
pub const MIN_KEY_DELAY_MS: u64 = 20;
pub const MAX_KEY_DELAY_MS: u64 = 500;
pub const MAX_KEY_STEPS: usize = 64;
/// Bytes in one `text` step.
pub const MAX_KEY_TEXT: usize = 1024;
/// `steps × delay_ms` may not pass this: the SDK's `Host::action` gives up
/// after 10 s, and a plugin that timed out mid-sequence cannot know what
/// state the dialog is in.
pub const MAX_KEY_SEQUENCE_MS: u64 = 8000;

/// The keys `send_keys` may press. A closed set: the runner matches each to
/// a fixed tmux key name, so nothing from the wire reaches tmux as a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Up,
    Down,
    Enter,
    Escape,
}

/// One step of a `send_keys`: `{"key": …}` or `{"text": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStep {
    Key(Key),
    Text(String),
}

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
    SendKeys {
        steps: Vec<KeyStep>,
        delay_ms: u64,
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
            "send_keys" => {
                reject_extra(&["action", "steps", "delay_ms"])?;
                let steps = obj
                    .get("steps")
                    .cloned()
                    .ok_or_else(|| D::Error::custom("missing field `steps`"))?;
                let steps: Vec<KeyStep> =
                    serde_json::from_value(steps).map_err(D::Error::custom)?;
                let delay_ms = match obj.get("delay_ms") {
                    Some(v) => serde_json::from_value(v.clone()).map_err(D::Error::custom)?,
                    None => DEFAULT_KEY_DELAY_MS,
                };
                Ok(PluginAction::SendKeys { steps, delay_ms })
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
            PluginAction::SendKeys { .. } => "send_keys",
            PluginAction::Restart => "restart",
            PluginAction::Stop => "stop",
        }
    }

    /// What serde cannot say: the bounds of a `send_keys` (Spec J §4.1).
    /// The message starts with the field, for the daemon's 400.
    pub fn validate(&self) -> Result<(), String> {
        let PluginAction::SendKeys { steps, delay_ms } = self else {
            return Ok(());
        };
        if steps.is_empty() || steps.len() > MAX_KEY_STEPS {
            return Err(format!(
                "steps: expected 1 to {MAX_KEY_STEPS} steps, got {}",
                steps.len()
            ));
        }
        if !(MIN_KEY_DELAY_MS..=MAX_KEY_DELAY_MS).contains(delay_ms) {
            return Err(format!(
                "delay_ms: expected {MIN_KEY_DELAY_MS} to {MAX_KEY_DELAY_MS}, got {delay_ms}"
            ));
        }
        let total = steps.len() as u64 * delay_ms;
        if total > MAX_KEY_SEQUENCE_MS {
            return Err(format!(
                "steps: {} steps at {delay_ms} ms is {total} ms, over the {MAX_KEY_SEQUENCE_MS} ms limit",
                steps.len()
            ));
        }
        for (i, step) in steps.iter().enumerate() {
            let KeyStep::Text(text) = step else { continue };
            if text.is_empty() || text.len() > MAX_KEY_TEXT {
                return Err(format!(
                    "steps[{i}].text: expected 1 to {MAX_KEY_TEXT} bytes, got {}",
                    text.len()
                ));
            }
            if text.chars().any(char::is_control) {
                return Err(format!(
                    "steps[{i}].text: a control character is not allowed"
                ));
            }
        }
        Ok(())
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

    #[test]
    fn send_keys_round_trips_and_defaults_its_delay() {
        let wire = json!({
            "action": "send_keys",
            "steps": [{ "key": "down" }, { "text": "teal-ish" }, { "key": "enter" }],
            "delay_ms": 150
        });
        let a: PluginAction = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            a,
            PluginAction::SendKeys {
                steps: vec![
                    KeyStep::Key(Key::Down),
                    KeyStep::Text("teal-ish".into()),
                    KeyStep::Key(Key::Enter),
                ],
                delay_ms: 150,
            }
        );
        assert_eq!(a.label(), "send_keys");
        assert_eq!(serde_json::to_value(&a).unwrap(), wire);

        let defaulted: PluginAction = serde_json::from_value(
            json!({ "action": "send_keys", "steps": [{ "key": "escape" }] }),
        )
        .unwrap();
        assert_eq!(
            defaulted,
            PluginAction::SendKeys {
                steps: vec![KeyStep::Key(Key::Escape)],
                delay_ms: DEFAULT_KEY_DELAY_MS,
            }
        );
    }

    #[test]
    fn send_keys_rejects_what_the_allowlist_does_not_name() {
        let bad = [
            json!({ "action": "send_keys", "steps": [{ "key": "tab" }] }),
            json!({ "action": "send_keys", "steps": [{ "key": "down", "text": "x" }] }),
            json!({ "action": "send_keys", "steps": [{}] }),
            json!({ "action": "send_keys", "steps": [{ "key": "down" }], "x": 1 }),
            json!({ "action": "send_keys" }),
        ];
        for v in bad {
            assert!(
                serde_json::from_value::<PluginAction>(v.clone()).is_err(),
                "{v}"
            );
        }
    }

    #[test]
    fn validate_bounds_steps_delay_text_and_the_whole_sequence() {
        let keys = |n: usize, delay_ms: u64| PluginAction::SendKeys {
            steps: vec![KeyStep::Key(Key::Down); n],
            delay_ms,
        };
        let text = |t: &str| PluginAction::SendKeys {
            steps: vec![KeyStep::Text(t.into())],
            delay_ms: DEFAULT_KEY_DELAY_MS,
        };
        assert_eq!(keys(1, 100).validate(), Ok(()));
        assert_eq!(keys(MAX_KEY_STEPS, 100).validate(), Ok(()));
        assert!(keys(0, 100).validate().unwrap_err().starts_with("steps:"));
        assert!(
            keys(MAX_KEY_STEPS + 1, 100)
                .validate()
                .unwrap_err()
                .starts_with("steps:")
        );
        assert!(keys(1, 19).validate().unwrap_err().starts_with("delay_ms:"));
        assert!(
            keys(1, 501)
                .validate()
                .unwrap_err()
                .starts_with("delay_ms:")
        );
        assert_eq!(keys(16, 500).validate(), Ok(()));
        assert!(
            keys(17, 500)
                .validate()
                .unwrap_err()
                .starts_with("steps: 17 steps at 500 ms")
        );
        assert_eq!(text("teal-ish; really").validate(), Ok(()));
        assert!(
            text("")
                .validate()
                .unwrap_err()
                .starts_with("steps[0].text:")
        );
        assert!(
            text(&"x".repeat(MAX_KEY_TEXT + 1))
                .validate()
                .unwrap_err()
                .starts_with("steps[0].text:")
        );
        assert!(
            text("two\nlines")
                .validate()
                .unwrap_err()
                .contains("control character")
        );
        assert!(text("esc\u{1b}[B").validate().is_err());
        assert_eq!(PluginAction::Stop.validate(), Ok(()));
    }
}
