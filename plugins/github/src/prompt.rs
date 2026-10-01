//! What the agent is told (Spec M §8.2, §9): the first prompt, and a
//! submitted review as one message.

use balerix_plugin_common::review::{self, Comment, Review, Side};

use crate::config::Kind;
use crate::github::ReviewComment;

/// The facts the first prompt renders.
#[derive(Debug, Clone)]
pub struct Start<'a> {
    pub repo: &'a str,
    pub kind: Kind,
    pub number: u64,
    pub title: &'a str,
    pub url: &'a str,
    pub body: &'a str,
    /// The agent's branch: the PR head, or `balerix/<fleet>/<crew>/<agent>`.
    pub branch: &'a str,
    /// What the branch is taken from (the crew ref) or merged into (the PR base).
    pub base: &'a str,
    pub asker: &'a str,
    /// The mentioning comment, when the mention was not the body itself.
    pub comment: Option<&'a str>,
    /// A row existed before (an idle stop): point at the earlier work.
    pub resumed: bool,
}

/// Spec M §8.2.
pub fn first(s: &Start<'_>) -> String {
    let what = match s.kind {
        Kind::Issue => "issue",
        Kind::Pr => "pull request",
    };
    let branch = match s.kind {
        Kind::Issue => format!("Branch: {} (from {})", s.branch, s.base),
        Kind::Pr => format!("Branch: {} (into {})", s.branch, s.base),
    };
    let mut out = format!(
        "You are attached to {} {what} #{}: {}\n{}\n\n{branch}\n",
        s.repo,
        s.number,
        s.title.trim(),
        s.url
    );
    if s.resumed {
        out.push_str(&format!(
            "\nEarlier work on this {what} is on branch {}; continue from it.\n",
            s.branch
        ));
    }
    out.push('\n');
    out.push_str(s.body.trim());
    out.push('\n');
    if let Some(c) = s.comment {
        out.push_str(&format!("\n---\n@{} asked:\n{}\n", s.asker, c.trim()));
    }
    out
}

/// Spec M §9: `Review by @bob: changes requested, at 3f9c2a1` over
/// common's rendering. `None` when there is nothing to deliver. `more`
/// says the review holds inline comments beyond `comments`.
pub fn review(
    reviewer: &str,
    state: &str,
    commit: &str,
    body: &str,
    comments: &[ReviewComment],
    more: bool,
    base: &str,
) -> Option<String> {
    if body.trim().is_empty() && comments.is_empty() {
        return None;
    }
    let verdict = match state.to_ascii_lowercase().as_str() {
        "approved" => "approved",
        "changes_requested" => "changes requested",
        "commented" => "commented",
        other => other,
    }
    .to_string();
    let short = commit.get(..7).unwrap_or(commit);
    let r = Review {
        head: short.to_string(),
        base_ref: base.to_string(),
        summary: body.trim().to_string(),
        comments: comments
            .iter()
            .map(|c| Comment {
                path: c.path.clone(),
                side: if c.side.eq_ignore_ascii_case("LEFT") {
                    Side::Old
                } else {
                    Side::New
                },
                line: c.line.or(c.original_line).unwrap_or(0),
                text: c.diff_hunk.lines().last().unwrap_or("").to_string(),
                body: c.body.clone(),
            })
            .collect(),
    };
    if let Err(e) = review::validate(&r) {
        return Some(format!(
            "Review by @{reviewer}: {verdict}, at {short}\n(not rendered: {e})"
        ));
    }
    let mut out = format!(
        "Review by @{reviewer}: {verdict}, at {short}\n{}",
        review::render_message(&r)
    );
    if more {
        out.truncate(out.trim_end().len());
        out.push_str(&format!(
            "\n\n(the first {} inline comments; the rest are on GitHub)",
            comments.len()
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start() -> Start<'static> {
        Start {
            repo: "acme/payments",
            kind: Kind::Issue,
            number: 12,
            title: "Refunds double-post ",
            url: "https://github.com/acme/payments/issues/12",
            body: "When a refund is retried it posts twice.\n\nSteps: …",
            branch: "balerix/gh-acme-payments/repo/issue-12",
            base: "main",
            asker: "alice",
            comment: Some("@balerix please fix this"),
            resumed: false,
        }
    }

    #[test]
    fn the_first_prompt_for_an_issue() {
        insta::assert_snapshot!(first(&start()));
    }

    #[test]
    fn the_first_prompt_for_a_pr_and_a_resume() {
        let s = Start {
            kind: Kind::Pr,
            number: 34,
            branch: "feature/refunds",
            base: "main",
            comment: None,
            resumed: true,
            ..start()
        };
        insta::assert_snapshot!(first(&s));
    }

    #[test]
    fn a_review_renders_once_with_the_header_and_skips_an_empty_one() {
        assert_eq!(
            review(
                "bob",
                "APPROVED",
                "3f9c2a1deadbeef",
                "  ",
                &[],
                false,
                "main"
            ),
            None
        );
        let comments = vec![ReviewComment {
            path: "src/lib.rs".into(),
            side: "RIGHT".into(),
            line: Some(42),
            original_line: None,
            diff_hunk: "@@ -1 +1 @@\n+    let x = foo();".into(),
            body: "This unwrap can panic.".into(),
        }];
        let m = review(
            "bob",
            "changes_requested",
            "3f9c2a1deadbeef",
            "Close.",
            &comments,
            false,
            "main",
        )
        .unwrap();
        let truncated = review(
            "bob",
            "changes_requested",
            "3f9c2a1deadbeef",
            "Close.",
            &comments,
            true,
            "main",
        )
        .unwrap();
        assert_eq!(
            truncated,
            format!(
                "{}\n\n(the first 1 inline comments; the rest are on GitHub)",
                m.trim_end()
            )
        );
        insta::assert_snapshot!(m);
        assert!(m.starts_with("Review by @bob: changes requested, at 3f9c2a1\n"));
        assert!(m.contains("src/lib.rs line 42 (new)"));
        assert!(m.contains("> +    let x = foo();"));
    }
}
