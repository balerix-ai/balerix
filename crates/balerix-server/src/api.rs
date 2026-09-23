//! The HTTP surface (Phase 3 spec §3.4): plain HTTP on loopback, admin
//! bearer on `/v1/fleets*`, per-agent secret on the events route.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{CONTENT_TYPE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use balerix_api::{
    DownQuery, ErrorBody, FleetRequest, FleetSummary, HelloRequest, HelloResponse, PluginStatus,
    SessionRequest, SessionResponse, SyncReport,
};
use balerix_core::{AgentName, FleetName, FleetRecord, Keep, NameError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{RateLimiter, bearer, constant_time_eq};
use crate::daemon::{Daemon, DaemonError};
use crate::hooks;
use crate::plugins::{PluginAddr, PluginError};
use crate::proxy;
use crate::sessions::{
    COOKIE, CodeRejected, MOUNT_PREFIX, cookie_value, login_target, same_origin, set_cookie,
};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) daemon: Arc<Daemon>,
    pub(crate) limiter: Arc<RateLimiter>,
}

/// Every error leaves as `{ "error": "<message>" }`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<DaemonError> for ApiError {
    fn from(e: DaemonError) -> Self {
        let status = match e {
            DaemonError::NotFound => StatusCode::NOT_FOUND,
            DaemonError::Conflict => StatusCode::CONFLICT,
            DaemonError::Invalid(_) => StatusCode::BAD_REQUEST,
            DaemonError::Unauthorized => StatusCode::UNAUTHORIZED,
            DaemonError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            DaemonError::Managed(_) => StatusCode::CONFLICT,
        };
        Self::new(status, e.to_string())
    }
}

impl From<PluginError> for ApiError {
    fn from(e: PluginError) -> Self {
        // A KV failure names the daemon's own on-disk path; the body goes
        // to a sandboxed plugin, which has no business learning it. The
        // operator gets the real one in the log.
        if let PluginError::Kv { path, message } = &e {
            tracing::warn!(path = %path.display(), "plugin kv storage error: {message}");
            return Self::new(StatusCode::INTERNAL_SERVER_ERROR, "kv: storage error");
        }
        let status = match &e {
            PluginError::Config { .. }
            | PluginError::Manifest(_)
            | PluginError::ManifestParse(_)
            | PluginError::Digest { .. }
            | PluginError::Package(_)
            | PluginError::StillDeclared(_)
            | PluginError::Activation { .. }
            | PluginError::KvKey(_) => StatusCode::BAD_REQUEST,
            PluginError::Fetch { .. } => StatusCode::BAD_GATEWAY,
            PluginError::Capability(_) => StatusCode::FORBIDDEN,
            PluginError::NotActive(_) => StatusCode::NOT_FOUND,
            PluginError::Io { .. } | PluginError::Internal(_) | PluginError::Kv { .. } => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self::new(status, e.to_string())
    }
}

impl From<balerix_core::WorkspaceError> for ApiError {
    fn from(e: balerix_core::WorkspaceError) -> Self {
        use balerix_core::WorkspaceError as W;
        // An `Io` names a daemon-side path; the plugin gets a fixed line
        // and the operator the real one in the log, as for kv.
        if let W::Io { path, message } = &e {
            tracing::warn!(path = %path.display(), "workspace read error: {message}");
            return Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "workspace: storage error",
            );
        }
        let status = match &e {
            W::Missing(_) | W::NoSuchPath => StatusCode::NOT_FOUND,
            W::InvalidPath(_) | W::NotAFile | W::NotADirectory => StatusCode::BAD_REQUEST,
            W::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            W::Tool { .. } | W::Filter { .. } | W::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

/// 20 events/s with a burst of 50 per agent (spec §3.5).
const HOOK_RATE: f64 = 20.0;
const HOOK_BURST: f64 = 50.0;

pub fn router(daemon: Arc<Daemon>) -> Router {
    let state = AppState {
        daemon,
        limiter: Arc::new(RateLimiter::new(HOOK_RATE, HOOK_BURST)),
    };
    let admin = Router::new()
        .route("/v1/fleets", get(list_fleets).post(create_fleet))
        .route(
            "/v1/fleets/{name}",
            get(get_fleet).put(update_fleet).delete(delete_fleet),
        )
        .route("/v1/plugins", get(list_plugins))
        .route("/v1/plugins/sync", post(sync_plugins))
        .route("/v1/plugins/{name}", delete(purge_plugin))
        .route("/v1/sessions", post(create_session))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .layer(DefaultBodyLimit::max(4 << 20));
    let agents = Router::new()
        .route(
            "/v1/agents/{fleet}/{crew}/{agent}/events",
            post(hooks::events),
        )
        .layer(DefaultBodyLimit::max(1 << 20));
    let plugins = Router::new()
        .route("/v1/plugin-host/hello", post(plugin_hello))
        .layer(DefaultBodyLimit::max(64 << 10));
    let plugin_host = crate::plugin_api::router().layer(DefaultBodyLimit::max(1 << 20));
    // The plugin mount authenticates itself (bearer or session cookie),
    // so it sits outside the admin middleware. `/v1/plugins/{name}` with
    // no slash stays the purge route for DELETE; a GET there is a browser
    // whose URL lost its final `/`, sent on to the mount.
    let mount = Router::new()
        .route("/v1/plugins/{name}", get(mount_root_redirect))
        .route("/v1/plugins/{name}/", any(proxy_root))
        .route("/v1/plugins/{name}/{*rest}", any(proxy_rest));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .route("/v1/login/{code}", get(login))
        .merge(admin)
        .merge(agents)
        .merge(plugins)
        .merge(plugin_host)
        .merge(mount)
        .with_state(state)
}

/// Runs until `shutdown` resolves; in-flight requests finish.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn require_admin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match bearer(req.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), state.daemon.token().as_bytes()) => {
            next.run(req).await
        }
        _ => ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid admin token")
            .into_response(),
    }
}

