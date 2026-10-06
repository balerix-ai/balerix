//! A Fleet into what the operator does with it (Spec O §5.2): the
//! resolved spec, through `balerix-config` beneath the Daemon's defaults;
//! the generated Crew and Agent objects; and the `PUT` body, built only
//! once every agent has a token.

use balerix_api::{AgentTokens, FleetRequest, RunnerKind, RunnerSettings};
use balerix_config::{ConfigError, ResolveOptions};
use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use serde_json::{Value, json};

use super::common::{Cond, DesiredError, Images, hash, labels, owner_of, typed};
use super::names;
use crate::api::{Agent, AgentSpec, Crew, CrewSpec, Daemon, Fleet};

/// A Fleet waits on this until every Agent is gone (§5.2, §8.5).
pub const FLEET_FINALIZER: &str = "balerix.ai/fleet";
/// An Agent waits on this until its branch is in the crew cache (§8.4).
pub const HARVEST_FINALIZER: &str = "balerix.ai/harvest";

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// `Resolved=False`, with this text: the config path comes first.
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Desired(#[from] DesiredError),
}

#[derive(Debug, Clone)]
pub struct FleetPlan {
    pub spec: balerix_api::FleetSpec,
    pub crews: Vec<Crew>,
    pub agents: Vec<Agent>,
    /// `fleet/crew/agent` of every agent with no token yet: the
    /// controller mints these, then plans again.
    pub missing_tokens: Vec<String>,
    /// The `PUT /v1/fleets/{name}` body; `None` while a token is missing.
    pub request: Option<FleetRequest>,
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        path: path.into(),
        message: message.into(),
    }
}

/// The Fleet's `spec` as the fleet file it is (§4.2), resolved beneath
/// the Daemon's `defaults` with `runner: { type: pod }` at the bottom.
/// Then what only Kubernetes can refuse: an object name over 63
/// characters, and runner shapes that are not Kubernetes'.
pub fn resolve_fleet(
    fleet: &Fleet,
    daemon_defaults: &Value,
) -> Result<balerix_api::FleetSpec, ConfigError> {
    let name = fleet
        .metadata
        .name
        .clone()
        .ok_or_else(|| invalid("metadata.name", "required"))?;
    let crews: serde_json::Map<String, Value> = fleet
        .spec
        .crews
        .iter()
        .map(|(crew_name, crew)| {
            let mut c = json!({
                "repo": crew.repo,
                "defaults": crew.defaults,
                "agents": crew.agents,
            });
            if let Some(git_ref) = &crew.git_ref {
                c["ref"] = json!(git_ref);
            }
            if let Some(git) = &crew.git {
                c["git"] = git.clone();
            }
            (crew_name.clone(), c)
        })
        .collect();
    let file = balerix_config::from_value(&json!({
        "apiVersion": balerix_api::API_VERSION,
        "kind": "Fleet",
        "name": name,
        "defaults": fleet.spec.defaults,
        "crews": crews,
    }))?;
    let layer = balerix_config::merge(&json!({ "runner": { "type": "pod" } }), daemon_defaults);
    let spec = balerix_config::resolve(
        &file,
        &ResolveOptions {
            operator_layer: Some(layer),
            runner: RunnerKind::Pod,
            ..Default::default()
        },
    )?;
    for (crew_name, crew) in &spec.crews {
        for (agent_name, settings) in &crew.agents {
            let path = format!("crews.{crew_name}.agents.{agent_name}");
            let object = names::agent(&name, crew_name, agent_name);
            if object.len() > 63 {
                return Err(invalid(
                    path,
                    format!(
                        "the object name {object} is {} characters; at most 63",
                        object.len()
                    ),
                ));
            }
            check_runner(&path, &settings.runner)?;
        }
    }
    Ok(spec)
}

