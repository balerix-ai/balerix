//! The review submission (Spec C §4.4): the body the page posts, its
//! limits, and the one message the agent receives through `send_text`.

use balerix_api::check_path;
use serde::Deserialize;

pub const MAX_COMMENTS: usize = 200;
/// `summary` plus every comment `body`, in bytes.
pub const MAX_BODY_BYTES: usize = 64 << 10;
/// One quoted diff line, in bytes.
pub const MAX_TEXT_BYTES: usize = 4096;
/// `head` and `base_ref` each, in bytes. A ref name is short; the field
/// is otherwise a free 4 KiB × 2 in the message header.
pub const MAX_REF_BYTES: usize = 256;
/// The whole rendered message, in bytes. The per-field caps do not bound
/// it on their own — 200 comments each carry a 4096-byte `path` and a
/// 4096-byte `text` beside the capped bodies — and the message travels
/// through the daemon into a tmux paste buffer.
pub const MAX_MESSAGE_BYTES: usize = 256 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::Old => "old",
            Side::New => "new",
        })
    }
}

/// One line comment: where it was made and the diff line it was made
/// on (`text`, sign included), so the agent can find the place even
/// after the tree moved (PC-6).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comment {
    pub path: String,
    pub side: Side,
    pub line: u64,
    #[serde(default)]
    pub text: String,
    pub body: String,
}

/// What `POST /agents/{id}/review` takes. Every field defaults so a page
/// that loaded no diff can still send a summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewBody {
    pub head: String,
    pub base_ref: String,
    pub summary: String,
    pub comments: Vec<Comment>,
}

/// Nothing downstream renders a NUL usefully: it is not valid in a ref, a
/// path or a diff line, and it truncates wherever the message meets a C
/// string on its way to the pane. `path` is not checked here — the shared
/// `check_path` already refuses NUL, with its own reason.
fn no_nul(field: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{field}: contains NUL"));
    }
    Ok(())
}

/// The 400 text, field path first, or `Ok` for a sendable review. Every
/// field the message renders is bounded here; the rendered message itself
/// is bounded by `MAX_MESSAGE_BYTES` at the route, since `path` and `text`
/// multiply by the comment count.
pub fn validate(body: &ReviewBody) -> Result<(), String> {
    if body.comments.is_empty() && body.summary.trim().is_empty() {
        return Err("nothing to send".into());
    }
    if body.comments.len() > MAX_COMMENTS {
        return Err(format!("comments: more than {MAX_COMMENTS}"));
    }
    if body.head.len() > MAX_REF_BYTES {
        return Err(format!("head: longer than {MAX_REF_BYTES} bytes"));
    }
    if body.base_ref.len() > MAX_REF_BYTES {
        return Err(format!("base_ref: longer than {MAX_REF_BYTES} bytes"));
    }
    no_nul("head", &body.head)?;
    no_nul("base_ref", &body.base_ref)?;
    no_nul("summary", &body.summary)?;
    let mut bytes = body.summary.len();
    for (i, c) in body.comments.iter().enumerate() {
        check_path(&c.path)
            .map_err(|r| format!("comments[{i}].path: workspace: invalid path: {r}"))?;
        if c.path.is_empty() {
            return Err(format!("comments[{i}].path: empty"));
        }
        if c.body.trim().is_empty() {
            return Err(format!("comments[{i}].body: empty"));
        }
        if c.text.len() > MAX_TEXT_BYTES {
            return Err(format!(
                "comments[{i}].text: longer than {MAX_TEXT_BYTES} bytes"
            ));
        }
        no_nul(&format!("comments[{i}].text"), &c.text)?;
        no_nul(&format!("comments[{i}].body"), &c.body)?;
        bytes += c.body.len();
    }
    if bytes > MAX_BODY_BYTES {
        return Err(format!(
            "summary and comment bodies: longer than {MAX_BODY_BYTES} bytes together"
        ));
    }
    Ok(())
}

