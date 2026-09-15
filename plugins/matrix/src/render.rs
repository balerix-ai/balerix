//! Every message body the plugin sends (Spec G §8), as pure functions.
//! Output is markdown; the adapter turns it into a plain `body` and an
//! HTML `formatted_body`.

use balerix_api::{AgentPhase, HookEvent};
use serde_json::Value;

/// One message holds this much (Spec G §8): comfortably under the 64 KiB
/// event limit a homeserver enforces, and the limit OpenClaw defaults to.
/// A longer body is split across messages by `split`, never cut — the
/// ceiling is readability on a phone, not the protocol's.
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

/// A turn's heading followed by what the assistant actually said. The
/// hook payload carries it, so no transcript read is needed; an empty or
/// absent message leaves the heading alone.
fn with_message(payload: &Value, heading: &str) -> String {
    match payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(message) => format!("{heading}\n\n{message}"),
        None => heading.to_string(),
    }
}

pub fn event_message(event: &HookEvent) -> String {
    let p = &event.payload;
    match event.name.as_str() {
        "SessionStart" => format!("session restarted ({})", text(p, "source", "unknown")),
        "SessionEnd" => format!("**session ended** ({})", text(p, "reason", "unknown")),
        "Notification" => format!("**needs you:** {}", text(p, "message", "notification")),
        "Stop" => with_message(p, "**turn finished**"),
        "SubagentStop" => with_message(p, "subagent finished"),
        "UserPromptSubmit" => format!("**prompt**\n\n{}", text(p, "prompt", "(empty)")),
        "PreToolUse" => format!("running {}", tool_summary(p)),
        "PostToolUse" => format!("finished {}", tool_summary(p)),
        "PreCompact" => format!("compacting ({})", text(p, "trigger", "unknown")),
        other => other.to_string(),
    }
}

pub fn phase_message(change: &PhaseChange) -> String {
    let base = format!("phase **{:?}** to **{:?}**", change.from, change.to);
    if change.message.trim().is_empty() {
        base
    } else {
        format!("{base}: {}", change.message)
    }
}

/// Room left in every part for its `(n/N)` marker and, inside a code
/// block, the fence this chunker closes and reopens around the break.
const PART_OVERHEAD: usize = 64;

/// An open code fence: its marker (``` or ~~~, however many characters)
/// and the info string to repeat when reopening it.
#[derive(Clone, PartialEq, Eq)]
struct Fence {
    marker: String,
    info: String,
}

/// The fence a line opens or closes, if it is a fence line at all.
fn fence_of(line: &str) -> Option<Fence> {
    let t = line.trim_start();
    let ch = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let marker: String = t.chars().take_while(|c| *c == ch).collect();
    (marker.len() >= 3).then(|| Fence {
        info: t[marker.len()..].trim().to_string(),
        marker,
    })
}

