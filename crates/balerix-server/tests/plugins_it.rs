//! The plugin host over the fakes through a real listener (plugins spec
//! §11 "server integration", phase 1 slice): sync, hello, list, the
//! reserved fleet, removal and purge.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{AgentPhase, AgentSettings, CrewSpec, FleetRequest, FleetSpec, GitSettings};
use balerix_core::{AgentId, FleetRecord, PassThrough, plugin_id};
use balerix_server::testing::{Harness, plugin_config_in};
use balerix_server::{Daemon, Metrics, router, serve};
use serde_json::{Value, json};

const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\nroutes: true\n";
const MISE: &str = "[tools]\n[tasks.serve]\nrun = \"true\"\n";

fn write_package(dir: &Path, manifest: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("balerix-plugin.yaml"), manifest).unwrap();
    fs::write(dir.join("mise.toml"), MISE).unwrap();
}

struct Api {
    base: String,
    agent: ureq::Agent,
}

impl Api {
    fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => self.agent.get(&url).force_send_body(),
            "POST" => self.agent.post(&url),
            "DELETE" => self.agent.delete(&url).force_send_body(),
            _ => unreachable!(),
        };
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let mut resp = match body {
            Some(b) => req.send_json(b).unwrap(),
            None => req.send_empty().unwrap(),
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap();
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, v)
    }
}

