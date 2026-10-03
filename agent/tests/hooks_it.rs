#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.1: the sidecar forwards hooks and fails open.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use std::future::IntoFuture;

use balerix_agent::hooks::{FORWARD_BUDGET, Hooks, router};
use balerix_agent::tls::{client_config, http_client};
use balerix_core::AgentId;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

/// The operator's token: the sidecar forwards with it.
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
/// The sidecar-local secret Claude presents on the hook hop (§7.1).
const SECRET: &str = "5ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2";

/// A fake Daemon: records what arrives, answers a verdict, optionally
/// after a sleep longer than the budget.
#[derive(Default)]
struct Seen(Mutex<Vec<(Option<String>, Value)>>);

async fn fake_events(
    State((seen, slow)): State<(Arc<Seen>, bool)>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    if slow {
        tokio::time::sleep(FORWARD_BUDGET + Duration::from_secs(2)).await;
    }
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    seen.0
        .lock()
        .await
        .push((auth, serde_json::from_slice(&body).unwrap()));
    axum::Json(json!({ "decision": "block", "reason": "no" })).into_response()
}
use axum::response::IntoResponse;

async fn fake_daemon(slow: bool) -> (Arc<Seen>, String) {
    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route("/v1/agents/{f}/{c}/{a}/events", post(fake_events))
        .with_state((seen.clone(), slow));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app).into_future());
    (seen, url)
}

fn ca_file(dir: &std::path::Path) -> std::path::PathBuf {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = ca.self_signed(&key).unwrap();
    let path = dir.join("ca.crt");
    std::fs::write(&path, cert.pem()).unwrap();
    path
}

async fn sidecar(daemon_url: &str) -> (String, Arc<AtomicU64>, mpsc::UnboundedReceiver<String>) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let tls = client_config(&ca_file(dir.path())).unwrap();
    let (events, rx) = mpsc::unbounded_channel();
    let failures = Arc::new(AtomicU64::new(0));
    let hooks = Arc::new(Hooks {
        id: "f/c/a".parse::<AgentId>().unwrap(),
        secret: SECRET.into(),
        token: TOKEN.into(),
        daemon_url: daemon_url.into(),
        http: http_client(&tls, Duration::from_secs(10)).unwrap(),
        failures: failures.clone(),
        events,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, router(hooks)).into_future());
    (url, failures, rx)
}

/// §10.3: the struct holds the local secret and the operator's token.
#[test]
fn debug_redacts_the_secret_and_the_token() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let tls = client_config(&ca_file(dir.path())).unwrap();
    let (events, _rx) = mpsc::unbounded_channel();
    let h = Hooks {
        id: "f/c/a".parse::<AgentId>().unwrap(),
        secret: SECRET.into(),
        token: TOKEN.into(),
        daemon_url: "https://d".into(),
        http: http_client(&tls, Duration::from_secs(10)).unwrap(),
        failures: Arc::new(AtomicU64::new(0)),
        events,
    };
    let shown = format!("{h:?}");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(!shown.contains(TOKEN), "{shown}");
}

async fn post_event(base: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut req = client
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn takes_the_local_secret_forwards_with_the_token_and_returns_the_daemons_answer() {
    let (seen, daemon) = fake_daemon(false).await;
    let (base, failures, mut names) = sidecar(&daemon).await;
    let body =
        r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_input":{"command":"ls"}}"#;
    let (status, text) = post_event(&base, "/v1/agents/f/c/a/events", Some(SECRET), body).await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({ "decision": "block", "reason": "no" })
    );
    let got = seen.0.lock().await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0.as_deref(), Some(&*format!("Bearer {TOKEN}")));
    assert_eq!(got[0].1, serde_json::from_str::<Value>(body).unwrap());
    assert_eq!(names.recv().await.unwrap(), "PreToolUse");
    assert_eq!(failures.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_another_agent_a_bad_token_and_a_bad_body() {
    let (_seen, daemon) = fake_daemon(false).await;
    let (base, _, _) = sidecar(&daemon).await;
    let ok = r#"{"hook_event_name":"Stop"}"#;
    assert_eq!(
        post_event(&base, "/v1/agents/f/c/b/events", Some(SECRET), ok).await,
        (401, r#"{"error":"unknown agent or bad secret"}"#.into())
    );
    assert_eq!(
        post_event(&base, "/v1/agents/f/c/a/events", Some("nope"), ok)
            .await
            .0,
        401
    );
    // §10.4: the operator's token is the sidecar's, not Claude's; the hook
    // hop takes only the local secret
    assert_eq!(
        post_event(&base, "/v1/agents/f/c/a/events", Some(TOKEN), ok)
            .await
            .0,
        401
    );
    assert_eq!(
        post_event(&base, "/v1/agents/f/c/a/events", None, ok)
            .await
            .0,
        401
    );
    assert_eq!(
        post_event(&base, "/v1/agents/f/c/a/events", Some(SECRET), "[1]").await,
        (400, r#"{"error":"body must be a JSON object"}"#.into())
    );
    assert_eq!(
        post_event(&base, "/v1/agents/f/c/a/events", Some(SECRET), r#"{"x":1}"#).await,
        (
            400,
            r#"{"error":"hook_event_name must be a string"}"#.into()
        )
    );
}

/// Review Focus 4: a Daemon that is down, and one that is too slow, both
/// get the empty chain's answer inside the budget, and are counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_is_down_or_slow_fails_open_inside_the_budget() {
    // down: a port nobody listens on
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let (base, failures, _) = sidecar(&closed).await;
    let start = Instant::now();
    let (status, text) = post_event(
        &base,
        "/v1/agents/f/c/a/events",
        Some(SECRET),
        r#"{"hook_event_name":"Stop"}"#,
    )
    .await;
    assert_eq!((status, text.as_str()), (200, "{}"));
    assert!(start.elapsed() < FORWARD_BUDGET, "{:?}", start.elapsed());
    assert_eq!(failures.load(Ordering::Relaxed), 1);

    // slow: answers after the budget
    let (_seen, slow) = fake_daemon(true).await;
    let (base, failures, _) = sidecar(&slow).await;
    let start = Instant::now();
    let (status, text) = post_event(
        &base,
        "/v1/agents/f/c/a/events",
        Some(SECRET),
        r#"{"hook_event_name":"Stop"}"#,
    )
    .await;
    assert_eq!((status, text.as_str()), (200, "{}"));
    assert!(
        start.elapsed() < FORWARD_BUDGET + Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(failures.load(Ordering::Relaxed), 1);
}

/// One budget covers the whole forward: a Daemon that sends its headers
/// inside the budget and then stalls the body is answered `200 {}` inside one budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_stalls_the_body_fails_open_inside_one_budget() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daemon = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                // headers late in the budget, so a second budget would show
                tokio::time::sleep(Duration::from_secs(2)).await;
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{",
                )
                .await
                .unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    let (base, failures, _) = sidecar(&daemon).await;
    let start = Instant::now();
    let (status, text) = post_event(
        &base,
        "/v1/agents/f/c/a/events",
        Some(SECRET),
        r#"{"hook_event_name":"Stop"}"#,
    )
    .await;
    assert_eq!((status, text.as_str()), (200, "{}"));
    assert!(
        start.elapsed() < FORWARD_BUDGET + Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(failures.load(Ordering::Relaxed), 1);
}