/// The one message the agent receives (Spec C §4.4): a header, the
/// comments in file then line order each quoting its line, and the
/// summary as `Overall:` when there is one. No trailing newline: the
/// runner's `submit` adds the Enter.
pub fn render_message(body: &ReviewBody) -> String {
    let mut comments: Vec<&Comment> = body.comments.iter().collect();
    comments.sort_by(|a, b| {
        (a.path.as_str(), a.line, a.side as u8).cmp(&(b.path.as_str(), b.line, b.side as u8))
    });
    let n = comments.len();
    let short: String = body.head.chars().take(7).collect();
    let mut out = String::from("Review");
    if !body.base_ref.is_empty() {
        out.push_str(&format!(" against {}", body.base_ref));
    }
    if !short.is_empty() {
        out.push_str(&format!(" at {short}"));
    }
    out.push_str(&format!(
        " ({n} comment{})\n",
        if n == 1 { "" } else { "s" }
    ));
    for c in comments {
        out.push_str(&format!(
            "\n{} line {} ({}):\n> {}\n{}\n",
            c.path,
            c.line,
            c.side,
            c.text,
            c.body.trim_end()
        ));
    }
    let summary = body.summary.trim();
    if !summary.is_empty() {
        out.push_str(&format!("\nOverall:\n{summary}\n"));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body() -> ReviewBody {
        serde_json::from_value(json!({
            "head": "3f9c2a1d5b7e8c0a4f6d2e1b9a8c7d6e5f4a3b2c",
            "base_ref": "origin/main",
            "summary": "Looks close. Please address the comments above and run the tests.\n",
            "comments": [
                { "path": "src/lib.rs", "side": "old", "line": 80, "text": "-    // TODO",
                  "body": "Good riddance, but the docs still mention this." },
                { "path": "src/lib.rs", "side": "new", "line": 42, "text": "+    let x = foo();",
                  "body": "This unwrap can panic on an empty list; return the error instead.\n" }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn the_message_is_the_spec_example() {
        assert_eq!(validate(&body()), Ok(()));
        assert_eq!(
            render_message(&body()),
            "Review against origin/main at 3f9c2a1 (2 comments)\n\
             \n\
             src/lib.rs line 42 (new):\n\
             > +    let x = foo();\n\
             This unwrap can panic on an empty list; return the error instead.\n\
             \n\
             src/lib.rs line 80 (old):\n\
             > -    // TODO\n\
             Good riddance, but the docs still mention this.\n\
             \n\
             Overall:\n\
             Looks close. Please address the comments above and run the tests."
        );
        let mut one = body();
        one.comments.truncate(1);
        one.summary.clear();
        one.base_ref.clear();
        let m = render_message(&one);
        assert!(m.starts_with("Review at 3f9c2a1 (1 comment)\n\n"), "{m}");
        assert!(!m.contains("Overall:"));
        assert!(!m.ends_with('\n'));
    }

    #[test]
    fn the_limits_are_enforced_with_a_field_path() {
        let mut b = body();
        b.comments.clear();
        b.summary.clear();
        assert_eq!(validate(&b), Err("nothing to send".to_string()));
        b.summary = "just a summary".into();
        assert_eq!(validate(&b), Ok(()), "a summary alone is a review");
        let mut b = body();
        b.comments[1].path = "../etc/passwd".into();
        assert_eq!(
            validate(&b),
            Err("comments[1].path: workspace: invalid path: \"..\" segment".to_string())
        );
        let mut b = body();
        b.comments[0].body = String::new();
        assert_eq!(validate(&b), Err("comments[0].body: empty".to_string()));
        let mut b = body();
        b.comments[0].text = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            validate(&b),
            Err(format!(
                "comments[0].text: longer than {MAX_TEXT_BYTES} bytes"
            ))
        );
        let mut b = body();
        b.comments = std::iter::repeat_n(b.comments[0].clone(), MAX_COMMENTS + 1).collect();
        assert_eq!(
            validate(&b),
            Err(format!("comments: more than {MAX_COMMENTS}"))
        );
        let mut b = body();
        b.summary = "y".repeat(MAX_BODY_BYTES);
        assert_eq!(
            validate(&b),
            Err(format!(
                "summary and comment bodies: longer than {MAX_BODY_BYTES} bytes together"
            ))
        );
        assert!(
            serde_json::from_value::<ReviewBody>(json!({ "comments": [], "nope": 1 })).is_err(),
            "unknown fields are refused"
        );
        let minimal: ReviewBody = serde_json::from_value(json!({})).unwrap();
        assert!(minimal.comments.is_empty() && minimal.head.is_empty());
        assert_eq!(Side::Old.to_string(), "old");
    }

    /// `head` and `base_ref` are rendered into the header and were
    /// unbounded; a NUL in any rendered field would end the text early
    /// wherever it is handed on as a C string.
    #[test]
    fn refs_are_bounded_and_no_rendered_field_may_carry_a_nul() {
        let mut b = body();
        b.head = "f".repeat(MAX_REF_BYTES);
        assert_eq!(validate(&b), Ok(()), "256 bytes of head is fine");
        b.head = "f".repeat(MAX_REF_BYTES + 1);
        assert_eq!(
            validate(&b),
            Err(format!("head: longer than {MAX_REF_BYTES} bytes"))
        );
        let mut b = body();
        b.base_ref = "r".repeat(MAX_REF_BYTES + 1);
        assert_eq!(
            validate(&b),
            Err(format!("base_ref: longer than {MAX_REF_BYTES} bytes"))
        );

        let mut b = body();
        b.head = "3f9c\0a1d".into();
        assert_eq!(validate(&b), Err("head: contains NUL".to_string()));
        let mut b = body();
        b.base_ref = "origin/\0main".into();
        assert_eq!(validate(&b), Err("base_ref: contains NUL".to_string()));
        let mut b = body();
        b.summary = "looks\0good".into();
        assert_eq!(validate(&b), Err("summary: contains NUL".to_string()));
        let mut b = body();
        b.comments[1].text = "+ let\0x = 1;".into();
        assert_eq!(
            validate(&b),
            Err("comments[1].text: contains NUL".to_string())
        );
        let mut b = body();
        b.comments[0].body = "fix\0this".into();
        assert_eq!(
            validate(&b),
            Err("comments[0].body: contains NUL".to_string())
        );
        // `path` is the shared rule's job, and it refuses NUL already.
        let mut b = body();
        b.comments[0].path = "src/\0lib.rs".into();
        assert_eq!(
            validate(&b),
            Err("comments[0].path: workspace: invalid path: contains NUL".to_string())
        );
    }

    /// The per-field caps leave the rendered message unbounded: 200
    /// comments, each with a `path` at the 4096-byte path limit and a
    /// 4096-byte `text`, pass `validate` and render past 1.6 MiB. The
    /// route checks the rendered length against `MAX_MESSAGE_BYTES`.
    #[test]
    fn a_valid_review_can_still_render_a_message_past_the_message_cap() {
        let big = ReviewBody {
            head: "3f9c2a1".into(),
            base_ref: "origin/main".into(),
            summary: String::new(),
            comments: std::iter::repeat_n(
                Comment {
                    path: "d/".repeat(2047) + "f",
                    side: Side::New,
                    line: 1,
                    text: "+".repeat(MAX_TEXT_BYTES),
                    body: "no".into(),
                },
                MAX_COMMENTS,
            )
            .collect(),
        };
        assert_eq!(validate(&big), Ok(()), "every field is within its cap");
        let message = render_message(&big);
        assert!(message.len() > MAX_MESSAGE_BYTES, "{} bytes", message.len());
        assert_eq!(MAX_MESSAGE_BYTES, 262144);
        // A review of the size the caps are written for stays well under.
        assert!(render_message(&body()).len() < MAX_MESSAGE_BYTES);
    }
}
