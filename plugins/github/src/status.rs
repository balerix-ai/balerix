//! The status comment (Spec M §8.3): one comment per session, edited in
//! place, twenty lines at most.

use std::collections::VecDeque;

use balerix_plugin_common::render::short_session;

/// Lines kept; older ones fall off the top.
pub const MAX_LINES: usize = 20;
/// Characters kept of a line; a longer one ends in `…`, so twenty lines
/// stay far inside a comment's 65 536.
pub const MAX_LINE_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub agent: String,
    pub session: Option<String>,
    pub phase: String,
    lines: VecDeque<(u64, String)>,
}

impl Status {
    pub fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_string(),
            session: None,
            phase: "starting".into(),
            lines: VecDeque::new(),
        }
    }

    /// Adds a line stamped `at` (unix seconds, rendered as UTC hh:mm),
    /// its first line only.
    pub fn push(&mut self, at: u64, text: &str) {
        let mut first = text.lines().next().unwrap_or("").trim().to_string();
        if let Some((cut, _)) = first.char_indices().nth(MAX_LINE_CHARS) {
            first.truncate(cut);
            first.push('…');
        }
        self.lines.push_back((at, first));
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
    }

    pub fn render(&self) -> String {
        let session = self
            .session
            .as_deref()
            .map(|s| format!(" · session `{}`", short_session(s)))
            .unwrap_or_default();
        let mut out = format!(
            "**balerix** · `{}`{session} · **{}**\n",
            self.agent, self.phase
        );
        for (at, line) in &self.lines {
            out.push_str(&format!("\n- {} {line}", hhmm(*at)));
        }
        out
    }
}

fn hhmm(secs: u64) -> String {
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_comment_keeps_twenty_lines_and_names_the_phase() {
        let mut s = Status::new("gh-acme-payments/repo/issue-12");
        s.session = Some("0199aa11-2222-3333".into());
        s.phase = "ready".into();
        s.push(
            14 * 3600 + 2 * 60,
            "session started (startup)\nsecond line ignored",
        );
        s.push(
            14 * 3600 + 5 * 60,
            "needs you: Claude needs your permission to use Bash",
        );
        insta::assert_snapshot!(s.render());
        for i in 0..25 {
            s.push(i * 60, &format!("line {i}"));
        }
        let r = s.render();
        assert_eq!(r.matches("\n- ").count(), MAX_LINES);
        assert!(!r.contains("session started"), "the oldest fell off");
        assert!(r.ends_with("- 00:24 line 24"));
        // a long line is cut at 200 characters, on a char boundary
        s.push(0, &format!("running Bash: {}", "é".repeat(500)));
        let last = s.render().lines().last().unwrap_or("").to_string();
        let text = last.trim_start_matches("- 00:00 ");
        assert_eq!(text.chars().count(), MAX_LINE_CHARS + 1);
        assert!(text.ends_with("é…"), "{text}");
    }
}
