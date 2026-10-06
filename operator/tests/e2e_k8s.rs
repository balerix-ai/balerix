#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Phase 3 journey on a kind cluster (Spec O §15, §21.4): apply a
//! Daemon and a Fleet, wait Ready, evict a pod and see it resume on its
//! claim, drop an agent and find its branch in the crew cache, delete the
//! Fleet with `retain: None` and find the crew's directories emptied and
//! the fleet gone from the Daemon's list. And the plugin journey (§23.6):
//! flow and web listed and Ready, a flow rule acting on an agent's Stop,
//! web's review page through the Daemon, web's config rolling its pod, and
//! the fake plugin's managed Fleet coming and going. Each journey has a
//! namespace of its own. The operator runs outside the cluster, as a
//! child of the test. Needs `KUBECONFIG` (scripts/kind-up.sh) and
//! `BALERIX_K8S_IMAGES`, and `BALERIX_K8S_PLUGIN_IMAGES` for the plugin
//! journey; skips without them, fails under `BALERIX_REQUIRE_TOOLS=1`.
//! kind's network plugin does not enforce NetworkPolicy: the operator's
//! policies are applied here but inert.
mod support;

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use balerix_operator::api::{
    Agent, Crew, Daemon, DaemonSpec, Fleet, FleetSpec, Plugin, PluginSpec,
};
use balerix_operator::daemon_client::DaemonClient;
use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Pod, Secret, Service};
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::{WatchStreamExt, watcher};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use support::wait_for;

const OPERATOR: &str = env!("CARGO_BIN_EXE_balerix-operator");

fn gate() -> Option<(String, String)> {
    let images = std::env::var("BALERIX_K8S_IMAGES").ok();
    let kubeconfig = std::env::var_os("KUBECONFIG").filter(|p| std::path::Path::new(p).is_file());
    match (images, kubeconfig) {
        (Some(images), Some(_)) => {
            let (daemon, agent) = images
                .split_once(',')
                .expect("BALERIX_K8S_IMAGES is <daemon>,<agent>");
            Some((daemon.to_string(), agent.to_string()))
        }
        _ => {
            if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
                panic!(
                    "KUBECONFIG and BALERIX_K8S_IMAGES are required (BALERIX_REQUIRE_TOOLS=1); run `mise run kind-up` first"
                );
            }
            eprintln!("skip: no kind cluster (KUBECONFIG, BALERIX_K8S_IMAGES)");
            None
        }
    }
}

/// Ends the operator child when the test ends, however it ends.
struct Operator(Child);
impl Drop for Operator {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A shell loop and the `kubectl` it runs, in a process group of their
/// own, killed whole when the test ends.
struct Forward(Child);
impl Forward {
    fn start(script: &str) -> Self {
        Forward(
            Command::new("bash")
                .args(["-c", script])
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    }
}
impl Drop for Forward {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.0.id())])
            .status();
        let _ = self.0.wait();
    }
}

