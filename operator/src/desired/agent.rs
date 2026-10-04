//! An Agent object into its pod and what the pod needs (Spec O §5.4,
//! §6.1): the claim that outlives the pod, the bundle Secret the sidecar
//! alone mounts, a NetworkPolicy with no ingress, and the two-container
//! pod. And the other way: a Pod and the Daemon's word into the Agent's
//! status.

use balerix_api::{AgentBundle, AgentSettings, CredentialBundle, GitSettings, RunnerSettings};
use k8s_openapi::api::core::v1::{ContainerStatus, PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
use serde::Serialize;
use serde_json::{Value, json};

use super::common::{
    Cond, DesiredError, OperatorConfig, claim, conditions, container_security, labels, owner_of,
    pod_security, typed,
};
use super::jobs::{AGENT_DIR, SHARED};
use super::names;
use crate::api::{Agent, AgentStatus, ClaimSpec, Daemon};

/// On the pod: the Agent's `specHash` it was made from (§5.4).
pub const SPEC_HASH_ANNOTATION: &str = "balerix.ai/spec-hash";

/// No `Debug`: `token` and `credentials` are secrets.
pub struct AgentInputs<'a> {
    pub agent: &'a Agent,
    pub daemon: &'a Daemon,
    /// The agent's token, from its Secret (minted by the controller).
    pub token: &'a str,
    /// Read from the Secrets the Daemon names (§5.2 step 2).
    pub credentials: &'a CredentialBundle,
    pub cfg: &'a OperatorConfig,
}

/// No `Debug`: the bundle Secret holds the token and the credentials.
pub struct AgentObjects {
    pub claim: PersistentVolumeClaim,
    pub bundle: Secret,
    pub policy: NetworkPolicy,
    pub pod: Pod,
}

