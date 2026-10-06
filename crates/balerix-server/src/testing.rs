//! Test doubles for the daemon: an in-memory `FleetStore` and a `Ports`
//! bundle over the `balerix-core` fakes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Mutex as StdMutex};
use std::time::Duration;

use balerix_api::Timestamp;
use balerix_core::fakes::{
    FakeClock, FakeCredentials, FakeMaterializer, FakeResolver, FakeRunner, FakeSystemToolchain,
    FakeWorkspace,
};
use balerix_core::{
    FleetName, FleetRecord, FleetSecrets, FleetStore, ReconcilePolicy, StoreError, SystemToolchain,
};

use crate::actor::Ports;
use crate::daemon::{Daemon, DaemonHandler};
use crate::kube::LinkHub;
use crate::metrics::Metrics;
use crate::plugins::{PluginClient, PluginKv, PluginRegistry, PluginSetup};
use crate::vault::Vault;

#[derive(Default)]
pub struct MemoryStore {
    fleets: Mutex<BTreeMap<String, (FleetRecord, FleetSecrets)>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, (FleetRecord, FleetSecrets)>> {
        self.fleets.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn names(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }
    pub fn get(&self, name: &str) -> Option<(FleetRecord, FleetSecrets)> {
        self.lock().get(name).cloned()
    }
}

impl FleetStore for MemoryStore {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError> {
        Ok(self.lock().values().cloned().collect())
    }
    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError> {
        self.lock()
            .insert(record.name().to_string(), (record.clone(), secrets.clone()));
        Ok(())
    }
    fn purge(&self, name: &FleetName) -> Result<(), StoreError> {
        self.lock().remove(name.as_str());
        Ok(())
    }
}

/// Fakes plus the `Ports` the actor and daemon take.
pub struct Harness {
    pub materializer: Arc<FakeMaterializer>,
    pub runner: Arc<FakeRunner>,
    pub clock: Arc<FakeClock>,
    pub store: Arc<MemoryStore>,
    pub workspace: Arc<FakeWorkspace>,
    pub resolver: Arc<FakeResolver>,
    pub credentials: Arc<FakeCredentials>,
    pub ports: Arc<Ports>,
    pub registry: Arc<PluginRegistry>,
    pub client: PluginClient,
    /// Owns the `kv` store's directory: dropped with the harness.
    pub kv_dir: tempfile::TempDir,
    pub kv: Arc<PluginKv>,
    /// `Harness::kube`'s link hub, also `ports.runner`, `ports.workspace`
    /// and `ports.kube`.
    pub hub: Option<Arc<LinkHub>>,
}

impl Harness {
    pub fn new(resync: Duration) -> Self {
        Self::with_policy(resync, ReconcilePolicy::default())
    }

    pub fn with_policy(resync: Duration, policy: ReconcilePolicy) -> Self {
        let materializer = Arc::new(FakeMaterializer::default());
        let runner = Arc::new(FakeRunner::default());
        let clock = Arc::new(FakeClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new());
        let workspace = Arc::new(FakeWorkspace::default());
        let resolver = Arc::new(FakeResolver::default());
        let credentials = Arc::new(FakeCredentials::default());
        let ports = Arc::new(Ports {
            materializer: materializer.clone(),
            runner: runner.clone(),
            clock: clock.clone(),
            store: store.clone(),
            workspace: workspace.clone(),
            resolver: resolver.clone(),
            credentials: credentials.clone(),
            policy,
            hook_url: "http://127.0.0.1:1".to_string(),
            resync,
            kube: None,
        });
        let kv_dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let kv = Arc::new(PluginKv::new(
            kv_dir.path().join("plugins"),
            Vault::from_key([7u8; 32]),
        ));
        Self {
            materializer,
            runner,
            clock,
            store,
            workspace,
            resolver,
            credentials,
            ports,
            registry: PluginRegistry::new(),
            client: PluginClient::new(None).unwrap_or_else(|e| panic!("http client: {e}")),
            kv_dir,
            kv,
            hub: None,
        }
    }

