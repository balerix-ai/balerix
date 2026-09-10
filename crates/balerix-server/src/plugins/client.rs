//! Every daemon → plugin call (plugins spec §4.2) on one `reqwest` client:
//! loopback, plain HTTP/1.1, no proxy, 5 s unless the caller says otherwise.
//! Failures are classified into the four `reason` labels of §4.3.
//!
//! Every call carries the plugin's own token as the bearer (§18.3): the
//! plugin's listener is a loopback port any local process can reach, and
//! this is how it tells the daemon from the rest.

use std::fmt;
use std::time::Duration;

use balerix_api::{
    ActivateRequest, DeactivateRequest, ErrorBody, EventBatch, InterceptRequest, InterceptResponse,
};
use serde::Serialize;
use serde_json::Value;

use super::PluginError;
use super::registry::PluginAddr;

/// Default per-call timeout (§4.2 "5 s unless stated").
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Plugin response bodies are capped like every other body.
const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallFailure {
    Timeout,
    Connect,
    Status { status: u16, message: String },
    Body(String),
}

impl CallFailure {
    /// The metrics `reason` label.
    pub fn reason(&self) -> &'static str {
        match self {
            CallFailure::Timeout => "timeout",
            CallFailure::Connect => "connect",
            CallFailure::Status { .. } => "status",
            CallFailure::Body(_) => "body",
        }
    }
}

impl fmt::Display for CallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallFailure::Timeout => f.write_str("timeout"),
            CallFailure::Connect => f.write_str("connection refused"),
            // A body-less non-2xx (a bare 503 health check) reads as
            // `HTTP 503`, not `HTTP 503: `.
            CallFailure::Status { status, message } if message.is_empty() => {
                write!(f, "HTTP {status}")
            }
            CallFailure::Status { status, message } => write!(f, "HTTP {status}: {message}"),
            CallFailure::Body(m) => write!(f, "bad response body: {m}"),
        }
    }
}

impl From<reqwest::Error> for CallFailure {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            CallFailure::Timeout
        } else if e.is_connect() || e.is_request() {
            CallFailure::Connect
        } else {
            CallFailure::Body(e.to_string())
        }
    }
}

#[derive(Clone)]
pub struct PluginClient {
    http: reqwest::Client,
}

impl fmt::Debug for PluginClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PluginClient")
    }
}

