//! POSIX `sh` single-quoting. The only way a value reaches `launch.sh`.

/// Wraps `s` in single quotes, escaping embedded single quotes as `'\''`.
/// Always quotes, even safe strings, so the output shape is uniform.
pub fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn quotes_plain_and_awkward_strings() {
        assert_eq!(sh_quote("abc"), "'abc'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("$HOME `x` \"y\" \\"), "'$HOME `x` \"y\" \\'");
    }

    proptest! {
        /// Round-trips through a real shell: `sh -c "printf %s <quoted>"`
        /// must print the original bytes.
        #[test]
        fn round_trips_through_sh(s in "[^\\x00]{0,40}") {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(&s)))
                .output()
                .unwrap();
            prop_assert!(out.status.success());
            prop_assert_eq!(String::from_utf8_lossy(&out.stdout), s);
        }
    }
}