fn exec(ns: &str, pod: &str, container: &str, script: &str) -> Result<String, String> {
    let out = Command::new("kubectl")
        .args([
            "-n", ns, "exec", pod, "-c", container, "--", "bash", "-c", script,
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn condition_true(
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
    type_: &str,
) -> bool {
    conditions
        .iter()
        .any(|c| c.type_ == type_ && c.status == "True")
}

/// `BALERIX_K8S_PLUGIN_IMAGES` as the flow, web and fake plugin images,
/// under `gate`'s rule: skips without it, fails under
/// `BALERIX_REQUIRE_TOOLS=1`.
fn plugin_gate() -> Option<(String, String, String)> {
    match std::env::var("BALERIX_K8S_PLUGIN_IMAGES") {
        Ok(images) => {
            let parts: Vec<&str> = images.split(',').collect();
            let [flow, web, fake] = parts[..] else {
                panic!("BALERIX_K8S_PLUGIN_IMAGES is <flow>,<web>,<fake>");
            };
            Some((flow.to_string(), web.to_string(), fake.to_string()))
        }
        Err(_) => {
            if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
                panic!(
                    "BALERIX_K8S_PLUGIN_IMAGES is required (BALERIX_REQUIRE_TOOLS=1); run through `scripts/operator.sh e2e`"
                );
            }
            eprintln!("skip: no plugin images (BALERIX_K8S_PLUGIN_IMAGES)");
            None
        }
    }
}

/// A namespace of this journey's own: `e2e-<label>-<pid>`.
async fn namespace_for(client: &kube::Client, label: &str) -> String {
    let ns = format!("e2e-{label}-{}", std::process::id());
    let namespace: Namespace = serde_json::from_value(
        serde_json::json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns } }),
    )
    .unwrap();
    Api::<Namespace>::all(client.clone())
        .create(&PostParams::default(), &namespace)
        .await
        .unwrap();
    ns
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The operator runs here, not in the cluster: the Daemon's Service is
/// reached through a port-forward, under the Service's own name (§21.6).
fn spawn_operator(ns: &str, daemon_image: &str, agent_image: &str, forward_port: u16) -> Operator {
    let resolve = format!("balerix-default.{ns}.svc=127.0.0.1:{forward_port}");
    Operator(
        Command::new(OPERATOR)
            .args([
                "run",
                "--watch-namespaces",
                ns,
                "--namespace",
                ns,
                "--daemon-image",
                daemon_image,
                "--agent-image",
                agent_image,
                "--resolve",
                &resolve,
            ])
            .env("RUST_LOG", "info,kube=warn")
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

/// A git server pod and its Service, seeded with one commit; the repo's URL.
async fn git_server(client: &kube::Client, ns: &str, agent_image: &str) -> String {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let git_pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "git", "namespace": ns, "labels": { "app": "git" } },
        "spec": {
            "containers": [{
                "name": "git", "image": agent_image, "command": ["bash", "-c",
                    "set -e; git init -q --bare /srv/repo.git; exec git daemon --base-path=/srv --export-all --enable=receive-pack --reuseaddr --listen=0.0.0.0 /srv"],
                "ports": [{ "containerPort": 9418 }],
                "volumeMounts": [{ "name": "srv", "mountPath": "/srv" }, { "name": "tmp", "mountPath": "/tmp" }]
            }],
            "volumes": [{ "name": "srv", "emptyDir": {} }, { "name": "tmp", "emptyDir": {} }]
        }
    })).unwrap();
    pods.create(&PostParams::default(), &git_pod).await.unwrap();
    let service: Service = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "git", "namespace": ns },
        "spec": { "selector": { "app": "git" }, "ports": [{ "port": 9418, "targetPort": 9418 }] }
    }))
    .unwrap();
    Api::<Service>::namespaced(client.clone(), ns)
        .create(&PostParams::default(), &service)
        .await
        .unwrap();
    wait_for("the git pod", Duration::from_secs(180), || async {
        pods.get("git")
            .await
            .unwrap()
            .status
            .and_then(|s| s.phase)
            .filter(|p| p == "Running")
    })
    .await;
    exec(ns, "git", "git", "set -e; cd /tmp && git clone -q /srv/repo.git w && cd w && echo hi > README && git add . && git -c user.name=t -c user.email=t@t commit -qm init && git push -q origin HEAD:main").unwrap();
    format!("git://git.{ns}.svc:9418/repo.git")
}

/// The Daemon `default`, whose claude is the fake one in the image,
/// listing `plugins` in that order.
fn daemon_object(plugins: &[&str]) -> Daemon {
    let spec: DaemonSpec = serde_json::from_value(serde_json::json!({
        "storage": { "state": { "size": "1Gi" }, "shared": { "size": "2Gi" }, "agent": { "size": "1Gi" } },
        "defaults": {
            "claude": { "binary": "/usr/local/bin/balerix", "args": ["dev", "fake-claude", "--verbose"], "settings": { "model": "sonnet" } },
            "sandbox": { "network": { "block": false } }
        },
        "plugins": plugins
    })).unwrap();
    Daemon::new("default", spec)
}

