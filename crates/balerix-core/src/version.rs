//! Tool version exactness, shared by manifest validation (`plugin.rs`) and
//! fleet-config validation (`balerix_config::validate`).

/// Rejects mise's fuzzy forms: `prefix:`/`ref:`/`path:`/`sub-` specs,
/// wildcards, bare `major` / `major.minor` numbers, and anything without a
/// digit — the empty string, the keywords ("latest", "lts", "system") and
/// channel names ("nightly", "stable", "beta", "canary").
pub fn is_exact_version(v: &str) -> bool {
    if v.ends_with(".x") || v.contains('*') || v.contains(':') {
        return false;
    }
    if !v.bytes().any(|b| b.is_ascii_digit()) {
        return false;
    }
    let numeric_parts = v
        .split('.')
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .count();
    let total_parts = v.split('.').count();
    // "22" and "22.11" are fuzzy; "3.7c", "1.0.0-rc.1", "v1.2.3" are exact.
    !(numeric_parts == total_parts && total_parts < 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_versions_are_accepted() {
        for v in [
            "22.11.0",
            "3.7c",
            "2026.9.1",
            "0.75.0",
            "2.1.261",
            "1.0.0-rc.1",
            "v1.2.3",
            "8.30.1",
        ] {
            assert!(is_exact_version(v), "{v:?} should be exact");
        }
    }

    #[test]
    fn fuzzy_versions_are_rejected() {
        for v in [
            "22",
            "22.11",
            "latest",
            "lts",
            "system",
            "22.x",
            "22.11.x",
            "22*",
            "prefix:22",
            "ref:main",
            "sub-1:latest",
            "path:/x",
            "nightly",
            "stable",
            "beta",
            "",
        ] {
            assert!(!is_exact_version(v), "{v:?} should be fuzzy");
        }
    }
}
