//! The daemon → plugin half (plugins spec §4.2, §7): implement `Plugin`,
//! hand it to `serve`. Every method has a no-op default so a plugin
//! implements only what it subscribes to.

use std::future::{Future, IntoFuture};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use balerix_api::{
    ActivateRequest, DeactivateRequest, ErrorBody, EventBatch, HookEvent, InterceptRequest,
    InterceptResponse, PluginManifest,
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
    /// The plugin's own `balerix-plugin.yaml`, usually
    /// `include_str!("../package/balerix-plugin.yaml")` (Spec O §23.1). A
    /// Daemon in Kubernetes mode refuses a `hello` without it and checks its
    /// `needs` against the plugin's grant; one machine reads the package.
    fn manifest(&self) -> Option<&'static str> {
        None
    }
}

/// The manifest `plugin` returns, parsed; `Err` when it does not parse,
/// which `serve` reports before saying hello.
pub fn parse_manifest<P: Plugin>(plugin: &P) -> Result<Option<PluginManifest>, SdkError> {
    plugin
        .manifest()
        .map(|text| {
            serde_norway::from_str(text)
                .map_err(|e| SdkError::Configure(format!("balerix-plugin.yaml: {e}")))
        })
        .transpose()
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
    // daemon → plugin request bodies are capped at 1 MiB (plugin-protocol
    // §1), matching the daemon's own `plugin_api::router` layer
    // (`balerix-server/src/api.rs`); axum's default (2 MiB) is otherwise
    // silently more permissive than the spec promises. An over-limit body
    // is drained before the answer, as on the daemon (#116).
    // The bearer is checked outside the drain: a caller without it is
    // answered before the body is read.
    balerix_api::body_limit::limited(base, balerix_api::body_limit::Drain::new(1 << 20))
        .layer(middleware::from_fn_with_state(token, require_daemon_bearer))
}

async fn require_daemon_bearer(
    State(token): State<Arc<str>>,
    req: Request,
    next: Next,
) -> Response {
    match bearer(req.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => {
            let no = error(StatusCode::UNAUTHORIZED, "bad daemon token");
            balerix_api::body_limit::refuse(req, no).await
        }
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

/// A listener on `addr` (`127.0.0.1:0` on one machine, `0.0.0.0:7644` in
/// a pod) and its `host:port`.
pub async fn bind_to(addr: &str) -> Result<(tokio::net::TcpListener, String), SdkError> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| SdkError::Bind(format!("{addr}: {e}")))?;
    let listen = listener
        .local_addr()
        .map_err(|e| SdkError::Bind(e.to_string()))?
        .to_string();
    Ok((listener, listen))
}

/// A loopback listener on an ephemeral port and its `host:port`.
pub async fn bind() -> Result<(tokio::net::TcpListener, String), SdkError> {
    bind_to("127.0.0.1:0").await
}