/// `balerix-api` keeps the pod's shapes opaque; here they get their
/// types, so a bad one is the Fleet's `Resolved=False` and never a pod
/// that fails to build.
fn check_runner(path: &str, runner: &RunnerSettings) -> Result<(), ConfigError> {
    let RunnerSettings::Pod {
        resources,
        storage,
        tolerations,
        ..
    } = runner
    else {
        return Ok(());
    };
    typed::<ResourceRequirements>(resources.clone())
        .map_err(|e| invalid(format!("{path}.runner.resources"), e.to_string()))?;
    typed::<Vec<Toleration>>(Value::Array(tolerations.clone()))
        .map_err(|e| invalid(format!("{path}.runner.tolerations"), e.to_string()))?;
    if let Some(storage) = storage {
        match storage.get("size").and_then(Value::as_str) {
            Some(size) if !size.is_empty() => {}
            _ => {
                return Err(invalid(
                    format!("{path}.runner.storage.size"),
                    "expected a quantity such as 40Gi",
                ));
            }
        }
    }
    Ok(())
}

pub fn plan_fleet(
    fleet: &Fleet,
    daemon: &Daemon,
    tokens: &AgentTokens,
    images: &Images,
) -> Result<FleetPlan, PlanError> {
    let spec = resolve_fleet(fleet, &daemon.spec.defaults)?;
    let namespace = fleet.metadata.namespace.clone();
    let daemon_name = fleet.spec.daemon.clone();
    let owner = owner_of(fleet)?;
    let mut crews = Vec::new();
    let mut agents = Vec::new();
    let mut wanted = Vec::new();
    for (crew_name, crew) in &spec.crews {
        let git = serde_json::to_value(&crew.git).map_err(DesiredError::from)?;
        let mut object = Crew::new(
            &names::crew(&spec.name, crew_name),
            CrewSpec {
                daemon: daemon_name.clone(),
                fleet: spec.name.clone(),
                crew: crew_name.clone(),
                repo: crew.repo.clone(),
                git_ref: crew.git_ref.clone(),
                git: git.clone(),
                fleet_tools: spec.tools.clone(),
                tools: crew.tools.clone(),
            },
        );
        object.metadata.namespace = namespace.clone();
        object.metadata.labels = Some(typed(labels(
            &daemon_name,
            "crew",
            &[
                ("balerix.ai/fleet", spec.name.as_str()),
                ("balerix.ai/crew", crew_name.as_str()),
            ],
        ))?);
        object.metadata.owner_references = Some(vec![typed(owner.clone())?]);
        crews.push(object);

        for (agent_name, settings) in &crew.agents {
            wanted.push(format!("{}/{crew_name}/{agent_name}", spec.name));
            let settings = serde_json::to_value(settings).map_err(DesiredError::from)?;
            // everything the pod is made from: a change here replaces it
            let spec_hash = hash(&json!({
                "repo": crew.repo,
                "ref": crew.git_ref,
                "git": git,
                "settings": settings,
                "image": images.agent,
            }));
            let mut object = Agent::new(
                &names::agent(&spec.name, crew_name, agent_name),
                AgentSpec {
                    daemon: daemon_name.clone(),
                    fleet: spec.name.clone(),
                    crew: crew_name.clone(),
                    agent: agent_name.clone(),
                    repo: crew.repo.clone(),
                    git_ref: crew.git_ref.clone(),
                    git: git.clone(),
                    settings,
                    spec_hash,
                },
            );
            object.metadata.namespace = namespace.clone();
            object.metadata.labels = Some(typed(labels(
                &daemon_name,
                "agent",
                &[
                    ("balerix.ai/fleet", spec.name.as_str()),
                    ("balerix.ai/crew", crew_name.as_str()),
                    ("balerix.ai/agent", agent_name.as_str()),
                ],
            ))?);
            object.metadata.owner_references = Some(vec![typed(owner.clone())?]);
            object.metadata.finalizers = Some(vec![HARVEST_FINALIZER.to_string()]);
            agents.push(object);
        }
    }
    let missing_tokens: Vec<String> = wanted
        .iter()
        .filter(|key| !tokens.contains_key(*key))
        .cloned()
        .collect();
    let request = missing_tokens.is_empty().then(|| FleetRequest {
        spec: spec.clone(),
        credentials: Default::default(),
        agent_tokens: Some(
            wanted
                .iter()
                .filter_map(|key| tokens.get(key).map(|t| (key.clone(), t.clone())))
                .collect(),
        ),
        managed_by: None,
    });
    Ok(FleetPlan {
        spec,
        crews,
        agents,
        missing_tokens,
        request,
    })
}