pub fn agent_objects(inputs: &AgentInputs<'_>) -> Result<AgentObjects, DesiredError> {
    let agent = inputs.agent;
    let spec = &agent.spec;
    let name = agent
        .metadata
        .name
        .as_deref()
        .ok_or(DesiredError::Missing("the Agent", "metadata.name"))?;
    let namespace = agent
        .metadata
        .namespace
        .as_deref()
        .ok_or(DesiredError::Missing("the Agent", "metadata.namespace"))?;
    let settings: AgentSettings = typed(spec.settings.clone())?;
    let RunnerSettings::Pod {
        resources,
        storage,
        node_selector,
        tolerations,
    } = &settings.runner
    else {
        return Err(DesiredError::Missing(
            "the Agent",
            "pod runner in spec.settings",
        ));
    };
    let owner = owner_of(agent)?;
    let labels = labels(
        &spec.daemon,
        "agent",
        &[
            ("balerix.ai/fleet", spec.fleet.as_str()),
            ("balerix.ai/crew", spec.crew.as_str()),
            ("balerix.ai/agent", spec.agent.as_str()),
        ],
    );
    let metadata = json!({
        "name": name,
        "namespace": namespace,
        "labels": labels,
        "ownerReferences": [owner],
    });

    // §8.2: sized by the fleet's `runner.storage`, else the Daemon's
    // `storage.agent`; deleted with the Agent, after the harvest.
    let default = &inputs.daemon.spec.storage.agent;
    let size = storage
        .as_ref()
        .and_then(|s| s.get("size"))
        .and_then(Value::as_str)
        .unwrap_or(&default.size);
    let mut agent_claim = claim(
        namespace,
        name,
        labels,
        &ClaimSpec {
            storage_class_name: default.storage_class_name.clone(),
            size: size.to_string(),
        },
        "ReadWriteOnce",
    )?;
    // `common::claim` leaves its claims unowned; the agent's goes with it.
    agent_claim.metadata.owner_references = Some(vec![typed::<OwnerReference>(owner)?]);

    let bundle = AgentBundle {
        agent: format!("{}/{}/{}", spec.fleet, spec.crew, spec.agent),
        repo: spec.repo.clone(),
        git_ref: spec.git_ref.clone(),
        git: typed::<GitSettings>(spec.git.clone())?,
        settings: settings.clone(),
        daemon_url: names::endpoint(namespace, &spec.daemon),
        token: inputs.token.to_string(),
        credentials: inputs.credentials.clone(),
    };
    let mut bundle_metadata = metadata.clone();
    bundle_metadata["name"] = json!(names::bundle(name));

    // §8.1: the crew's slice, read-only at the mount (O-6). Whole pool
    // directories: each holds `mise/`, and the crew's also `no-hooks/`.
    let shared = |sub_path: String, at: &str| json!({ "name": "shared", "mountPath": format!("{SHARED}/{at}"), "subPath": sub_path, "readOnly": true });
    let slice = [
        shared(
            format!(
                "{}/.git/objects",
                names::vol_crew_repo(&spec.fleet, &spec.crew)
            ),
            "repo/.git/objects",
        ),
        shared(names::vol_crew_pool(&spec.fleet, &spec.crew), "crew"),
        shared(names::vol_fleet_pool(&spec.fleet), "fleet"),
        shared(names::vol_daemon_pool(), "daemon"),
    ];
    let both = |extra: Vec<Value>| -> Vec<Value> {
        let mut mounts =
            vec![json!({ "name": "agent", "mountPath": "/balerix/agent", "subPath": AGENT_DIR })];
        mounts.extend(slice.iter().cloned());
        mounts.push(json!({ "name": "run", "mountPath": "/balerix/run" }));
        mounts.push(json!({ "name": "tmp", "mountPath": "/tmp" }));
        mounts.extend(extra);
        mounts
    };
    let mut pod_metadata = metadata.clone();
    pod_metadata["annotations"] = json!({ SPEC_HASH_ANNOTATION: spec.spec_hash });

    Ok(AgentObjects {
        claim: agent_claim,
        bundle: typed(json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": bundle_metadata,
            "type": "Opaque",
            "stringData": { "agent.json": serde_json::to_string(&bundle)? },
        }))?,
        // §10.2, O-10: no inbound connection at all; egress is nono's to
        // restrict (`sandbox.network`), so it holds where the network
        // plugin ignores policies.
        policy: typed(json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "NetworkPolicy",
            "metadata": metadata,
            "spec": {
                "podSelector": { "matchLabels": {
                    "balerix.ai/fleet": spec.fleet,
                    "balerix.ai/crew": spec.crew,
                    "balerix.ai/agent": spec.agent,
                } },
                "policyTypes": ["Ingress"],
                "ingress": [],
            },
        }))?,
        pod: typed(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": pod_metadata,
            "spec": {
                "restartPolicy": "Always",
                "automountServiceAccountToken": false,
                "enableServiceLinks": false,
                "securityContext": pod_security(),
                "nodeSelector": node_selector,
                "tolerations": tolerations,
                "initContainers": [{
                    // the claim's directory, made by the pods' own user:
                    // the kubelet would make a missing sub-path as root
                    "name": "claim",
                    "image": inputs.cfg.images.agent,
                    "command": ["mkdir", "-p", format!("/balerix/claim/{AGENT_DIR}")],
                    // a failed `mkdir` reports its stderr as the message
                    "terminationMessagePolicy": "FallbackToLogsOnError",
                    "securityContext": container_security(),
                    "volumeMounts": [{ "name": "agent", "mountPath": "/balerix/claim" }],
                }, {
                    // a native sidecar: an init container that keeps running
                    "name": "sidecar",
                    "image": inputs.cfg.images.agent,
                    "restartPolicy": "Always",
                    "command": ["balerix-agent", "sidecar"],
                    "resources": { "requests": { "cpu": "50m", "memory": "64Mi" } },
                    "securityContext": container_security(),
                    // agent pods accept no connection (O-10): a file
                    "readinessProbe": {
                        "exec": { "command": ["test", "-f", "/balerix/run/ready"] },
                        "periodSeconds": 5,
                    },
                    "volumeMounts": both(vec![
                        json!({ "name": "bundle", "mountPath": "/balerix/secret", "readOnly": true }),
                        json!({ "name": "authority", "mountPath": "/balerix/tls", "readOnly": true }),
                    ]),
                }],
                "containers": [{
                    "name": "agent",
                    "image": inputs.cfg.images.agent,
                    "command": ["balerix-agent", "run"],
                    "resources": resources,
                    "securityContext": container_security(),
                    "volumeMounts": both(vec![]),
                }],
                "volumes": [
                    { "name": "agent", "persistentVolumeClaim": { "claimName": name } },
                    { "name": "shared", "persistentVolumeClaim": {
                        "claimName": names::shared_claim(&spec.daemon), "readOnly": true } },
                    { "name": "run", "emptyDir": {} },
                    { "name": "tmp", "emptyDir": {} },
                    { "name": "bundle", "secret": { "secretName": names::bundle(name), "defaultMode": 0o440 } },
                    { "name": "authority", "configMap": { "name": names::authority(&spec.daemon) } },
                ],
            },
        }))?,
    })
}

