//! The controllers (Spec O §5, §21.2): the dumb executor in front of
//! `desired`. A reconcile observes (the owned objects by label, the
//! Secrets it needs), calls the pure function, applies the result with
//! server-side apply under one field manager, and patches status. One
//! `Context` is shared by every controller of the process.

pub mod agent;
pub mod crew;
pub mod daemon;
pub mod fleet;
pub mod jobs;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use balerix_api::FleetRecord;
use futures_util::future::BoxFuture;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::{Api, Client, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::daemon_client::{ClientError, DaemonClient};
use crate::desired::common::{
    Cond, DesiredError, Images, MANAGER, OperatorConfig, conditions, hash,
};
use crate::desired::fleet::PlanError;
use crate::pki::PkiError;

/// Unix seconds. Tests move it; the binary reads the system clock.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct RunConfig {
    /// `CARGO_PKG_VERSION`: what a Daemon may ask for (§4.1).
    pub version: String,
    pub images: Images,
    /// The operator's own namespace (the Daemon's NetworkPolicy admits it).
    pub namespace: String,
    /// `None` watches every namespace (§5.6).
    pub watch_namespaces: Option<Vec<String>>,
    /// The Fleet's and the Agent's requeue: the status poll (§21.1).
    pub fleet_period: Duration,
    /// The Daemon's and the Crew's requeue: renewal and drift.
    pub period: Duration,
    pub clock: Clock,
    /// Tests only: every Daemon is this plain-HTTP stub. A pod's Daemon is
    /// `https` and the client refuses anything else.
    pub insecure_daemon_url: Option<String>,
    /// Service host to socket address, for an operator outside the
    /// cluster (`e2e-k8s`): the name still verifies against the
    /// certificate; only the connection goes elsewhere.
    pub resolve: Vec<(String, std::net::SocketAddr)>,
}