/// Per-plugin `/v1/metrics` scrape budget (plugins spec §9).
const SCRAPE_TIMEOUT: Duration = Duration::from_millis(500);

async fn metrics(State(state): State<AppState>) -> Response {
    let snapshots = state.daemon.snapshots().await;
    state.daemon.metrics().set_gauges(&snapshots);
    let mut body = state.daemon.metrics().encode();
    let registry = state.daemon.registry().clone();
    // The plugin name lives outside the spawned task (not inside its
    // returned value) so a panicked or cancelled scrape is still
    // attributable: a bare `JoinError` carries no payload to recover it
    // from. Every task is spawned before any is awaited, so this keeps
    // the scrapes running in parallel despite the sequential awaits below.
    let mut scrapes = Vec::new();
    for name in registry.names() {
        let Some(addr) = registry.ready_addr(&name) else {
            continue;
        };
        let client = state.daemon.client().clone();
        let handle = tokio::spawn(async move { client.metrics(&addr, SCRAPE_TIMEOUT).await });
        scrapes.push((name, handle));
    }
    for (name, handle) in scrapes {
        match handle.await {
            Ok(Ok(text)) if crate::plugin_api::families_ok(&text, name.as_str()) => {
                body.push_str(&text);
                if !text.ends_with('\n') {
                    body.push('\n');
                }
            }
            Ok(Ok(_)) => state.daemon.metrics().scrape_failure(name.as_str()),
            Ok(Err(e)) => {
                tracing::debug!(plugin = %name, "metrics scrape failed: {e}");
                state.daemon.metrics().scrape_failure(name.as_str());
            }
            Err(e) => {
                tracing::warn!(plugin = %name, "metrics scrape task failed: {e}");
                state.daemon.metrics().scrape_failure(name.as_str());
            }
        }
    }
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

fn fleet_name(s: &str) -> Result<FleetName, ApiError> {
    s.parse()
        .map_err(|e: balerix_core::NameError| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))
}