/// A serde `lowercase` enum as its wire word (`ready`, `active`). Never
/// empty: a condition's reason may not be. The fallback fires only if a
/// type stops serializing as a plain string; the unit test on the current
/// variants would catch that first.
fn word(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(s)) if !s.is_empty() => s,
        _ => "Unknown".to_string(),
    }
}

fn sidecar(pod: &Pod) -> Option<&ContainerStatus> {
    pod.status
        .as_ref()?
        .init_container_statuses
        .iter()
        .flatten()
        .find(|c| c.name == "sidecar")
}

/// The sidecar's last termination message, current state first.
fn termination_message(sidecar: &ContainerStatus) -> Option<String> {
    [sidecar.state.as_ref(), sidecar.last_state.as_ref()]
        .into_iter()
        .flatten()
        .find_map(|s| s.terminated.as_ref()?.message.clone())
        .map(|m| m.trim_end().to_string())
        .filter(|m| !m.is_empty())
}

const PULL_FAILURES: [&str; 3] = ["ImagePullBackOff", "ErrImagePull", "InvalidImageName"];

fn scheduled(pod: Option<&Pod>) -> Cond {
    let Some(status) = pod.and_then(|p| p.status.as_ref()) else {
        return Cond::no("Scheduled", "PodMissing", "");
    };
    let placed = status
        .conditions
        .iter()
        .flatten()
        .find(|c| c.type_ == "PodScheduled");
    if let Some(c) = placed
        && c.status == "False"
    {
        // a False PodScheduled normally carries a reason; the API would
        // reject an empty one, so name the usual cause when it is absent
        return Cond::no(
            "Scheduled",
            c.reason.as_deref().unwrap_or("Unschedulable"),
            c.message.as_deref().unwrap_or(""),
        );
    }
    let pull = status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten())
        .filter_map(|c| c.state.as_ref()?.waiting.as_ref())
        .find_map(|w| {
            let reason = w.reason.as_deref().filter(|r| PULL_FAILURES.contains(r))?;
            // a waiting state may carry no message
            Some((reason, w.message.as_deref().unwrap_or("")))
        });
    if let Some((reason, message)) = pull {
        return Cond::no("Scheduled", reason, message);
    }
    match placed {
        Some(_) => Cond::yes("Scheduled", "Scheduled", ""),
        None => Cond::unknown("Scheduled", "Pending", ""),
    }
}