impl RunConfig {
    pub fn new(version: &str, images: Images, namespace: &str) -> Self {
        Self {
            version: version.to_string(),
            images,
            namespace: namespace.to_string(),
            watch_namespaces: None,
            fleet_period: Duration::from_secs(15),
            period: Duration::from_secs(60),
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            }),
            insecure_daemon_url: None,
            resolve: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Daemon(#[from] ClientError),
    #[error("{0}")]
    Desired(#[from] DesiredError),
    #[error("{0}")]
    Plan(#[from] PlanError),
    #[error("{0}")]
    Pki(#[from] PkiError),
    #[error("{0}")]
    Finalizer(Box<kube::runtime::finalizer::Error<Error>>),
    /// An object the reconcile needs is not there: `{what} has no {field}`
    /// or `{what} {name} does not exist`.
    #[error("{0}")]
    Missing(String),
    /// A cleanup that is not finished: requeued in 2 s, not an error.
    #[error("waiting: {0}")]
    Waiting(String),
    /// The reconcile ran past `RECONCILE_TIMEOUT` and was dropped: requeued in 2 s twice in a row, then backed off with an Event (§22.2).
    #[error("the reconcile did not finish in {0:?}")]
    TimedOut(Duration),
}

impl From<kube::runtime::finalizer::Error<Error>> for Error {
    fn from(e: kube::runtime::finalizer::Error<Error>) -> Self {
        Error::Finalizer(Box::new(e))
    }
}

/// How long one reconcile may run. Each request is bounded
/// (`request_client`, 10 s), but a reconcile makes many, and the runtime
/// never starts a second reconcile of an object whose first still runs: a
/// reconcile that never ends would hold its object for good.
pub const RECONCILE_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeouts in a row that still requeue in 2 s: one most likely means one
/// lost request (§22.2).
pub const QUICK_TIMEOUTS: u32 = 2;

/// A controller's reconcile under `RECONCILE_TIMEOUT`, for `.run(...)`.
pub fn bounded<K, F, Fut>(
    reconcile: F,
) -> impl FnMut(Arc<K>, Arc<Context>) -> BoxFuture<'static, Result<Action, Error>>
where
    K: Resource + Send + Sync + 'static,
    K::DynamicType: Default,
    F: Fn(Arc<K>, Arc<Context>) -> Fut,
    Fut: Future<Output = Result<Action, Error>> + Send + 'static,
{
    move |object, ctx| {
        let reconcile = reconcile(object.clone(), ctx.clone());
        Box::pin(async move { within(RECONCILE_TIMEOUT, object, &ctx, reconcile).await })
    }
}

/// `reconcile`, or `TimedOut` once it has run for `limit`: the future is
/// dropped, and whatever it held with it. Counts the timeout, which
/// `error_policy` reads, and from the third in a row says so on the
/// object (§22.2).
pub async fn within<K>(
    limit: Duration,
    object: Arc<K>,
    ctx: &Context,
    reconcile: impl Future<Output = Result<Action, Error>>,
) -> Result<Action, Error>
where
    K: Resource,
    K::DynamicType: Default,
{
    match tokio::time::timeout(limit, reconcile).await {
        Ok(result) => result,
        Err(_) => {
            let n = ctx.count_error(&object_key(object.as_ref()));
            tracing::warn!(
                namespace = %object.namespace().unwrap_or_default(),
                name = %object.name_any(),
                timeouts = n,
                "the reconcile did not finish in {limit:?}: dropped and requeued"
            );
            if n > QUICK_TIMEOUTS {
                // fixed, so the Recorder folds the repeats into one series
                let note = format!(
                    "the reconcile did not finish in {limit:?}, {} or more times in a row: backing off",
                    QUICK_TIMEOUTS + 1
                );
                ctx.warn(object.as_ref(), "ReconcileTimedOut", note).await;
            }
            Err(Error::TimedOut(limit))
        }
    }
}

/// A Daemon client and what it was built from, so a changed authority or
/// token rebuilds it.
struct CachedClient {
    fingerprint: String,
    client: Arc<DaemonClient>,
}

pub struct Context {
    pub client: Client,
    pub cfg: OperatorConfig,
    pub run: RunConfig,
    /// `<ns>/<fleet>` to the record the last Fleet reconcile read (§21.1).
    pub records: RwLock<BTreeMap<String, FleetRecord>>,
    /// `<ns>/<fleet>` to the Agent object names the last plan made: the
    /// Agent controller's Fleet watch maps through this.
    pub fleet_agents: RwLock<BTreeMap<String, Vec<String>>>,
    /// `<ns>/<daemon>` to the Fleets naming it: the Fleet controller's
    /// Daemon watch maps through this.
    pub daemon_fleets: RwLock<BTreeMap<String, Vec<String>>>,
    clients: Mutex<BTreeMap<String, CachedClient>>,
    /// `<ns>/<fleet>/<crew>` to the crew's lock (§5.3): held across
    /// `jobs::crew_busy` and the create, so a sync and a harvest that both
    /// see the crew idle cannot both start. In-process: the operator is one
    /// replica (§21.1).
    crew_locks: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Consecutive reconcile errors per object, for `error_policy`.
    errors: Mutex<BTreeMap<String, u32>>,
    /// Warning Events on the objects (§22.5).
    recorder: Recorder,
}

impl Context {
    pub fn new(client: Client, run: RunConfig) -> Self {
        let cfg = OperatorConfig {
            version: run.version.clone(),
            images: run.images.clone(),
            namespace: run.namespace.clone(),
        };
        let recorder = Recorder::new(
            client.clone(),
            Reporter {
                controller: "balerix-operator".into(),
                instance: None,
            },
        );
        Self {
            client,
            cfg,
            run,
            records: RwLock::new(BTreeMap::new()),
            fleet_agents: RwLock::new(BTreeMap::new()),
            daemon_fleets: RwLock::new(BTreeMap::new()),
            clients: Mutex::new(BTreeMap::new()),
            crew_locks: Mutex::new(BTreeMap::new()),
            errors: Mutex::new(BTreeMap::new()),
            recorder,
        }
    }

    /// A Warning Event on `object` (§22.5). Best effort: an Event the API
    /// server refuses (no RBAC, a lost request) is logged and dropped.
    pub async fn warn<K>(&self, object: &K, reason: &str, note: String)
    where
        K: Resource,
        K::DynamicType: Default,
    {
        let event = Event {
            type_: EventType::Warning,
            reason: reason.into(),
            note: Some(note),
            action: "Reconcile".into(),
            secondary: None,
        };
        let reference = object.object_ref(&Default::default());
        if let Err(e) = self.recorder.publish(&event, &reference).await {
            tracing::warn!(
                namespace = %object.namespace().unwrap_or_default(),
                name = %object.name_any(),
                reason,
                "cannot publish the Event: {e}"
            );
        }
    }

    /// One more consecutive failure of the object; the count.
    fn count_error(&self, key: &str) -> u32 {
        let mut errors = self.errors.lock().unwrap_or_else(|e| e.into_inner());
        let n = errors.entry(key.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    }

    fn errors_of(&self, key: &str) -> u32 {
        self.errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    pub fn now(&self) -> i64 {
        (self.run.clock)()
    }

    pub fn k8s_now(&self) -> Time {
        // the epoch if the clock is out of jiff's range, which a test clock is not
        Time(k8s_openapi::jiff::Timestamp::from_second(self.now()).unwrap_or_default())
    }

    pub fn key(namespace: &str, name: &str) -> String {
        format!("{namespace}/{name}")
    }

    /// The lock of one crew; the same `Arc` for every caller.
    pub fn crew_lock(
        &self,
        namespace: &str,
        fleet: &str,
        crew: &str,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.crew_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(format!("{namespace}/{fleet}/{crew}"))
            .or_default()
            .clone()
    }

    /// The client for one Daemon, rebuilt when its authority or token
    /// changed. Under `insecure_daemon_url` every Daemon is the stub.
    pub fn daemon_client(
        &self,
        namespace: &str,
        daemon: &str,
        endpoint: &str,
        authority_pem: &str,
        token: &str,
    ) -> Result<Arc<DaemonClient>, Error> {
        let fingerprint = hash(&serde_json::json!([endpoint, authority_pem, token]));
        let key = Self::key(namespace, daemon);
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = clients.get(&key).filter(|c| c.fingerprint == fingerprint) {
            return Ok(cached.client.clone());
        }
        let client = match &self.run.insecure_daemon_url {
            Some(url) => DaemonClient::insecure_for_tests(url, token)?,
            None => DaemonClient::new_resolving(
                endpoint,
                authority_pem,
                token,
                Duration::from_secs(10),
                &self.run.resolve,
            )?,
        };
        let client = Arc::new(client);
        clients.insert(
            key,
            CachedClient {
                fingerprint,
                client: client.clone(),
            },
        );
        Ok(client)
    }
}

/// `<ns>/<name>` of any object, for the caches and the logs.
pub fn object_key<K: Resource>(object: &K) -> String {
    Context::key(&object.namespace().unwrap_or_default(), &object.name_any())
}

/// Server-side apply, forced, under the operator's field manager (§5).
pub async fn apply<K>(client: &Client, object: &K) -> Result<K, Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + DeserializeOwned
        + Serialize,
    K::DynamicType: Default,
{
    let namespace = object.namespace().ok_or_else(|| {
        Error::Missing(format!("{} has no metadata.namespace", object.name_any()))
    })?;
    let api: Api<K> = Api::namespaced(client.clone(), &namespace);
    Ok(api
        .patch(
            &object.name_any(),
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(object),
        )
        .await?)
}

/// A merge patch of `status` through the subresource.
pub async fn patch_status<K, S>(client: &Client, object: &K, status: &S) -> Result<(), Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + DeserializeOwned,
    K::DynamicType: Default,
    S: Serialize,
{
    let namespace = object.namespace().ok_or_else(|| {
        Error::Missing(format!("{} has no metadata.namespace", object.name_any()))
    })?;
    let api: Api<K> = Api::namespaced(client.clone(), &namespace);
    api.patch_status(
        &object.name_any(),
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "status": status })),
    )
    .await?;
    Ok(())
}

/// `old` with one condition replaced in place (appended when absent),
/// its transition time kept while its status holds (`conditions`).
pub fn replace_condition(
    old: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
    new: Cond,
    generation: Option<i64>,
    now: &Time,
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    let mut all = old.to_vec();
    for c in conditions(old, &[new], generation, now) {
        match all.iter_mut().find(|o| o.type_ == c.type_) {
            Some(o) => *o = c,
            None => all.push(c),
        }
    }
    all
}

/// A reconcile that ended well resets the object's error count.
pub fn reconciled<K: Resource>(ctx: &Context, object: &K) {
    ctx.errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&object_key(object));
}

/// The requeue after a failed reconcile: `Waiting` in 2 s, `TimedOut` per §22.2, anything else by `backoff`.
pub fn error_policy<K: Resource>(object: Arc<K>, error: &Error, ctx: Arc<Context>) -> Action {
    let key = object_key(object.as_ref());
    let (namespace, name) = (object.namespace().unwrap_or_default(), object.name_any());
    if let Error::Waiting(why) = error {
        tracing::debug!(namespace = %namespace, name = %name, "{why}");
        return Action::requeue(Duration::from_secs(2));
    }
    // `within` counted it: the first ones are lost requests, not the object
    if let Error::TimedOut(_) = error {
        let n = ctx.errors_of(&key);
        return Action::requeue(if n <= QUICK_TIMEOUTS {
            Duration::from_secs(2)
        } else {
            backoff(n)
        });
    }
    let attempt = ctx.count_error(&key);
    let delay = backoff(attempt);
    tracing::warn!(namespace = %namespace, name = %name, attempt, "reconcile failed: {error}");
    Action::requeue(delay)
}

/// 5 s, doubling per consecutive failure of the same object, at most 5 min.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((5u64 << attempt.saturating_sub(1).min(6)).min(300))
}

/// What a controller's stream yields, as a log line.
pub fn report<K: Resource>(
    kind: &'static str,
    result: Result<
        (kube::runtime::reflector::ObjectRef<K>, Action),
        kube::runtime::controller::Error<Error, kube::runtime::watcher::Error>,
    >,
) where
    K::DynamicType: std::fmt::Debug + std::hash::Hash + Eq + Clone,
{
    match result {
        Ok((object, _)) => tracing::debug!(kind, object = %object, "reconciled"),
        Err(e) => tracing::warn!(kind, "{e}"),
    }
}

/// The Api a controller set works on: one namespace, or every one.
pub fn api_in<K>(client: &Client, namespace: Option<&str>) -> Api<K>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>,
    K::DynamicType: Default,
{
    match namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    }
}

