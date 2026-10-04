#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Phase 3 journey on a kind cluster (Spec O §15, §21.4): apply a
//! Daemon and a Fleet, wait Ready, evict a pod and see it resume on its
//! claim, drop an agent and find its branch in the crew cache, delete the
//! Fleet with `retain: None` and find the crew's directories gone. The
//! operator runs outside the cluster, as a child of this test. Needs
//! `KUBECONFIG` (scripts/kind-up.sh) and `BALERIX_K8S_IMAGES`; skips
//! without them, fails under `BALERIX_REQUIRE_TOOLS=1`.
mod support;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use balerix_operator::api::{Agent, Crew, Daemon, DaemonSpec, Fleet, FleetSpec};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Namespace, Pod, Service};
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};
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

/// Ends a child (the operator, the port-forward) when the test ends,
/// however it ends.
struct Operator(Child);
impl Drop for Operator {
    fn drop(&mut self) {
        let _ = self.0.kill();
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

#[tokio::test(flavor = "multi_thread")]
async fn the_phase_3_journey_on_kind() {
    let Some((daemon_image, agent_image)) = gate() else {
        return;
    };
    let client = Client::try_default().await.unwrap();
    let ns = format!("e2e-{}", std::process::id());
    let namespace: Namespace = serde_json::from_value(
        serde_json::json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns } }),
    )
    .unwrap();
    Api::<Namespace>::all(client.clone())
        .create(&PostParams::default(), &namespace)
        .await
        .unwrap();

    // the operator runs here, not in the cluster: the Daemon's Service is
    // reached through a port-forward, under the Service's own name (§21.6)
    let forward_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let resolve = format!("balerix-default.{ns}.svc=127.0.0.1:{forward_port}");
    let _operator = Operator(
        Command::new(OPERATOR)
            .args([
                "run",
                "--watch-namespaces",
                &ns,
                "--namespace",
                &ns,
                "--daemon-image",
                &daemon_image,
                "--agent-image",
                &agent_image,
                "--resolve",
                &resolve,
            ])
            .env("RUST_LOG", "info,kube=warn")
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );

    // 1. a git server pod and its Service, seeded with one commit
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
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
    Api::<Service>::namespaced(client.clone(), &ns)
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
    exec(&ns, "git", "git", "set -e; cd /tmp && git clone -q /srv/repo.git w && cd w && echo hi > README && git add . && git -c user.name=t -c user.email=t@t commit -qm init && git push -q origin HEAD:main").unwrap();
    let repo = format!("git://git.{ns}.svc:9418/repo.git");

    // 2. a Daemon whose claude is the fake one in the image
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), &ns);
    let spec: DaemonSpec = serde_json::from_value(serde_json::json!({
        "storage": { "state": { "size": "1Gi" }, "shared": { "size": "2Gi" }, "agent": { "size": "1Gi" } },
        "defaults": {
            "claude": { "binary": "/usr/local/bin/balerix", "args": ["dev", "fake-claude", "--verbose"], "settings": { "model": "sonnet" } },
            "sandbox": { "network": { "block": false } }
        }
    })).unwrap();
    daemons
        .create(&PostParams::default(), &Daemon::new("default", spec))
        .await
        .unwrap();
    let services: Api<Service> = Api::namespaced(client.clone(), &ns);
    wait_for("the Daemon's Service", Duration::from_secs(120), || async {
        services.get_opt("balerix-default").await.unwrap()
    })
    .await;
    let _forward = Operator(
        Command::new("kubectl")
            .args([
                "-n",
                &ns,
                "port-forward",
                "svc/balerix-default",
                &format!("{forward_port}:7643"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    // the pool Job installs claude and gh with mise inside the cluster:
    // minutes, and GitHub's unauthenticated rate limit if the runner's
    // address is busy (a finding for §21.6 if it bites)
    wait_for("the Daemon Ready", Duration::from_secs(600), || async {
        let s = daemons.get("default").await.unwrap().status?;
        condition_true(&s.conditions, "Ready").then_some(())
    })
    .await;

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
    assert_eq!(
        agents
            .get("f-c-alice")
            .await
            .unwrap()
            .status
            .unwrap()
            .phase
            .as_deref(),
        Some("ready")
    );
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

    // 5. drop bob: harvested into the crew cache
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
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let harvest = jobs.get("f-c-bob-harvest").await.unwrap();
    assert_eq!(harvest.status.as_ref().and_then(|s| s.succeeded), Some(1));
    let message = pods
        .list(&ListParams::default().labels("job-name=f-c-bob-harvest"))
        .await
        .unwrap()
        .items
        .iter()
        .find_map(|p| {
            p.status
                .as_ref()?
                .container_statuses
                .as_ref()?
                .iter()
                .find_map(|c| c.state.as_ref()?.terminated.as_ref()?.message.clone())
        })
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
    let listed = exec(
        &ns,
        "probe",
        "probe",
        &format!("git --git-dir=/balerix/volume/fleets/f/crews/c/repo branch --list {branch}"),
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
    exec(&ns, "probe", "probe", "test -z \"$(ls -A /balerix/volume/fleets/f/crews/c/repo)\" && test -z \"$(ls -A /balerix/volume/fleets/f/crews/c/pool)\"").unwrap();
}
