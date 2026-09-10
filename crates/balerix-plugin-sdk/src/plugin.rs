//! The daemon → plugin half (plugins spec §4.2, §7): implement `Plugin`,
//! hand it to `serve`. Every method has a no-op default so a plugin
//! implements only what it subscribes to.

use std::future::{Future, IntoFuture};
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use balerix_api::{
    ActivateRequest, DeactivateRequest, ErrorBody, EventBatch, HookEvent, InterceptRequest,
    InterceptResponse,
};
use serde_json::{Value, json};

use crate::auth::{bearer, constant_time_eq};
use crate::{Host, Metrics, SdkError};

pub trait Plugin: Send + Sync + 'static {
    /// `activate`: `Err(message)` rejects the agent's config; the daemon
    /// reports it as `crews.<c>.agents.<a>.plugins.<name>: <message>`.
    /// An `activate` for an agent the plugin already holds replaces that
    /// agent's config in place — a changed `update` and every re-send
    /// after `hello` arrive this way, with no `deactivate` before them —
    /// so a plugin keeping per-agent resources releases the old ones
    /// itself, and a rejection must leave what it held untouched.
    fn activate(
        &self,
        agent: &str,
        config: Value,
    ) -> impl Future<Output = Result<(), String>> + Send {
        let _ = (agent, config);
        async { Ok(()) }
    }
    fn deactivate(&self, agent: &str) -> impl Future<Output = ()> + Send {
        let _ = agent;
        async {}
    }
    /// The daemon-level config from the `hello` reply (plugins spec §2.1).
    /// `serve` calls this once, after `hello` succeeds. `Err(message)`
    /// aborts the server and returns `SdkError::Configure`, so the process
    /// exits 1 and the daemon reports the plugin as not ready. The daemon
    /// may deliver an `activate` before this returns, so a plugin that
    /// needs the config must buffer until it arrives.
    fn configure(&self, config: Value) -> impl Future<Output = Result<(), String>> + Send {
        let _ = config;
        async { Ok(()) }
    }
    fn observe(&self, events: Vec<HookEvent>) -> impl Future<Output = ()> + Send {
        let _ = events;
        async {}
    }
    /// The verdict; default passes `response_so_far` through untouched.
    fn intercept(
        &self,
        event: HookEvent,
        response_so_far: Value,
        deadline_ms: u64,
    ) -> impl Future<Output = InterceptResponse> + Send {
        let _ = (event, deadline_ms);
        async move {
            InterceptResponse {
                response: response_so_far,
                actions: Vec::new(),
            }
        }
    }
    fn health(&self) -> impl Future<Output = Result<(), String>> + Send {
        async { Ok(()) }
    }
    /// The plugin's registry, rendered by the router as Prometheus text;
    /// every family it holds is already `balerix_plugin_<name>_`-prefixed
    /// (plugins spec §17.4). `None` renders an empty body.
    fn metrics(&self) -> Option<&Metrics> {
        None
    }
    /// The plugin's own HTTP surface, mounted by the daemon under
    /// `/v1/plugins/<name>/` when the manifest says `routes: true`
    /// (plugin-protocol §4.1). Served under `/v1/routes` behind the same
    /// bearer check as every other route; the request carries
    /// `X-Balerix-Forwarded-Prefix` for building links.
    fn routes(&self) -> Option<Router> {
        None
    }
}

/// The §4.2 router for `plugin`. Every route, the plugin's own under
/// `/v1/routes` included, needs `Authorization: Bearer <token>` — the
/// daemon presents the plugin's own token (plugins spec §18.3), because
/// the listener is a loopback port any local process can reach.
pub fn router<P: Plugin>(plugin: Arc<P>, token: &str) -> Router {
    let token: Arc<str> = Arc::from(token);
    // The state is applied before nesting so both routers are `Router<()>`.
    let base = Router::new()
        .route("/v1/activate", post(activate::<P>))
        .route("/v1/deactivate", post(deactivate::<P>))
        .route("/v1/events", post(events::<P>))
        .route("/v1/intercept", post(intercept::<P>))
        .route("/v1/health", get(health::<P>))
        .route("/v1/metrics", get(metrics::<P>))
        .with_state(plugin.clone());
    let base = match plugin.routes() {
        Some(routes) => base.nest("/v1/routes", routes),
        None => base,
    };
    base.layer(middleware::from_fn_with_state(token, require_daemon_bearer))
        // daemon → plugin request bodies are capped at 1 MiB (plugin-protocol
        // §1), matching the daemon's own `plugin_api::router` layer
        // (`balerix-server/src/api.rs`); axum's default (2 MiB) is otherwise
        // silently more permissive than the spec promises.
        .layer(DefaultBodyLimit::max(1 << 20))
}

