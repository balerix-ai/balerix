//! `/v1/plugin-host/*` beyond `hello` (plugins spec §4.1): the plugin's
//! bearer identifies it, the manifest's `needs` gates every route.

use std::collections::{BTreeMap, BTreeSet};

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, JsonRejection, PathRejection, QueryRejection};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use balerix_api::{
    Capability, DownQuery, KvKeys, PluginAction, WorkspaceDiff, WorkspaceTree, WorkspaceVersion,
};
use balerix_core::{AgentId, AgentName, FleetName, FleetRecord, Keep, is_reserved_fleet};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiError, AppState, down_flags};
use crate::auth::bearer;
use crate::daemon::{Caller, DaemonError};
use crate::plugins::PluginError;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/plugin-host/fleets", get(list_fleets))
        .route("/v1/plugin-host/fleets/watch", get(watch_fleets))
        .route(
            "/v1/plugin-host/fleets/{name}",
            get(get_fleet).put(put_fleet).delete(delete_fleet),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/actions",
            axum::routing::post(post_action),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/attach",
            get(attach),
        )
        .route("/v1/plugin-host/kv", get(list_keys))
        // `{*key}` never matches an empty key; without this the bare
        // `kv/` would be axum's empty 404, unauthenticated (#14)
        .route("/v1/plugin-host/kv/", any(empty_key))
        .route(
            "/v1/plugin-host/kv/{*key}",
            get(get_key).put(put_key).delete(delete_key),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/diff",
            get(workspace_diff),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/file",
            get(workspace_file),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/tree",
            get(workspace_tree),
        )
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/version",
            get(workspace_version),
        )
}

/// The calling plugin, from its token, with the capability check.
async fn caller(
    state: &AppState,
    headers: &HeaderMap,
    cap: Capability,
) -> Result<AgentName, ApiError> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "unknown plugin or bad token");
    let token = bearer(headers).ok_or_else(unauthorized)?;
    let name = state
        .daemon
        .plugin_for_token(token)
        .await
        .ok_or_else(unauthorized)?;
    if !state.daemon.registry().has(&name, cap) {
        return Err(PluginError::Capability(crate::plugins::wire_label(cap)).into());
    }
    Ok(name)
}

async fn list_fleets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<FleetRecord>>, ApiError> {
    caller(&state, &headers, Capability::Fleets).await?;
    Ok(Json(state.daemon.plugin_fleets().await))
}

async fn get_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    name: Result<Path<String>, PathRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    caller(&state, &headers, Capability::Fleets).await?;
    let Path(name) = name.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let name: FleetName = name
        .parse()
        .map_err(|_: balerix_core::NameError| ApiError::from(DaemonError::NotFound))?;
    if is_reserved_fleet(name.as_str()) {
        return Err(DaemonError::NotFound.into());
    }
    state
        .daemon
        .get(&name)
        .await
        .map(Json)
        .ok_or_else(|| DaemonError::NotFound.into())
}

/// Body of `PUT fleets/{name}` (Spec L §3.1): the YAML fleet file's
/// structure as JSON. Nothing else: the daemon resolves and reads the
/// credentials itself.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetFileBody {
    file: Value,
}

/// The fleet of a manage route. A bad name is a 400 that names it: the
/// daemon has nothing to be "not found" for a name it never accepts, and
/// the plugin built it. The read routes (`get_fleet`, `post_action`,
/// `attach`, the workspace routes) answer 404 for the same name on
/// purpose: there it names something to look up, and no fleet or agent
/// can exist under a name the grammar refuses, so "not found" is true
/// and tells a plugin nothing a valid name would not (#14).
fn manage_name(name: Result<Path<String>, PathRejection>) -> Result<FleetName, ApiError> {
    let Path(name) = name.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    name.parse().map_err(|e: balerix_core::NameError| {
        ApiError::new(StatusCode::BAD_REQUEST, format!("name: {e}"))
    })
}