async fn wait_daemon_service(client: &kube::Client, ns: &str) {
    let services: Api<Service> = Api::namespaced(client.clone(), ns);
    wait_for("the Daemon's Service", Duration::from_secs(120), || async {
        services.get_opt("balerix-default").await.unwrap()
    })
    .await;
}

/// `kubectl port-forward` exits when the pod behind the Service is not
/// running yet ("pod is not running. Current status=Pending") and when
/// it restarts, so a loop restarts it.
fn forward(ns: &str, port: u16) -> Forward {
    Forward::start(&format!(
        "while :; do kubectl -n {ns} port-forward svc/balerix-default {port}:7643 >/dev/null; sleep 1; done"
    ))
}

/// The pool Job installs claude and gh with mise inside the cluster:
/// minutes, and GitHub's unauthenticated rate limit if the runner's
/// address is busy (a finding for §21.6 if it bites).
async fn wait_daemon_ready(client: &kube::Client, ns: &str) {
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), ns);
    wait_for("the Daemon Ready", Duration::from_secs(600), || async {
        let s = daemons.get("default").await.unwrap().status?;
        condition_true(&s.conditions, "Ready").then_some(())
    })
    .await;
}

/// The admin token and the authority the operator minted for the Daemon.
async fn admin_credentials(client: &kube::Client, ns: &str) -> (String, String) {
    let admin = Api::<Secret>::namespaced(client.clone(), ns)
        .get("balerix-default-admin")
        .await
        .unwrap();
    let token = String::from_utf8(admin.data.unwrap()["token"].0.clone()).unwrap();
    let authority = Api::<ConfigMap>::namespaced(client.clone(), ns)
        .get("balerix-default-ca")
        .await
        .unwrap()
        .data
        .unwrap()["ca.crt"]
        .clone();
    (token, authority)
}