/// `Scheduled`, `Materialized`, `Ready` from the Pod (§5.4), and the
/// phase, restarts and plugin states from the Daemon's status of the
/// agent, which is the sidecar's own (§7.4).
pub fn agent_status(
    agent: &Agent,
    pod: Option<&Pod>,
    reported: Option<&balerix_api::AgentStatus>,
    now: &Time,
) -> AgentStatus {
    use balerix_api::AgentPhase as P;
    let side = pod.and_then(sidecar);
    let past_materialize =
        reported.is_some_and(|r| matches!(r.phase, P::Starting | P::Ready | P::Dead | P::Stopped));
    // §21.5: a termination message is read only while the sidecar is not
    // running; a running sidecar's old crash in `lastState` is history.
    let running = side.is_some_and(|s| s.state.as_ref().is_some_and(|st| st.running.is_some()));
    let materialized = if past_materialize {
        Cond::yes("Materialized", "Materialized", "")
    } else if let Some(message) = side.filter(|_| !running).and_then(termination_message) {
        let reason = if message.starts_with("SandboxUnavailable") {
            "SandboxUnavailable"
        } else {
            "MaterializeFailed"
        };
        Cond::no("Materialized", reason, &message)
    } else {
        Cond::unknown("Materialized", "Materializing", "")
    };
    let ready = if side.is_some_and(|s| s.ready) {
        Cond::yes("Ready", "Ready", "")
    } else {
        match reported {
            Some(r) => Cond::no("Ready", &word(&r.phase), &r.message),
            None => Cond::no("Ready", "NotReady", ""),
        }
    };
    let old = agent
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or(&[]);
    AgentStatus {
        observed_generation: agent.metadata.generation,
        conditions: conditions(
            old,
            &[scheduled(pod), materialized, ready],
            agent.metadata.generation,
            now,
        ),
        phase: reported.map(|r| word(&r.phase)),
        pod: pod.and_then(|p| p.metadata.name.clone()),
        restarts: reported.map(|r| i64::from(r.restarts)),
        session: None,
        plugins: reported
            .map(|r| {
                r.plugins
                    .iter()
                    .map(|(name, activation)| (name.clone(), word(&activation.state)))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use balerix_api::{AgentPhase, CredentialBundle, PluginActivation};
    use serde_json::{Value, json};

    use super::*;
    use crate::desired::common::Images;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn agent(runner: Value) -> Agent {
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Agent",
            "metadata": {
                "name": "payments-backend-alice", "namespace": "team-a", "uid": "agent-uid", "generation": 2,
                "labels": { "balerix.ai/fleet": "payments", "balerix.ai/crew": "backend", "balerix.ai/agent": "alice" }
            },
            "spec": {
                "daemon": "default", "fleet": "payments", "crew": "backend", "agent": "alice",
                "repo": "acme/payments-api", "ref": "main", "git": { "push": true, "auth": "gh" },
                "settings": { "claude": { "settings": { "model": "sonnet" } }, "runner": runner },
                "specHash": "abc123"
            }
        }))
        .unwrap()
    }

    fn daemon() -> Daemon {
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Daemon",
            "metadata": { "name": "default", "namespace": "team-a", "uid": "daemon-uid" },
            "spec": { "storage": {
                "state": { "size": "5Gi" },
                "shared": { "storageClassName": "efs", "size": "100Gi" },
                "agent": { "storageClassName": "standard", "size": "20Gi" }
            } }
        }))
        .unwrap()
    }

    fn cfg() -> OperatorConfig {
        OperatorConfig {
            version: "0.2.0".into(),
            images: Images::for_version("0.2.0"),
            namespace: "balerix-system".into(),
        }
    }

    fn objects(runner: Value) -> AgentObjects {
        let credentials: CredentialBundle =
            serde_json::from_value(json!({ "gh_token": "gho_SECRET" })).unwrap();
        agent_objects(&AgentInputs {
            agent: &agent(runner),
            daemon: &daemon(),
            token: TOKEN,
            credentials: &credentials,
            cfg: &cfg(),
        })
        .unwrap()
    }

    #[test]
    fn the_agents_objects() {
        let o = objects(json!({
            "type": "pod",
            "resources": { "requests": { "cpu": "1", "memory": "2Gi" } },
            "storage": { "size": "40Gi" },
            "nodeSelector": { "pool": "agents" },
            "tolerations": [{ "key": "agents", "operator": "Exists" }]
        }));
        insta::assert_yaml_snapshot!("agent_claim", o.claim);
        insta::assert_yaml_snapshot!("agent_policy", o.policy);
        insta::assert_yaml_snapshot!("agent_pod", o.pod);
        // the pod names its secrets; it never holds them
        let pod = serde_json::to_string(&o.pod).unwrap();
        assert!(!pod.contains(TOKEN) && !pod.contains("gho_SECRET"), "{pod}");
    }

    #[test]
    fn the_bundle_is_what_the_sidecar_loads() {
        let o = objects(json!({ "type": "pod" }));
        let secret = serde_json::to_value(&o.bundle).unwrap();
        assert_eq!(secret["metadata"]["name"], "payments-backend-alice-bundle");
        assert_eq!(secret["metadata"]["ownerReferences"][0]["kind"], "Agent");
        let text = secret["stringData"]["agent.json"].as_str().unwrap();
        let bundle: balerix_api::AgentBundle = serde_json::from_str(text).unwrap();
        assert_eq!(bundle.agent, "payments/backend/alice");
        assert_eq!(bundle.repo, "acme/payments-api");
        assert_eq!(bundle.git_ref, "main");
        assert_eq!(bundle.daemon_url, "https://balerix-default.team-a.svc:7643");
        assert_eq!(bundle.token, TOKEN);
        assert_eq!(bundle.credentials.gh_token.as_deref(), Some("gho_SECRET"));
        assert_eq!(bundle.settings.claude.settings["model"], "sonnet");
    }

    #[test]
    fn the_claim_is_the_daemons_size_unless_the_runner_sets_one() {
        let size = |o: &AgentObjects| {
            serde_json::to_value(&o.claim).unwrap()["spec"]["resources"]["requests"]["storage"]
                .clone()
        };
        assert_eq!(size(&objects(json!({ "type": "pod" }))), "20Gi");
        assert_eq!(
            size(&objects(
                json!({ "type": "pod", "storage": { "size": "40Gi" } })
            )),
            "40Gi"
        );
    }

    #[test]
    fn the_claim_is_owned_by_the_agent_and_the_pod_carries_the_daemon_label() {
        let o = objects(json!({ "type": "pod" }));
        let owners = o.claim.metadata.owner_references.unwrap();
        assert_eq!(owners[0].kind, "Agent");
        assert_eq!(owners[0].name, "payments-backend-alice");
        let labels = o.pod.metadata.labels.unwrap();
        assert_eq!(labels["balerix.ai/daemon"], "default");
        assert_eq!(labels["balerix.ai/component"], "agent");
        assert_eq!(
            o.pod.spec.unwrap().automount_service_account_token,
            Some(false)
        );
    }

    #[test]
    fn a_tmux_agent_object_is_refused() {
        let credentials = CredentialBundle::default();
        let e = agent_objects(&AgentInputs {
            agent: &agent(json!({ "type": "tmux" })),
            daemon: &daemon(),
            token: TOKEN,
            credentials: &credentials,
            cfg: &cfg(),
        })
        .err()
        .unwrap();
        assert_eq!(
            e.to_string(),
            "the Agent has no pod runner in spec.settings"
        );
    }

    #[test]
    fn a_word_is_the_wire_word_and_never_empty() {
        assert_eq!(word(&AgentPhase::Starting), "starting");
        assert_eq!(word(&PluginActivation::active().state), "active");
    }

    fn at(secs: i64) -> Time {
        Time(k8s_openapi::jiff::Timestamp::from_second(secs).unwrap())
    }

    fn pod(status: Value) -> Pod {
        serde_json::from_value(
            json!({ "metadata": { "name": "payments-backend-alice" }, "status": status }),
        )
        .unwrap()
    }

    fn sidecar(ready: bool, state: Value, last: Value) -> Value {
        json!({ "name": "sidecar", "ready": ready, "restartCount": 0, "image": "i", "imageID": "",
                "state": state, "lastState": last })
    }

    fn reported(phase: AgentPhase, message: &str, restarts: u32) -> balerix_api::AgentStatus {
        balerix_api::AgentStatus {
            phase,
            message: message.into(),
            restarts,
            plugins: [("flow".to_string(), PluginActivation::active())].into(),
            ..Default::default()
        }
    }

    fn cond<'a>(status: &'a AgentStatus, type_: &str) -> (&'a str, &'a str, &'a str) {
        let c = status.conditions.iter().find(|c| c.type_ == type_).unwrap();
        (c.status.as_str(), c.reason.as_str(), c.message.as_str())
    }

    #[test]
    fn a_ready_sidecar_is_a_ready_agent_with_the_daemons_phase() {
        let p = pod(json!({
            "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [sidecar(true, json!({ "running": {} }), json!({}))]
        }));
        let s = agent_status(
            &agent(json!({ "type": "pod" })),
            Some(&p),
            Some(&reported(AgentPhase::Ready, "", 2)),
            &at(1),
        );
        assert_eq!(cond(&s, "Scheduled"), ("True", "Scheduled", ""));
        assert_eq!(cond(&s, "Materialized"), ("True", "Materialized", ""));
        assert_eq!(cond(&s, "Ready"), ("True", "Ready", ""));
        assert_eq!(s.phase.as_deref(), Some("ready"));
        assert_eq!(s.restarts, Some(2));
        assert_eq!(s.pod.as_deref(), Some("payments-backend-alice"));
        assert_eq!(s.plugins["flow"], "active");
        assert_eq!(s.observed_generation, Some(2));
    }

    #[test]
    fn a_pod_that_cannot_be_placed_or_pulled_is_not_scheduled() {
        let a = agent(json!({ "type": "pod" }));
        let s = agent_status(&a, None, None, &at(1));
        assert_eq!(cond(&s, "Scheduled"), ("False", "PodMissing", ""));
        assert_eq!(cond(&s, "Ready").0, "False");
        assert_eq!(s.pod, None);

        let unschedulable = pod(json!({ "conditions": [{
            "type": "PodScheduled", "status": "False", "reason": "Unschedulable",
            "message": "0/3 nodes are available: 3 Insufficient memory."
        }] }));
        let s = agent_status(&a, Some(&unschedulable), None, &at(1));
        assert_eq!(
            cond(&s, "Scheduled"),
            (
                "False",
                "Unschedulable",
                "0/3 nodes are available: 3 Insufficient memory."
            )
        );

        let pull = pod(json!({
            "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [sidecar(
                false,
                json!({ "waiting": { "reason": "ImagePullBackOff", "message": "Back-off pulling image" } }),
                json!({})
            )]
        }));
        let s = agent_status(&a, Some(&pull), None, &at(1));
        assert_eq!(
            cond(&s, "Scheduled"),
            ("False", "ImagePullBackOff", "Back-off pulling image")
        );
    }

    /// §5.4, §11: the sidecar's termination message is the reason text.
    #[test]
    fn a_sidecar_that_ended_with_a_message_is_not_materialized() {
        let a = agent(json!({ "type": "pod" }));
        let ended = |message: &str| {
            pod(json!({
                "conditions": [{ "type": "PodScheduled", "status": "True" }],
                "initContainerStatuses": [sidecar(
                    false,
                    json!({ "waiting": { "reason": "CrashLoopBackOff" } }),
                    json!({ "terminated": { "exitCode": 1, "message": format!("{message}\n") } })
                )]
            }))
        };
        let s = agent_status(
            &a,
            Some(&ended("SandboxUnavailable: Landlock not available")),
            None,
            &at(1),
        );
        assert_eq!(
            cond(&s, "Materialized"),
            (
                "False",
                "SandboxUnavailable",
                "SandboxUnavailable: Landlock not available"
            )
        );
        assert_eq!(cond(&s, "Ready").0, "False");
        let s = agent_status(
            &a,
            Some(&ended("payments/backend/alice: git clone failed")),
            None,
            &at(1),
        );
        assert_eq!(
            cond(&s, "Materialized"),
            (
                "False",
                "MaterializeFailed",
                "payments/backend/alice: git clone failed"
            )
        );
        // an old message does not outlive a later success
        let recovered = pod(json!({
            "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [sidecar(
                false,
                json!({ "running": {} }),
                json!({ "terminated": { "exitCode": 1, "message": "payments/backend/alice: git clone failed" } })
            )]
        }));
        let s = agent_status(
            &a,
            Some(&recovered),
            Some(&reported(AgentPhase::Starting, "", 0)),
            &at(1),
        );
        assert_eq!(cond(&s, "Materialized"), ("True", "Materialized", ""));
        assert_eq!(cond(&s, "Ready"), ("False", "starting", ""));
        // running, nothing reported yet
        let fresh = pod(json!({
            "conditions": [{ "type": "PodScheduled", "status": "True" }],
            "initContainerStatuses": [sidecar(false, json!({ "running": {} }), json!({}))]
        }));
        let s = agent_status(&a, Some(&fresh), None, &at(1));
        assert_eq!(cond(&s, "Materialized"), ("Unknown", "Materializing", ""));
        assert_eq!(cond(&s, "Ready"), ("False", "NotReady", ""));
    }

    #[test]
    fn both_containers_mount_an_empty_dir_at_tmp() {
        let spec = objects(json!({ "type": "pod" })).pod.spec.unwrap();
        // the sidecar and the agent; the `claim` init container only runs `mkdir`
        let both: Vec<_> = spec
            .init_containers
            .iter()
            .flatten()
            .chain(spec.containers.iter())
            .filter(|c| c.name != "claim")
            .collect();
        assert_eq!(both.len(), 2);
        for c in both {
            assert!(
                c.volume_mounts
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|m| m.mount_path == "/tmp"),
                "{}",
                c.name
            );
        }
        assert!(
            spec.volumes
                .unwrap()
                .iter()
                .any(|v| v.name == "tmp" && v.empty_dir.is_some())
        );
    }

    #[test]
    fn a_running_sidecars_old_crash_is_not_a_failure() {
        let a = agent(json!({ "type": "pod" }));
        let last =
            json!({ "terminated": { "exitCode": 1, "message": "MaterializeFailed: git: boom" } });
        let with = |state: Value| {
            pod(json!({
                "conditions": [{ "type": "PodScheduled", "status": "True" }],
                "initContainerStatuses": [sidecar(false, state, last.clone())]
            }))
        };
        let s = agent_status(&a, Some(&with(json!({ "running": {} }))), None, &at(1));
        assert_eq!(cond(&s, "Materialized"), ("Unknown", "Materializing", ""));
        // the same message on a sidecar that is not running is the failure
        let s = agent_status(
            &a,
            Some(&with(
                json!({ "waiting": { "reason": "CrashLoopBackOff" } }),
            )),
            None,
            &at(1),
        );
        assert_eq!(
            cond(&s, "Materialized"),
            ("False", "MaterializeFailed", "MaterializeFailed: git: boom")
        );
    }
}