/// `PUT fleets/{name}` (Spec L §3.1): an unresolved fleet file becomes a
/// fleet this plugin owns. `manage` gates it; the answer is the record for
/// a caller with `fleets`, 204 otherwise (Spec M §12.2).
async fn put_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    name: Result<Path<String>, PathRejection>,
    body: Result<Json<FleetFileBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let plugin = caller(&state, &headers, Capability::Manage).await?;
    let name = manage_name(name)?;
    let body = crate::api::body(body)?;
    if !body.file.is_object() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "file: expected a mapping",
        ));
    }
    let record = state.daemon.manage_fleet(&plugin, &name, body.file).await?;
    // Spec M §12.2: the record carries the resolved spec, host settings
    // folded in; reading records is what `fleets` means.
    if state.daemon.registry().has(&plugin, Capability::Fleets) {
        Ok(Json(record).into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
}

/// `DELETE fleets/{name}?…` (Spec L §3.2): the admin `DELETE`'s flags,
/// for a fleet this plugin owns. `force` is read and ignored: ownership
/// is the rule here.
async fn delete_fleet(
    State(state): State<AppState>,
    headers: HeaderMap,
    name: Result<Path<String>, PathRejection>,
    q: Result<Query<DownQuery>, QueryRejection>,
) -> Result<Json<FleetRecord>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Manage).await?;
    let name = manage_name(name)?;
    let q = down_flags(q)?;
    let keep = Keep {
        repos: q.keep_repos,
        sessions: q.keep_sessions,
    };
    let record = state
        .daemon
        .down_as(&name, keep, q.purge, &Caller::Plugin(plugin))
        .await?;
    // Kubernetes mode: the operator downs it from the stored request (§23.3)
    state.daemon.managed_down(&name, q);
    Ok(Json(record))
}

/// `GET fleets/watch` (WS): every change, as the whole list (§18.4).
async fn watch_fleets(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    caller(&state, &headers, Capability::Fleets).await?;
    let daemon = state.daemon.clone();
    Ok(ws.on_upgrade(move |socket| crate::watch::serve_watch(socket, daemon)))
}

/// `GET agents/{id}/attach` (WS): a terminal on the agent's window, for a
/// plugin active on it (§18.4). The runner attaches on a blocking thread
/// before the upgrade, so a failure is an ordinary 500.
async fn attach(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let plugin = caller(&state, &headers, Capability::Attach).await?;
    let Path((f, c, a)) = path.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let agent: AgentId = format!("{f}/{c}/{a}")
        .parse()
        .map_err(|_: balerix_core::NameError| ApiError::from(DaemonError::NotFound))?;
    if !state.daemon.registry().is_active(&agent, &plugin) {
        return Err(PluginError::NotActive(agent.to_string()).into());
    }
    let runner = state.daemon.runner();
    let id = agent.clone();
    let stream = tokio::task::spawn_blocking(move || runner.attach(&id))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tracing::info!(plugin = %plugin, agent = %agent, "attach opened");
    // The guard, not the stream: an upgrade that never completes leaves
    // axum dropping this closure on a runtime task, and ending a tmux
    // attach blocks (`StreamGuard`).
    let guard = crate::attach::StreamGuard::new(stream);
    Ok(ws.on_upgrade(move |socket| crate::attach::bridge(socket, guard)))
}

async fn post_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    body: Result<Json<PluginAction>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Actions).await?;
    let Path((f, c, a)) = path.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let agent: AgentId = format!("{f}/{c}/{a}")
        .parse()
        .map_err(|_: balerix_core::NameError| ApiError::from(DaemonError::NotFound))?;
    let action = crate::api::body(body)?;
    if !state.daemon.registry().is_active(&agent, &plugin) {
        return Err(PluginError::NotActive(agent.to_string()).into());
    }
    state
        .daemon
        .execute_action(&agent, &action, Some(plugin.as_str()))
        .await?;
    Ok(Json(json!({})))
}

/// The plugin, active for `agent`, that a workspace route serves (Spec C
/// §2.2): the `workspace` capability, then the pair.
async fn workspace_caller(
    state: &AppState,
    headers: &HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
) -> Result<AgentId, ApiError> {
    let plugin = caller(state, headers, Capability::Workspace).await?;
    let Path((f, c, a)) = path.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let agent: AgentId = format!("{f}/{c}/{a}")
        .parse()
        .map_err(|_: balerix_core::NameError| ApiError::from(DaemonError::NotFound))?;
    if !state.daemon.registry().is_active(&agent, &plugin) {
        return Err(PluginError::NotActive(agent.to_string()).into());
    }
    Ok(agent)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PathQuery {
    path: String,
}

fn path_query(q: Result<Query<PathQuery>, QueryRejection>) -> Result<String, ApiError> {
    q.map(|Query(q)| q.path)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))
}

async fn workspace_diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
) -> Result<Json<WorkspaceDiff>, ApiError> {
    let agent = workspace_caller(&state, &headers, path).await?;
    let base_ref = state
        .daemon
        .base_ref(&agent)
        .await
        .ok_or(DaemonError::NotFound)?;
    let ws = state.daemon.workspace();
    let id = agent.clone();
    let diff = tokio::task::spawn_blocking(move || ws.diff(&id, &base_ref))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(diff))
}