/// The controllers, one set per watched namespace, until the future is
/// dropped. `client` serves the reconciles; `watches`, from
/// `watch_client::watch_client`, every watch (hyper#4207, #129).
pub async fn run(client: Client, watches: Client, cfg: RunConfig) {
    let ctx = Arc::new(Context::new(client, cfg));
    let namespaces: Vec<Option<String>> = match &ctx.run.watch_namespaces {
        Some(list) if !list.is_empty() => list.iter().cloned().map(Some).collect(),
        _ => vec![None],
    };
    let sets = namespaces
        .into_iter()
        .map(|ns| set(ctx.clone(), &watches, ns));
    futures_util::future::join_all(sets).await;
}

/// The four controllers over one namespace (or all).
async fn set(ctx: Arc<Context>, watches: &Client, namespace: Option<String>) {
    tracing::info!(namespace = namespace.as_deref().unwrap_or("*"), "watching");
    let ns = namespace.as_deref();
    let controllers: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> = vec![
        Box::pin(daemon::controller(ctx.clone(), watches, ns)),
        Box::pin(fleet::controller(ctx.clone(), watches, ns)),
        Box::pin(crew::controller(ctx.clone(), watches, ns)),
        Box::pin(agent::controller(ctx.clone(), watches, ns)),
    ];
    futures_util::future::join_all(controllers).await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use k8s_openapi::api::core::v1::ConfigMap;

    fn object() -> Arc<ConfigMap> {
        let mut c = ConfigMap::default();
        c.metadata.name = Some("o".into());
        c.metadata.namespace = Some("ns".into());
        Arc::new(c)
    }

    /// Nothing listens on port 9: building the client opens no connection,
    /// and an Event published through it fails at once.
    fn context() -> Arc<Context> {
        let client =
            Client::try_from(kube::Config::new("http://127.0.0.1:9".parse().unwrap())).unwrap();
        Arc::new(Context::new(
            client,
            RunConfig::new(
                "0.2.0",
                crate::desired::common::Images::for_version("0.2.0"),
                "ns",
            ),
        ))
    }

    #[tokio::test]
    async fn a_reconcile_inside_the_bound_keeps_its_result() {
        let done = within(Duration::from_secs(30), object(), &context(), async {
            Ok(Action::requeue(Duration::from_secs(7)))
        })
        .await;
        assert_eq!(done.unwrap(), Action::requeue(Duration::from_secs(7)));
    }

    #[tokio::test]
    async fn two_timeouts_requeue_in_two_seconds_and_the_third_backs_off() {
        let ctx = context();
        let limit = Duration::from_millis(20);
        let mut delays = Vec::new();
        for _ in 0..4 {
            // a request that is never answered
            let lost = std::future::pending::<Result<Action, Error>>();
            let error = within(limit, object(), &ctx, lost).await.unwrap_err();
            assert!(matches!(error, Error::TimedOut(d) if d == limit), "{error}");
            delays.push(error_policy(object(), &error, ctx.clone()));
        }
        // the third and fourth also published the Event; that it could not
        // be written cost nothing
        let secs = |s| Action::requeue(Duration::from_secs(s));
        assert_eq!(delays, vec![secs(2), secs(2), secs(20), secs(40)]);
        // a reconcile that ends well starts the count again
        reconciled(&ctx, object().as_ref());
        let lost = std::future::pending::<Result<Action, Error>>();
        let error = within(limit, object(), &ctx, lost).await.unwrap_err();
        assert_eq!(error_policy(object(), &error, ctx), secs(2));
    }

    #[test]
    fn the_backoff_doubles_from_five_seconds_to_five_minutes() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(3), Duration::from_secs(20));
        assert_eq!(backoff(7), Duration::from_secs(300));
        assert_eq!(backoff(60), Duration::from_secs(300));
    }
}