fn wait(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "condition not reached"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

const ADMIN: Option<&str> = Some("admin-tok");

/// One synced plugin `hello` behind a real listener: what every test
/// below starts from.
struct Fixture {
    dir: tempfile::TempDir,
    plugins_yaml: std::path::PathBuf,
    h: Harness,
    daemon: Arc<Daemon>,
    api: Api,
    id: AgentId,
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let plugins_yaml = dir.path().join("plugins.yaml");
        let package = dir.path().join("hello-pkg");
        write_package(&package, MANIFEST);
        fs::write(
            &plugins_yaml,
            "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n",
        )
        .unwrap();

        let h = Harness::new(Duration::from_secs(3600));
        let daemon = h.daemon(Arc::new(PassThrough), dir.path());
        let report = daemon.sync_plugins().await.unwrap();
        assert_eq!(report.installed, vec!["hello"]);
        assert!(report.stopped.is_empty() && report.unchanged.is_empty());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, router(daemon.clone()), async {
            let _ = stop_rx.await;
        }));
        let api = Api {
            base,
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into(),
        };
        let id: AgentId = plugin_id(&"hello".parse().unwrap());
        Fixture {
            dir,
            plugins_yaml,
            h,
            daemon,
            api,
            id,
            stop,
            server,
        }
    }

    async fn token(&self) -> String {
        self.daemon.hook_secret(&self.id).await.unwrap()
    }

    /// The plugin says a good `hello` and the actor marks it Ready.
    async fn ready(&self) {
        let fx = self;
        let token = fx.token().await;
        let hello = |name: &str, protocol: u32, listen: &str| json!({ "name": name, "version": "0.1.0", "protocol": protocol, "listen": listen });
        let (s, body) = fx.api.call(
            "POST",
            "/v1/plugin-host/hello",
            Some(&token),
            Some(&hello("hello", 1, "127.0.0.1:4000")),
        );
        assert_eq!(s, 200, "{body}");
        assert_eq!(body["config"]["greeting"], "hi");
        {
            let d = fx.daemon.clone();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let rows = d.plugin_host().unwrap().list().await;
                    if rows[0].phase == AgentPhase::Ready {
                        assert_eq!(rows[0].listen.as_deref(), Some("127.0.0.1:4000"));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        self.server.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_starts_the_plugin_and_lists_it() {
    let fx = Fixture::new().await;
    // the actor's pass materialized and started the synthetic agent
    {
        let m = fx.h.materializer.clone();
        let r = fx.h.runner.clone();
        tokio::task::spawn_blocking(move || {
            wait(|| {
                m.calls()
                    .contains(&"materialize_plugin balerix/plugins/hello".to_string())
                    && r.calls()
                        .contains(&"ensure_agent balerix/plugins/hello".to_string())
            })
        })
        .await
        .unwrap();
    }
    let (s, rows) = fx.api.call("GET", "/v1/plugins", ADMIN, None);
    assert_eq!(s, 200);
    assert_eq!(rows[0]["name"], "hello");
    assert_eq!(rows[0]["version"], "0.1.0");
    assert_eq!(rows[0]["routes"], true);
    assert!(rows[0]["listen"].is_null());
    assert_eq!(rows[0]["phase"], "starting");
    let (s, _) = fx.api.call("GET", "/v1/plugins", None, None);
    assert_eq!(s, 401, "admin token required");

    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_checks_the_token_protocol_and_listen_then_answers_the_config() {
    let fx = Fixture::new().await;
    let token = fx.token().await;
    // hello: token, protocol, listen, then Ready with the config
    let hello = |name: &str, protocol: u32, listen: &str| json!({ "name": name, "version": "0.1.0", "protocol": protocol, "listen": listen });
    let (s, _) = fx.api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some("wrong"),
        Some(&hello("hello", 1, "127.0.0.1:4000")),
    );
    assert_eq!(s, 401);
    let (s, _) = fx.api.call(
        "POST",
        "/v1/plugin-host/hello",
        None,
        Some(&hello("hello", 1, "127.0.0.1:4000")),
    );
    assert_eq!(s, 401);
    let (s, body) = fx.api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some(&token),
        Some(&hello("hello", 2, "127.0.0.1:4000")),
    );
    assert_eq!(s, 400);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("hello.protocol"),
        "{body}"
    );
    let (s, body) = fx.api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some(&token),
        Some(&hello("hello", 1, "0.0.0.0:4000")),
    );
    assert_eq!(
        (s, body["error"].as_str().unwrap()),
        (400, "hello.listen: must be a loopback address")
    );
    let (s, _body) = fx.api.call(
        "POST",
        "/v1/plugin-host/hello",
        Some(&token),
        Some(&hello("other", 1, "127.0.0.1:4000")),
    );
    // the handler checks the token against the body's name: this proves
    // that lookup (another plugin's token is no token for `other`), not
    // `PluginHost::hello`'s own name check, which HTTP cannot reach
    assert_eq!(s, 401, "a token for another name answers like a bad token");
    fx.ready().await;

    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reserved_fleet_is_readable_not_writable_and_not_listed() {
    let fx = Fixture::new().await;
    fx.ready().await;
    // the reserved fleet: readable, not writable, not listed
    let (s, rec) = fx.api.call("GET", "/v1/fleets/balerix", ADMIN, None);
    assert_eq!(s, 200);
    assert_eq!(
        rec["status"]["agents"]["balerix/plugins/hello"]["phase"],
        "ready"
    );
    let (s, rows) = fx.api.call("GET", "/v1/fleets", ADMIN, None);
    assert_eq!(
        (s, rows.as_array().unwrap().len()),
        (200, 0),
        "plugins are not a fleet row"
    );
    let spec = FleetSpec {
        name: "balerix".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let req = serde_json::to_value(FleetRequest {
        spec,
        credentials: Default::default(),
        agent_tokens: None,
        managed_by: None,
    })
    .unwrap();
    let (s, body) = fx.api.call("POST", "/v1/fleets", ADMIN, Some(&req));
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "name: \"balerix\" is reserved for the daemon's plugins"
    );
    let mut watch = req.clone();
    watch["spec"]["name"] = "watch".into();
    let (s, body) = fx.api.call("POST", "/v1/fleets", ADMIN, Some(&watch));
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "name: \"watch\" is reserved: it would shadow the plugin host's fleets/watch route"
    );
    let (s, _) = fx.api.call(
        "DELETE",
        "/v1/fleets/balerix?keep_repos=false&keep_sessions=false&purge=false",
        ADMIN,
        None,
    );
    assert_eq!(s, 400);

    // metrics carry the plugin fleet's gauges
    let (_, metrics) = fx.api.call("GET", "/metrics", None, None);
    assert!(
        metrics
            .as_str()
            .unwrap()
            .contains("balerix_agents{crew=\"plugins\",fleet=\"balerix\",phase=\"ready\"} 1"),
        "{metrics}"
    );

    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_sync_changes_nothing() {
    let fx = Fixture::new().await;
    // a secret colliding with an inline config key fails the sync, changes
    // nothing, and the error composes one `plugins.yaml:` prefix with the
    // entry index and the full `secrets.<key>` path — never the file's
    // contents.
    {
        use std::os::unix::fs::PermissionsExt;
        let secret = fx.dir.path().join("secret-pw");
        fs::write(&secret, "s3cr3t\n").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            &fx.plugins_yaml,
            "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n    secrets:\n      greeting: ./secret-pw\n",
        )
        .unwrap();
        let (s, body) = fx.api.call("POST", "/v1/plugins/sync", ADMIN, None);
        assert_eq!(s, 400);
        assert_eq!(
            body["error"],
            "plugins.yaml: plugins[0].secrets.greeting: collides with config.greeting"
        );
        assert!(
            !body["error"].as_str().unwrap().contains("s3cr3t"),
            "{body}"
        );
        // restore the file the "broken manifest" case below builds on.
        fs::write(
            &fx.plugins_yaml,
            "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n",
        )
        .unwrap();
    }

    // a broken manifest fails the sync and changes nothing
    let bad = fx.dir.path().join("bad-pkg");
    write_package(
        &bad,
        &MANIFEST
            .replace("name: hello", "name: bad")
            .replace("protocol: 1", "protocol: 7"),
    );
    fs::write(
        &fx.plugins_yaml,
        "plugins:\n  - name: hello\n    source: ./hello-pkg\n    config: { greeting: hi }\n  - name: bad\n    source: ./bad-pkg\n",
    )
    .unwrap();
    let (s, body) = fx.api.call("POST", "/v1/plugins/sync", ADMIN, None);
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "plugins.yaml: plugins[1]: balerix-plugin.yaml: protocol: this daemon speaks protocol 1, got 7"
    );
    assert_eq!(
        fx.daemon.plugin_host().unwrap().list().await.len(),
        1,
        "previous set kept"
    );

    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_refuses_while_declared_and_deletes_after_removal() {
    let fx = Fixture::new().await;
    fx.ready().await;
    // purge refuses while declared; removal stops; purge then deletes
    let (s, body) = fx.api.call("DELETE", "/v1/plugins/hello", ADMIN, None);
    assert_eq!(s, 400);
    assert_eq!(
        body["error"],
        "plugin \"hello\" is still declared in plugins.yaml; remove it first"
    );
    fs::write(&fx.plugins_yaml, "plugins: []\n").unwrap();
    let (s, report) = fx.api.call("POST", "/v1/plugins/sync", ADMIN, None);
    assert_eq!(s, 200);
    assert_eq!(report["stopped"], json!(["hello"]));
    assert!(fx.daemon.plugin_host().unwrap().list().await.is_empty());
    assert!(
        fx.daemon.hook_secret(&fx.id).await.is_none(),
        "a removed plugin's token is revoked"
    );
    // The actor answers `Apply` before the pass that stops the plugin, so
    // the sync above can return while the plugin is still running: purge
    // must wait for the pass rather than delete under it. Nothing here
    // waits for `stop_agent` — the DELETE does.
    let (s, _) = fx.api.call("DELETE", "/v1/plugins/hello", ADMIN, None);
    assert_eq!(s, 200);
    assert!(
        fx.h.runner
            .calls()
            .contains(&"stop_agent balerix/plugins/hello".to_string()),
        "purge waited until the plugin was stopped"
    );
    assert!(
        !fx.daemon
            .plugin_host()
            .unwrap()
            .record()
            .status
            .agents
            .contains_key("balerix/plugins/hello"),
        "and until the actor took it out of the record"
    );
    assert!(
        fx.h.materializer
            .calls()
            .contains(&"purge_plugin hello".to_string())
    );
    let (s, _) = fx.api.call("DELETE", "/v1/plugins/Nope", ADMIN, None);
    assert_eq!(s, 400);

    fx.finish().await;
}