async fn workspace_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    q: Result<Query<PathQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let agent = workspace_caller(&state, &headers, path).await?;
    let rel = path_query(q)?;
    let ws = state.daemon.workspace();
    let bytes = tokio::task::spawn_blocking(move || ws.read_file(&agent, &rel))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn workspace_tree(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    q: Result<Query<PathQuery>, QueryRejection>,
) -> Result<Json<WorkspaceTree>, ApiError> {
    let agent = workspace_caller(&state, &headers, path).await?;
    let rel = path_query(q)?;
    let ws = state.daemon.workspace();
    let tree = tokio::task::spawn_blocking(move || ws.list_dir(&agent, &rel))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(tree))
}

/// Spec D §2.2: the worktree's fingerprint against the crew's base, gated
/// like `diff`.
async fn workspace_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
) -> Result<Json<WorkspaceVersion>, ApiError> {
    let agent = workspace_caller(&state, &headers, path).await?;
    let base_ref = state
        .daemon
        .base_ref(&agent)
        .await
        .ok_or(DaemonError::NotFound)?;
    let ws = state.daemon.workspace();
    let id = agent.clone();
    let version = tokio::task::spawn_blocking(move || ws.version(&id, &base_ref))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(version))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct KvQuery {
    prefix: String,
    secret: bool,
}

fn kv_query(q: Result<Query<KvQuery>, QueryRejection>) -> Result<KvQuery, ApiError> {
    q.map(|Query(q)| q)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))
}

fn key_of(p: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    p.map(|Path(k)| k)
        .map_err(|e| ApiError::new(e.status(), e.body_text()))
}

/// `kv/` with any method: the empty key, refused like any other bad one.
async fn empty_key(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    caller(&state, &headers, Capability::Kv).await?;
    let why = balerix_api::check_kv_key("").err().unwrap_or_default();
    Err(PluginError::KvKey(why).into())
}

async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<KvQuery>, QueryRejection>,
) -> Result<Json<KvKeys>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let q = kv_query(q)?;
    let kv = state.daemon.kv().clone();
    let keys = tokio::task::spawn_blocking(move || kv.list(&plugin, &q.prefix))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(KvKeys { keys }))
}

async fn get_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let kv = state.daemon.kv().clone();
    let value = tokio::task::spawn_blocking(move || kv.get(&plugin, &key))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    match value {
        Some(bytes) => Ok(([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response()),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "no such key")),
    }
}

async fn put_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
    q: Result<Query<KvQuery>, QueryRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let q = kv_query(q)?;
    let body = body.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let kv = state.daemon.kv().clone();
    tokio::task::spawn_blocking(move || kv.put(&plugin, &key, &body, q.secret))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({})))
}

async fn delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    key: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let plugin = caller(&state, &headers, Capability::Kv).await?;
    let key = key_of(key)?;
    let kv = state.daemon.kv().clone();
    tokio::task::spawn_blocking(move || kv.delete(&plugin, &key))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(json!({})))
}

/// The metrics prefix rule (plugins spec §9): every family name in the
/// body starts with `balerix_plugin_<name>_`, each `-` of the plugin's
/// name written `_` as the SDK's `Metrics::family` writes it, and is a
/// valid Prometheus name (#224): one invalid name would make a strict
/// parser reject the daemon's whole `/metrics`. Comment lines are
/// checked through their `# TYPE`/`# HELP` family, sample lines through
/// the name before `{` or the first space.
///
/// `Some` with the body's collision keys (`collision_keys`) when it
/// passes, for `/metrics` to check against the names already merged;
/// `None` when it breaks the rule.
pub fn families_ok(body: &str, plugin: &str) -> Option<BTreeSet<String>> {
    let prefix = format!("balerix_plugin_{}_", plugin.replace('-', "_"));
    let mut keys = BTreeSet::new();
    for line in body.lines() {
        match line_name(line) {
            LineName::Skip => {}
            LineName::Malformed => return None,
            LineName::Name(n) if n.starts_with(&prefix) && is_metric_name(n) => {
                keys.extend(collision_keys(n));
            }
            LineName::Name(_) => return None,
        }
    }
    Some(keys)
}

/// Plugin scrapes merged into the daemon's `/metrics`, in order (#224):
/// every name merged so far and who owns it. The daemon's own families
/// sit under `balerix_plugin_` too, and with `-` written `_` plugin `a`'s
/// `b_x` and plugin `a-b`'s `x` are the same name, so a scrape sharing a
/// name with what is already merged is dropped whole: the exposition
/// never repeats a family.
pub struct MetricsMerge {
    seen: BTreeMap<String, String>,
}