/// What the Daemon said to the `PUT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    Yes,
    /// A 400: a plugin rejected an agent's config. Nothing landed, and
    /// the controller touches no child object (§5.2 step 4).
    Rejected(String),
    /// The Daemon did not answer; existing pods are left alone.
    DaemonUnavailable(String),
    /// The Fleet did not resolve, so nothing was sent.
    NotAttempted,
}

/// `Resolved`, `Accepted`, `Ready`, in that order (§4.2).
pub fn fleet_conditions(
    resolved: Result<(), String>,
    accepted: &Accepted,
    ready: usize,
    total: usize,
) -> Vec<Cond> {
    let resolved_ok = resolved.is_ok();
    let first = match &resolved {
        Ok(()) => Cond::yes("Resolved", "Resolved", ""),
        Err(message) => Cond::no("Resolved", "InvalidFleet", message),
    };
    let second = match accepted {
        Accepted::Yes => Cond::yes("Accepted", "Accepted", ""),
        Accepted::Rejected(message) => Cond::no("Accepted", "Rejected", message),
        Accepted::DaemonUnavailable(message) => {
            Cond::unknown("Accepted", "DaemonUnavailable", message)
        }
        Accepted::NotAttempted => Cond::unknown("Accepted", "NotResolved", ""),
    };
    let third = match accepted {
        _ if !resolved_ok => Cond::no("Ready", "InvalidFleet", ""),
        Accepted::DaemonUnavailable(message) => Cond::no("Ready", "DaemonUnavailable", message),
        Accepted::Rejected(_) => Cond::no("Ready", "Rejected", ""),
        Accepted::NotAttempted => Cond::no("Ready", "NotResolved", ""),
        Accepted::Yes if ready == total => Cond::yes("Ready", "AgentsReady", ""),
        Accepted::Yes => Cond::no(
            "Ready",
            "AgentsNotReady",
            &format!("{ready} of {total} agents ready"),
        ),
    };
    vec![first, second, third]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use balerix_api::{AgentTokens, RunnerKind};
    use balerix_config::ResolveOptions;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::*;
    use crate::api::{Daemon, Fleet};

    fn fleet(name: &str, spec: Value) -> Fleet {
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Fleet",
            "metadata": { "name": name, "namespace": "team-a", "uid": "fleet-uid", "generation": 3 },
            "spec": spec
        }))
        .unwrap()
    }

    fn daemon(defaults: Value) -> Daemon {
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1",
            "kind": "Daemon",
            "metadata": { "name": "default", "namespace": "team-a", "uid": "daemon-uid" },
            "spec": {
                "storage": {
                    "state": { "size": "5Gi" },
                    "shared": { "storageClassName": "efs", "size": "100Gi" },
                    "agent": { "size": "20Gi" }
                },
                "defaults": defaults
            }
        }))
        .unwrap()
    }

    fn payments() -> Value {
        json!({
            "daemon": "default",
            "retain": "Branches",
            "defaults": {
                "claude": { "settings": { "permissions": { "allow": ["Bash(git *)"] } }, "resume": true },
                "tools": { "node": "22.11.0" },
                "runner": { "resources": { "requests": { "cpu": "1", "memory": "2Gi" } }, "storage": { "size": "40Gi" } }
            },
            "crews": { "backend": {
                "repo": "acme/payments-api",
                "ref": "main",
                "git": { "push": true, "auth": "gh" },
                "defaults": { "tools": { "python": "3.12.8" } },
                "agents": { "alice": {}, "bob": { "claude": { "settings": { "model": "opus" } } } }
            } }
        })
    }

    fn images() -> Images {
        Images::for_version("0.2.0")
    }

    fn tokens(keys: &[&str]) -> AgentTokens {
        keys.iter()
            .map(|k| {
                (
                    k.to_string(),
                    "0123456789abcdef0123456789abcdef".to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn the_daemons_defaults_sit_beneath_the_fleets_and_the_runner_is_a_pod() {
        let f = fleet("payments", payments());
        let spec = resolve_fleet(
            &f,
            &json!({ "claude": { "settings": { "model": "sonnet", "theme": "dark" } } }),
        )
        .unwrap();
        assert_eq!(spec.name, "payments");
        let crew = &spec.crews["backend"];
        assert_eq!(crew.agents["alice"].claude.settings["model"], "sonnet");
        assert_eq!(crew.agents["bob"].claude.settings["model"], "opus");
        assert_eq!(crew.agents["bob"].claude.settings["theme"], "dark");
        assert_eq!(
            crew.agents["alice"].runner.kind(),
            RunnerKind::Pod,
            "omitted is pod"
        );
        assert_eq!(spec.tools["node"], "22.11.0");
        assert_eq!(crew.tools["python"], "3.12.8");
    }

    /// Review focus 5: each of these refuses the whole Fleet.
    #[test]
    fn a_fleet_that_cannot_run_as_pods_fails_with_the_config_path() {
        let mut tmux = payments();
        tmux["crews"]["backend"]["agents"]["bob"]["runner"] = json!({ "type": "tmux" });
        let e = plan_fleet(
            &fleet("payments", tmux),
            &daemon(json!({})),
            &tokens(&[]),
            &images(),
        )
        .err()
        .unwrap();
        assert_eq!(
            e.to_string(),
            "crews.backend.agents.bob.runner.type: `tmux` is not available on Kubernetes; a Fleet's agents run as pods"
        );

        let long = "a-fleet-with-a-name-that-is-exactly-fifty-chars-xx";
        assert_eq!(long.len(), 50);
        let e = plan_fleet(
            &fleet(long, payments()),
            &daemon(json!({})),
            &tokens(&[]),
            &images(),
        )
        .err()
        .unwrap();
        assert_eq!(
            e.to_string(),
            format!(
                "crews.backend.agents.alice: the object name {long}-backend-alice is 64 characters; at most 63"
            )
        );

        let mut shape = payments();
        shape["defaults"]["runner"]["resources"] = json!({ "requests": "lots" });
        let e = plan_fleet(
            &fleet("payments", shape),
            &daemon(json!({})),
            &tokens(&[]),
            &images(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            e.starts_with("crews.backend.agents.alice.runner.resources: "),
            "{e}"
        );

        let mut tolerations = payments();
        tolerations["defaults"]["runner"]["tolerations"] = json!(["not-a-toleration"]);
        let e = plan_fleet(
            &fleet("payments", tolerations),
            &daemon(json!({})),
            &tokens(&[]),
            &images(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            e.starts_with("crews.backend.agents.alice.runner.tolerations: "),
            "{e}"
        );

        let e = plan_fleet(
            &fleet("balerix", payments()),
            &daemon(json!({})),
            &tokens(&[]),
            &images(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(e.starts_with("name: "), "the reserved name: {e}");
    }

    #[test]
    fn no_request_is_built_until_every_agent_has_a_token() {
        let f = fleet("payments", payments());
        let d = daemon(json!({}));
        let none = plan_fleet(&f, &d, &tokens(&[]), &images()).unwrap();
        assert_eq!(
            none.missing_tokens,
            ["payments/backend/alice", "payments/backend/bob"]
        );
        assert!(none.request.is_none(), "the Daemon refuses a partial map");
        let one = plan_fleet(&f, &d, &tokens(&["payments/backend/alice"]), &images()).unwrap();
        assert_eq!(one.missing_tokens, ["payments/backend/bob"]);
        assert!(one.request.is_none());
        // a token for an agent the Fleet no longer has is left out
        let all = plan_fleet(
            &f,
            &d,
            &tokens(&[
                "payments/backend/alice",
                "payments/backend/bob",
                "payments/backend/gone",
            ]),
            &images(),
        )
        .unwrap();
        assert!(all.missing_tokens.is_empty());
        let request = all.request.unwrap();
        assert_eq!(request.spec, all.spec);
        let sent: Vec<&String> = request.agent_tokens.as_ref().unwrap().keys().collect();
        assert_eq!(sent, ["payments/backend/alice", "payments/backend/bob"]);
    }

    #[test]
    fn crews_and_agents_are_owned_labelled_and_snapshot() {
        let f = fleet("payments", payments());
        let plan = plan_fleet(
            &f,
            &daemon(json!({ "claude": { "settings": { "model": "sonnet" } } })),
            &tokens(&["payments/backend/alice", "payments/backend/bob"]),
            &images(),
        )
        .unwrap();
        assert_eq!(plan.crews.len(), 1);
        assert_eq!(plan.agents.len(), 2);
        insta::assert_yaml_snapshot!("fleet_crews", plan.crews);
        insta::assert_yaml_snapshot!("fleet_agents", plan.agents);
        // the hash follows the settings and the image, and nothing else
        let again = plan_fleet(
            &f,
            &daemon(json!({ "claude": { "settings": { "model": "sonnet" } } })),
            &tokens(&[]),
            &images(),
        )
        .unwrap();
        assert_eq!(
            again.agents[0].spec.spec_hash,
            plan.agents[0].spec.spec_hash
        );
        let newer = plan_fleet(
            &f,
            &daemon(json!({ "claude": { "settings": { "model": "sonnet" } } })),
            &tokens(&[]),
            &Images::for_version("0.3.0"),
        )
        .unwrap();
        assert_ne!(
            newer.agents[0].spec.spec_hash,
            plan.agents[0].spec.spec_hash
        );
        let other = plan_fleet(
            &f,
            &daemon(json!({ "claude": { "settings": { "model": "haiku" } } })),
            &tokens(&[]),
            &images(),
        )
        .unwrap();
        assert_ne!(
            other.agents[0].spec.spec_hash, plan.agents[0].spec.spec_hash,
            "alice inherits the model"
        );
        assert_eq!(
            other.agents[1].spec.spec_hash, plan.agents[1].spec.spec_hash,
            "bob sets his own"
        );
    }

    #[test]
    fn conditions_follow_resolution_acceptance_and_the_agents() {
        let c = fleet_conditions(
            Err("crews.c.agents.a: x".into()),
            &Accepted::NotAttempted,
            0,
            0,
        );
        assert_eq!(
            (c[0].type_, c[0].status, c[0].reason.as_str()),
            ("Resolved", Some(false), "InvalidFleet")
        );
        assert_eq!(c[0].message, "crews.c.agents.a: x");
        assert_eq!(
            (c[1].type_, c[1].status, c[1].reason.as_str()),
            ("Accepted", None, "NotResolved")
        );
        assert_eq!((c[2].type_, c[2].status), ("Ready", Some(false)));

        let c = fleet_conditions(
            Ok(()),
            &Accepted::Rejected("flow: states.x: unknown".into()),
            0,
            2,
        );
        assert_eq!(
            (c[1].status, c[1].reason.as_str(), c[1].message.as_str()),
            (Some(false), "Rejected", "flow: states.x: unknown")
        );
        assert_eq!(c[2].status, Some(false));

        let c = fleet_conditions(
            Ok(()),
            &Accepted::DaemonUnavailable("connection refused".into()),
            1,
            2,
        );
        assert_eq!(
            (c[1].status, c[1].reason.as_str()),
            (None, "DaemonUnavailable")
        );
        assert_eq!(
            (c[2].status, c[2].reason.as_str()),
            (Some(false), "DaemonUnavailable")
        );

        let c = fleet_conditions(Ok(()), &Accepted::Yes, 1, 2);
        assert_eq!(
            (c[2].status, c[2].reason.as_str(), c[2].message.as_str()),
            (Some(false), "AgentsNotReady", "1 of 2 agents ready")
        );
        let c = fleet_conditions(Ok(()), &Accepted::Yes, 2, 2);
        assert_eq!(
            (c[2].status, c[2].reason.as_str()),
            (Some(true), "AgentsReady")
        );
    }

    /// Spec O §18: the example fleet, moved under a Fleet's `spec` with
    /// `runner.type: pod`, resolves to the same agent settings as the file.
    #[test]
    fn the_example_fleet_resolves_as_its_file_does() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/payments.yaml");
        let mut file: Value =
            serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        file["defaults"]["runner"] = json!({ "type": "pod" });
        let from_file = balerix_config::resolve(
            &balerix_config::from_value(&file).unwrap(),
            &ResolveOptions {
                runner: RunnerKind::Pod,
                ..Default::default()
            },
        )
        .unwrap();
        let mut spec = file.clone();
        let name = spec["name"].as_str().unwrap().to_string();
        for key in ["apiVersion", "kind", "name"] {
            spec.as_object_mut().unwrap().remove(key);
        }
        spec["daemon"] = json!("default");
        assert_eq!(
            resolve_fleet(&fleet(&name, spec), &json!({})).unwrap(),
            from_file
        );
    }

    fn layer() -> impl Strategy<Value = Value> {
        (
            proptest::option::of(prop_oneof![Just("sonnet"), Just("opus"), Just("haiku")]),
            proptest::option::of(prop_oneof![Just("22.11.0"), Just("20.18.1")]),
            proptest::bool::ANY,
        )
            .prop_map(|(model, node, resume)| {
                let mut l = json!({});
                if let Some(m) = model {
                    l["claude"] = json!({ "settings": { "model": m }, "resume": resume });
                }
                if let Some(n) = node {
                    l["tools"] = json!({ "node": n });
                }
                l
            })
    }

    proptest! {
        /// Spec O §15: resolving a Fleet's `spec` equals resolving the
        /// same content as a fleet file.
        #[test]
        fn a_fleets_spec_resolves_as_the_same_content_does_as_a_file(
            defaults in layer(),
            crews in proptest::collection::btree_map(
                prop_oneof![Just("backend"), Just("web"), Just("infra")],
                (layer(), proptest::collection::btree_map(
                    prop_oneof![Just("alice"), Just("bob"), Just("carol")], layer(), 1..3)),
                1..3,
            ),
        ) {
            let crews: serde_json::Map<String, Value> = crews
                .into_iter()
                .map(|(name, (crew_defaults, agents))| {
                    (name.to_string(), json!({
                        "repo": "acme/api", "ref": "main", "git": { "auth": "none" },
                        "defaults": crew_defaults, "agents": agents,
                    }))
                })
                .collect();
            let mut file_defaults = defaults.clone();
            file_defaults["runner"] = json!({ "type": "pod" });
            let file = json!({
                "apiVersion": "balerix/v1", "kind": "Fleet", "name": "f",
                "defaults": file_defaults, "crews": crews,
            });
            let from_file = balerix_config::resolve(
                &balerix_config::from_value(&file).unwrap(),
                &ResolveOptions { runner: RunnerKind::Pod, ..Default::default() },
            ).unwrap();
            let object = fleet("f", json!({ "daemon": "default", "defaults": defaults, "crews": file["crews"] }));
            prop_assert_eq!(resolve_fleet(&object, &json!({})).unwrap(), from_file);
        }
    }
}