/// #6: a second sync of the same `plugins.yaml` is the unchanged path:
/// the hash matches, nothing restarts, and the ready plugin keeps the
/// `listen` its `hello` gave.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_sync_leaves_the_plugin_unchanged_and_keeps_its_listen() {
    let fx = Fixture::new().await;
    fx.ready().await;
    let materialized = |calls: Vec<String>| {
        calls
            .iter()
            .filter(|c| *c == "materialize_plugin balerix/plugins/hello")
            .count()
    };
    let before = materialized(fx.h.materializer.calls());
    let (s, report) = fx.api.call("POST", "/v1/plugins/sync", ADMIN, None);
    assert_eq!(s, 200, "{report}");
    assert_eq!(
        (
            &report["installed"],
            &report["unchanged"],
            &report["stopped"]
        ),
        (&json!([]), &json!(["hello"]), &json!([])),
        "{report}"
    );
    let rows = fx.daemon.plugin_host().unwrap().list().await;
    assert_eq!(rows[0].listen.as_deref(), Some("127.0.0.1:4000"));
    assert_eq!(rows[0].phase, AgentPhase::Ready);
    assert_eq!(
        fx.daemon
            .registry()
            .ready_addr(&"hello".parse().unwrap())
            .map(|a| a.base()),
        Some("http://127.0.0.1:4000".to_string()),
        "still routable"
    );
    // the pass that follows the apply finds nothing to do: a barrier
    // answers after it (and one more) have run
    let (reply, rx) = tokio::sync::oneshot::channel();
    fx.daemon
        .plugin_host()
        .unwrap()
        .handle()
        .tx
        .send(balerix_server::Msg::Barrier { reply })
        .await
        .unwrap();
    assert!(rx.await.unwrap(), "a pass ran");
    assert_eq!(
        materialized(fx.h.materializer.calls()),
        before,
        "not restarted"
    );
    fx.finish().await;
}

