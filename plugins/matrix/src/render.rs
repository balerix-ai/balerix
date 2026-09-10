//! Every message body the plugin sends (Spec G §8), as pure functions.
//! Output is markdown; the adapter turns it into a plain `body` and an
//! HTML `formatted_body`.

use balerix_api::{AgentPhase, HookEvent};
use serde_json::Value;

/// Bodies are cut here (Spec G §8): comfortably under the 64 KiB event
/// limit a homeserver enforces, and the limit OpenClaw defaults to.
pub const BODY_LIMIT: usize = 4000;

/// One agent's phase transition, from the fleet watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseChange {
    pub agent: String,
    pub from: AgentPhase,
    pub to: AgentPhase,
    pub message: String,
}

/// The first eight characters of a session id: enough to tell two apart
/// in a room, short enough to read on a phone.
pub fn short_session(session_id: &str) -> &str {
    let end = session_id
        .char_indices()
        .nth(8)
        .map(|(i, _)| i)
        .unwrap_or(session_id.len());
    &session_id[..end]
}

/// The message a thread is rooted on.
pub fn thread_root(agent: &str, session_id: &str, source: &str) -> String {
    let name = agent.rsplit('/').next().unwrap_or(agent);
    format!(
        "**{name}** session `{}` started ({source})\n\n`{agent}`",
        short_session(session_id)
    )
}

fn text<'a>(payload: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback)
}

/// A tool call in one line: the field that says what it touched, when the
/// tool has an obvious one.
fn tool_summary(payload: &Value) -> String {
    let name = text(payload, "tool_name", "tool");
    let input = payload.get("tool_input");
    let field = |key: &str| {
        input
            .and_then(|i| i.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match field("command")
        .or_else(|| field("file_path"))
        .or_else(|| field("pattern"))
    {
        Some(detail) => format!("`{name}` {detail}"),
        None => format!("`{name}`"),
    }
}

pub fn event_message(event: &HookEvent) -> String {
    let p = &event.payload;
    let body = match event.name.as_str() {
        "SessionStart" => format!("session restarted ({})", text(p, "source", "unknown")),
        "SessionEnd" => format!("**session ended** ({})", text(p, "reason", "unknown")),
        "Notification" => format!("**needs you:** {}", text(p, "message", "notification")),
        "Stop" => "**turn finished**".to_string(),
        "SubagentStop" => "subagent finished".to_string(),
        "UserPromptSubmit" => format!("**prompt**\n\n{}", text(p, "prompt", "(empty)")),
        "PreToolUse" => format!("running {}", tool_summary(p)),
        "PostToolUse" => format!("finished {}", tool_summary(p)),
        "PreCompact" => format!("compacting ({})", text(p, "trigger", "unknown")),
        other => other.to_string(),
    };
    truncate(&body)
}

pub fn phase_message(change: &PhaseChange) -> String {
    let base = format!("phase **{:?}** to **{:?}**", change.from, change.to);
    let body = if change.message.trim().is_empty() {
        base
    } else {
        format!("{base}: {}", change.message)
    };
    truncate(&body)
}

/// Cut at the last line boundary inside the limit, or at the limit when a
/// single line is longer than it.
pub fn truncate(text: &str) -> String {
    if text.len() <= BODY_LIMIT {
        return text.to_string();
    }
    let mut end = BODY_LIMIT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let cut = head.rfind('\n').map(|i| &head[..i]).unwrap_or(head);
    format!("{cut}\n\n… truncated")
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::event;
    use serde_json::json;

    #[test]
    fn a_thread_root_names_the_agent_the_session_and_the_source() {
        insta::assert_snapshot!(thread_root(
            "payments/backend/alice",
            "0199aa11-2233-4455-6677-889900aabbcc",
            "startup"
        ));
    }

    #[test]
    fn every_event_renders() {
        let cases = [
            ("SessionStart", json!({ "source": "compact" })),
            ("SessionEnd", json!({ "reason": "clear" })),
            (
                "Notification",
                json!({ "message": "Claude needs your permission to use Bash" }),
            ),
            ("Stop", json!({ "stop_hook_active": false })),
            ("SubagentStop", json!({})),
            ("UserPromptSubmit", json!({ "prompt": "run the tests" })),
            (
                "PreToolUse",
                json!({ "tool_name": "Bash", "tool_input": { "command": "cargo test" } }),
            ),
            (
                "PostToolUse",
                json!({ "tool_name": "Write", "tool_input": { "file_path": "src/a.rs" } }),
            ),
            ("PreCompact", json!({ "trigger": "auto" })),
        ];
        let rendered: Vec<String> = cases
            .iter()
            .map(|(name, payload)| {
                format!(
                    "{name}\n{}",
                    event_message(&event("f/c/a", name, payload.clone()))
                )
            })
            .collect();
        insta::assert_snapshot!(rendered.join("\n\n---\n\n"));
    }

    #[test]
    fn a_missing_payload_field_still_renders() {
        let e = event("f/c/a", "Notification", json!({}));
        assert!(!event_message(&e).is_empty());
        let e = event("f/c/a", "PreToolUse", json!({}));
        assert!(!event_message(&e).is_empty());
    }

    #[test]
    fn a_phase_change_names_both_phases_and_the_message() {
        insta::assert_snapshot!(phase_message(&PhaseChange {
            agent: "payments/backend/alice".into(),
            from: AgentPhase::Ready,
            to: AgentPhase::Dead,
            message: "tmux window gone".into(),
        }));
    }

    #[test]
    fn truncate_cuts_at_a_line_boundary_and_says_so() {
        let short = "one\ntwo";
        assert_eq!(truncate(short), short, "under the limit is untouched");
        let long = "abcd\n".repeat(2000);
        let cut = truncate(&long);
        assert!(cut.len() <= BODY_LIMIT + 32, "len {}", cut.len());
        assert!(cut.ends_with("truncated"), "{}", &cut[cut.len() - 40..]);
        assert!(
            cut.trim_end_matches("\n\n… truncated").ends_with("abcd"),
            "cut on a line boundary"
        );
    }

    #[test]
    fn truncate_does_not_panic_when_the_limit_lands_inside_a_multi_byte_character() {
        // "é" is 2 bytes; one leading ASCII byte shifts every following
        // character's start to an odd offset, so BODY_LIMIT (a round 4000)
        // lands on the second byte of the 2000th "é" rather than between
        // two characters.
        let long = format!("x{}", "é".repeat(2000));
        assert_eq!(long.len(), 4001);
        assert!(
            !long.is_char_boundary(BODY_LIMIT),
            "limit must split a character for this test to mean anything"
        );

        let cut = truncate(&long);

        assert!(cut.len() <= BODY_LIMIT + 32, "len {}", cut.len());
        assert!(cut.ends_with("truncated"), "{}", &cut[cut.len() - 40..]);
        let body = cut.trim_end_matches("\n\n… truncated");
        assert!(
            std::str::from_utf8(body.as_bytes()).is_ok(),
            "cut on a real character boundary"
        );
        assert!(body.chars().next_back().is_some());
    }

    #[test]
    fn a_short_session_is_the_first_eight_characters() {
        assert_eq!(short_session("0199aa11-2233-4455"), "0199aa11");
        assert_eq!(short_session("abc"), "abc");
    }
}
