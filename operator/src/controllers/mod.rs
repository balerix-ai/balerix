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
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use balerix_api::FleetRecord;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Api, Client, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::daemon_client::{ClientError, DaemonClient};
use crate::desired::common::{DesiredError, Images, MANAGER, OperatorConfig, hash};
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
}

impl From<kube::runtime::finalizer::Error<Error>> for Error {
    fn from(e: kube::runtime::finalizer::Error<Error>) -> Self {
        Error::Finalizer(Box::new(e))
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
    /// Consecutive reconcile errors per object, for `error_policy`.
    errors: Mutex<BTreeMap<String, u32>>,
}

impl Context {
    pub fn new(client: Client, run: RunConfig) -> Self {
        let cfg = OperatorConfig {
            version: run.version.clone(),
            images: run.images.clone(),
            namespace: run.namespace.clone(),
        };
        Self {
            client,
            cfg,
            run,
            records: RwLock::new(BTreeMap::new()),
            fleet_agents: RwLock::new(BTreeMap::new()),
            daemon_fleets: RwLock::new(BTreeMap::new()),
            clients: Mutex::new(BTreeMap::new()),
            errors: Mutex::new(BTreeMap::new()),
        }
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

/// A reconcile that ended well resets the object's error count.
pub fn reconciled<K: Resource>(ctx: &Context, object: &K) {
    ctx.errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&object_key(object));
}

/// 5 s, doubling per consecutive failure of the same object, at most 5 min.
pub fn error_policy<K: Resource>(object: Arc<K>, error: &Error, ctx: Arc<Context>) -> Action {
    let key = object_key(object.as_ref());
    if let Error::Waiting(why) = error {
        tracing::debug!(object = %key, "{why}");
        return Action::requeue(Duration::from_secs(2));
    }
    let attempt = {
        let mut errors = ctx.errors.lock().unwrap_or_else(|e| e.into_inner());
        let n = errors.entry(key.clone()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    };
    let delay = Duration::from_secs((5u64 << attempt.saturating_sub(1).min(6)).min(300));
    tracing::warn!(object = %key, attempt, "reconcile failed: {error}");
    Action::requeue(delay)
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
/// dropped. Tasks 5–8 add a line per controller to `set`.
pub async fn run(client: Client, cfg: RunConfig) {
    let ctx = Arc::new(Context::new(client, cfg));
    let namespaces: Vec<Option<String>> = match &ctx.run.watch_namespaces {
        Some(list) if !list.is_empty() => list.iter().cloned().map(Some).collect(),
        _ => vec![None],
    };
    let sets = namespaces.into_iter().map(|ns| set(ctx.clone(), ns));
    futures_util::future::join_all(sets).await;
}

/// The four controllers over one namespace (or all).
async fn set(ctx: Arc<Context>, namespace: Option<String>) {
    tracing::info!(namespace = namespace.as_deref().unwrap_or("*"), "watching");
    let ns = namespace.as_deref();
    let controllers: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> = vec![
        Box::pin(daemon::controller(ctx.clone(), ns)),
        Box::pin(fleet::controller(ctx.clone(), ns)),
        // Task 7: Box::pin(crew::controller(ctx.clone(), ns)),
        // Task 8: Box::pin(agent::controller(ctx.clone(), ns)),
    ];
    futures_util::future::join_all(controllers).await;
}
