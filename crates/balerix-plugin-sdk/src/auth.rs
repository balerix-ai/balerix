//! The bearer check every SDK route runs (plugins spec §18.3): the daemon
//! presents the plugin's own token. Small twins of the daemon's helpers —
//! the SDK depends on `balerix-api` only.

use axum::http::HeaderMap;

/// Length-then-bytes comparison with no early exit on the bytes.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b) {
        acc |= x ^ y;
    }
    acc == 0
}

/// The token of an `Authorization: Bearer <token>` header, trimmed.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get("authorization")?.to_str().ok()?;
    let (scheme, rest) = v.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let tok = rest.trim();
    (!tok.is_empty()).then_some(tok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn bearer_and_constant_time_eq_behave() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert("authorization", HeaderValue::from_static("Bearer  tok-1 "));
        assert_eq!(bearer(&h), Some("tok-1"));
        h.insert("authorization", HeaderValue::from_static("Basic x"));
        assert_eq!(bearer(&h), None);
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