impl PluginClient {
    pub fn new() -> Result<Self, PluginError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(CALL_TIMEOUT)
            .build()
            .map_err(|e| PluginError::Internal(format!("http client: {e}")))?;
        Ok(Self { http })
    }

    fn url(addr: &PluginAddr, path: &str) -> String {
        format!("http://{}{path}", addr.listen)
    }

    /// Reads at most `MAX_BODY` bytes off the wire, chunk by chunk, so an
    /// untrusted plugin cannot force the whole (unbounded) body into memory
    /// before the cap is enforced: as soon as the accumulated length passes
    /// `MAX_BODY` this stops reading and fails, without draining the rest
    /// of the response. A non-2xx becomes `Status` with the `{ "error" }`
    /// message or the raw text.
    async fn body(mut resp: reqwest::Response) -> Result<Vec<u8>, CallFailure> {
        let status = resp.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_BODY {
                return Err(CallFailure::Body(format!(
                    "body exceeds {MAX_BODY} bytes (1 MiB cap)"
                )));
            }
        }
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes).trim().to_string();
            let message = serde_json::from_slice::<ErrorBody>(&bytes)
                .map(|e| e.error)
                .unwrap_or(text);
            return Err(CallFailure::Status { status, message });
        }
        Ok(bytes)
    }

    async fn post<T: Serialize + ?Sized>(
        &self,
        addr: &PluginAddr,
        path: &str,
        body: &T,
        timeout: Duration,
    ) -> Result<Vec<u8>, CallFailure> {
        let resp = self
            .http
            .post(Self::url(addr, path))
            .bearer_auth(&addr.token)
            .timeout(timeout)
            .json(body)
            .send()
            .await?;
        Self::body(resp).await
    }

    pub async fn activate(
        &self,
        addr: &PluginAddr,
        req: &ActivateRequest,
    ) -> Result<(), CallFailure> {
        self.post(addr, "/v1/activate", req, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    pub async fn deactivate(
        &self,
        addr: &PluginAddr,
        req: &DeactivateRequest,
    ) -> Result<(), CallFailure> {
        self.post(addr, "/v1/deactivate", req, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    pub async fn events(&self, addr: &PluginAddr, batch: &EventBatch) -> Result<(), CallFailure> {
        self.post(addr, "/v1/events", batch, CALL_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// `timeout` is what remains of the chain's budget. A `response` that
    /// is not a JSON object is a `Body` failure (§4.3).
    pub async fn intercept(
        &self,
        addr: &PluginAddr,
        req: &InterceptRequest,
        timeout: Duration,
    ) -> Result<InterceptResponse, CallFailure> {
        let bytes = self.post(addr, "/v1/intercept", req, timeout).await?;
        let verdict: InterceptResponse =
            serde_json::from_slice(&bytes).map_err(|e| CallFailure::Body(e.to_string()))?;
        if !matches!(verdict.response, Value::Object(_)) {
            return Err(CallFailure::Body("response is not a JSON object".into()));
        }
        Ok(verdict)
    }

    pub async fn health(&self, addr: &PluginAddr) -> Result<(), CallFailure> {
        let resp = self
            .http
            .get(Self::url(addr, "/v1/health"))
            .bearer_auth(&addr.token)
            .send()
            .await?;
        Self::body(resp).await.map(|_| ())
    }

    pub async fn metrics(
        &self,
        addr: &PluginAddr,
        timeout: Duration,
    ) -> Result<String, CallFailure> {
        let resp = self
            .http
            .get(Self::url(addr, "/v1/metrics"))
            .bearer_auth(&addr.token)
            .timeout(timeout)
            .send()
            .await?;
        let bytes = Self::body(resp).await?;
        String::from_utf8(bytes).map_err(|e| CallFailure::Body(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use balerix_api::{HookEvent, Timestamp};
    use serde_json::{Value, json};

    /// A plugin stub: activate rejects agent "bad", intercept echoes the
    /// event name (or returns a non-object for "PreCompact", or sleeps 3 s
    /// past any short caller timeout for "Stop"), health is fine, metrics
    /// is one family. `oversized_stub` below is a second, dedicated stub
    /// whose `/v1/metrics` answers with a body over the 1 MiB cap.
    async fn stub() -> String {
        let app = Router::new()
            .route(
                "/v1/activate",
                post(|Json(v): Json<Value>| async move {
                    if v["agent"] == "f/c/bad" {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({ "error": "states.working: unknown event" })),
                        )
                    } else {
                        (axum::http::StatusCode::OK, Json(json!({})))
                    }
                }),
            )
            .route("/v1/deactivate", post(|| async { Json(json!({})) }))
            .route("/v1/events", post(|| async { Json(json!({})) }))
            .route(
                "/v1/intercept",
                post(|Json(v): Json<Value>| async move {
                    if v["event"]["name"] == "PreCompact" {
                        Json(json!({ "response": 7 }))
                    } else if v["event"]["name"] == "Stop" {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        Json(json!({ "response": {} }))
                    } else {
                        Json(json!({
                            "response": { "seen": v["event"]["name"], "so_far": v["response_so_far"] },
                            "actions": [{ "action": "stop" }]
                        }))
                    }
                }),
            )
            .route("/v1/health", get(|| async { "ok" }))
            .route(
                "/v1/metrics",
                get(|| async { "# TYPE balerix_plugin_x_up gauge\nbalerix_plugin_x_up 1\n" }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    /// A second, dedicated stub whose `/v1/metrics` answers with a body
    /// one byte over the 1 MiB cap.
    async fn oversized_stub() -> String {
        let app = Router::new().route("/v1/metrics", get(|| async { "x".repeat(MAX_BODY + 1) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    fn event(name: &str) -> HookEvent {
        HookEvent {
            agent: "f/c/a".into(),
            name: name.into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({}),
        }
    }

    #[tokio::test]
    async fn calls_reach_the_plugin_and_classify_failures() {
        let addr = PluginAddr {
            listen: stub().await,
            token: "t".into(),
        };
        let c = PluginClient::new().unwrap();
        c.activate(
            &addr,
            &ActivateRequest {
                agent: "f/c/a".into(),
                config: json!({}),
            },
        )
        .await
        .unwrap();
        let e = c
            .activate(
                &addr,
                &ActivateRequest {
                    agent: "f/c/bad".into(),
                    config: json!({}),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            e,
            CallFailure::Status {
                status: 400,
                message: "states.working: unknown event".into()
            }
        );
        assert_eq!(e.reason(), "status");
        assert_eq!(e.to_string(), "HTTP 400: states.working: unknown event");
        assert_eq!(
            CallFailure::Status {
                status: 503,
                message: String::new()
            }
            .to_string(),
            "HTTP 503",
            "an empty body renders without the trailing colon"
        );
        c.deactivate(
            &addr,
            &DeactivateRequest {
                agent: "f/c/a".into(),
            },
        )
        .await
        .unwrap();
        c.events(
            &addr,
            &EventBatch {
                events: vec![event("Notification")],
            },
        )
        .await
        .unwrap();

        let v = c
            .intercept(
                &addr,
                &InterceptRequest {
                    event: event("PreToolUse"),
                    response_so_far: json!({ "a": 1 }),
                    deadline_ms: 1000,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(v.response["seen"], "PreToolUse");
        assert_eq!(v.response["so_far"]["a"], 1);
        assert_eq!(v.actions, vec![balerix_api::PluginAction::Stop]);
        let e = c
            .intercept(
                &addr,
                &InterceptRequest {
                    event: event("PreCompact"),
                    response_so_far: json!({}),
                    deadline_ms: 1000,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(e.reason(), "body");
        assert!(matches!(e, CallFailure::Body(_)), "{e}");
        let e = c
            .intercept(
                &addr,
                &InterceptRequest {
                    event: event("Stop"),
                    response_so_far: json!({}),
                    deadline_ms: 100,
                },
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert_eq!(e, CallFailure::Timeout);
        let e = c
            .health(&PluginAddr {
                listen: "127.0.0.1:1".into(),
                token: "t".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(e, CallFailure::Connect);
        assert_eq!(e.reason(), "connect");
        c.health(&addr).await.unwrap();
        let text = c.metrics(&addr, Duration::from_millis(500)).await.unwrap();
        assert!(text.starts_with("# TYPE balerix_plugin_x_up"));
    }

    #[tokio::test]
    async fn a_body_over_the_cap_fails_without_being_fully_buffered() {
        let addr = PluginAddr {
            listen: oversized_stub().await,
            token: "t".into(),
        };
        let c = PluginClient::new().unwrap();
        let e = c.metrics(&addr, Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(e.reason(), "body");
        assert!(matches!(e, CallFailure::Body(_)), "{e}");
    }
}