/// Malformed JSON and a body that doesn't match `FleetRequest` (including
/// an unknown field: `deny_unknown_fields`) are both a plain 400 — the
/// spec's error table only names 400 for a bad request body. Only the
/// rejections whose status carries information the client actually needs
/// pass through axum's own: a missing/wrong `Content-Type` (415) and a
/// body over the limit (413, via `BytesRejection`).
fn body(b: Result<Json<FleetRequest>, JsonRejection>) -> Result<FleetRequest, ApiError> {
    b.map(|Json(r)| r).map_err(|e| {
        let status = match &e {
            JsonRejection::JsonDataError(_) | JsonRejection::JsonSyntaxError(_) => {
                StatusCode::BAD_REQUEST
            }
            _ => e.status(),
        };
        ApiError::new(status, e.body_text())
    })
}

fn path_name(p: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    p.map(|Path(name)| name)
        .map_err(|e| ApiError::new(e.status(), e.body_text()))
}

async fn create_fleet(
    State(state): State<AppState>,
    b: Result<Json<FleetRequest>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let req = body(b)?;
    let name = fleet_name(&req.spec.name)?;
    Ok(Json(
        state
            .daemon
            .apply(&name, req.spec, req.credentials, false)
            .await?,
    ))
}

async fn update_fleet(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
    b: Result<Json<FleetRequest>, JsonRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let req = body(b)?;
    let name = fleet_name(&path_name(name)?)?;
    Ok(Json(
        state
            .daemon
            .apply(&name, req.spec, req.credentials, true)
            .await?,
    ))
}

async fn get_fleet(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let name = fleet_name(&path_name(name)?)?;
    state
        .daemon
        .get(&name)
        .await
        .map(Json)
        .ok_or_else(|| DaemonError::NotFound.into())
}

async fn list_fleets(State(state): State<AppState>) -> Json<Vec<FleetSummary>> {
    Json(state.daemon.list().await)
}

async fn delete_fleet(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
    q: Result<Query<DownQuery>, QueryRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let Query(q) = q.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    if q.purge && (q.keep_repos || q.keep_sessions) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "purge cannot be combined with keep flags",
        ));
    }
    let name = fleet_name(&path_name(name)?)?;
    let keep = Keep {
        repos: q.keep_repos,
        sessions: q.keep_sessions,
    };
    Ok(Json(state.daemon.down(&name, keep, q.purge).await?))
}

/// `POST /v1/plugin-host/hello`: the plugin's bearer token, verified
/// against the `balerix/plugins/<name>` secret; unknown plugin and bad
/// token answer alike.
async fn plugin_hello(
    State(state): State<AppState>,
    headers: HeaderMap,
    b: Result<Json<HelloRequest>, JsonRejection>,
) -> Result<Json<HelloResponse>, ApiError> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "unknown plugin or bad token");
    let Some(token) = bearer(&headers) else {
        return Err(unauthorized());
    };
    let req = b
        .map(|Json(r)| r)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let name: AgentName = req.name.parse().map_err(|_: NameError| unauthorized())?;
    state
        .daemon
        .plugin_hello(&name, token, req)
        .await
        .map(Json)
        .map_err(|e| match e {
            DaemonError::Unauthorized => unauthorized(),
            other => other.into(),
        })
}

async fn list_plugins(State(state): State<AppState>) -> Json<Vec<PluginStatus>> {
    Json(state.daemon.plugins().list().await)
}

async fn sync_plugins(State(state): State<AppState>) -> Result<Json<SyncReport>, ApiError> {
    Ok(Json(state.daemon.sync_plugins().await?))
}