/// `text` as chunks that each fit `budget`, broken at line boundaries
/// where possible and at a character boundary otherwise. A chunk that
/// ends inside a code fence closes it, and the next one reopens it.
fn chunks(text: &str, budget: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut open: Option<Fence> = None;
    let mut rest = text;
    while !rest.is_empty() {
        // Reopening costs the fence line; closing costs one more.
        let reopen = open
            .as_ref()
            .map(|f| format!("{}{}\n", f.marker, f.info))
            .unwrap_or_default();
        let room = budget.saturating_sub(reopen.len());
        if rest.len() <= room && open.is_none() {
            out.push(format!("{reopen}{rest}"));
            break;
        }
        // Largest character boundary within the room, preferring the last
        // line break so a chunk holds whole lines.
        let mut end = room.min(rest.len());
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        let head = &rest[..end];
        let cut = match head.rfind('\n') {
            // +1 keeps the newline with the chunk it ends.
            Some(i) if i > 0 => i + 1,
            _ => end,
        };
        let (piece, tail) = rest.split_at(cut);
        // Track the fence state this piece leaves behind.
        for line in piece.lines() {
            match (&open, fence_of(line)) {
                (None, Some(f)) => open = Some(f),
                (Some(cur), Some(f)) if f.marker.starts_with(&cur.marker) && f.info.is_empty() => {
                    open = None;
                }
                _ => {}
            }
        }
        let close = open
            .as_ref()
            .map(|f| {
                let nl = if piece.ends_with('\n') { "" } else { "\n" };
                format!("{nl}{}\n", f.marker)
            })
            .unwrap_or_default();
        out.push(format!("{reopen}{piece}{close}"));
        rest = tail;
        if tail.is_empty() {
            break;
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// `text` as the messages to post, in order. One part when it fits;
/// otherwise chunks that each fit `BODY_LIMIT`, every part marked
/// `(n/N)`. Past `max_parts` the last part says how many were dropped —
/// the only case that loses text.
pub fn split(text: &str, max_parts: usize) -> Vec<String> {
    if text.len() <= BODY_LIMIT {
        return vec![text.to_string()];
    }
    let max_parts = max_parts.max(1);
    let mut parts = chunks(text, BODY_LIMIT - PART_OVERHEAD);
    if parts.len() <= 1 {
        return parts;
    }
    if parts.len() > max_parts {
        let omitted = parts.len() - max_parts;
        parts.truncate(max_parts);
        if let Some(last) = parts.last_mut() {
            let plural = if omitted == 1 { "part" } else { "parts" };
            last.push_str(&format!("\n\n… truncated, {omitted} {plural} omitted"));
        }
    }
    let total = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}\n\n({}/{})", p.trim_end(), i + 1, total))
        .collect()
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
            (
                "Stop",
                json!({ "last_assistant_message": "Fixed the flaky test:\n\n```rust\nassert_eq!(a, b);\n```" }),
            ),
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
    fn split_returns_one_unchanged_part_when_under_the_limit() {
        assert_eq!(split("one\ntwo", 10), vec!["one\ntwo".to_string()]);
    }

    #[test]
    fn split_breaks_at_a_line_boundary() {
        let long = "abcd\n".repeat(2000);
        let parts = split(&long, 10);
        assert!(parts.len() > 1, "must split");
        for (i, p) in parts.iter().enumerate() {
            assert!(p.len() <= BODY_LIMIT, "part {i} is {} bytes", p.len());
        }
        // every part's content is whole "abcd" lines
        for p in &parts {
            let body = p.rsplit_once("\n\n(").map(|(b, _)| b).unwrap_or(p);
            for line in body.lines().filter(|l| !l.is_empty()) {
                assert_eq!(line, "abcd", "split mid-line");
            }
        }
    }

    #[test]
    fn split_marks_each_part_with_its_number() {
        let parts = split(&"abcd\n".repeat(2000), 10);
        let n = parts.len();
        assert!(n > 1);
        for (i, p) in parts.iter().enumerate() {
            assert!(
                p.ends_with(&format!("({}/{})", i + 1, n)),
                "part {} lacks its marker: {:?}",
                i + 1,
                &p[p.len().saturating_sub(20)..]
            );
        }
    }

    #[test]
    fn split_hard_splits_a_single_over_long_line() {
        let long = "x".repeat(BODY_LIMIT * 2);
        let parts = split(&long, 10);
        assert!(parts.len() > 1, "must split a line with no breaks");
        for p in &parts {
            assert!(p.len() <= BODY_LIMIT, "part is {} bytes", p.len());
        }
    }

    #[test]
    fn split_does_not_panic_when_the_limit_lands_inside_a_multi_byte_character() {
        let long = format!("x{}", "é".repeat(BODY_LIMIT));
        let parts = split(&long, 10);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(p.len() <= BODY_LIMIT, "part is {} bytes", p.len());
            assert!(std::str::from_utf8(p.as_bytes()).is_ok());
        }
    }

    #[test]
    fn split_reopens_a_code_fence_across_parts() {
        let body = format!("```\n{}```\n", "line of code\n".repeat(500));
        let parts = split(&body, 10);
        assert!(parts.len() > 1, "must split");
        // Each part must be fence-balanced on its own.
        for (i, p) in parts.iter().enumerate() {
            let fences = p
                .lines()
                .filter(|l| l.trim_start().starts_with("```"))
                .count();
            assert_eq!(
                fences % 2,
                0,
                "part {} has an unbalanced fence:\n{}",
                i + 1,
                p
            );
        }
    }

    #[test]
    fn split_reopens_a_fence_with_its_language_tag() {
        let body = format!("```rust\n{}```\n", "let x = 1;\n".repeat(500));
        let parts = split(&body, 10);
        assert!(parts.len() > 1, "must split");
        for p in parts.iter().skip(1) {
            assert!(
                p.starts_with("```rust"),
                "a continuation must reopen the fence with its tag: {:?}",
                &p[..40.min(p.len())]
            );
        }
    }

    #[test]
    fn split_caps_at_max_parts_and_says_what_was_dropped() {
        let long = "abcd\n".repeat(20_000);
        let parts = split(&long, 3);
        assert_eq!(parts.len(), 3, "capped");
        let last = parts.last().unwrap();
        assert!(
            last.contains("parts omitted"),
            "the last part must say what was dropped: {:?}",
            &last[last.len().saturating_sub(60)..]
        );
        for p in &parts {
            assert!(p.len() <= BODY_LIMIT, "part is {} bytes", p.len());
        }
    }

    #[test]
    fn a_short_session_is_the_first_eight_characters() {
        assert_eq!(short_session("0199aa11-2233-4455"), "0199aa11");
        assert_eq!(short_session("abc"), "abc");
    }
}
