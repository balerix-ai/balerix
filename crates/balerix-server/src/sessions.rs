//! Browser sessions for the proxied plugin routes (plugins spec §18.2): a
//! one-time login code minted for the admin, exchanged by the browser for
//! an in-memory session cookie. The admin token never enters the browser;
//! nothing here is persisted, so sessions die with the daemon.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

use crate::auth::constant_time_eq;
use crate::vault::random_hex;

/// How long a login code may sit unused.
pub const CODE_TTL: Duration = Duration::from_secs(60);
/// How long a browser session lives.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// How long a dead code (used or expired) is remembered, so a second
/// visit to its URL is told what happened instead of "unknown".
pub const TOMBSTONE_TTL: Duration = Duration::from_secs(10 * 60);
pub const COOKIE: &str = "balerix_session";
/// Every proxied path starts here; a login may only redirect below it, and
/// the cookie is scoped to it.
pub const MOUNT_PREFIX: &str = "/v1/plugins/";

/// Why a login code was refused. The page is for a human who pasted a
/// stale URL; naming the reason tells them whether to hurry (expired)
/// or to look for what else fetched the URL first (used).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeRejected {
    /// Never issued, or dead for longer than `TOMBSTONE_TTL`.
    Unknown,
    /// Redeemed once already, `ago` earlier.
    Used { ago: Duration },
    /// Sat unused past `CODE_TTL`; it lapsed `ago` earlier.
    Expired { ago: Duration },
}

impl fmt::Display for CodeRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown login code"),
            Self::Used { ago } => write!(f, "login code already used {}s ago", ago.as_secs()),
            Self::Expired { ago } => write!(
                f,
                "login code expired {}s ago (a code is valid for {}s after it is printed)",
                ago.as_secs(),
                CODE_TTL.as_secs()
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum Fate {
    Used,
    Expired,
}

#[derive(Default)]
struct Inner {
    /// code → expiry
    codes: HashMap<String, Instant>,
    /// dead code → (when it died, how)
    dead: HashMap<String, (Instant, Fate)>,
    /// session id → expiry
    sessions: HashMap<String, Instant>,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<Inner>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn prune(inner: &mut Inner, now: Instant) {
        let Inner {
            codes,
            dead,
            sessions,
        } = inner;
        codes.retain(|code, expiry| {
            let live = *expiry > now;
            if !live {
                dead.insert(code.clone(), (*expiry, Fate::Expired));
            }
            live
        });
        dead.retain(|_, (at, _)| *at + TOMBSTONE_TTL > now);
        sessions.retain(|_, expiry| *expiry > now);
    }

    /// The entry whose key equals `code`, compared in constant time each:
    /// a lookup by key would leak through timing what a guesser is after.
    fn find<'a, V>(map: &'a HashMap<String, V>, code: &str) -> Option<(&'a String, &'a V)> {
        map.iter()
            .find(|(k, _)| constant_time_eq(k.as_bytes(), code.as_bytes()))
    }

    /// A fresh single-use code, 32 random bytes as hex, good for `CODE_TTL`.
    pub fn issue_code(&self) -> String {
        self.issue_code_at(Instant::now())
    }

    pub fn issue_code_at(&self, now: Instant) -> String {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        let code = random_hex(32);
        inner.codes.insert(code.clone(), now + CODE_TTL);
        code
    }

    /// Exchanges a live code for a session id; the code is gone either way.
    pub fn redeem(&self, code: &str) -> Result<String, CodeRejected> {
        self.redeem_at(code, Instant::now())
    }

    pub fn redeem_at(&self, code: &str, now: Instant) -> Result<String, CodeRejected> {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        let Some((key, _)) = Self::find(&inner.codes, code) else {
            return Err(match Self::find(&inner.dead, code) {
                Some((_, (at, Fate::Used))) => CodeRejected::Used {
                    ago: now.saturating_duration_since(*at),
                },
                Some((_, (at, Fate::Expired))) => CodeRejected::Expired {
                    ago: now.saturating_duration_since(*at),
                },
                None => CodeRejected::Unknown,
            });
        };
        let key = key.clone();
        inner.codes.remove(&key);
        inner.dead.insert(key, (now, Fate::Used));
        let id = random_hex(32);
        inner.sessions.insert(id.clone(), now + SESSION_TTL);
        Ok(id)
    }

    pub fn is_valid(&self, id: &str) -> bool {
        self.is_valid_at(id, Instant::now())
    }

    pub fn is_valid_at(&self, id: &str, now: Instant) -> bool {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        inner
            .sessions
            .keys()
            .any(|k| constant_time_eq(k.as_bytes(), id.as_bytes()))
    }
}