async fn purge_plugin(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let name: AgentName = path_name(name)?
        .parse()
        .map_err(|e: NameError| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    state.daemon.plugins().purge(&name).await?;
    Ok(Json(json!({})))
}

/// `POST /v1/sessions`: a single-use login URL for the admin's browser
/// (plugins spec §18.2). The admin token itself never enters the browser.
async fn create_session(
    State(state): State<AppState>,
    b: Result<Json<SessionRequest>, JsonRejection>,
) -> Result<Json<SessionResponse>, ApiError> {
    let req = b
        .map(|Json(r)| r)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let to = login_target(req.to.as_deref()).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("to: must be a path under {MOUNT_PREFIX}"),
        )
    })?;
    let code = state.daemon.sessions().issue_code();
    Ok(Json(SessionResponse {
        login_url: format!("{}/v1/login/{code}?to={to}", state.daemon.origin()),
    }))
}

async fn proxy_root(
    State(state): State<AppState>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    proxied(&state, &name, req).await
}

/// `GET /v1/plugins/{name}`: a 308 to the mount root proper. Nothing is
/// checked or revealed; the mount authenticates the follow-up itself.
async fn mount_root_redirect(Path(name): Path<String>) -> Response {
    match name.parse::<AgentName>() {
        Ok(name) => (
            StatusCode::PERMANENT_REDIRECT,
            [(LOCATION, format!("{MOUNT_PREFIX}{name}/"))],
        )
            .into_response(),
        Err(_) => ApiError::new(
            StatusCode::NOT_FOUND,
            format!("plugin {name:?} has no routes"),
        )
        .into_response(),
    }
}

/// `rest` is captured only so the route matches; the proxy reads it back
/// off the raw path, because `Path` percent-decodes what it captures.
async fn proxy_rest(
    State(state): State<AppState>,
    Path((name, _rest)): Path<(String, String)>,
    req: Request,
) -> Response {
    proxied(&state, &name, req).await
}

/// What `/v1/plugins/{name}/…` resolved to. Only the two named variants
/// may appear in the proxy counter's `plugin` label: the label is minted
/// from the registry, never from the path, so an anonymous caller cannot
/// fill the series with one entry per guessed name.
enum Mount {
    /// Authenticated, installed with `routes: true`, and listening.
    Ready(AgentName, PluginAddr),
    /// Authenticated and installed with `routes: true`, but no `hello` yet.
    NotReady(AgentName),
    /// Anything else: unauthenticated, unparseable, unknown, or routeless.
    Refused(ApiError),
}

/// The mount (plugins spec §6, §18.2): authenticate, resolve the plugin,
/// forward, count.
async fn proxied(state: &AppState, name: &str, req: Request) -> Response {
    let (label, resp) = match resolve_mount(state, name, req.headers()) {
        Mount::Ready(plugin, addr) => {
            let resp =
                proxy::forward(state.daemon.proxy_client(), &addr, plugin.as_str(), req).await;
            (plugin.to_string(), resp)
        }
        Mount::NotReady(plugin) => {
            let e = ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("plugin {:?} is not ready", plugin.as_str()),
            );
            (plugin.to_string(), e.into_response())
        }
        Mount::Refused(e) => ("unknown".to_string(), e.into_response()),
    };
    state
        .daemon
        .metrics()
        .proxy_request(&label, resp.status().as_u16());
    resp
}

fn resolve_mount(state: &AppState, name: &str, headers: &HeaderMap) -> Mount {
    if let Err(e) = authenticate_browser_or_admin(state, headers) {
        return Mount::Refused(e);
    }
    let no_routes = || {
        Mount::Refused(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("plugin {name:?} has no routes"),
        ))
    };
    let Ok(plugin) = name.parse::<AgentName>() else {
        return no_routes();
    };
    let registry = state.daemon.registry();
    if !registry.plugin(&plugin).is_some_and(|p| p.manifest.routes) {
        return no_routes();
    }
    match registry.ready_addr(&plugin) {
        Some(addr) => Mount::Ready(plugin, addr),
        None => Mount::NotReady(plugin),
    }
}