    /// Kubernetes mode (Spec O §7): the runner and the workspace reader are
    /// the link hub and `Ports.kube` is set, so actors mirror and plan
    /// nothing. The fake materializer stays: nothing calls it.
    pub fn kube(resync: Duration) -> Self {
        let mut h = Self::new(resync);
        let hub = LinkHub::new();
        let ports = Ports {
            runner: hub.clone(),
            workspace: hub.clone(),
            kube: Some(hub.clone()),
            ..(*h.ports).clone()
        };
        h.ports = Arc::new(ports);
        h.hub = Some(hub);
        h
    }

    /// A daemon over these fakes with an empty fleet set.
    pub fn daemon(&self, handler: Arc<dyn DaemonHandler>, plugin_dir: &Path) -> Arc<Daemon> {
        self.daemon_with_token(handler, plugin_dir, "admin-tok")
    }

    /// The same, with the admin token the test's client presents.
    pub fn daemon_with_token(
        &self,
        handler: Arc<dyn DaemonHandler>,
        plugin_dir: &Path,
        token: &str,
    ) -> Arc<Daemon> {
        self.daemon_full(
            handler,
            plugin_dir,
            token,
            Metrics::new().unwrap_or_else(|e| panic!("metrics: {e}")),
            Vec::new(),
            ready_toolchain(),
        )
    }

    /// The same, over a `Metrics` the caller also gave the event handler:
    /// `/metrics` encodes the daemon's registry, so the chain's counters
    /// only show up there when the two share one.
    pub fn daemon_with(
        &self,
        handler: Arc<dyn DaemonHandler>,
        plugin_dir: &Path,
        metrics: Metrics,
    ) -> Arc<Daemon> {
        self.daemon_full(
            handler,
            plugin_dir,
            "admin-tok",
            metrics,
            Vec::new(),
            ready_toolchain(),
        )
    }

    /// The same, starting from stored records and over a system toolchain
    /// the caller controls: the records are what a daemon restart hands
    /// `Daemon::start`, and the toolchain is what its pool actor drives —
    /// the only thing that opens the fleets' readiness gate (Spec F §4).
    pub fn daemon_with_existing(
        &self,
        handler: Arc<dyn DaemonHandler>,
        plugin_dir: &Path,
        existing: Vec<(FleetRecord, FleetSecrets)>,
        toolchain: Arc<dyn SystemToolchain>,
    ) -> Arc<Daemon> {
        self.daemon_full(
            handler,
            plugin_dir,
            "admin-tok",
            Metrics::new().unwrap_or_else(|e| panic!("metrics: {e}")),
            existing,
            toolchain,
        )
    }

    /// Kubernetes mode's plugin source (Spec O §23.2): the operator's list,
    /// hellos persisted under `dir/plugins`, managed fleet requests under
    /// `dir/managed`, plugins called with `client`
    /// (with or without an authority).
    pub fn daemon_declared(
        &self,
        handler: Arc<dyn DaemonHandler>,
        dir: &Path,
        token: &str,
        client: PluginClient,
    ) -> Arc<Daemon> {
        self.start(
            handler,
            token,
            Metrics::new().unwrap_or_else(|e| panic!("metrics: {e}")),
            Vec::new(),
            ready_toolchain(),
            PluginSetup::Declared {
                state_dir: dir.join("plugins"),
                managed_dir: dir.join("managed"),
            },
            client,
        )
    }