/// The value of cookie `name` across every `Cookie` header, if any.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k.trim() == name).then(|| v.trim().to_string())
        })
}

/// The `Set-Cookie` value: HttpOnly, never cross-site, scoped to the mount.
pub fn set_cookie(id: &str) -> String {
    format!("{COOKIE}={id}; HttpOnly; SameSite=Strict; Path={MOUNT_PREFIX}")
}

/// Whether a cookie-authenticated request came from the daemon's own
/// origin (§18.2): `Sec-Fetch-Site` when the browser sends it, else
/// `Origin` — absent on a plain navigation; on a fetch or WebSocket from a
/// page the daemon served, exactly `http://127.0.0.1:<port>` (`localhost`
/// is another origin to a browser) or, through a reverse proxy, an origin
/// whose authority is the request's own `Host`. A WebSocket handshake
/// arrives without `Sec-Fetch-Site` through at least one proxy in the wild.
pub fn same_origin(headers: &HeaderMap, origin: &str) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return matches!(site.trim(), "same-origin" | "none");
    }
    let Some(o) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let o = o.trim().trim_end_matches('/');
    if o == origin {
        return true;
    }
    // Behind a TLS-terminating reverse proxy the browser never sees the
    // daemon's loopback origin: its `Origin` is the proxy's hostname and
    // its scheme https. A browser fills `Host` from the URL the page itself
    // connected to and cannot forge `Origin`, so the two agree only for a
    // page that host served — the classic cross-site WebSocket hijacking
    // check. The scheme is ignored on purpose: the proxy rewrites it.
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    let authority = o.split_once("://").map(|(_, a)| a);
    matches!((host, authority), (Some(h), Some(a)) if !a.is_empty() && a == h)
}