/// Serves the router until the future is dropped: plain HTTP, or TLS with
/// `tls = (certificate, key)` (Spec O §23.1). A renewed certificate is a
/// restart's, never reloaded.
pub async fn run<P: Plugin>(
    listener: tokio::net::TcpListener,
    plugin: Arc<P>,
    token: &str,
    tls: Option<&(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<(), SdkError> {
    let app = router(plugin, token);
    let Some((cert, key)) = tls else {
        return axum::serve(listener, app)
            .into_future()
            .await
            .map_err(|e| SdkError::Bind(e.to_string()));
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
        .await
        .map_err(|e| SdkError::Bind(format!("{}: {e}", cert.display())))?;
    let std = listener
        .into_std()
        .map_err(|e| SdkError::Bind(e.to_string()))?;
    axum_server::from_tcp_rustls(std, config)
        .map_err(|e| SdkError::Bind(e.to_string()))?
        .serve(app.into_make_service())
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))
}

/// Bind, say hello, serve. Returns only on a bind or hello failure, or
/// when the server stops.
pub async fn serve<P: Plugin>(host: &Host, version: &str, plugin: P) -> Result<(), SdkError> {
    let (listener, listen) = bind_to(&host.env().listen).await?;
    serve_on(host, version, plugin, listener, listen).await
}

/// Spec O §23.8: the first wait after a 409 hello, doubled per refusal.
const HELLO_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
const HELLO_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// `hello`, again after every 409: a pod that started before the Daemon
/// holds its revision waits for the list rather than exit and crash-loop.
async fn hello_until_listed(
    host: &Host,
    version: &str,
    listen: &str,
    manifest: Option<&PluginManifest>,
) -> Result<balerix_api::HelloResponse, SdkError> {
    let mut wait = HELLO_RETRY_FIRST;
    loop {
        match host.hello(version, listen, manifest).await {
            Err(SdkError::Status {
                status: 409,
                message,
            }) => {
                tracing::info!("hello: {message}; again in {wait:?}");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(HELLO_RETRY_MAX);
            }
            other => return other,
        }
    }
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
    // Spec O §23.1: only with an authority, so a released one-machine
    // daemon (which refuses unknown fields in hello) never sees it
    let manifest = match host.env().ca {
        Some(_) => parse_manifest(&plugin)?,
        None => None,
    };
    let plugin = Arc::new(plugin);
    let token = host.env().token.clone();
    let tls = host.env().tls.clone();
    let listener_plugin = plugin.clone();
    let server =
        tokio::spawn(async move { run(listener, listener_plugin, &token, tls.as_ref()).await });
    let stop = |server: tokio::task::JoinHandle<Result<(), SdkError>>| async move {
        server.abort();
        let _ = server.await;
    };
    let reply = match hello_until_listed(host, version, &listen, manifest.as_ref()).await {
        Ok(reply) => {
            // the Daemon accepts only a hello whose revision it holds
            // (Spec O §23.8): the e2e reads this line in a rolled pod's log
            tracing::info!("hello accepted");
            reply
        }
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

    struct WithManifest;
    impl Plugin for WithManifest {
        fn manifest(&self) -> Option<&'static str> {
            Some(
                "apiVersion: balerix/v1\nkind: Plugin\nname: t\nversion: 0.1.0\nprotocol: 1\nstart: serve\nneeds: [kv]\n",
            )
        }
    }

    #[tokio::test]
    async fn hello_carries_the_manifest_only_with_an_authority() {
        // the SDK's fake host over plain http: the CA-less case
        let _ = rustls::crypto::ring::default_provider().install_default();
        let fake = crate::testing::FakeHost::start("tok", serde_json::json!({}), vec![]).await;
        let host = Host::new(fake.env("t", std::path::Path::new("scratch"))).unwrap();
        let (listener, listen) = bind().await.unwrap();
        let h = host.clone();
        let server =
            tokio::spawn(
                async move { serve_on(&h, "0.1.0", WithManifest, listener, listen).await },
            );
        let start = std::time::Instant::now();
        while fake.hellos().is_empty() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "no hello"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // `manifest` is skipped when `None`, so `None` here is the key absent
        // on the wire: what a 0.2.0 daemon's deny_unknown_fields needs
        assert_eq!(fake.hellos()[0].manifest, None);
        server.abort();
    }

    #[test]
    fn the_manifest_a_plugin_returns_is_parsed_before_hello() {
        let m = parse_manifest(&WithManifest).unwrap().unwrap();
        assert_eq!(m.name, "t");
        struct Broken;
        impl Plugin for Broken {
            fn manifest(&self) -> Option<&'static str> {
                Some("not: [a manifest")
            }
        }
        let err = parse_manifest(&Broken).unwrap_err().to_string();
        assert!(err.starts_with("configure: balerix-plugin.yaml:"), "{err}");
    }

    #[tokio::test]
    async fn run_serves_tls_with_the_given_certificate() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let (ca, cert, key) = crate::test_tls::authority(dir.path());
        let (listener, listen) = bind().await.unwrap();
        let tls = (cert, key);
        let server =
            tokio::spawn(async move { run(listener, Arc::new(Silent), "tok", Some(&tls)).await });
        let client = reqwest::Client::builder()
            .use_preconfigured_tls((*crate::tls::client_config(&ca).unwrap()).clone())
            .build()
            .unwrap();
        let r = client
            .get(format!("https://{listen}/v1/health"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        server.abort();
    }

    async fn post(url: &str, token: &str, body: Value) -> (u16, Value) {
        let _ = rustls::crypto::ring::default_provider().install_default();
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
        tokio::spawn(run(listener, Arc::new(Silent), "tok", None));
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
        let _ = rustls::crypto::ring::default_provider().install_default();
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
        tokio::spawn(run(listener, Arc::new(Silent), "tok", None));
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
        let _ = rustls::crypto::ring::default_provider().install_default();
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
        tokio::spawn(run(listener, Arc::new(Routed), "tok", None));
        let _ = rustls::crypto::ring::default_provider().install_default();
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
        let _ = rustls::crypto::ring::default_provider().install_default();
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

    /// Spec O §23.8: a pod that says hello before the list naming its
    /// revision arrives is answered 409; it waits, keeps its server bound,
    /// and is configured once a hello is accepted.
    #[tokio::test]
    async fn a_409_hello_is_retried_until_accepted() {
        struct Recording(Arc<std::sync::Mutex<Option<Value>>>);
        impl Plugin for Recording {
            async fn configure(&self, config: Value) -> Result<(), String> {
                *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(config);
                Ok(())
            }
        }

        let fake = crate::testing::FakeHost::start("tok", json!({ "k": 1 }), vec![]).await;
        fake.refuse_hellos(
            2,
            409,
            "hello.revision: this daemon holds r1, the plugin is r2; the list has not arrived yet",
        );
        let host = Host::new(fake.env("rec", std::path::Path::new("/s"))).unwrap();
        let (listener, listen) = bind().await.unwrap();
        let configured = Arc::new(std::sync::Mutex::new(None::<Value>));
        let plugin = Recording(configured.clone());
        let started = std::time::Instant::now();
        let serving =
            tokio::spawn(async move { serve_on(&host, "0.1.0", plugin, listener, listen).await });
        let got = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(c) = configured.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                    return c;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("configured after the retries");
        assert_eq!(got, json!({ "k": 1 }));
        assert_eq!(fake.hellos().len(), 3, "two refused, one accepted");
        // 1 s, then 2 s
        assert!(started.elapsed() >= std::time::Duration::from_secs(3));
        serving.abort();
    }

    #[tokio::test]
    async fn any_other_refused_hello_still_ends_serve() {
        let fake = crate::testing::FakeHost::start("tok", json!({}), vec![]).await;
        fake.refuse_hellos(1, 400, "hello.revision: required in kubernetes mode");
        let host = Host::new(fake.env("rec", std::path::Path::new("/s"))).unwrap();
        let (listener, listen) = bind().await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve_on(&host, "0.1.0", Silent, listener, listen),
        )
        .await
        .unwrap();
        assert!(
            matches!(&result, Err(SdkError::Status { status: 400, .. })),
            "{result:?}"
        );
    }

    /// A plugin that implements nothing accepts any config.
    #[tokio::test]
    async fn the_default_configure_accepts_anything() {
        assert_eq!(Silent.configure(json!({ "anything": 1 })).await, Ok(()));
    }

    /// #116, as on the daemon: a body over the 1 MiB limit is read to the
    /// end (up to 4 MiB) before the answer, so a client that writes all of
    /// it first reads the route's answer (`activate` maps a body
    /// rejection to 400) instead of a reset.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_over_limit_body_sent_whole_reads_its_answer() {
        // A small receive buffer, so the kernel cannot absorb what the
        // server leaves unread: without the drain the client's write fails.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(16).unwrap();
        let listen = listener.local_addr().unwrap().to_string();
        let app = router(Arc::new(Silent), "tok");
        tokio::spawn(async move { axum::serve(listener, app).await });
        for len in [(1 << 20) + 1, 4 << 20] {
            let listen = listen.clone();
            let (status, body) = tokio::task::spawn_blocking(move || {
                use std::io::{Read, Write};
                let mut s = std::net::TcpStream::connect(&listen).unwrap();
                s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                write!(
                    s,
                    "POST /v1/activate HTTP/1.1\r\nHost: {listen}\r\nAuthorization: Bearer tok\r\n\
                     Content-Type: application/json\r\nContent-Length: {len}\r\n\r\n"
                )
                .unwrap();
                s.write_all(&vec![b'x'; len])
                    .unwrap_or_else(|e| panic!("{len} bytes: {e}"));
                let mut answer = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap();
                    assert!(n > 0, "closed before a whole answer");
                    answer.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&answer).to_string();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if body.len() >= length {
                        let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
                        break (status, body.to_string());
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(status, 400, "{len} bytes: {body}");
            let e: ErrorBody =
                serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
            assert!(e.error.contains("length limit"), "{}", e.error);
        }
    }

    /// #116 review: a caller without the daemon's token is answered 401
    /// before its body is read, however long it says the body is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bad_token_is_answered_before_the_body_is_read() {
        let (listener, listen) = bind().await.unwrap();
        let app = router(Arc::new(Silent), "tok");
        tokio::spawn(async move { axum::serve(listener, app).await });
        let started = std::time::Instant::now();
        let head = tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(&listen).unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            write!(
                s,
                "POST /v1/activate HTTP/1.1\r\nHost: {listen}\r\nAuthorization: Bearer nope\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                2 << 20
            )
            .unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        })
        .await
        .unwrap();
        assert!(head.starts_with("HTTP/1.1 401"), "{head}");
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }
}
