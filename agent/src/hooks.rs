//! The sidecar's hook ingress (Spec O §7.1). Claude posts to
//! `127.0.0.1:<port>` with its per-agent secret, exactly as it posts to
//! the daemon on one machine: the same `settings.json` shape, the same
//! `open_port` grant, the same `hook-relay` for `SessionStart`. That
//! secret is the sidecar's own (`state::hook_secret`); each event is
//! forwarded to the Daemon's events route with the operator's token,
//! which never reaches the agent's files (§10.4), and the Daemon's answer
//! returned. A Daemon that cannot be reached inside the
//! budget gets the empty chain's answer, `200 {}`, and the failure is
//! counted: a Daemon outage degrades the fleet, it never breaks the agent.
//! Every accepted event's name also goes to the sidecar's loop, which
//! turns `SessionStart` into `Ready`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection};
use axum::extract::{Path, Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use balerix_core::AgentId;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use balerix_api::body_limit::{Drain, limited, refuse};

/// Under `hook-relay`'s 5 s and Claude's 10 s command timeout; above the
/// Daemon's own 2 s handler timeout, so a slow chain is the Daemon's
/// answer and not ours.
pub const FORWARD_BUDGET: Duration = Duration::from_millis(3000);
/// The Daemon's own cap on an event body.
pub const BODY_LIMIT: usize = 1 << 20;

pub struct Hooks {
    pub id: AgentId,
    /// What Claude presents: the sidecar-local hook secret.
    pub secret: String,
    /// What the sidecar forwards with: the operator's token.
    pub token: String,
    /// `https://host:port`, no path.
    pub daemon_url: String,
    pub http: reqwest::Client,
    /// Events answered for a Daemon that could not be reached.
    pub failures: Arc<AtomicU64>,
    /// Every accepted event's `hook_event_name`.
    pub events: mpsc::UnboundedSender<String>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .field("token", &"<redacted>")
            .field("daemon_url", &self.daemon_url)
            .finish_non_exhaustive()
    }
}

/// The secret is checked before the body is read (`require_secret`, a
/// `route_layer` outside the drain); an over-cap body is then read to its
/// end before its 413 (#168, `body_limit`).
pub fn router(hooks: Arc<Hooks>) -> Router {
    let events = Router::new().route("/v1/agents/{fleet}/{crew}/{agent}/events", post(events));
    limited(events, Drain::new(BODY_LIMIT))
        .route_layer(middleware::from_fn_with_state(
            hooks.clone(),
            require_secret,
        ))
        .with_state(hooks)
}

/// The caller presents this agent's local secret, or is answered 401
/// having had at most `body_limit::REFUSAL_READ` of its body read.
async fn require_secret(
    State(h): State<Arc<Hooks>>,
    path: Result<Path<(String, String, String)>, PathRejection>,
    req: Request,
    next: Next,
) -> Response {
    let ok = match path {
        Ok(Path((fleet, crew, agent))) => {
            format!("{fleet}/{crew}/{agent}") == h.id.to_string()
                && bearer(req.headers())
                    .is_some_and(|t| constant_time_eq(t.as_bytes(), h.secret.as_bytes()))
        }
        Err(_) => false,
    };
    if ok {
        next.run(req).await
    } else {
        refuse(req, unauthorized()).await
    }
}

fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "unknown agent or bad secret")
}

/// Length-then-bytes comparison with no early exit (as the daemon's
/// `auth::constant_time_eq`).
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

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get("authorization")?.to_str().ok()?;
    let (scheme, rest) = v.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let tok = rest.trim();
    (!tok.is_empty()).then_some(tok)
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// `require_secret` has checked the caller.
async fn events(State(h): State<Arc<Hooks>>, body: Result<Bytes, BytesRejection>) -> Response {
    let body = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), &e.body_text()),
    };
    let name = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(map)) => match map.get("hook_event_name") {
            Some(Value::String(s)) => s.clone(),
            _ => return error(StatusCode::BAD_REQUEST, "hook_event_name must be a string"),
        },
        Ok(_) => return error(StatusCode::BAD_REQUEST, "body must be a JSON object"),
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("body is not JSON: {e}")),
    };
    let _ = h.events.send(name.clone());
    let url = format!(
        "{}/v1/agents/{}/events",
        h.daemon_url.trim_end_matches('/'),
        h.id
    );
    // one budget over the whole exchange: the answer, headers and body
    let forward = async {
        let resp = h
            .http
            .post(&url)
            .bearer_auth(&h.token)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        Ok::<_, reqwest::Error>((status, resp.bytes().await?))
    };
    match tokio::time::timeout(FORWARD_BUDGET, forward).await {
        Ok(Ok((status, bytes))) => {
            if (200..300).contains(&status) && bytes.is_empty() {
                return Json(json!({})).into_response();
            }
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                [(CONTENT_TYPE, "application/json")],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => fail_open(&h, &name, &e.to_string()),
        Err(_) => fail_open(&h, &name, "no answer within the budget"),
    }
}

fn fail_open(h: &Hooks, name: &str, why: &str) -> Response {
    h.failures.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        event = name,
        "hook not forwarded, answered as an empty chain: {why}"
    );
    Json(json!({})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_compares_whole_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