/// Why `MetricsMerge::admit` dropped a scrape.
#[derive(Debug, PartialEq, Eq)]
pub enum Dropped {
    /// It broke the prefix rule (`families_ok`).
    Rule,
    /// `name` is already `owner`'s (a plugin, or `the daemon`).
    Collides { name: String, owner: String },
}

impl MetricsMerge {
    /// Seeded with the daemon's own families, sampled yet or not.
    pub fn new<'a>(own: impl IntoIterator<Item = &'a String>) -> Self {
        let seen = own
            .into_iter()
            .flat_map(|f| collision_keys(f))
            .map(|k| (k, "the daemon".to_string()))
            .collect();
        Self { seen }
    }

    /// `Ok` when `text` may be appended: its names are now `plugin`'s.
    pub fn admit(&mut self, plugin: &str, text: &str) -> Result<(), Dropped> {
        let keys = families_ok(text, plugin).ok_or(Dropped::Rule)?;
        // the longest shared key: the full name rather than its stem
        if let Some((name, owner)) = keys
            .iter()
            .filter_map(|k| self.seen.get_key_value(k))
            .max_by_key(|(k, _)| k.len())
        {
            return Err(Dropped::Collides {
                name: name.clone(),
                owner: owner.clone(),
            });
        }
        self.seen
            .extend(keys.into_iter().map(|k| (k, plugin.to_string())));
        Ok(())
    }
}

/// The suffixes a family's samples carry: a histogram's `x` is sampled
/// as `x_bucket`, `x_sum` and `x_count`, a summary's as `x_sum` and
/// `x_count`, and an OpenMetrics counter `x` as `x_total` and
/// `x_created`.
const SAMPLE_SUFFIXES: [&str; 5] = ["_bucket", "_sum", "_count", "_total", "_created"];

/// A name and, when it ends in a sample suffix, its stem: two names
/// collide when they are equal or one is the other plus a suffix (a
/// family `x` and another's `x_count`), and both cases share a key.
/// Conservative: `x_sum` and `x_count` from different owners collide too.
fn collision_keys(name: &str) -> impl Iterator<Item = String> + '_ {
    let stem = SAMPLE_SUFFIXES
        .iter()
        .find_map(|s| name.strip_suffix(s))
        .filter(|stem| !stem.is_empty());
    std::iter::once(name.to_string()).chain(stem.map(str::to_string))
}

enum LineName<'a> {
    Skip,
    Malformed,
    Name(&'a str),
}

fn line_name(line: &str) -> LineName<'_> {
    let line = line.trim();
    if line.is_empty() {
        return LineName::Skip;
    }
    if let Some(rest) = line.strip_prefix('#') {
        let mut words = rest.split_whitespace();
        return match words.next() {
            Some("TYPE" | "HELP") => words.next().map_or(LineName::Malformed, LineName::Name),
            _ => LineName::Skip,
        };
    }
    LineName::Name(
        line.split(|c: char| c == '{' || c.is_whitespace())
            .next()
            .unwrap_or(""),
    )
}

