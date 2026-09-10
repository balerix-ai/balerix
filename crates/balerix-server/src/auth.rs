//! Authentication primitives: constant-time comparison, bearer extraction,
//! a per-key token bucket (Phase 3 spec §3.5).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

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

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token bucket per key: `rate_per_sec` refills up to `burst`.
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(rate_per_sec: f64, burst: f64) -> Self {
        Self {
            rate: rate_per_sec,
            burst,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        self.allow_at(key, Instant::now())
    }

    pub fn allow_at(&self, key: &str, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let b = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: self.burst,
            last: now,
        });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.rate).min(self.burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use proptest::prelude::*;
    use std::time::Duration;

    #[test]
    fn constant_time_eq_compares_whole_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn bearer_extracts_the_token_or_nothing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert("authorization", HeaderValue::from_static("Bearer  tok-1 "));
        assert_eq!(bearer(&h), Some("tok-1"));
        h.insert("authorization", HeaderValue::from_static("Basic xyz"));
        assert_eq!(bearer(&h), None);
        h.insert("authorization", HeaderValue::from_static("bearer lower"));
        assert_eq!(bearer(&h), Some("lower"), "scheme is case-insensitive");
    }

    #[test]
    fn bucket_allows_a_burst_then_refills_at_rate() {
        let l = RateLimiter::new(2.0, 3.0);
        let t0 = Instant::now();
        assert!(l.allow_at("a", t0) && l.allow_at("a", t0) && l.allow_at("a", t0));
        assert!(!l.allow_at("a", t0), "burst spent");
        assert!(l.allow_at("b", t0), "keys are independent");
        assert!(
            !l.allow_at("a", t0 + Duration::from_millis(400)),
            "0.8 tokens: not yet"
        );
        assert!(
            l.allow_at("a", t0 + Duration::from_millis(600)),
            "1.2 tokens"
        );
        assert!(!l.allow_at("a", t0 + Duration::from_millis(600)));
        assert!(
            l.allow_at("a", t0 + Duration::from_secs(60)),
            "long idle refills…"
        );
        assert!(l.allow_at("a", t0 + Duration::from_secs(60)));
        assert!(l.allow_at("a", t0 + Duration::from_secs(60)));
        assert!(
            !l.allow_at("a", t0 + Duration::from_secs(60)),
            "…but only to the burst"
        );
    }

    proptest! {
        #[test]
        fn immediate_calls_never_exceed_the_burst(n in 0usize..50, burst in 1u32..10) {
            let l = RateLimiter::new(1.0, f64::from(burst));
            let t = Instant::now();
            let allowed = (0..n).filter(|_| l.allow_at("k", t)).count();
            prop_assert_eq!(allowed, n.min(burst as usize));
        }
    }
}