/// The admin bearer, or a live session cookie on a same-origin request
/// (§18.2). A bearer that is present but wrong is refused outright; the
/// cookie is never consulted then.
fn authenticate_browser_or_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid admin token");
    if let Some(t) = bearer(headers) {
        return if constant_time_eq(t.as_bytes(), state.daemon.token().as_bytes()) {
            Ok(())
        } else {
            Err(unauthorized())
        };
    }
    let hdr = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string()
    };
    match cookie_value(headers, COOKIE) {
        Some(id) if state.daemon.sessions().is_valid(&id) => {
            if same_origin(headers, state.daemon.origin()) {
                Ok(())
            } else {
                tracing::warn!(
                    host = hdr("host"),
                    sec_fetch_site = hdr("sec-fetch-site"),
                    origin = hdr("origin"),
                    "proxy: session cookie refused as cross-origin"
                );
                Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "cross-origin request refused",
                ))
            }
        }
        found => {
            // Names only: a value is a credential. Tells apart a browser that
            // withheld the cookie from a proxy that stripped the header and
            // from a session this daemon never issued (a restart, say).
            tracing::warn!(
                host = hdr("host"),
                sec_fetch_site = hdr("sec-fetch-site"),
                cookies = cookie_names(headers),
                session_cookie = match found {
                    Some(_) => "present but not a live session",
                    None => "absent",
                },
                "proxy: no bearer and no live session cookie"
            );
            Err(unauthorized())
        }
    }
}

/// The names of every cookie sent, for the log; values stay out of it.
fn cookie_names(headers: &HeaderMap) -> String {
    let names: Vec<&str> = headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('=').map(|(k, _)| k.trim()))
        .collect();
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join(",")
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LoginQuery {
    to: Option<String>,
}

/// `GET /v1/login/{code}`: the browser's end. A live code becomes a session
/// cookie and a 303 to `to`; anything else is a plain-text 404 naming why
/// — the page is for a human who pasted a stale URL, not for a client
/// parsing JSON. Every attempt is logged with the headers that show which
/// way it came (a reverse proxy rewrites `Host`; a prefetch or a redirect
/// through an identity provider shows in `Sec-Fetch-Site`), never the code.
async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    code: Result<Path<String>, PathRejection>,
    q: Result<Query<LoginQuery>, QueryRejection>,
) -> Response {
    let hdr = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string()
    };
    let (host, site, agent) = (hdr("host"), hdr("sec-fetch-site"), hdr("user-agent"));
    let to = match q {
        Ok(Query(q)) => match login_target(q.to.as_deref()) {
            Some(to) => to,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("to: must be a path under {MOUNT_PREFIX}"),
                )
                    .into_response();
            }
        },
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    let Ok(Path(code)) = code else {
        return (StatusCode::NOT_FOUND, CodeRejected::Unknown.to_string()).into_response();
    };
    match state.daemon.sessions().redeem(&code) {
        Ok(id) => {
            tracing::info!(
                host,
                sec_fetch_site = site,
                user_agent = agent,
                "login: code redeemed, session cookie set"
            );
            (
                StatusCode::SEE_OTHER,
                [(LOCATION, to), (SET_COOKIE, set_cookie(&id))],
            )
                .into_response()
        }
        Err(why) => {
            tracing::warn!(
                host,
                sec_fetch_site = site,
                user_agent = agent,
                "login refused: {why}"
            );
            (StatusCode::NOT_FOUND, why.to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A KV failure must not render the daemon's on-disk path into a body
    /// a sandboxed plugin reads.
    #[test]
    fn a_kv_storage_error_is_a_fixed_500() {
        let e = ApiError::from(PluginError::Kv {
            path: "/home/op/.local/share/balerix/plugins/flow/kv/k".into(),
            message: "permission denied".into(),
        });
        assert_eq!(
            (e.status, e.message.as_str()),
            (StatusCode::INTERNAL_SERVER_ERROR, "kv: storage error")
        );
    }
}