    fn daemon_full(
        &self,
        handler: Arc<dyn DaemonHandler>,
        plugin_dir: &Path,
        token: &str,
        metrics: Metrics,
        existing: Vec<(FleetRecord, FleetSecrets)>,
        toolchain: Arc<dyn SystemToolchain>,
    ) -> Arc<Daemon> {
        self.start(
            handler,
            token,
            metrics,
            existing,
            toolchain,
            PluginSetup::Packages(plugin_config_in(plugin_dir)),
            self.client.clone(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &self,
        handler: Arc<dyn DaemonHandler>,
        token: &str,
        metrics: Metrics,
        existing: Vec<(FleetRecord, FleetSecrets)>,
        toolchain: Arc<dyn SystemToolchain>,
        plugins: PluginSetup,
        client: PluginClient,
    ) -> Arc<Daemon> {
        // The runner, the workspace reader and `kube` come from `self.ports`:
        // `Harness::kube` swaps those three for the link hub.
        let ports = Ports {
            materializer: self.materializer.clone(),
            clock: self.clock.clone(),
            store: self.store.clone(),
            resolver: self.resolver.clone(),
            credentials: self.credentials.clone(),
            ..(*self.ports).clone()
        };
        Daemon::start(
            ports,
            handler,
            metrics,
            token.to_string(),
            existing,
            plugins,
            self.registry.clone(),
            client,
            self.kv.clone(),
            toolchain,
        )
    }
}

/// The system toolchain every daemon helper that does not care about the
/// pool is built with: ready on the first attempt, so the readiness gate
/// (Spec F §5) is open and these tests see the behaviour they always did.
pub fn ready_toolchain() -> Arc<dyn SystemToolchain> {
    Arc::new(FakeSystemToolchain::ready())
}

/// A `PluginHostConfig` under a test directory: no `plugins.yaml` yet, so
/// the first sync is a no-op.
pub fn plugin_config_in(dir: &std::path::Path) -> crate::plugins::PluginHostConfig {
    crate::plugins::PluginHostConfig {
        plugins_file: dir.join("plugins.yaml"),
        install_root: dir.join("plugins"),
    }
}

/// A scripted plugin endpoint for daemon tests: records every call,
/// rejects activation for agents named in `reject` with that message,
/// answers intercepts with `verdict` (merged over `response_so_far`).
#[derive(Clone, Default)]
pub struct StubScript {
    pub reject: BTreeMap<String, String>,
    pub verdict: serde_json::Value,
    pub actions: Vec<balerix_api::PluginAction>,
    pub health_ok: bool,
    pub metrics_body: String,
    pub expect_token: Option<String>,
    /// (certificate, key) PEM: serve `https://` (Spec O §23.1).
    pub tls: Option<(PathBuf, PathBuf)>,
    /// While it holds `true`, `/v1/health` answers 503 whatever
    /// `health_ok` says: missed polls, then a recovery (§23.2).
    pub health_down: Option<Arc<std::sync::atomic::AtomicBool>>,
}

#[derive(Clone)]
pub struct StubPlugin {
    pub listen: String,
    pub calls: Arc<StdMutex<Vec<(String, serde_json::Value)>>>,
}

impl StubPlugin {
    pub fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn calls_named(&self, route: &str) -> Vec<serde_json::Value> {
        self.calls()
            .into_iter()
            .filter(|(r, _)| r == route)
            .map(|(_, v)| v)
            .collect()
    }
}

pub async fn stub_plugin(script: StubScript) -> StubPlugin {
    use axum::extract::State;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};
    let script_tls = script.tls.clone();
    let calls = Arc::new(StdMutex::new(Vec::new()));
    #[derive(Clone)]
    struct S {
        script: StubScript,
        calls: Arc<StdMutex<Vec<(String, Value)>>>,
    }
    let record = |s: &S, route: &str, v: Value| {
        s.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((route.to_string(), v));
    };
    async fn check_token(
        State(s): State<S>,
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let ok = match &s.script.expect_token {
            None => true,
            Some(want) => crate::auth::bearer(req.headers())
                .is_some_and(|got| crate::auth::constant_time_eq(got.as_bytes(), want.as_bytes())),
        };
        if ok {
            next.run(req).await
        } else {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "bad daemon token" })),
            )
                .into_response()
        }
    }
    let state = S {
        script,
        calls: calls.clone(),
    };
    let app = Router::new()
        .route(
            "/v1/activate",
            post(move |State(s): State<S>, Json(v): Json<Value>| async move {
                record(&s, "activate", v.clone());
                let agent = v["agent"].as_str().unwrap_or_default();
                match s.script.reject.get(agent) {
                    Some(msg) => (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({ "error": msg })),
                    ),
                    None => (axum::http::StatusCode::OK, Json(json!({}))),
                }
            }),
        )
        .route(
            "/v1/deactivate",
            post(move |State(s): State<S>, Json(v): Json<Value>| async move {
                record(&s, "deactivate", v);
                Json(json!({}))
            }),
        )
        .route(
            "/v1/events",
            post(move |State(s): State<S>, Json(v): Json<Value>| async move {
                record(&s, "events", v);
                Json(json!({}))
            }),
        )
        .route(
            "/v1/intercept",
            post(move |State(s): State<S>, Json(v): Json<Value>| async move {
                record(&s, "intercept", v.clone());
                let mut r = v["response_so_far"].clone();
                if let (Some(dst), Some(src)) = (r.as_object_mut(), s.script.verdict.as_object()) {
                    for (k, val) in src {
                        dst.insert(k.clone(), val.clone());
                    }
                }
                Json(json!({ "response": r, "actions": s.script.actions }))
            }),
        )
        .route(
            "/v1/health",
            get(move |State(s): State<S>| async move {
                record(&s, "health", json!({}));
                let down = s
                    .script
                    .health_down
                    .as_ref()
                    .is_some_and(|d| d.load(std::sync::atomic::Ordering::SeqCst));
                if s.script.health_ok && !down {
                    axum::http::StatusCode::OK
                } else {
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        )
        .route(
            "/v1/metrics",
            get(move |State(s): State<S>| async move { s.script.metrics_body.clone() }),
        )
        .route(
            "/v1/routes/ws",
            get(|ws: axum::extract::ws::WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(msg)) = socket.recv().await {
                        if matches!(msg, axum::extract::ws::Message::Text(_))
                            && socket.send(msg).await.is_err()
                        {
                            break;
                        }
                    }
                })
            }),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            check_token,
        ))
        .with_state(state);
    let tls = script_tls;
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("bind: {e}"));
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|e| panic!("nonblocking: {e}"));
    let port = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("addr: {e}"))
        .port();
    let listen = if tls.is_some() {
        format!("https://127.0.0.1:{port}")
    } else {
        format!("127.0.0.1:{port}")
    };
    if let Some((cert, key)) = tls {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
            .await
            .unwrap_or_else(|e| panic!("tls: {e}"));
        let server = axum_server::from_tcp_rustls(listener, config)
            .unwrap_or_else(|e| panic!("tls listener: {e}"));
        tokio::spawn(async move {
            let _ = server.serve(app.into_make_service()).await;
        });
    } else {
        let listener =
            tokio::net::TcpListener::from_std(listener).unwrap_or_else(|e| panic!("listener: {e}"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
    }
    StubPlugin { listen, calls }
}

/// A package directory for tests: `balerix-plugin.yaml` with the given
/// extra lines (hooks, needs) and a `mise.toml` whose start task is `true`.
pub fn write_plugin_package(dir: &Path, name: &str, manifest_extra: &str) {
    let _ = std::fs::create_dir_all(dir);
    let manifest = format!(
        "apiVersion: balerix/v1\nkind: Plugin\nname: {name}\nversion: 0.1.0\nprotocol: 1\nstart: serve\n{manifest_extra}"
    );
    let _ = std::fs::write(dir.join("balerix-plugin.yaml"), manifest);
    let _ = std::fs::write(
        dir.join("mise.toml"),
        "[tools]\n[tasks.serve]\nrun = \"true\"\n",
    );
}

/// (ca.crt, tls.crt, tls.key) for 127.0.0.1 in `dir`: a throwaway authority
/// and the leaf it signs, for the TLS stub plugin (Spec O §23.1).
pub fn test_authority(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let io = |what: &str, e: &dyn std::fmt::Display| -> ! { panic!("authority: {what}: {e}") };
    let mut ca =
        rcgen::CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|e| io("ca params", &e));
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap_or_else(|e| io("ca key", &e));
    let ca_cert = ca
        .self_signed(&ca_key)
        .unwrap_or_else(|e| io("ca cert", &e));
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let key = rcgen::KeyPair::generate().unwrap_or_else(|e| io("key", &e));
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap_or_else(|e| io("leaf params", &e))
        .signed_by(&key, &issuer)
        .unwrap_or_else(|e| io("leaf", &e));
    let paths = (dir.join("ca.crt"), dir.join("tls.crt"), dir.join("tls.key"));
    for (path, pem) in [
        (&paths.0, ca_cert.pem()),
        (&paths.1, leaf.pem()),
        (&paths.2, key.serialize_pem()),
    ] {
        std::fs::write(path, pem).unwrap_or_else(|e| io("write", &e));
    }
    paths
}