/// A client asking the Daemon as the operator does: trusting its
/// authority, `balerix-default.<ns>.svc` resolved to the port-forward;
/// with the base URL and the admin token. The TLS setup is
/// `DaemonClient::new_resolving`'s; reqwest keeps a URL's port, so the
/// base carries the forward's.
async fn admin_http(
    client: &kube::Client,
    ns: &str,
    forward_port: u16,
) -> (reqwest::Client, String, String) {
    let (token, authority) = admin_credentials(client, ns).await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(authority.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let host = format!("balerix-default.{ns}.svc");
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .resolve(
            &host,
            std::net::SocketAddr::from(([127, 0, 0, 1], forward_port)),
        )
        .build()
        .unwrap();
    (http, format!("https://{host}:{forward_port}"), token)
}

/// Waits for the Plugin's condition `type_` to be `True`.
async fn wait_for_condition(plugins: &Api<Plugin>, name: &str, type_: &str, timeout: Duration) {
    wait_for(&format!("Plugin {name} {type_}"), timeout, || async {
        let s = plugins.get(name).await.unwrap().status?;
        condition_true(&s.conditions, type_).then_some(())
    })
    .await;
}

/// Waits for the Agent's `Ready` and the Daemon's `ready` phase for it;
/// its pod's name.
async fn wait_agent_ready(
    client: &kube::Client,
    ns: &str,
    agent: &str,
    timeout: Duration,
) -> String {
    let agents: Api<Agent> = Api::namespaced(client.clone(), ns);
    wait_for(&format!("Agent {agent} Ready"), timeout, || async {
        let s = agents.get_opt(agent).await.unwrap()?.status?;
        let ready = condition_true(&s.conditions, "Ready") && s.phase.as_deref() == Some("ready");
        if ready { s.pod } else { None }
    })
    .await
}

/// The uid of web's one running pod, not being deleted; `None` while
/// there is none, or two (a roll under way).
async fn web_pod_uid(client: &kube::Client, ns: &str) -> Option<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let listed = pods
        .list(&ListParams::default().labels("balerix.ai/plugin=web"))
        .await
        .unwrap();
    let running: Vec<Pod> = listed
        .items
        .into_iter()
        .filter(|p| {
            p.metadata.deletion_timestamp.is_none()
                && p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running")
        })
        .collect();
    match &running[..] {
        [one] => one.metadata.uid.clone(),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_phase_3_journey_on_kind() {
    let Some((daemon_image, agent_image)) = gate() else {
        return;
    };
    let client =
        balerix_operator::request_client::request_client(kube::Config::infer().await.unwrap())
            .unwrap();
    let ns = namespace_for(&client, "phase-3").await;
    let forward_port = free_port();
    let _operator = spawn_operator(&ns, &daemon_image, &agent_image, forward_port);

    // 1. a git server pod and its Service, seeded with one commit
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let repo = git_server(&client, &ns, &agent_image).await;

    // 2. a Daemon whose claude is the fake one in the image
    Api::<Daemon>::namespaced(client.clone(), &ns)
        .create(&PostParams::default(), &daemon_object(&[]))
        .await
        .unwrap();
    wait_daemon_service(&client, &ns).await;
    let _forward = forward(&ns, forward_port);
    wait_daemon_ready(&client, &ns).await;

    // 3. a Fleet of one crew and two agents, Ready when both forwarded SessionStart
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    let spec: FleetSpec = serde_json::from_value(serde_json::json!({
        "daemon": "default", "retain": "None",
        "crews": { "c": { "repo": repo, "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "alice": {}, "bob": {} } } }
    }))
    .unwrap();
    fleets
        .create(&PostParams::default(), &Fleet::new("f", spec))
        .await
        .unwrap();
    wait_for("the Fleet Ready", Duration::from_secs(900), || async {
        let s = fleets.get("f").await.unwrap().status?;
        condition_true(&s.conditions, "Ready").then_some(())
    })
    .await;
    let agents: Api<Agent> = Api::namespaced(client.clone(), &ns);
    // an Agent's phase is the Daemon's record, which the Fleet's reconcile
    // caches and the Agent's next reconcile copies: it can trail the
    // Fleet's Ready by a reconcile (seen on kind: alice `pending` the
    // moment the Fleet was Ready)
    wait_for("alice's phase ready", Duration::from_secs(60), || async {
        let phase = agents.get("f-c-alice").await.unwrap().status?.phase;
        (phase.as_deref() == Some("ready")).then_some(())
    })
    .await;
    assert!(
        Api::<Crew>::namespaced(client.clone(), &ns)
            .get("f-c")
            .await
            .unwrap()
            .status
            .unwrap()
            .cache_ref
            .is_some()
    );

    // 4. evict alice's pod: recreated on the same claim, Ready again, home intact
    exec(
        &ns,
        "f-c-alice",
        "agent",
        "touch /balerix/agent/home/e2e-marker",
    )
    .unwrap();
    let first = pods.get("f-c-alice").await.unwrap();
    pods.delete("f-c-alice", &DeleteParams::default())
        .await
        .unwrap();
    wait_for("the new pod Ready", Duration::from_secs(600), || async {
        let p = pods.get_opt("f-c-alice").await.unwrap()?;
        if p.metadata.uid == first.metadata.uid {
            return None;
        }
        // the new pod's own readiness, not only the Agent's condition,
        // which still says the old pod's until the Agent reconciles
        let pod_ready = p
            .status?
            .conditions?
            .iter()
            .any(|c| c.type_ == "Ready" && c.status == "True");
        if !pod_ready {
            return None;
        }
        let a = agents.get("f-c-alice").await.unwrap().status?;
        condition_true(&a.conditions, "Ready").then_some(())
    })
    .await;
    exec(
        &ns,
        "f-c-alice",
        "agent",
        "test -f /balerix/agent/home/e2e-marker",
    )
    .unwrap();

    // 5. drop bob: harvested into the crew cache. The harvest Job and its
    // pod are bob's Agent's and go with it, so their outcome is watched
    // as it happens, not read after
    // on a client of their own (`watch_client`, #129): a watch on the pooled
    // `client` could hold up the probes below for up to 290 s
    let watch_client =
        balerix_operator::watch_client::watch_client(kube::Config::infer().await.unwrap()).unwrap();
    let jobs: Api<Job> = Api::namespaced(watch_client.clone(), &ns);
    let watch_pods: Api<Pod> = Api::namespaced(watch_client, &ns);
    let succeeded = Arc::new(Mutex::new(None::<i32>));
    let message = Arc::new(Mutex::new(None::<String>));
    let job_watch = tokio::spawn({
        let (jobs, succeeded) = (jobs.clone(), succeeded.clone());
        async move {
            let config = watcher::Config::default().fields("metadata.name=f-c-bob-harvest");
            let mut events =
                std::pin::pin!(watcher(jobs, config).default_backoff().applied_objects());
            while let Some(event) = events.next().await {
                if let Some(n) = event.ok().and_then(|j| j.status?.succeeded) {
                    *succeeded.lock().unwrap() = Some(n);
                }
            }
        }
    });
    let pod_watch = tokio::spawn({
        let (pods, message) = (watch_pods, message.clone());
        async move {
            let config = watcher::Config::default().labels("job-name=f-c-bob-harvest");
            let mut events =
                std::pin::pin!(watcher(pods, config).default_backoff().applied_objects());
            while let Some(event) = events.next().await {
                let found = event.ok().and_then(|p| {
                    p.status?
                        .container_statuses?
                        .into_iter()
                        .find_map(|c| c.state?.terminated?.message)
                });
                if let Some(m) = found {
                    *message.lock().unwrap() = Some(m);
                }
            }
        }
    });
    fleets
        .patch(
            "f",
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(
                serde_json::json!({ "spec": { "crews": { "c": { "agents": { "bob": null } } } } }),
            ),
        )
        .await
        .unwrap();
    wait_for("bob gone", Duration::from_secs(600), || async {
        agents
            .get_opt("f-c-bob")
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    job_watch.abort();
    pod_watch.abort();
    assert_eq!(*succeeded.lock().unwrap(), Some(1));
    let message = message
        .lock()
        .unwrap()
        .clone()
        .expect("the harvest Job's message");
    let branch = message
        .trim()
        .strip_prefix("harvested ")
        .expect("harvested <branch>")
        .to_string();
    let probe: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "probe", "namespace": ns },
        "spec": { "containers": [{ "name": "probe", "image": agent_image, "command": ["sleep", "infinity"],
            "volumeMounts": [{ "name": "shared", "mountPath": "/balerix/volume" }] }],
            "volumes": [{ "name": "shared", "persistentVolumeClaim": { "claimName": "balerix-default-shared" } }] }
    })).unwrap();
    pods.create(&PostParams::default(), &probe).await.unwrap();
    wait_for("the probe pod", Duration::from_secs(180), || async {
        pods.get("probe")
            .await
            .unwrap()
            .status
            .and_then(|s| s.phase)
            .filter(|p| p == "Running")
    })
    .await;
    // the cache is a clone with a work tree: its objects are `repo/.git`'s
    let listed = exec(
        &ns,
        "probe",
        "probe",
        &format!("git --git-dir=/balerix/volume/fleets/f/crews/c/repo/.git branch --list {branch}"),
    )
    .unwrap();
    assert!(
        listed.contains(&branch),
        "branch {branch} not in the cache: {listed:?}"
    );

    // 6. the Fleet deleted with retain: None: the crew's directories emptied
    fleets.delete("f", &DeleteParams::default()).await.unwrap();
    wait_for("the Fleet gone", Duration::from_secs(600), || async {
        fleets.get_opt("f").await.unwrap().is_none().then_some(())
    })
    .await;
    // emptied, not removed: the directories are mount points of the Jobs,
    // and a directory that never existed would pass an emptiness test
    exec(
        &ns,
        "probe",
        "probe",
        "set -e; for d in repo pool; do p=/balerix/volume/fleets/f/crews/c/$d; test -d \"$p\"; test -z \"$(ls -A \"$p\")\"; done",
    )
    .unwrap();

    // and the Daemon still lists the fleet, downed (O-16: `retain: None` is a
    // plain `down`, which keeps the record; only a purge drops it): asked as
    // the operator asks, with the
    // admin token and the authority the operator minted, through the
    // port-forward under the Service's own name
    let (token, authority) = admin_credentials(&client, &ns).await;
    let daemon = DaemonClient::new_resolving(
        &format!("https://balerix-default.{ns}.svc:7643"),
        &authority,
        &token,
        Duration::from_secs(10),
        &[(
            format!("balerix-default.{ns}.svc"),
            std::net::SocketAddr::from(([127, 0, 0, 1], forward_port)),
        )],
    )
    .unwrap();
    // the port-forward restarts now and then: retry the transport, not the answer
    let listed = wait_for("the Daemon to answer", Duration::from_secs(60), || async {
        daemon.get("f").await.ok()
    })
    .await;
    // the Down reply is sent after the mirror pass, so the phase is already
    // Down and the Daemon's view of the pods is cleared
    let record = listed.expect("the Daemon keeps the downed fleet f");
    assert!(
        record.is_down(),
        "fleet f is not down: {:?}",
        record.desired
    );
    assert!(
        record.status.agents.is_empty(),
        "the downed fleet f still lists agents"
    );
}

/// Spec O §17 sub-project 4 / §23.6: flow and web listed and Ready; a flow
/// rule acting on an agent's Stop; web's review page through the Daemon;
/// web's config changing with no restart by hand (§23.8); the fake plugin's
/// managed Fleet coming up and going with the plugin.
#[tokio::test(flavor = "multi_thread")]
async fn the_plugin_journey_on_kind() {
    let Some((daemon_image, agent_image)) = gate() else {
        return;
    };
    let Some((flow_image, web_image, fake_image)) = plugin_gate() else {
        return;
    };
    let client =
        balerix_operator::request_client::request_client(kube::Config::infer().await.unwrap())
            .unwrap();
    let ns = namespace_for(&client, "plugins").await;
    let forward_port = free_port();
    let _operator = spawn_operator(&ns, &daemon_image, &agent_image, forward_port);
    let repo = git_server(&client, &ns, &agent_image).await;

    // 1. Plugins flow and web, listed by the Daemon in that order; the
    // Daemon's Ready needs PluginsReady=True now
    let plugins: Api<Plugin> = Api::namespaced(client.clone(), &ns);
    let plugin = |image: &str, needs: &[&str], config: serde_json::Value| -> PluginSpec {
        serde_json::from_value(
            serde_json::json!({ "image": image, "needs": needs, "config": config }),
        )
        .unwrap()
    };
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new(
                "flow",
                plugin(&flow_image, &["actions", "kv"], serde_json::json!({})),
            ),
        )
        .await
        .unwrap();
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new(
                "web",
                plugin(
                    &web_image,
                    &["fleets", "attach", "actions", "workspace"],
                    serde_json::json!({}),
                ),
            ),
        )
        .await
        .unwrap();
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), &ns);
    daemons
        .create(&PostParams::default(), &daemon_object(&["flow", "web"]))
        .await
        .unwrap();
    wait_daemon_service(&client, &ns).await;
    let _forward = forward(&ns, forward_port);
    wait_daemon_ready(&client, &ns).await;
    for name in ["flow", "web"] {
        wait_for_condition(&plugins, name, "Ready", Duration::from_secs(300)).await;
    }

    // 2. a Fleet whose alice runs flow and web: her Stop moves flow to
    // review, whose send_text lands in her stdin. A pair is activated only
    // for an agent whose `plugins` block names the plugin, so web is named
    // too, for step 3
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    let alice_plugins = serde_json::json!({
        "flow": { "initial": "working", "states": {
            "working": { "on": [{ "event": "Stop", "goto": "review", "send": { "text": "flow says: run the tests" } }] },
            "review": { "on": [{ "event": "Stop", "goto": "done" }] },
            "done": {} } },
        "web": {}
    });
    let spec: FleetSpec = serde_json::from_value(serde_json::json!({
        "daemon": "default",
        "crews": { "c": { "repo": repo, "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "alice": { "plugins": alice_plugins } } } }
    }))
    .unwrap();
    fleets
        .create(&PostParams::default(), &Fleet::new("f", spec))
        .await
        .unwrap();
    let alice_pod = wait_agent_ready(&client, &ns, "f-c-alice", Duration::from_secs(900)).await;
    let stdin = wait_for(
        "flow's send_text in alice's stdin",
        Duration::from_secs(120),
        || async {
            exec(
                &ns,
                &alice_pod,
                "agent",
                "cat /balerix/agent/home/fake-claude.stdin 2>/dev/null",
            )
            .ok()
            .filter(|s| s.contains("flow says: run the tests"))
        },
    )
    .await;
    assert!(stdin.contains("flow says"), "{stdin}");

    // 3. web's review page, through the Daemon's mount over the
    // port-forward; the forward restarts now and then, and web learns of
    // alice at her activation: retried until it answers 200
    let (http, base, token) = admin_http(&client, &ns, forward_port).await;
    let url = format!("{base}/v1/plugins/web/agents/f/c/alice/review");
    let body = wait_for(
        "web's review page for alice",
        Duration::from_secs(120),
        || async {
            let page = http.get(&url).bearer_auth(&token).send().await.ok()?;
            if page.status() != reqwest::StatusCode::OK {
                return None;
            }
            page.text().await.ok()
        },
    )
    .await;
    assert!(body.contains("review f/c/alice"), "{body}");

    // 4. §23.8: web's config changes; its pod rolls and says hello under
    // the new revision with no restart by hand; flow stays Ready
    let before = wait_for("web's pod", Duration::from_secs(60), || {
        web_pod_uid(&client, &ns)
    })
    .await;
    plugins
        .patch(
            "web",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "config": { "enabled": true } } })),
        )
        .await
        .unwrap();
    wait_for("web's pod rolled", Duration::from_secs(300), || async {
        web_pod_uid(&client, &ns)
            .await
            .filter(|now| *now != before)
            .map(|_| ())
    })
    .await;
    wait_for_condition(&plugins, "web", "Ready", Duration::from_secs(300)).await;
    wait_for_condition(&plugins, "flow", "Ready", Duration::from_secs(60)).await;

    // 5. the fake plugin manages Fleet m; its agent becomes Ready. The
    // fleet file is a whole one: the Daemon parses it as a file, header
    // and all
    let managed_file = serde_json::json!({
        "apiVersion": "balerix/v1", "kind": "Fleet",
        "crews": { "c": { "repo": repo, "ref": "main", "git": { "push": false, "auth": "none" },
            "agents": { "carol": {} } } }
    });
    plugins
        .create(
            &PostParams::default(),
            &Plugin::new(
                "fake",
                plugin(
                    &fake_image,
                    &["actions", "fleets", "kv", "manage"],
                    serde_json::json!({ "manage": { "fleet": "m", "file": managed_file } }),
                ),
            ),
        )
        .await
        .unwrap();
    daemons
        .patch(
            "default",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "plugins": ["flow", "web", "fake"] } })),
        )
        .await
        .unwrap();
    let m = wait_for("the managed Fleet m", Duration::from_secs(600), || async {
        fleets.get_opt("m").await.unwrap()
    })
    .await;
    assert_eq!(
        m.metadata.labels.unwrap_or_default()["balerix.ai/managed-by"],
        "fake"
    );
    wait_agent_ready(&client, &ns, "m-c-carol", Duration::from_secs(900)).await;

    // 6. dropping fake from spec.plugins deletes its Fleet
    daemons
        .patch(
            "default",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "plugins": ["flow", "web"] } })),
        )
        .await
        .unwrap();
    wait_for(
        "the managed Fleet m gone",
        Duration::from_secs(600),
        || async { fleets.get_opt("m").await.unwrap().is_none().then_some(()) },
    )
    .await;
}