async fn require_daemon_bearer(
    State(token): State<Arc<str>>,
    req: Request,
    next: Next,
) -> Response {
    match bearer(req.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => error(StatusCode::UNAUTHORIZED, "bad daemon token"),
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

async fn activate<P: Plugin>(
    State(p): State<Arc<P>>,
    body: Result<Json<ActivateRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    match p.activate(&req.agent, req.config).await {
        Ok(()) => Json(json!({})).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn deactivate<P: Plugin>(
    State(p): State<Arc<P>>,
    body: Result<Json<DeactivateRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    p.deactivate(&req.agent).await;
    Json(json!({})).into_response()
}

async fn events<P: Plugin>(
    State(p): State<Arc<P>>,
    body: Result<Json<EventBatch>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(batch) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    p.observe(batch.events).await;
    Json(json!({})).into_response()
}

async fn intercept<P: Plugin>(
    State(p): State<Arc<P>>,
    body: Result<Json<InterceptRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let verdict = p
        .intercept(req.event, req.response_so_far, req.deadline_ms)
        .await;
    Json(verdict).into_response()
}

async fn health<P: Plugin>(State(p): State<Arc<P>>) -> Response {
    match p.health().await {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(message) => error(StatusCode::SERVICE_UNAVAILABLE, message),
    }
}

async fn metrics<P: Plugin>(State(p): State<Arc<P>>) -> Response {
    let body = match p.metrics().map(Metrics::render) {
        None => String::new(),
        Some(Ok(text)) => text,
        Some(Err(e)) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

/// A loopback listener on an ephemeral port and its `host:port`.
pub async fn bind() -> Result<(tokio::net::TcpListener, String), SdkError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))?;
    let listen = listener
        .local_addr()
        .map_err(|e| SdkError::Bind(e.to_string()))?
        .to_string();
    Ok((listener, listen))
}

/// Serves the router until the future is dropped.
pub async fn run<P: Plugin>(
    listener: tokio::net::TcpListener,
    plugin: Arc<P>,
    token: &str,
) -> Result<(), SdkError> {
    axum::serve(listener, router(plugin, token))
        .into_future()
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))
}

/// Bind, say hello, serve. Returns only on a bind or hello failure, or
/// when the server stops.
pub async fn serve<P: Plugin>(host: &Host, version: &str, plugin: P) -> Result<(), SdkError> {
    let (listener, listen) = bind().await?;
    serve_on(host, version, plugin, listener, listen).await
}

/// `serve`'s body, taking an already-bound listener so tests can observe
/// its address. If `hello` fails, the spawned server is aborted (and
/// awaited, so the port is free again) before the error is returned — a
/// dropped `JoinHandle` alone would only detach the task, leaking the
/// listener and leaving `axum::serve` running forever.
async fn serve_on<P: Plugin>(
    host: &Host,
    version: &str,
    plugin: P,
    listener: tokio::net::TcpListener,
    listen: String,
) -> Result<(), SdkError> {
    let plugin = Arc::new(plugin);
    let token = host.env().token.clone();
    let listener_plugin = plugin.clone();
    let server = tokio::spawn(async move { run(listener, listener_plugin, &token).await });
    let stop = |server: tokio::task::JoinHandle<Result<(), SdkError>>| async move {
        server.abort();
        let _ = server.await;
    };
    let reply = match host.hello(version, &listen).await {
        Ok(reply) => reply,
        Err(e) => {
            stop(server).await;
            return Err(e);
        }
    };
    if let Err(message) = plugin.configure(reply.config).await {
        stop(server).await;
        return Err(SdkError::Configure(message));
    }
    server.await.map_err(|e| SdkError::Bind(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::Timestamp;
    use serde_json::{Value, json};

    /// A plugin with every method left at its default.
    struct Silent;
    impl Plugin for Silent {}

    async fn post(url: &str, token: &str, body: Value) -> (u16, Value) {
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        let r = c
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    #[tokio::test]
    async fn defaults_accept_everything_and_pass_the_response_through() {
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, Arc::new(Silent), "tok"));
        let base = format!("http://{listen}");
        let (s, _) = post(
            &format!("{base}/v1/activate"),
            "tok",
            json!({ "agent": "f/c/a", "config": {} }),
        )
        .await;
        assert_eq!(s, 200);
        let event = HookEvent {
            agent: "f/c/a".into(),
            name: "Stop".into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({}),
        };
        let (s, v) = post(
            &format!("{base}/v1/intercept"),
            "tok",
            json!({ "event": event, "response_so_far": { "x": 2 }, "deadline_ms": 5 }),
        )
        .await;
        assert_eq!((s, v), (200, json!({ "response": { "x": 2 } })));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            c.get(format!("{base}/v1/health"))
                .bearer_auth("tok")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        assert_eq!(
            c.get(format!("{base}/v1/metrics"))
                .bearer_auth("tok")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn every_route_refuses_a_call_without_the_daemon_bearer() {
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, Arc::new(Silent), "tok"));
        let base = format!("http://{listen}");
        for token in ["", "nope"] {
            let (s, v) = post(
                &format!("{base}/v1/activate"),
                token,
                json!({ "agent": "f/c/a", "config": {} }),
            )
            .await;
            assert_eq!(
                (s, v),
                (401, json!({ "error": "bad daemon token" })),
                "{token:?}"
            );
        }
        // every route, not a sample: the middleware is one, but a route
        // registered outside it would pass unnoticed
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        for (method, path) in [
            ("POST", "/v1/activate"),
            ("POST", "/v1/deactivate"),
            ("POST", "/v1/events"),
            ("POST", "/v1/intercept"),
            ("GET", "/v1/health"),
            ("GET", "/v1/metrics"),
        ] {
            let req = match method {
                "POST" => c.post(format!("{base}{path}")).json(&json!({})),
                _ => c.get(format!("{base}{path}")),
            };
            assert_eq!(
                req.send().await.unwrap().status().as_u16(),
                401,
                "{method} {path} without a bearer"
            );
        }
        assert_eq!(
            c.get(format!("{base}/v1/metrics"))
                .bearer_auth("tok")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
    }

    #[tokio::test]
    async fn routes_are_nested_behind_the_bearer() {
        struct Routed;
        impl Plugin for Routed {
            fn routes(&self) -> Option<Router> {
                Some(
                    Router::new()
                        .route("/", get(|| async { "root" }))
                        .route("/x", get(|| async { "x" })),
                )
            }
        }
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, Arc::new(Routed), "tok"));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        for (path, want) in [("/v1/routes", "root"), ("/v1/routes/x", "x")] {
            let r = c
                .get(format!("http://{listen}{path}"))
                .bearer_auth("tok")
                .send()
                .await
                .unwrap();
            assert_eq!(
                (r.status().as_u16(), r.text().await.unwrap().as_str()),
                (200, want),
                "{path}"
            );
        }
        let r = c
            .get(format!("http://{listen}/v1/routes/x"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status().as_u16(),
            401,
            "the plugin's routes need the bearer too"
        );
        let r = c
            .get(format!("http://{listen}/v1/routes/nope"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn serve_binds_says_hello_and_runs() {
        let fake = crate::testing::FakeHost::start("tok", json!({}), vec![]).await;
        let host = Host::new(fake.env("rec", std::path::Path::new("/s"))).unwrap();
        let handle = tokio::spawn(async move { serve(&host, "0.1.0", Silent).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while fake.hellos().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let listen = fake.hellos()[0].listen.clone();
        assert!(listen.starts_with("127.0.0.1:"));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            c.get(format!("http://{listen}/v1/health"))
                .bearer_auth("tok")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        handle.abort();
    }

    /// serve() must not leak the spawned server (and its listener) when
    /// `hello` fails: dropping a `JoinHandle` only detaches the task, it
    /// does not cancel it.
    #[tokio::test]
    async fn a_failed_hello_stops_the_server_and_frees_the_port() {
        let fake = crate::testing::FakeHost::start("tok", json!({}), vec![]).await;
        let mut env = fake.env("rec", std::path::Path::new("/s"));
        env.token = "wrong".into();
        let host = Host::new(env).unwrap();
        let (listener, listen) = bind().await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve_on(&host, "0.1.0", Silent, listener, listen.clone()),
        )
        .await
        .unwrap();
        assert!(
            matches!(&result, Err(SdkError::Status { status: 401, .. })),
            "{result:?}"
        );
        assert!(fake.hellos().is_empty(), "the daemon never saw a hello");

        // The abort is asynchronous: the aborted task's future (and the
        // listener it owns) is dropped on the runtime's next poll, not
        // synchronously inside `abort()`. Retry briefly rather than
        // asserting on the first attempt.
        let freed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if tokio::net::TcpListener::bind(&listen).await.is_ok() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            freed.is_ok(),
            "port {listen} not freed after the failed hello"
        );
    }

    /// `serve` hands the hello reply's config to `configure`, and a
    /// rejection stops the server instead of leaving it listening.
    #[tokio::test]
    async fn serve_hands_the_hello_config_to_configure_and_a_rejection_stops_it() {
        use crate::testing::FakeHost;
        use std::sync::{Arc, Mutex};

        struct Recorder {
            seen: Arc<Mutex<Vec<Value>>>,
            reject: Option<String>,
        }
        impl Plugin for Recorder {
            async fn configure(&self, config: Value) -> Result<(), String> {
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(config);
                match &self.reject {
                    Some(m) => Err(m.clone()),
                    None => Ok(()),
                }
            }
        }

        let fake = FakeHost::start("tok", json!({ "homeserver": "https://h" }), Vec::new()).await;
        let env = fake.env("matrix", std::path::Path::new("scratch"));
        let host = Host::new(env).unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let err = serve(
            &host,
            "test",
            Recorder {
                seen: seen.clone(),
                reject: Some("bad homeserver".into()),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "configure: bad homeserver");
        let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(seen.len(), 1, "configure called once");
        assert_eq!(seen[0]["homeserver"], "https://h");
    }

    /// A plugin that implements nothing accepts any config.
    #[tokio::test]
    async fn the_default_configure_accepts_anything() {
        assert_eq!(Silent.configure(json!({ "anything": 1 })).await, Ok(()));
    }
}
