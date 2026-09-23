//! The rule for an agent's `branch` (Spec L §6): what `git check-ref-format
//! --branch` accepts, as a pure function with a reason. It lives in this
//! leaf crate so `balerix-config` validates a fleet file with it and a
//! plugin that builds a fleet file can check a name before sending it.
//! The leading-`-` refusal is what keeps a name from being read as a flag
//! when it reaches `git worktree add -b <branch>` (Spec L §7).

/// `Err` is the reason, for `crews.<c>.agents.<a>.branch: <reason>`.
pub fn check_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty".into());
    }
    if name.len() > 255 {
        return Err("longer than 255 bytes".into());
    }
    if name.starts_with('-') {
        return Err("starts with '-'".into());
    }
    if name.starts_with('/') {
        return Err("starts with '/'".into());
    }
    if name.ends_with('/') {
        return Err("ends with '/'".into());
    }
    if name.ends_with('.') {
        return Err("ends with '.'".into());
    }
    if name.contains("//") {
        return Err("contains \"//\"".into());
    }
    if name.contains("..") {
        return Err("contains \"..\"".into());
    }
    if name.contains("@{") {
        return Err("contains \"@{\"".into());
    }
    for c in name.chars() {
        if c.is_ascii_control() {
            return Err("contains a control character".into());
        }
        if c == ' ' {
            return Err("contains a space".into());
        }
        if matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return Err(format!("contains {c:?}"));
        }
    }
    for component in name.split('/') {
        if component.starts_with('.') {
            return Err("a component starts with '.'".into());
        }
        if component.ends_with(".lock") {
            return Err("a component ends with \".lock\"".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_what_git_check_ref_format_branch_accepts() {
        for ok in [
            "main",
            "feature/x",
            "release-1.2",
            "a.b/c-d_e",
            "pr/12/head",
            "x@y",
            "issue#12",
            "ünïcode",
            "a/b.lockfile",
            "v1.0",
            // `check-ref-format --branch` always validates `refs/heads/<name>`,
            // so a name that already looks fully qualified is just another
            // slash-separated ref component, not a special case; and a bare
            // "@" is only the reserved alias for HEAD when it is the *whole*
            // refname, which `refs/heads/@` never is.
            "refs/heads/main",
            "@",
        ] {
            assert_eq!(check_branch_name(ok), Ok(()), "{ok:?}");
        }
    }

    #[test]
    fn refuses_every_git_rule_with_a_reason() {
        for (bad, reason) in [
            ("", "empty"),
            ("-x", "starts with '-'"),
            ("--force", "starts with '-'"),
            ("/main", "starts with '/'"),
            ("main/", "ends with '/'"),
            ("main.", "ends with '.'"),
            ("a//b", "contains \"//\""),
            ("a..b", "contains \"..\""),
            ("a@{1}", "contains \"@{\""),
            ("a b", "contains a space"),
            ("a\tb", "contains a control character"),
            ("a\x7fb", "contains a control character"),
            ("a~1", "contains '~'"),
            ("a^b", "contains '^'"),
            ("a:b", "contains ':'"),
            ("a?b", "contains '?'"),
            ("a*b", "contains '*'"),
            ("a[b", "contains '['"),
            ("a\\b", "contains '\\\\'"),
            (".hidden", "a component starts with '.'"),
            ("a/.b", "a component starts with '.'"),
            ("a.lock", "a component ends with \".lock\""),
            ("a.lock/b", "a component ends with \".lock\""),
        ] {
            assert_eq!(check_branch_name(bad), Err(reason.to_string()), "{bad:?}");
        }
        let long = "x".repeat(256);
        assert_eq!(
            check_branch_name(&long),
            Err("longer than 255 bytes".to_string())
        );
        assert_eq!(check_branch_name(&"x".repeat(255)), Ok(()));
    }
}