/// #6: `hello` takes the per-plugin bucket after authentication, as the
/// hook route does per agent; a bad token never reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_is_rate_limited_per_plugin_after_authentication() {
    let fx = Fixture::new().await;
    let token = fx.token().await;
    // protocol 2: refused after the bucket, so nothing reaches the actor
    let bad =
        json!({ "name": "hello", "version": "0.1.0", "protocol": 2, "listen": "127.0.0.1:4000" });
    // 120 at once against a burst of 50 refilling at 20/s: however slow
    // the host, they all land within a second or two, far over the bucket
    let api = &fx.api;
    let statuses: Vec<u16> = tokio::task::block_in_place(|| {
        let start = std::sync::Barrier::new(120);
        std::thread::scope(|scope| {
            let calls: Vec<_> = (0..120)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        api.call("POST", "/v1/plugin-host/hello", Some(&token), Some(&bad))
                            .0
                    })
                })
                .collect();
            calls.into_iter().map(|c| c.join().unwrap()).collect()
        })
    });
    let limited = statuses.iter().filter(|s| **s == 429).count();
    assert!(limited > 0, "none of 120 limited: {statuses:?}");
    assert!(
        statuses.iter().all(|s| [400, 429].contains(s)),
        "{statuses:?}"
    );
    // with the bucket empty, a bad token is still refused, never limited
    for _ in 0..5 {
        let (s, _) = fx
            .api
            .call("POST", "/v1/plugin-host/hello", Some("wrong"), Some(&bad));
        assert_eq!(s, 401, "a bad token is refused, never limited");
    }
    fx.finish().await;
}

/// #1: a daemon that restarts with a plugin's window still running and
/// no record entry for it (the plugin was removed from `plugins.yaml`).
/// `purge` waits for a pass after its own request, so it never deletes
/// under a window the record never held; with no pass possible, it
/// deletes nothing.
async fn orphan_daemon(
    toolchain: Arc<dyn balerix_core::SystemToolchain>,
) -> (tempfile::TempDir, Harness, Arc<Daemon>) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("plugins.yaml"), "plugins: []\n").unwrap();
    let h = Harness::new(Duration::from_secs(3600));
    h.runner.set_state(
        &plugin_id(&"hello".parse().unwrap()),
        balerix_core::ProcessState::Running { pid: 4242 },
    );
    let daemon = h.daemon_with_existing(Arc::new(PassThrough), dir.path(), Vec::new(), toolchain);
    (dir, h, daemon)
}

fn stopped_hello(calls: &[String]) -> bool {
    calls
        .iter()
        .any(|c| c.starts_with("stop_") && c.contains("balerix/plugins"))
}

