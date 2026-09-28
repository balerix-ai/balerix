//! Ingress (Spec M §7): `POST /webhook` on the plugin's own listener,
//! HMAC-verified, deduplicated by delivery id, parsed into
//! `WebhookEvent` and handed to the actor. Answers 202 and never waits
//! on anything (G-11).

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use balerix_plugin_common::config::Secret;
use balerix_plugin_sdk::auth::constant_time_eq;
use balerix_plugin_sdk::metrics::IntCounterVec;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;

/// Bodies beyond this are 413.
pub const MAX_BODY: usize = 1 << 20;
/// Delivery ids remembered.
pub const RING: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub login: String,
    /// `user.type == "Bot"`.
    pub bot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookEvent {
    IssueOpened {
        repo: String,
        installation: u64,
        number: u64,
        author: Author,
        title: String,
        body: String,
        url: String,
    },
    PrOpened {
        repo: String,
        installation: u64,
        number: u64,
        author: Author,
        title: String,
        body: String,
        url: String,
        head: String,
        head_repo: String,
        base: String,
    },
    Comment {
        repo: String,
        installation: u64,
        number: u64,
        author: Author,
        comment_id: u64,
        body: String,
        is_pr: bool,
    },
    Closed {
        repo: String,
        installation: u64,
        number: u64,
        merged: bool,
    },
    ReviewSubmitted {
        repo: String,
        installation: u64,
        number: u64,
        author: Author,
        review_id: u64,
        state: String,
        body: String,
        commit: String,
    },
}

impl WebhookEvent {
    pub fn repo(&self) -> &str {
        match self {
            Self::IssueOpened { repo, .. }
            | Self::PrOpened { repo, .. }
            | Self::Comment { repo, .. }
            | Self::Closed { repo, .. }
            | Self::ReviewSubmitted { repo, .. } => repo,
        }
    }
    pub fn number(&self) -> u64 {
        match self {
            Self::IssueOpened { number, .. }
            | Self::PrOpened { number, .. }
            | Self::Comment { number, .. }
            | Self::Closed { number, .. }
            | Self::ReviewSubmitted { number, .. } => *number,
        }
    }
    pub fn installation(&self) -> u64 {
        match self {
            Self::IssueOpened { installation, .. }
            | Self::PrOpened { installation, .. }
            | Self::Comment { installation, .. }
            | Self::Closed { installation, .. }
            | Self::ReviewSubmitted { installation, .. } => *installation,
        }
    }
}

/// `X-Hub-Signature-256: sha256=<hex>` over the raw body, compared in
/// constant time.
pub fn verify(secret: &Secret, body: &[u8], header: Option<&str>) -> bool {
    let Some(hex) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.expose().as_bytes()) else {
        return false;
    };
    mac.update(body);
    let ours: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    constant_time_eq(ours.as_bytes(), hex.trim().as_bytes())
}

