//! Hook ingress (Phase 3 spec §3.5): body validation and the axum handler.

use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use balerix_core::AgentId;
use serde_json::Value;

use crate::api::{ApiError, AppState};
use crate::auth::bearer;

/// Claude blocks on the response; the handler is pure, so 2 s is generous.
const HANDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// `POST /v1/agents/{fleet}/{crew}/{agent}/events`. Order (spec §3.5): the
/// path, then the secret — verified against the index in constant time,
/// 401 for a bad one or an unknown agent alike — *then* the per-agent rate
/// limit (429), so an unauthenticated caller can never touch, let alone
/// drain or grow, another agent's bucket. Body next (400, or 413 for the
/// 1 MiB cap), then the fleet's actor and the handler under a timeout
/// (503). Every rejection renders as `ApiError`'s `{ "error": … }` JSON,
/// never axum's own plain-text body.
pub(crate) async fn events(
    State(state): State<AppState>,
    path: Result<Path<(String, String, String)>, PathRejection>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let unauthorized =
        || ApiError::new(StatusCode::UNAUTHORIZED, "unknown agent or bad secret").into_response();
    let Path((fleet, crew, agent)) = match path {
        Ok(p) => p,
        Err(e) => return ApiError::new(e.status(), e.body_text()).into_response(),
    };
    let Ok(id) = format!("{fleet}/{crew}/{agent}").parse::<AgentId>() else {
        return unauthorized();
    };
    let Some(secret) = bearer(&headers) else {
        return unauthorized();
    };
    if !state.daemon.verify_secret(&id, secret).await {
        return unauthorized();
    }
    if !state.limiter.allow(&id.to_string()) {
        return ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }
    let body = match body {
        Ok(b) => b,
        Err(e) => return ApiError::new(e.status(), e.body_text()).into_response(),
    };
    let parsed = match parse_event(&body) {
        Ok(p) => p,
        Err(e) => return ApiError::new(StatusCode::BAD_REQUEST, e).into_response(),
    };
    match tokio::time::timeout(HANDLE_TIMEOUT, state.daemon.event(&id, secret, parsed)).await {
        Ok(Ok(outcome)) => {
            // The verdict's actions run after the response (spec §16.4):
            // Claude is unblocked the moment this JSON is written.
            if !outcome.actions.is_empty() {
                tokio::spawn(
                    state
                        .daemon
                        .clone()
                        .run_actions(id.clone(), outcome.actions.clone()),
                );
            }
            Json(outcome.response).into_response()
        }
        Ok(Err(e)) => ApiError::from(e).into_response(),
        Err(_) => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "hook handling timed out")
            .into_response(),
    }
}

/// The three things the daemon needs from a hook body.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub name: String,
    pub session_id: Option<String>,
    pub payload: Value,
}

/// Validates at the edge: JSON object with a string `hook_event_name`.
/// Unknown names are fine — Claude's list grows. The error is the 400 body.
pub fn parse_event(body: &[u8]) -> Result<ParsedEvent, String> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|e| format!("body is not JSON: {e}"))?;
    let Value::Object(map) = &payload else {
        return Err("body must be a JSON object".to_string());
    };
    let name = match map.get("hook_event_name") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err("hook_event_name must be a string".to_string()),
    };
    let session_id = map
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(ParsedEvent {
        name,
        session_id,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_name_session_and_keeps_the_payload_raw() {
        let body = json!({ "hook_event_name": "PreToolUse", "session_id": "s1", "tool_input": { "command": "ls" } });
        let e = parse_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(e.name, "PreToolUse");
        assert_eq!(e.session_id.as_deref(), Some("s1"));
        assert_eq!(e.payload, body);
        let e = parse_event(br#"{"hook_event_name":"Whatever"}"#).unwrap();
        assert_eq!((e.name.as_str(), e.session_id), ("Whatever", None));
    }

    #[test]
    fn rejects_non_json_non_objects_and_missing_names() {
        assert!(
            parse_event(b"nope")
                .unwrap_err()
                .starts_with("body is not JSON")
        );
        assert_eq!(
            parse_event(b"[1]").unwrap_err(),
            "body must be a JSON object"
        );
        assert_eq!(
            parse_event(br#"{"x":1}"#).unwrap_err(),
            "hook_event_name must be a string"
        );
        assert_eq!(
            parse_event(br#"{"hook_event_name":7}"#).unwrap_err(),
            "hook_event_name must be a string"
        );
    }
}