/// One ordered log two fakes share: whether the window's stop came
/// before the purge's deletion.
#[derive(Default)]
struct Journal(std::sync::Mutex<Vec<String>>);

impl Journal {
    fn push(&self, entry: String) {
        self.0.lock().unwrap().push(entry);
    }
    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

use balerix_core::fakes::{FakeMaterializer, FakeRunner};
use balerix_core::{
    AgentName, AgentRunner, CrewRef, CrewTools, FleetName, HookTarget, Keep, LaunchPlan,
    MaterializeError, Materializer, ObservedState, PtyStream, RepoRef, ResolvedAgent,
    ResolvedPlugin, RunnerError,
};

/// The harness runner, its stops written to the journal.
struct JournalRunner(Arc<FakeRunner>, Arc<Journal>);

impl AgentRunner for JournalRunner {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.0.ensure_crew(crew)
    }
    fn ensure_agent(&self, agent: &AgentId, plan: &LaunchPlan) -> Result<(), RunnerError> {
        self.0.ensure_agent(agent, plan)
    }
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.1.push(format!("stop_agent {agent}"));
        self.0.stop_agent(agent)
    }
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.1.push(format!("stop_crew {crew}"));
        self.0.stop_crew(crew)
    }
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        self.0.observe(fleet)
    }
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        self.0.send_text(agent, text, submit)
    }
    fn send_keys(
        &self,
        agent: &AgentId,
        steps: &[balerix_api::KeyStep],
        delay: Duration,
    ) -> Result<(), RunnerError> {
        self.0.send_keys(agent, steps, delay)
    }
    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError> {
        self.0.attach(agent)
    }
}

/// The harness materializer, its `purge_plugin` written to the journal.
struct JournalMaterializer(Arc<FakeMaterializer>, Arc<Journal>);

impl Materializer for JournalMaterializer {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        repo: &RepoRef,
        git_ref: &str,
        git: &GitSettings,
        creds: &balerix_api::CredentialBundle,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        self.0.ensure_crew(crew, repo, git_ref, git, creds, tools)
    }
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        creds: &balerix_api::CredentialBundle,
        hooks: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        self.0.materialize(agent, creds, hooks)
    }
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        self.0.remove_agent(agent)
    }
    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError> {
        self.0.remove_crew(crew, keep)
    }
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        self.0.materialize_plugin(plugin, host)
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        self.1.push(format!("purge_plugin {name}"));
        self.0.purge_plugin(name)
    }
}

