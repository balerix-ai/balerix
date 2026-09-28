//! Mention detection and the names the plugin builds (Spec M §6, §8.2).

/// `@<slug>`, case-insensitive, at a word boundary, outside fenced and
/// inline code.
pub fn mentions(body: &str, slug: &str) -> bool {
    let needle = format!("@{}", slug.to_ascii_lowercase());
    let mut in_fence = false;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // strip inline code spans
        let mut out = String::with_capacity(line.len());
        let mut in_code = false;
        for c in line.chars() {
            if c == '`' {
                in_code = !in_code;
                out.push(' ');
            } else if in_code {
                out.push(' ');
            } else {
                out.push(c);
            }
        }
        let lower = out.to_ascii_lowercase();
        let mut from = 0;
        while let Some(i) = lower[from..].find(&needle) {
            let start = from + i;
            let end = start + needle.len();
            let before_ok = start == 0
                || !lower[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '@');
            let after_ok = !lower[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_');
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

/// `gh-` + `owner/name` lower-cased, every run outside `[a-z0-9]` one
/// `-`, trimmed of `-`, cut to 63 bytes (the daemon's name rule).
pub fn fleet_name(repo: &str) -> String {
    let mut out = String::from("gh-");
    let mut dash = false;
    for c in repo.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    let mut s = trimmed.to_string();
    if s.len() > 63 {
        s.truncate(63);
        s = s.trim_end_matches('-').to_string();
    }
    s
}

/// The names the plugin owns inside a crew (Spec M §6 check 3).
pub fn is_plugin_agent(name: &str) -> bool {
    let n = name
        .strip_prefix("issue-")
        .or_else(|| name.strip_prefix("pr-"));
    n.is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_match_the_slug_at_word_boundaries_case_insensitively() {
        assert!(mentions("@balerix please look", "balerix"));
        assert!(mentions("hey @Balerix, look", "balerix"));
        assert!(mentions("(@balerix)", "balerix"));
        assert!(!mentions("@balerix-bot please", "balerix"));
        assert!(!mentions("email me@balerix", "balerix"));
        assert!(!mentions("nothing here", "balerix"));
    }

    #[test]
    fn mentions_inside_code_are_not_mentions() {
        assert!(!mentions("run `@balerix` here", "balerix"));
        assert!(!mentions("```\n@balerix\n```", "balerix"));
        assert!(!mentions("~~~sh\n@balerix\n~~~", "balerix"));
        assert!(mentions("```\ncode\n```\n@balerix after", "balerix"));
    }

    #[test]
    fn fleet_names_are_sanitised_and_cut() {
        assert_eq!(fleet_name("Acme/Payments"), "gh-acme-payments");
        assert_eq!(fleet_name("acme/my_repo.v2"), "gh-acme-my-repo-v2");
        assert_eq!(fleet_name("a//b--"), "gh-a-b");
        let long = fleet_name(&format!("o/{}", "x".repeat(100)));
        assert_eq!(long.len(), 63);
        assert!(!long.ends_with('-'));
        let cut_on_dash = fleet_name(&format!("o/{}-{}", "x".repeat(58), "y".repeat(10)));
        assert!(cut_on_dash.len() <= 63 && !cut_on_dash.ends_with('-'));
    }

    #[test]
    fn plugin_agent_names_are_issue_and_pr_numbers() {
        assert!(is_plugin_agent("issue-12") && is_plugin_agent("pr-3"));
        assert!(
            !is_plugin_agent("issue-") && !is_plugin_agent("pr-x") && !is_plugin_agent("alice")
        );
    }
}