/// Where a login may send the browser: a path under the mount made of
/// `[A-Za-z0-9/._~-]`, so it needs no encoding in a URL and cannot smuggle
/// a query, a fragment, a header or another host. A `.` or `..` segment is
/// rejected too — a browser normalizes it out of the `Location` path per
/// RFC 3986, and it could otherwise walk the redirect back out of the
/// mount. `None` for anything else. A bare mount root, `/v1/plugins/<name>`,
/// gains its trailing slash: without it the path is the admin purge route,
/// and a URL's final `/` is the first thing a copy-paste loses.
pub fn login_target(to: Option<&str>) -> Option<String> {
    let to = to.unwrap_or(MOUNT_PREFIX);
    let charset_ok = to
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'~' | b'-'));
    let no_dot_segments = to.split('/').all(|s| !matches!(s, "." | ".."));
    if !(to.starts_with(MOUNT_PREFIX) && charset_ok && no_dot_segments) {
        return None;
    }
    let rest = &to[MOUNT_PREFIX.len()..];
    let bare_root = !rest.is_empty() && !rest.contains('/');
    Some(if bare_root {
        format!("{to}/")
    } else {
        to.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::time::Duration;

    #[test]
    fn a_code_is_single_use_and_expires() {
        let s = Sessions::new();
        let t0 = Instant::now();
        let code = s.issue_code_at(t0);
        assert_eq!(code.len(), 64);
        assert!(s.redeem_at("nope", t0).is_err());
        let id = s.redeem_at(&code, t0 + Duration::from_secs(30)).unwrap();
        assert_eq!(id.len(), 64);
        assert_ne!(id, code, "the session id is not the code");
        assert!(s.redeem_at(&code, t0).is_err(), "single use");
        let late = s.issue_code_at(t0);
        assert!(
            s.redeem_at(&late, t0 + CODE_TTL + Duration::from_secs(1))
                .is_err(),
            "expired"
        );
        assert!(s.is_valid_at(&id, t0 + Duration::from_secs(3600)));
        assert!(!s.is_valid_at("nope", t0));
        assert!(
            !s.is_valid_at(
                &id,
                t0 + Duration::from_secs(30) + SESSION_TTL + Duration::from_secs(1)
            ),
            "sessions expire"
        );
    }

    #[test]
    fn a_dead_code_says_why_for_a_while() {
        let s = Sessions::new();
        let t0 = Instant::now();
        let sec = Duration::from_secs;
        assert_eq!(s.redeem_at("nope", t0), Err(CodeRejected::Unknown));

        let used = s.issue_code_at(t0);
        assert!(s.redeem_at(&used, t0 + sec(5)).is_ok());
        assert_eq!(
            s.redeem_at(&used, t0 + sec(8)),
            Err(CodeRejected::Used { ago: sec(3) }),
            "a second redemption is told when the first happened"
        );

        let stale = s.issue_code_at(t0);
        assert_eq!(
            s.redeem_at(&stale, t0 + CODE_TTL + sec(30)),
            Err(CodeRejected::Expired { ago: sec(30) }),
            "expiry is measured from the end of the code's life"
        );

        assert_eq!(
            s.redeem_at(&used, t0 + sec(5) + TOMBSTONE_TTL + sec(1)),
            Err(CodeRejected::Unknown),
            "the reason is forgotten after a while"
        );
        assert_eq!(
            s.redeem_at(&stale, t0 + CODE_TTL + TOMBSTONE_TTL + sec(1)),
            Err(CodeRejected::Unknown)
        );

        assert_eq!(CodeRejected::Unknown.to_string(), "unknown login code");
        assert_eq!(
            CodeRejected::Used { ago: sec(3) }.to_string(),
            "login code already used 3s ago"
        );
        assert_eq!(
            CodeRejected::Expired { ago: sec(30) }.to_string(),
            "login code expired 30s ago (a code is valid for 60s after it is printed)"
        );
    }

    #[test]
    fn the_cookie_is_found_among_others_and_rendered_with_its_flags() {
        let mut h = HeaderMap::new();
        assert_eq!(cookie_value(&h, COOKIE), None);
        h.insert(
            "cookie",
            HeaderValue::from_static("a=1; balerix_session=abc; b=2"),
        );
        assert_eq!(cookie_value(&h, COOKIE).as_deref(), Some("abc"));
        h.insert("cookie", HeaderValue::from_static("balerix_session_x=zzz"));
        assert_eq!(cookie_value(&h, COOKIE), None, "prefix is not a match");
        assert_eq!(
            set_cookie("abc"),
            "balerix_session=abc; HttpOnly; SameSite=Strict; Path=/v1/plugins/"
        );
    }

    #[test]
    fn same_origin_prefers_sec_fetch_site_then_origin() {
        let origin = "http://127.0.0.1:7643";
        let with = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_str(v).unwrap());
            }
            h
        };
        assert!(
            same_origin(&with(&[]), origin),
            "a navigation sends neither"
        );
        assert!(same_origin(
            &with(&[("sec-fetch-site", "same-origin")]),
            origin
        ));
        assert!(same_origin(&with(&[("sec-fetch-site", "none")]), origin));
        assert!(!same_origin(
            &with(&[("sec-fetch-site", "cross-site")]),
            origin
        ));
        assert!(!same_origin(
            &with(&[("sec-fetch-site", "same-site")]),
            origin
        ));
        assert!(same_origin(&with(&[("origin", origin)]), origin));
        assert!(same_origin(
            &with(&[("origin", "http://127.0.0.1:7643/")]),
            origin
        ));
        assert!(!same_origin(
            &with(&[("origin", "http://localhost:7643")]),
            origin
        ));
        assert!(!same_origin(
            &with(&[("origin", "http://evil.example")]),
            origin
        ));
        assert!(
            !same_origin(
                &with(&[("sec-fetch-site", "cross-site"), ("origin", origin)]),
                origin
            ),
            "sec-fetch-site wins when present"
        );
    }

    /// Behind a TLS-terminating reverse proxy the browser's `Origin` is the
    /// proxy's hostname, never the daemon's loopback origin; a handshake
    /// without `Sec-Fetch-Site` (a WebSocket) is then judged by whether
    /// `Origin`'s authority is the request's own `Host`.
    #[test]
    fn same_origin_accepts_an_origin_whose_authority_is_the_request_host() {
        let origin = "http://127.0.0.1:7643";
        let with = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_str(v).unwrap());
            }
            h
        };
        let host = "balerix-7643.example.test";
        assert!(same_origin(
            &with(&[
                ("host", host),
                ("origin", "https://balerix-7643.example.test")
            ]),
            origin
        ));
        assert!(
            same_origin(
                &with(&[
                    ("host", host),
                    ("origin", "https://balerix-7643.example.test/")
                ]),
                origin
            ),
            "a trailing slash is tolerated as before"
        );
        assert!(same_origin(
            &with(&[
                ("host", "127.0.0.1:7643"),
                ("origin", "http://127.0.0.1:7643")
            ]),
            origin
        ));
        assert!(
            !same_origin(
                &with(&[("host", host), ("origin", "https://evil.example")]),
                origin
            ),
            "another host"
        );
        assert!(
            !same_origin(
                &with(&[
                    ("host", host),
                    ("origin", "https://balerix-7643.example.test:8443")
                ]),
                origin
            ),
            "another port is another origin"
        );
        assert!(
            !same_origin(
                &with(&[
                    ("host", host),
                    ("origin", "https://balerix-7643.example.test.evil.example")
                ]),
                origin
            ),
            "a prefix is not a match"
        );
        assert!(
            !same_origin(&with(&[("host", host), ("origin", "null")]), origin),
            "an opaque origin"
        );
        assert!(
            !same_origin(
                &with(&[("origin", "https://balerix-7643.example.test")]),
                origin
            ),
            "no Host to agree with"
        );
        assert!(
            !same_origin(
                &with(&[
                    ("sec-fetch-site", "cross-site"),
                    ("host", host),
                    ("origin", "https://balerix-7643.example.test")
                ]),
                origin
            ),
            "sec-fetch-site still wins"
        );
    }

    #[test]
    fn a_mount_root_target_gains_its_trailing_slash() {
        // `/v1/plugins/<name>` without the slash is the admin purge route,
        // not the mount; a `to` that lost its final `/` in a copy-paste
        // must still land on the mount.
        assert_eq!(
            login_target(Some("/v1/plugins/web")).as_deref(),
            Some("/v1/plugins/web/")
        );
        assert_eq!(
            login_target(Some("/v1/plugins/web/agents/x")).as_deref(),
            Some("/v1/plugins/web/agents/x"),
            "deeper paths are the plugin's business"
        );
        assert_eq!(
            login_target(Some("/v1/plugins/web/index.html")).as_deref(),
            Some("/v1/plugins/web/index.html")
        );
    }

    #[test]
    fn a_login_may_only_land_under_the_mount() {
        assert_eq!(login_target(None).as_deref(), Some("/v1/plugins/"));
        assert_eq!(
            login_target(Some("/v1/plugins/web/")).as_deref(),
            Some("/v1/plugins/web/")
        );
        assert_eq!(
            login_target(Some("/v1/plugins/web/agents/f/c/a")).as_deref(),
            Some("/v1/plugins/web/agents/f/c/a")
        );
        assert_eq!(
            login_target(Some("/v1/plugins/web/index.html")).as_deref(),
            Some("/v1/plugins/web/index.html")
        );
        for bad in [
            "/v1/fleets",
            "http://evil.example/v1/plugins/",
            "//evil.example/v1/plugins/",
            "/v1/plugins/web/?x=1",
            "/v1/plugins/web/\r\nSet-Cookie: x",
            "/v1/plugins",
            "",
            "/v1/plugins/../fleets",
            "/v1/plugins/..",
            "/v1/plugins/./web/",
        ] {
            assert_eq!(login_target(Some(bad)), None, "{bad:?}");
        }
    }
}