fn s(v: &Value, ptr: &str) -> String {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
fn n(v: &Value, ptr: &str) -> Option<u64> {
    v.pointer(ptr).and_then(Value::as_u64)
}
fn author(v: &Value, ptr: &str) -> Author {
    Author {
        login: s(v, &format!("{ptr}/login")),
        bot: v.pointer(&format!("{ptr}/type")).and_then(Value::as_str) == Some("Bot"),
    }
}

/// The events the plugin reads; everything else is `None`.
pub fn parse(event: &str, p: &Value) -> Option<WebhookEvent> {
    let repo = s(p, "/repository/full_name");
    let installation = n(p, "/installation/id")?;
    if repo.is_empty() {
        return None;
    }
    let action = s(p, "/action");
    match (event, action.as_str()) {
        ("issues", "opened") => Some(WebhookEvent::IssueOpened {
            repo,
            installation,
            number: n(p, "/issue/number")?,
            author: author(p, "/issue/user"),
            title: s(p, "/issue/title"),
            body: s(p, "/issue/body"),
            url: s(p, "/issue/html_url"),
        }),
        ("issues", "closed") => Some(WebhookEvent::Closed {
            repo,
            installation,
            number: n(p, "/issue/number")?,
            merged: false,
        }),
        ("pull_request", "opened") => Some(WebhookEvent::PrOpened {
            repo,
            installation,
            number: n(p, "/pull_request/number")?,
            author: author(p, "/pull_request/user"),
            title: s(p, "/pull_request/title"),
            body: s(p, "/pull_request/body"),
            url: s(p, "/pull_request/html_url"),
            head: s(p, "/pull_request/head/ref"),
            head_repo: s(p, "/pull_request/head/repo/full_name"),
            base: s(p, "/pull_request/base/ref"),
        }),
        ("pull_request", "closed") => Some(WebhookEvent::Closed {
            repo,
            installation,
            number: n(p, "/pull_request/number")?,
            merged: p
                .pointer("/pull_request/merged")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        ("issue_comment", "created") => Some(WebhookEvent::Comment {
            repo,
            installation,
            number: n(p, "/issue/number")?,
            author: author(p, "/comment/user"),
            comment_id: n(p, "/comment/id")?,
            body: s(p, "/comment/body"),
            is_pr: p.pointer("/issue/pull_request").is_some(),
        }),
        ("pull_request_review", "submitted") => Some(WebhookEvent::ReviewSubmitted {
            repo,
            installation,
            number: n(p, "/pull_request/number")?,
            author: author(p, "/review/user"),
            review_id: n(p, "/review/id")?,
            state: s(p, "/review/state"),
            body: s(p, "/review/body"),
            commit: s(p, "/review/commit_id"),
        }),
        _ => None,
    }
}

/// The listener's state.
pub struct Listener {
    secret: Secret,
    sink: Box<dyn Fn(WebhookEvent) + Send + Sync>,
    counters: IntCounterVec,
    seen: Mutex<(VecDeque<String>, HashSet<String>)>,
}

impl Listener {
    pub fn new(
        secret: Secret,
        sink: impl Fn(WebhookEvent) + Send + Sync + 'static,
        counters: IntCounterVec,
    ) -> Arc<Self> {
        Arc::new(Self {
            secret,
            sink: Box::new(sink),
            counters,
            seen: Mutex::new((VecDeque::new(), HashSet::new())),
        })
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/webhook", post(deliver))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
            .with_state(self)
    }

    fn count(&self, event: &str, outcome: &str) {
        self.counters.with_label_values(&[event, outcome]).inc();
    }

    /// `true` when `id` was already seen; remembers it otherwise.
    fn replayed(&self, id: &str) -> bool {
        let mut g = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if g.1.contains(id) {
            return true;
        }
        g.0.push_back(id.to_string());
        g.1.insert(id.to_string());
        while g.0.len() > RING {
            if let Some(old) = g.0.pop_front() {
                g.1.remove(&old);
            }
        }
        false
    }
}

/// The `event` label for a verified delivery: the names `parse` knows,
/// else `other`, so no header value mints a counter child.
fn event_label(event: &str) -> &'static str {
    match event {
        "issues" => "issues",
        "pull_request" => "pull_request",
        "issue_comment" => "issue_comment",
        "pull_request_review" => "pull_request_review",
        "ping" => "ping",
        _ => "other",
    }
}

async fn deliver(State(l): State<Arc<Listener>>, headers: HeaderMap, body: Bytes) -> Response {
    let header = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    if !verify(&l.secret, &body, header("x-hub-signature-256").as_deref()) {
        // Unverified: the event header is the sender's choice, never a label.
        l.count("unverified", "bad_signature");
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let event = header("x-github-event").unwrap_or_default();
    let label = event_label(&event);
    if event == "ping" {
        l.count(label, "handled");
        return StatusCode::OK.into_response();
    }
    if let Some(id) = header("x-github-delivery")
        && l.replayed(&id)
    {
        l.count(label, "duplicate");
        return StatusCode::ACCEPTED.into_response();
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        l.count(label, "ignored");
        return StatusCode::ACCEPTED.into_response();
    };
    match parse(&event, &payload) {
        Some(ev) => {
            l.count(label, "handled");
            (l.sink)(ev);
        }
        None => l.count(label, "ignored"),
    }
    StatusCode::ACCEPTED.into_response()
}

/// Serves `router` on `listener` until the task is dropped.
pub async fn serve(listener: tokio::net::TcpListener, router: Router) -> std::io::Result<()> {
    axum::serve(listener, router).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!(
            "sha256={}",
            mac.finalize()
                .into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }

    #[test]
    fn signatures_verify_in_constant_time_and_need_the_prefix() {
        let secret = Secret::new("s3cret");
        let body = b"{\"x\":1}";
        let good = sign("s3cret", body);
        assert!(verify(&secret, body, Some(&good)));
        assert!(!verify(
            &secret,
            body,
            Some(good.trim_start_matches("sha256="))
        ));
        assert!(!verify(&secret, b"{\"x\":2}", Some(&good)));
        assert!(!verify(&secret, body, Some(&sign("other", body))));
        assert!(!verify(&secret, body, None));
    }

    fn base(repo: &str) -> Value {
        json!({ "repository": { "full_name": repo }, "installation": { "id": 77 } })
    }

    #[test]
    fn the_five_events_parse_and_the_rest_are_none() {
        let mut p = base("acme/api");
        p["action"] = json!("created");
        p["issue"] = json!({ "number": 12, "pull_request": { "url": "x" } });
        p["comment"] =
            json!({ "id": 5, "body": "@balerix go", "user": { "login": "alice", "type": "User" } });
        assert_eq!(
            parse("issue_comment", &p),
            Some(WebhookEvent::Comment {
                repo: "acme/api".into(),
                installation: 77,
                number: 12,
                author: Author {
                    login: "alice".into(),
                    bot: false
                },
                comment_id: 5,
                body: "@balerix go".into(),
                is_pr: true
            })
        );
        let mut p = base("acme/api");
        p["action"] = json!("opened");
        p["pull_request"] = json!({ "number": 34, "title": "t", "body": null, "html_url": "u", "user": { "login": "bot[bot]", "type": "Bot" }, "head": { "ref": "feature/x", "repo": { "full_name": "fork/api" } }, "base": { "ref": "main" } });
        match parse("pull_request", &p).unwrap() {
            WebhookEvent::PrOpened {
                head,
                head_repo,
                base,
                author,
                body,
                ..
            } => {
                assert_eq!(
                    (head.as_str(), head_repo.as_str(), base.as_str()),
                    ("feature/x", "fork/api", "main")
                );
                assert!(author.bot);
                assert_eq!(body, "");
            }
            other => panic!("{other:?}"),
        }
        let mut p = base("acme/api");
        p["action"] = json!("closed");
        p["pull_request"] = json!({ "number": 34, "merged": true });
        assert_eq!(
            parse("pull_request", &p),
            Some(WebhookEvent::Closed {
                repo: "acme/api".into(),
                installation: 77,
                number: 34,
                merged: true
            })
        );
        let mut p = base("acme/api");
        p["action"] = json!("submitted");
        p["pull_request"] = json!({ "number": 34 });
        p["review"] = json!({ "id": 9, "state": "changes_requested", "body": "close", "commit_id": "3f9c2a1dead", "user": { "login": "bob", "type": "User" } });
        assert!(matches!(
            parse("pull_request_review", &p),
            Some(WebhookEvent::ReviewSubmitted { review_id: 9, .. })
        ));
        let mut p = base("acme/api");
        p["action"] = json!("reopened");
        p["issue"] = json!({ "number": 1 });
        assert_eq!(parse("issues", &p), None);
        assert_eq!(
            parse("pull_request_review_comment", &base("acme/api")),
            None
        );
        assert_eq!(
            parse(
                "issues",
                &json!({ "action": "opened", "issue": { "number": 1 } })
            ),
            None,
            "no installation"
        );
    }

    fn started(
        secret: &str,
    ) -> (
        Arc<Listener>,
        std::sync::mpsc::Receiver<WebhookEvent>,
        IntCounterVec,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let metrics = balerix_plugin_sdk::Metrics::new("github");
        let counters = metrics
            .int_counter_vec("webhooks_total", "t", &["event", "outcome"])
            .unwrap();
        let l = Listener::new(
            Secret::new(secret),
            move |e| {
                let _ = tx.send(e);
            },
            counters.clone(),
        );
        (l, rx, counters)
    }

    #[tokio::test]
    async fn the_listener_verifies_dedupes_and_answers_202() {
        let (l, rx, counters) = started("s3cret");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, l.router()));
        let c = reqwest::Client::new();
        let url = format!("http://{addr}/webhook");
        let mut p = base("acme/api");
        p["action"] = json!("created");
        p["issue"] = json!({ "number": 12 });
        p["comment"] =
            json!({ "id": 5, "body": "hi", "user": { "login": "alice", "type": "User" } });
        let body = serde_json::to_vec(&p).unwrap();
        let post = |sig: String, delivery: &str, event: &str, body: Vec<u8>| {
            c.post(&url)
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Delivery", delivery.to_string())
                .header("X-GitHub-Event", event.to_string())
                .body(body)
                .send()
        };
        assert_eq!(
            post(sign("nope", &body), "d1", "issue_comment", body.clone())
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            post(sign("s3cret", &body), "d1", "issue_comment", body.clone())
                .await
                .unwrap()
                .status(),
            202
        );
        assert!(matches!(
            rx.recv().unwrap(),
            WebhookEvent::Comment { comment_id: 5, .. }
        ));
        assert_eq!(
            post(sign("s3cret", &body), "d1", "issue_comment", body.clone())
                .await
                .unwrap()
                .status(),
            202,
            "a replayed delivery"
        );
        assert!(rx.try_recv().is_err(), "enqueued nothing");
        assert_eq!(
            post(sign("s3cret", b"{}"), "d2", "ping", b"{}".to_vec())
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            post(sign("s3cret", &body), "d3", "star", body.clone())
                .await
                .unwrap()
                .status(),
            202
        );
        let big = vec![b' '; MAX_BODY + 1];
        assert_eq!(
            post(sign("s3cret", &big), "d4", "issue_comment", big)
                .await
                .unwrap()
                .status(),
            413
        );
        let get = |event: &str, outcome: &str| counters.with_label_values(&[event, outcome]).get();
        assert_eq!(get("unverified", "bad_signature"), 1);
        assert_eq!(get("issue_comment", "handled"), 1);
        assert_eq!(get("issue_comment", "duplicate"), 1);
        assert_eq!(get("ping", "handled"), 1);
        assert_eq!(get("other", "ignored"), 1);
        let unsigned = c
            .post(&url)
            .header("X-GitHub-Delivery", "d5")
            .header("X-GitHub-Event", "zzz-random")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(unsigned.status(), 401);
        assert_eq!(get("unverified", "bad_signature"), 2);
        assert_eq!(get("other", "ignored"), 1);
        assert_eq!(get("other", "bad_signature"), 0);
        assert_eq!(get("zzz-random", "bad_signature"), 0);
        assert_eq!(get("issue_comment", "bad_signature"), 0);
    }

    #[test]
    fn a_replayed_delivery_is_counted_and_dropped_after_the_ring_wraps() {
        let (l, _rx, _c) = started("s");
        assert!(!l.replayed("a"));
        assert!(l.replayed("a"));
        for i in 0..RING {
            l.replayed(&format!("id{i}"));
        }
        assert!(!l.replayed("a"), "fell off the ring");
    }
}