/// Prometheus's metric name grammar, `[a-zA-Z_:][a-zA-Z0-9_:]*`.
fn is_metric_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b':')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_(body: &str, plugin: &str) -> bool {
        families_ok(body, plugin).is_some()
    }

    #[test]
    fn a_body_passes_only_if_every_family_carries_the_plugin_prefix() {
        let ok = "# HELP balerix_plugin_flow_state Current state\n# TYPE balerix_plugin_flow_state gauge\nbalerix_plugin_flow_state{agent=\"a\"} 1\n\nbalerix_plugin_flow_transitions_total{from=\"a\",to=\"b\"} 2\n";
        assert!(ok_(ok, "flow"));
        assert!(!ok_(ok, "web"), "another plugin's prefix");
        assert!(!ok_(
            "balerix_plugin_flow_x 1\nprocess_cpu_seconds_total 3\n",
            "flow"
        ));
        assert!(
            !ok_("balerix_agents{fleet=\"f\"} 1\n", "flow"),
            "daemon families cannot be spoofed"
        );
        assert!(ok_("", "flow"), "empty is fine");
        assert!(ok_("# just a comment\n", "flow"));
        assert!(
            !ok_("balerix_plugin_flow 1\n", "flow"),
            "the prefix needs the trailing underscore"
        );
    }

    /// #224: a hyphenated plugin's prefix is the SDK's, `-` written `_`,
    /// and every family must be a valid Prometheus name.
    #[test]
    fn a_hyphenated_plugin_uses_the_underscored_prefix_and_names_must_be_valid() {
        let ok = "# HELP balerix_plugin_my_plugin_up Up\n# TYPE balerix_plugin_my_plugin_up gauge\nbalerix_plugin_my_plugin_up 1\nbalerix_plugin_my_plugin_calls_total{route=\"x\"} 2\n";
        assert!(ok_(ok, "my-plugin"));
        assert!(
            !ok_("balerix_plugin_my-plugin_up 1\n", "my-plugin"),
            "the raw name is not a valid family name"
        );
        for bad in [
            "balerix_plugin_flow_a-b 1\n",
            "# TYPE balerix_plugin_flow_a.b gauge\n",
            "balerix_plugin_flow_é 1\n",
            "balerix_plugin_flow_x/y{a=\"b\"} 1\n",
        ] {
            assert!(!ok_(bad, "flow"), "{bad:?}");
        }
        assert!(ok_("balerix_plugin_flow_a:b_9 1\n", "flow"));
        assert!(is_metric_name("_x") && is_metric_name(":x") && is_metric_name("x"));
        assert!(!is_metric_name("") && !is_metric_name("9x") && !is_metric_name("x-y"));
    }

    /// #224: a passing body's keys are its names plus each sample
    /// suffix's stem, so a family and another owner's `_count` or
    /// `_total` of it share a key.
    #[test]
    fn collision_keys_cover_names_and_their_sample_stems() {
        let keys = families_ok(
            "# TYPE balerix_plugin_a_b_x counter\nbalerix_plugin_a_b_x_total 1\nbalerix_plugin_a_h_bucket{le=\"1\"} 1\n",
            "a",
        )
        .unwrap();
        let want: BTreeSet<String> = [
            "balerix_plugin_a_b_x",
            "balerix_plugin_a_b_x_total",
            "balerix_plugin_a_h",
            "balerix_plugin_a_h_bucket",
        ]
        .map(String::from)
        .into();
        assert_eq!(keys, want);

        assert_eq!(collision_keys("_total").collect::<Vec<_>>(), vec!["_total"]);
    }

    fn flow_like(plugin: &str, short: &str) -> String {
        let m = balerix_plugin_sdk::Metrics::new(plugin);
        m.int_counter(short, "x").unwrap().inc();
        m.render().unwrap()
    }

    /// #224: a plugin cannot claim a daemon family, sampled or not: a
    /// plugin `events` registering `total` is `balerix_plugin_events_total`.
    #[test]
    fn a_plugin_scrape_naming_a_daemon_family_is_dropped() {
        let daemon = crate::metrics::Metrics::new().unwrap();
        assert!(
            !daemon.encode().contains("balerix_plugin_events_total"),
            "not sampled yet, so only `families` names it"
        );
        let mut merge = MetricsMerge::new(daemon.families());
        assert_eq!(
            merge.admit("events", &flow_like("events", "total")),
            Err(Dropped::Collides {
                name: "balerix_plugin_events_total".into(),
                owner: "the daemon".into()
            })
        );
        // a histogram's sample names: `intercept`'s `duration_seconds_count`
        assert_eq!(
            merge.admit(
                "intercept",
                &flow_like("intercept", "duration_seconds_count")
            ),
            Err(Dropped::Collides {
                name: "balerix_plugin_intercept_duration_seconds".into(),
                owner: "the daemon".into()
            })
        );
        assert_eq!(merge.admit("events", &flow_like("events", "mine")), Ok(()));
        assert_eq!(merge.admit("web", "process_cpu 1\n"), Err(Dropped::Rule));
    }

    /// #224: plugin `a`'s `b_x` and plugin `a-b`'s `x` are one name; the
    /// first in order keeps it and the other's scrape is dropped whole.
    #[test]
    fn a_and_a_hyphen_b_cannot_both_have_a_b_x() {
        let mut merge = MetricsMerge::new(&[]);
        assert_eq!(merge.admit("a", &flow_like("a", "b_x")), Ok(()));
        let ab = flow_like("a-b", "x");
        assert!(ab.contains("balerix_plugin_a_b_x 1"), "{ab}");
        assert_eq!(
            merge.admit("a-b", &ab),
            Err(Dropped::Collides {
                name: "balerix_plugin_a_b_x".into(),
                owner: "a".into()
            })
        );
        assert_eq!(merge.admit("a-b", &flow_like("a-b", "y")), Ok(()));
        assert_eq!(merge.admit("a", &flow_like("a", "c")), Ok(()));
    }
}