/// #1, lane H review: the window appears only once the actor is idle
/// (its start-up and pool passes done, the resync an hour off), so no
/// pass can stop it before `purge` asks for one. With the barrier the
/// stop is journalled before the deletion; without it the deletion runs
/// with the window still up and nothing stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_of_a_window_the_record_never_held_stops_it_before_deleting() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("plugins.yaml"), "plugins: []\n").unwrap();
    let h = Harness::new(Duration::from_secs(3600));
    let journal = Arc::new(Journal::default());
    let ports = balerix_server::Ports {
        materializer: Arc::new(JournalMaterializer(h.materializer.clone(), journal.clone())),
        runner: Arc::new(JournalRunner(h.runner.clone(), journal.clone())),
        clock: h.clock.clone(),
        store: h.store.clone(),
        workspace: h.workspace.clone(),
        resolver: h.resolver.clone(),
        credentials: h.credentials.clone(),
        policy: Default::default(),
        hook_url: "http://127.0.0.1:1".into(),
        resync: Duration::from_secs(3600),
        kube: None,
    };
    let daemon = Daemon::start(
        ports,
        Arc::new(PassThrough),
        Metrics::new().unwrap(),
        "admin-tok".into(),
        Vec::new(),
        balerix_server::PluginSetup::Packages(plugin_config_in(dir.path())),
        h.registry.clone(),
        h.client.clone(),
        h.kv.clone(),
        balerix_server::testing::ready_toolchain(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while daemon.system_pool_state() != balerix_server::SystemPoolState::Ready {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // every pass queued so far has run: the actor is idle from here on
    let host = daemon.plugin_host().unwrap();
    let (reply, rx) = tokio::sync::oneshot::channel();
    host.handle()
        .tx
        .send(balerix_server::Msg::Barrier { reply })
        .await
        .unwrap();
    assert!(rx.await.unwrap(), "a pass ran");
    h.runner.set_state(
        &plugin_id(&"hello".parse().unwrap()),
        balerix_core::ProcessState::Running { pid: 4242 },
    );
    host.purge(&"hello".parse().unwrap()).await.unwrap();
    let entries = journal.entries();
    let stop = entries
        .iter()
        .position(|e| e.starts_with("stop_") && e.contains("balerix/plugins"));
    let purge = entries.iter().position(|e| e == "purge_plugin hello");
    assert!(
        matches!((stop, purge), (Some(s), Some(p)) if s < p),
        "the stop before the deletion: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_deletes_nothing_while_no_pass_can_run() {
    let (_dir, h, daemon) = orphan_daemon(Arc::new(
        balerix_core::fakes::FakeSystemToolchain::always_failing(),
    ))
    .await;
    let e = daemon
        .plugin_host()
        .unwrap()
        .purge(&"hello".parse().unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(e, balerix_server::PluginError::Unavailable(_)),
        "a 503, retryable: {e:?}"
    );
    assert!(e.to_string().contains("no reconcile pass ran"), "{e}");
    let resp = axum::response::IntoResponse::into_response(balerix_server::ApiError::from(e));
    assert_eq!(resp.status(), 503);
    assert!(!stopped_hello(&h.runner.calls()));
    assert!(
        !h.materializer
            .calls()
            .contains(&"purge_plugin hello".to_string()),
        "nothing deleted under a running window"
    );
}

/// A `fleet.json` under the reserved name — written before the name was
/// reserved, or by hand — must not get an actor: the plugin host already
/// owns `balerix`, its tmux session and its state root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_fleet_under_the_reserved_name_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("plugins.yaml"), "plugins: []\n").unwrap();

    let h = Harness::new(Duration::from_secs(3600));
    let mut record = FleetRecord::new(FleetSpec {
        name: "balerix".into(),
        crews: BTreeMap::from([(
            "plugins".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: BTreeMap::from([("hello".to_string(), AgentSettings::default())]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    });
    record.status.entry("balerix/plugins/hello");
    let ports = balerix_server::Ports {
        materializer: h.materializer.clone(),
        runner: h.runner.clone(),
        clock: h.clock.clone(),
        store: h.store.clone(),
        workspace: h.workspace.clone(),
        resolver: h.resolver.clone(),
        credentials: h.credentials.clone(),
        policy: Default::default(),
        hook_url: "http://127.0.0.1:1".into(),
        resync: Duration::from_secs(3600),
        kube: None,
    };
    // Not `Harness::daemon`: this test needs a *stored* record, which only
    // `Daemon::start` takes.
    let daemon = Daemon::start(
        ports,
        Arc::new(PassThrough),
        Metrics::new().unwrap(),
        "admin-tok".into(),
        vec![(record, Default::default())],
        balerix_server::PluginSetup::Packages(plugin_config_in(dir.path())),
        h.registry.clone(),
        h.client.clone(),
        h.kv.clone(),
        balerix_server::testing::ready_toolchain(),
    );
    daemon.sync_plugins().await.unwrap();

    let name: balerix_core::FleetName = "balerix".parse().unwrap();
    let rec = daemon.get(&name).await.unwrap();
    assert!(
        rec.status.agents.is_empty(),
        "the plugin host's own record answers, not the stored one: {:?}",
        rec.status.agents
    );
    assert!(daemon.list().await.is_empty(), "and it is not a fleet row");
    // no actor ran the stored spec: its agent was never materialized or
    // started. The plugin host's own actor may have observed `balerix` by
    // now — `plugins.yaml` is empty, so it touches nothing else.
    let r = h.runner.clone();
    let m = h.materializer.clone();
    tokio::task::spawn_blocking(move || {
        let mut calls = r.calls();
        calls.extend(m.calls());
        assert!(
            calls.iter().all(|c| !c.contains("hello")),
            "the stored record's agent was acted on: {calls:?}"
        );
    })
    .await
    .unwrap();
}
