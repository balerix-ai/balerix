//! A listed Plugin into what runs it (Spec O §5.5, §23.4): its token and
//! serving Secrets, the optional scratch claim, a Deployment of one
//! hardened pod, its Service and NetworkPolicy; and its entry in the
//! Daemon's `PUT /v1/plugins`. The token, the certificate and the
//! resolved config come in as `PluginInputs`; nothing random is made here.

use std::collections::{BTreeMap, BTreeSet};

use balerix_api::{AgentPhase, Capability, DeclaredPlugin, DownQuery, ManagedFleet, PluginStatus};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use serde_json::{Value, json};

use super::common::{
    Cond, DAEMON_PORT, DesiredError, HASH_ANNOTATION, claim, container_security, hash, labels,
    owner_of, pod_security, typed,
};
use super::names;
use crate::api::{ClaimSpec, Daemon, Fleet, Plugin, PluginSpec, Retain};
use crate::pki::Issued;

pub const PLUGIN_PORT: i32 = 7644;
pub const PLUGIN_LABEL: &str = "balerix.ai/plugin";
/// On a Fleet the operator wrote for a plugin's managed request (§23.4).
pub const MANAGED_BY_LABEL: &str = "balerix.ai/managed-by";

/// What a listed Plugin's objects and list entry are built from.
pub struct PluginInputs {
    pub token: String,
    pub serving: Issued,
    /// `spec.config` with `spec.secrets` injected.
    pub config: Value,
}

fn name_of(plugin: &Plugin) -> Result<(&str, &str), DesiredError> {
    let name = plugin
        .metadata
        .name
        .as_deref()
        .ok_or(DesiredError::Missing("the Plugin", "metadata.name"))?;
    let namespace = plugin
        .metadata
        .namespace
        .as_deref()
        .ok_or(DesiredError::Missing("the Plugin", "metadata.namespace"))?;
    Ok((name, namespace))
}

/// Why a Plugin may not take `name`, if it may not: the Daemon's own rule
/// for a plugin's name (`AgentName` and the reserved names, which
/// `PUT /v1/plugins` answers 400 to) and a Service's (`<name>` is the
/// plugin's Service, a DNS-1035 label: it starts with a letter).
pub fn check_name(name: &str) -> Result<(), String> {
    balerix_core::name::validate_name(name).map_err(|why| format!("metadata.name: {why}"))?;
    if let Some(why) = balerix_core::reserved_plugin_reason(name) {
        return Err(format!("metadata.name: {why}"));
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(
            "metadata.name: starts with a digit; its Service's name must start with a letter"
                .to_string(),
        );
    }
    Ok(())
}

/// `spec.needs` as capabilities, the first unknown one named.
pub fn grant(spec: &PluginSpec) -> Result<BTreeSet<Capability>, String> {
    spec.needs
        .iter()
        .enumerate()
        .map(|(i, n)| {
            serde_json::from_value::<Capability>(json!(n))
                .map_err(|_| format!("spec.needs[{i}]: unknown capability {n:?}"))
        })
        .collect()
}

/// `plugins.yaml`'s `secrets` rule (plugins spec G-7): each key goes in at
/// the config's top level and may not replace a config key.
pub fn inject_secrets(config: &Value, values: &BTreeMap<String, String>) -> Result<Value, String> {
    let mut out = config.clone();
    let map = out.as_object_mut().ok_or("spec.config: not a mapping")?;
    for (key, value) in values {
        if map.contains_key(key) {
            return Err(format!(
                "spec.secrets.{key}: collides with spec.config.{key}"
            ));
        }
        map.insert(key.clone(), json!(value));
    }
    Ok(out)
}

/// §23.8: one hash over everything the pod and the list entry are built
/// from. The Deployment rolls on it and the pod's hello carries it.
pub fn revision(plugin: &Plugin, inputs: &PluginInputs) -> Result<String, DesiredError> {
    let (name, _) = name_of(plugin)?;
    Ok(hash(&json!({
        "name": name,
        "spec": serde_json::to_value(&plugin.spec)?,
        "config": inputs.config,
        "token": inputs.token,
        "certificate": inputs.serving.cert_pem,
    })))
}

pub fn declared(
    plugin: &Plugin,
    namespace: &str,
    inputs: &PluginInputs,
) -> Result<DeclaredPlugin, DesiredError> {
    let (name, _) = name_of(plugin)?;
    let grant = grant(&plugin.spec).map_err(DesiredError::Invalid)?;
    Ok(DeclaredPlugin {
        name: name.to_string(),
        grant,
        config: inputs.config.clone(),
        fleet_defaults: plugin.spec.fleet_defaults.clone(),
        token: inputs.token.clone(),
        url: names::plugin_url(namespace, name),
        revision: revision(plugin, inputs)?,
    })
}

pub struct PluginObjects {
    pub token: Secret,
    pub serving: Secret,
    pub claim: Option<PersistentVolumeClaim>,
    pub deployment: Deployment,
    pub service: Service,
    pub policy: NetworkPolicy,
}

pub fn plugin_objects(
    plugin: &Plugin,
    daemon: &str,
    inputs: &PluginInputs,
) -> Result<PluginObjects, DesiredError> {
    let (name, namespace) = name_of(plugin)?;
    let owner = owner_of(plugin)?;
    let labels = labels(daemon, "plugin", &[(PLUGIN_LABEL, name)]);
    let selector = json!({ PLUGIN_LABEL: name, "balerix.ai/component": "plugin" });
    let daemon_pod = json!({ "balerix.ai/daemon": names::daemon_label(daemon), "balerix.ai/component": "daemon" });
    let metadata = |object: String| {
        json!({
            "name": object, "namespace": namespace, "labels": labels, "ownerReferences": [owner],
        })
    };
    let revision = revision(plugin, inputs)?;
    let env = |n: &str, v: &str| json!({ "name": n, "value": v });
    let mut ports = vec![json!({ "name": "plugin", "containerPort": PLUGIN_PORT })];
    let mut service_ports =
        vec![json!({ "name": "plugin", "port": PLUGIN_PORT, "targetPort": "plugin" })];
    let mut ingress = vec![json!({
        "from": [{ "podSelector": { "matchLabels": daemon_pod } }],
        "ports": [{ "protocol": "TCP", "port": PLUGIN_PORT }],
    })];
    if let Some(expose) = &plugin.spec.expose {
        ports.push(json!({ "name": "expose", "containerPort": expose.port }));
        service_ports
            .push(json!({ "name": "expose", "port": expose.port, "targetPort": "expose" }));
        ingress.push(json!({ "ports": [{ "protocol": "TCP", "port": expose.port }] }));
    }
    let scratch_volume = match &plugin.spec.scratch {
        Some(_) => {
            json!({ "name": "scratch", "persistentVolumeClaim": { "claimName": names::plugin_scratch(name) } })
        }
        None => json!({ "name": "scratch", "emptyDir": {} }),
    };
    let secret = |object: String, type_: &str, data: Value| -> Result<Secret, DesiredError> {
        typed(
            json!({ "apiVersion": "v1", "kind": "Secret", "metadata": metadata(object), "type": type_, "stringData": data }),
        )
    };
    Ok(PluginObjects {
        token: secret(
            names::plugin_token(name),
            "Opaque",
            json!({ "token": inputs.token }),
        )?,
        serving: {
            let mut s = secret(
                names::plugin_serving(name),
                "kubernetes.io/tls",
                json!({ "tls.crt": inputs.serving.cert_pem, "tls.key": inputs.serving.key_pem }),
            )?;
            s.metadata.annotations = Some(BTreeMap::from([(
                super::daemon::NOT_AFTER_ANNOTATION.to_string(),
                inputs.serving.not_after.to_string(),
            )]));
            s
        },
        claim: plugin
            .spec
            .scratch
            .as_ref()
            .map(|s| {
                let mut c = claim(
                    namespace,
                    &names::plugin_scratch(name),
                    labels.clone(),
                    &ClaimSpec {
                        storage_class_name: None,
                        size: s.size.clone(),
                    },
                    "ReadWriteOnce",
                )?;
                c.metadata.owner_references = Some(vec![serde_json::from_value(owner.clone())?]);
                Ok::<_, DesiredError>(c)
            })
            .transpose()?,
        deployment: typed(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": metadata(names::plugin(name)),
            "spec": {
                "replicas": 1,
                // the old pod goes first: a scratch claim is ReadWriteOnce,
                // and a new pod on another node would wait on it forever
                "strategy": { "type": "Recreate" },
                "selector": { "matchLabels": selector },
                "template": {
                    "metadata": { "labels": labels, "annotations": { HASH_ANNOTATION: revision } },
                    "spec": {
                        "automountServiceAccountToken": false,
                        "enableServiceLinks": false,
                        "securityContext": pod_security(),
                        "containers": [{
                            "name": "plugin",
                            "image": plugin.spec.image,
                            "env": [
                                env("BALERIX_API_URL", &names::endpoint(namespace, daemon)),
                                env("BALERIX_PLUGIN_NAME", name),
                                env("BALERIX_PLUGIN_TOKEN_FILE", "/balerix/token/token"),
                                env("BALERIX_PLUGIN_SCRATCH", "/balerix/scratch"),
                                env("BALERIX_CA_FILE", "/balerix/ca/ca.crt"),
                                env("BALERIX_PLUGIN_TLS_CERT", "/balerix/tls/tls.crt"),
                                env("BALERIX_PLUGIN_TLS_KEY", "/balerix/tls/tls.key"),
                                env("BALERIX_PLUGIN_LISTEN", &format!("0.0.0.0:{PLUGIN_PORT}")),
                                env("BALERIX_PLUGIN_REVISION", &revision),
                                env("HOME", "/tmp"),
                            ],
                            "ports": ports,
                            "resources": plugin.spec.resources,
                            "securityContext": container_security(),
                            "volumeMounts": [
                                { "name": "token", "mountPath": "/balerix/token", "readOnly": true },
                                { "name": "tls", "mountPath": "/balerix/tls", "readOnly": true },
                                { "name": "ca", "mountPath": "/balerix/ca", "readOnly": true },
                                { "name": "scratch", "mountPath": "/balerix/scratch" },
                                { "name": "tmp", "mountPath": "/tmp" },
                            ],
                        }],
                        "volumes": [
                            { "name": "token", "secret": { "secretName": names::plugin_token(name), "defaultMode": 0o440 } },
                            { "name": "tls", "secret": { "secretName": names::plugin_serving(name), "defaultMode": 0o440 } },
                            { "name": "ca", "configMap": { "name": names::authority(daemon) } },
                            scratch_volume,
                            { "name": "tmp", "emptyDir": {} },
                        ],
                    },
                },
            },
        }))?,
        service: typed(json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": metadata(name.to_string()),
            "spec": { "selector": selector, "ports": service_ports },
        }))?,
        // §23.4: in from the Daemon's pod (and anyone to `expose`); out to
        // the Daemon, DNS and 443
        policy: typed(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
            "metadata": metadata(names::plugin(name)),
            "spec": {
                "podSelector": { "matchLabels": selector },
                "policyTypes": ["Ingress", "Egress"],
                "ingress": ingress,
                "egress": [
                    { "to": [{ "podSelector": { "matchLabels": daemon_pod } }], "ports": [{ "protocol": "TCP", "port": DAEMON_PORT }] },
                    { "ports": [{ "protocol": "UDP", "port": 53 }, { "protocol": "TCP", "port": 53 }] },
                    { "ports": [{ "protocol": "TCP", "port": 443 }] },
                ],
            },
        }))?,
    })
}

pub enum Listing {
    None,
    One(String),
    Many(Vec<String>),
}

/// The Daemons in the Plugin's namespace whose `spec.plugins` name it.
pub fn listing(plugin: &str, daemons: &[Daemon]) -> Listing {
    let mut by: Vec<String> = daemons
        .iter()
        .filter(|d| d.spec.plugins.iter().any(|p| p == plugin))
        .filter_map(|d| d.metadata.name.clone())
        .collect();
    by.sort();
    match by.len() {
        0 => Listing::None,
        1 => Listing::One(by.remove(0)),
        _ => Listing::Many(by),
    }
}

pub enum PluginState<'a> {
    NotListed,
    ListedTwice(Vec<String>),
    /// Listed, but its objects cannot be built: `WaitingForDaemon`,
    /// `InvalidSpec`, `SecretMissing`.
    Blocked {
        reason: &'static str,
        message: String,
    },
    /// Its objects applied. `row` is the Daemon's answer: its row (or none
    /// yet), or why the Daemon could not be asked.
    Running {
        available: bool,
        row: Result<Option<&'a PluginStatus>, String>,
    },
}

/// `Deployed`, then `Ready` (§23.4).
pub fn plugin_conditions(state: &PluginState<'_>) -> Vec<Cond> {
    let both = |reason: &str, message: &str| {
        vec![
            Cond::no("Deployed", reason, message),
            Cond::no("Ready", reason, message),
        ]
    };
    match state {
        PluginState::NotListed => both(
            "NotListed",
            "no Daemon in this namespace lists it in spec.plugins",
        ),
        PluginState::ListedTwice(ds) => both(
            "PluginListedTwice",
            &format!("listed by Daemons {}", ds.join(", ")),
        ),
        PluginState::Blocked { reason, message } => both(reason, message),
        PluginState::Running { available, row } => {
            let deployed = if *available {
                Cond::yes("Deployed", "Available", "")
            } else {
                Cond::no(
                    "Deployed",
                    "DeploymentNotAvailable",
                    "the plugin's Deployment has no available replica",
                )
            };
            let ready = match row {
                Err(e) => Cond::no("Ready", "DaemonUnavailable", e),
                Ok(None) => Cond::no(
                    "Ready",
                    "PluginNotReady",
                    "the Daemon has no row for it yet",
                ),
                Ok(Some(r)) if r.phase == AgentPhase::Ready => Cond::yes("Ready", "Ready", ""),
                Ok(Some(r)) if r.phase == AgentPhase::Failed => {
                    Cond::no("Ready", "PluginRefused", &r.message)
                }
                Ok(Some(r)) => Cond::no(
                    "Ready",
                    "PluginNotReady",
                    &format!("the Daemon lists it {}", wire(&r.phase)),
                ),
            };
            vec![deployed, ready]
        }
    }
}

/// The Daemon's `PluginsReady` (§23.4): the first listed plugin, in list
/// order, that is not ready names why.
pub fn plugins_ready(
    listed: &[String],
    missing: &[String],
    twice: &[String],
    rows: &[PluginStatus],
) -> Cond {
    if listed.is_empty() {
        return Cond::yes("PluginsReady", "NoPlugins", "");
    }
    for name in listed {
        if missing.contains(name) {
            return Cond::no(
                "PluginsReady",
                "PluginMissing",
                &format!("Plugin {name} does not exist"),
            );
        }
        if twice.contains(name) {
            return Cond::no(
                "PluginsReady",
                "PluginListedTwice",
                &format!("Plugin {name} is listed by another Daemon too"),
            );
        }
        match rows.iter().find(|r| &r.name == name) {
            None => {
                return Cond::no(
                    "PluginsReady",
                    "PluginNotReady",
                    &format!("{name}: not declared yet"),
                );
            }
            Some(r) if r.phase == AgentPhase::Ready => {}
            Some(r) if r.phase == AgentPhase::Failed => {
                return Cond::no(
                    "PluginsReady",
                    "PluginRefused",
                    &format!("{name}: {}", r.message),
                );
            }
            Some(r) => {
                return Cond::no(
                    "PluginsReady",
                    "PluginNotReady",
                    &format!("{name}: {}", wire(&r.phase)),
                );
            }
        }
    }
    Cond::yes("PluginsReady", "AllReady", "")
}

/// `keep-repos` keeps the branches (O-16); any other down is plain.
pub fn retain_of(down: &DownQuery) -> Retain {
    if down.keep_repos {
        Retain::Branches
    } else {
        Retain::None
    }
}

/// A managed request as a Fleet (§23.4): the file's defaults merged over
/// the Plugin's `fleetDefaults`, its crews as they are, labelled for the
/// plugin and owned by its Plugin. The Daemon resolved the file when the
/// plugin sent it; the restricted surface is not checked again.
pub fn managed_fleet(
    row: &ManagedFleet,
    plugin: &Plugin,
    daemon: &str,
) -> Result<Fleet, DesiredError> {
    let (plugin_name, namespace) = name_of(plugin)?;
    let file_defaults = row
        .file
        .get("defaults")
        .cloned()
        .unwrap_or_else(|| json!({}));
    typed(json!({
        "apiVersion": "balerix.ai/v1alpha1",
        "kind": "Fleet",
        "metadata": {
            "name": row.name,
            "namespace": namespace,
            "labels": { MANAGED_BY_LABEL: plugin_name },
            "ownerReferences": [owner_of(plugin)?],
        },
        "spec": {
            "daemon": daemon,
            "retain": Retain::None,
            "defaults": balerix_config::merge(&plugin.spec.fleet_defaults, &file_defaults),
            "crews": row.file.get("crews").cloned().unwrap_or_else(|| json!({})),
        },
    }))
}

/// An `AgentPhase` as the wire spells it.
fn wire(phase: &AgentPhase) -> String {
    serde_json::to_value(phase)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::json;

    use super::*;

    fn plugin(extra: Value) -> Plugin {
        let mut spec = json!({ "image": "balerix-plugin-web:e2e", "needs": ["fleets", "attach", "actions", "workspace"],
                               "config": { "enabled": true } });
        for (k, v) in extra.as_object().unwrap() {
            spec[k] = v.clone();
        }
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1", "kind": "Plugin",
            "metadata": { "name": "web", "namespace": "team-a", "uid": "plugin-uid", "generation": 2 },
            "spec": spec
        }))
        .unwrap()
    }

    fn row(name: &str, phase: AgentPhase, message: &str) -> PluginStatus {
        PluginStatus {
            name: name.into(),
            version: "1".into(),
            phase,
            listen: None,
            routes: false,
            message: message.into(),
            active_agents: 0,
        }
    }

    #[test]
    fn plugins_ready_names_the_first_plugin_in_list_order_that_is_not() {
        let listed = vec!["flow".to_string(), "web".to_string()];
        let f = |missing: &[&str], twice: &[&str], rows: &[PluginStatus]| {
            let c = plugins_ready(
                &listed,
                &missing.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &twice.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                rows,
            );
            (c.status, c.reason, c.message)
        };
        assert_eq!(plugins_ready(&[], &[], &[], &[]).reason, "NoPlugins");
        let ok = [
            row("flow", AgentPhase::Ready, ""),
            row("web", AgentPhase::Ready, ""),
        ];
        assert_eq!(
            f(&[], &[], &ok),
            (Some(true), "AllReady".into(), String::new())
        );
        assert_eq!(
            f(&["flow"], &[], &ok),
            (
                Some(false),
                "PluginMissing".into(),
                "Plugin flow does not exist".into()
            )
        );
        assert_eq!(
            f(&[], &["web"], &ok),
            (
                Some(false),
                "PluginListedTwice".into(),
                "Plugin web is listed by another Daemon too".into()
            )
        );
        assert_eq!(
            f(
                &[],
                &[],
                &[
                    row(
                        "flow",
                        AgentPhase::Failed,
                        "hello.manifest.needs: kv is not granted"
                    ),
                    ok[1].clone()
                ]
            ),
            (
                Some(false),
                "PluginRefused".into(),
                "flow: hello.manifest.needs: kv is not granted".into()
            )
        );
        assert_eq!(
            f(&[], &[], &[ok[0].clone()]),
            (
                Some(false),
                "PluginNotReady".into(),
                "web: not declared yet".into()
            )
        );
        assert_eq!(
            f(
                &[],
                &[],
                &[ok[0].clone(), row("web", AgentPhase::Starting, "")]
            ),
            (Some(false), "PluginNotReady".into(), "web: starting".into())
        );
    }

    #[test]
    fn a_managed_fleet_is_the_file_over_the_plugins_fleet_defaults() {
        let p = plugin(json!({ "fleetDefaults": { "claude": { "model": "sonnet" },
            "sandbox": { "network": { "block": false } } } }));
        let row: ManagedFleet = serde_json::from_value(json!({ "name": "m", "plugin": "web", "file": {
            "apiVersion": "balerix/v1", "kind": "Fleet", "name": "m",
            "defaults": { "claude": { "model": "opus" } },
            "crews": { "c": { "repo": "git://git/repo.git", "ref": "main", "git": { "auth": "none" },
                "agents": { "carol": {} } } } } }))
        .unwrap();
        let f = managed_fleet(&row, &p, "default").unwrap();
        insta::assert_yaml_snapshot!("managed_fleet", f);
        assert_eq!(f.metadata.labels.as_ref().unwrap()[MANAGED_BY_LABEL], "web");
        assert_eq!(f.spec.daemon, "default");
        assert_eq!(
            f.spec.defaults["claude"]["model"],
            json!("opus"),
            "the file wins"
        );
        assert_eq!(
            f.spec.defaults["sandbox"]["network"]["block"],
            json!(false),
            "beneath it, the plugin's"
        );
        assert_eq!(f.metadata.owner_references.as_ref().unwrap()[0].name, "web");
    }

    #[test]
    fn a_plugin_name_is_one_the_daemon_and_a_service_can_take() {
        for ok in ["web", "flow", "github2", "a-b"] {
            assert_eq!(check_name(ok), Ok(()), "{ok}");
        }
        let long = "a".repeat(64);
        for (bad, why) in [
            (
                "1password",
                "metadata.name: starts with a digit; its Service's name must start with a letter",
            ),
            (
                "kubernetes",
                "metadata.name: reserved: it is the owner name of the operator's fleets",
            ),
            (
                "my.plugin",
                "metadata.name: contains characters other than a-z, 0-9 and '-'",
            ),
            (long.as_str(), "metadata.name: longer than 63 characters"),
        ] {
            assert_eq!(check_name(bad), Err(why.to_string()), "{bad}");
        }
    }

    #[test]
    fn keep_repos_is_branches_and_anything_else_is_none() {
        assert_eq!(
            retain_of(&DownQuery {
                keep_repos: true,
                ..Default::default()
            }),
            Retain::Branches
        );
        assert_eq!(
            retain_of(&DownQuery {
                purge: true,
                ..Default::default()
            }),
            Retain::None
        );
    }

    fn inputs() -> PluginInputs {
        PluginInputs {
            token: "TOKEN".into(),
            serving: Issued {
                cert_pem: "TLS CERT".into(),
                key_pem: "TLS KEY".into(),
                not_after: 1_807_776_000,
            },
            config: json!({ "enabled": true }),
        }
    }

    #[test]
    fn names_and_url() {
        assert_eq!(names::plugin("web"), "balerix-plugin-web");
        assert_eq!(names::plugin_token("web"), "balerix-plugin-web-token");
        assert_eq!(names::plugin_serving("web"), "balerix-plugin-web-tls");
        assert_eq!(names::plugin_scratch("web"), "balerix-plugin-web-scratch");
        assert_eq!(
            names::plugin_url("team-a", "web"),
            "https://web.team-a.svc:7644"
        );
        assert_eq!(
            crate::pki::plugin_names("team-a", "web"),
            [
                "web",
                "web.team-a",
                "web.team-a.svc",
                "web.team-a.svc.cluster.local"
            ]
        );
    }

    #[test]
    fn the_grant_is_the_needs_and_an_unknown_one_is_named() {
        assert_eq!(grant(&plugin(json!({})).spec).unwrap().len(), 4);
        assert_eq!(
            grant(&plugin(json!({ "needs": ["kv", "telepathy"] })).spec).unwrap_err(),
            "spec.needs[1]: unknown capability \"telepathy\""
        );
    }

    #[test]
    fn secrets_go_in_at_the_top_and_never_over_a_config_key() {
        let values = BTreeMap::from([("password".to_string(), "hunter2".to_string())]);
        assert_eq!(
            inject_secrets(&json!({ "user": "u" }), &values).unwrap(),
            json!({ "user": "u", "password": "hunter2" })
        );
        assert_eq!(
            inject_secrets(&json!({ "password": "x" }), &values).unwrap_err(),
            "spec.secrets.password: collides with spec.config.password"
        );
        assert_eq!(
            inject_secrets(&json!([]), &values).unwrap_err(),
            "spec.config: not a mapping"
        );
    }

    #[test]
    fn the_revision_moves_with_every_input_and_nothing_else() {
        let p = plugin(json!({}));
        let r = revision(&p, &inputs()).unwrap();
        assert_eq!(r.len(), 64);
        assert_eq!(r, revision(&p, &inputs()).unwrap(), "stable");
        let mut token = inputs();
        token.token = "OTHER".into();
        let mut cert = inputs();
        cert.serving.cert_pem = "RENEWED".into();
        let mut config = inputs();
        config.config = json!({ "enabled": false });
        for changed in [token, cert, config] {
            assert_ne!(revision(&p, &changed).unwrap(), r);
        }
        assert_ne!(
            revision(&plugin(json!({ "image": "other:1" })), &inputs()).unwrap(),
            r
        );
        // status and metadata other than the name do not count
        let mut later = p.clone();
        later.metadata.generation = Some(9);
        assert_eq!(revision(&later, &inputs()).unwrap(), r);
    }

    #[test]
    fn the_list_entry() {
        let d = declared(
            &plugin(json!({ "fleetDefaults": { "claude": { "model": "sonnet" } } })),
            "team-a",
            &inputs(),
        )
        .unwrap();
        assert_eq!(d.name, "web");
        assert_eq!(d.url, "https://web.team-a.svc:7644");
        assert_eq!(d.token, "TOKEN");
        assert_eq!(d.config, json!({ "enabled": true }));
        assert_eq!(d.fleet_defaults, json!({ "claude": { "model": "sonnet" } }));
        assert_eq!(d.grant.len(), 4);
        assert_eq!(
            d.revision,
            revision(
                &plugin(json!({ "fleetDefaults": { "claude": { "model": "sonnet" } } })),
                &inputs()
            )
            .unwrap()
        );
    }

    #[test]
    fn the_plugins_objects() {
        let o = plugin_objects(
            &plugin(
                json!({ "expose": { "port": 8080 }, "scratch": { "size": "1Gi" },
            "resources": { "requests": { "cpu": "50m" } } }),
            ),
            "default",
            &inputs(),
        )
        .unwrap();
        insta::assert_yaml_snapshot!("plugin_secret_token", o.token);
        insta::assert_yaml_snapshot!("plugin_secret_serving", o.serving);
        insta::assert_yaml_snapshot!("plugin_claim", o.claim);
        insta::assert_yaml_snapshot!("plugin_deployment", o.deployment);
        insta::assert_yaml_snapshot!("plugin_service", o.service);
        insta::assert_yaml_snapshot!("plugin_policy", o.policy);
    }

    #[test]
    fn without_scratch_or_expose_there_is_an_empty_dir_and_one_port() {
        let o = plugin_objects(&plugin(json!({})), "default", &inputs()).unwrap();
        assert!(o.claim.is_none());
        let pod = serde_json::to_value(&o.deployment).unwrap()["spec"]["template"]["spec"].clone();
        let scratch = pod["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "scratch")
            .unwrap();
        assert_eq!(scratch["emptyDir"], json!({}));
        assert_eq!(
            serde_json::to_value(&o.service).unwrap()["spec"]["ports"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let policy = serde_json::to_value(&o.policy).unwrap();
        assert_eq!(policy["spec"]["ingress"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_pod_says_its_revision_twice_and_rolls_on_it() {
        let p = plugin(json!({}));
        let o = plugin_objects(&p, "default", &inputs()).unwrap();
        let v = serde_json::to_value(&o.deployment).unwrap();
        let r = revision(&p, &inputs()).unwrap();
        assert_eq!(
            v["spec"]["template"]["metadata"]["annotations"][HASH_ANNOTATION],
            json!(r)
        );
        let env = v["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        let get = |n: &str| {
            env.iter()
                .find(|e| e["name"] == n)
                .map(|e| e["value"].clone())
        };
        assert_eq!(get("BALERIX_PLUGIN_REVISION"), Some(json!(r)));
        assert_eq!(
            get("BALERIX_API_URL"),
            Some(json!("https://balerix-default.team-a.svc:7643"))
        );
        assert_eq!(get("BALERIX_PLUGIN_LISTEN"), Some(json!("0.0.0.0:7644")));
        // the config is never on the pod (§23.1)
        assert!(!v.to_string().contains("\"enabled\""), "{v}");
    }

    #[test]
    fn listing_counts_the_daemons_that_name_the_plugin() {
        let daemon = |name: &str, plugins: &[&str]| -> Daemon {
            serde_json::from_value(json!({ "apiVersion": "balerix.ai/v1alpha1", "kind": "Daemon",
                "metadata": { "name": name, "namespace": "team-a" },
                "spec": { "storage": { "state": { "size": "1Gi" }, "shared": { "size": "1Gi" }, "agent": { "size": "1Gi" } },
                          "plugins": plugins } })).unwrap()
        };
        assert!(matches!(
            listing("web", &[daemon("a", &["flow"])]),
            Listing::None
        ));
        assert!(
            matches!(listing("web", &[daemon("a", &["web"]), daemon("b", &[])]), Listing::One(d) if d == "a")
        );
        assert!(
            matches!(listing("web", &[daemon("a", &["web"]), daemon("b", &["web"])]),
            Listing::Many(ds) if ds == ["a", "b"])
        );
    }

    #[test]
    fn the_conditions() {
        let c = |s: &PluginState<'_>| {
            plugin_conditions(s)
                .into_iter()
                .map(|c| (c.type_, c.status, c.reason, c.message))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            c(&PluginState::NotListed)[0],
            (
                "Deployed",
                Some(false),
                "NotListed".into(),
                "no Daemon in this namespace lists it in spec.plugins".into()
            )
        );
        assert_eq!(
            c(&PluginState::ListedTwice(vec!["a".into(), "b".into()]))[1].2,
            "PluginListedTwice"
        );
        let ready = PluginStatus {
            name: "web".into(),
            version: "0.2.1".into(),
            phase: AgentPhase::Ready,
            listen: None,
            routes: true,
            message: String::new(),
            active_agents: 0,
        };
        let refused = PluginStatus {
            phase: AgentPhase::Failed,
            message: "hello.manifest.needs: kv is not granted".into(),
            ..ready.clone()
        };
        assert_eq!(
            c(&PluginState::Running {
                available: true,
                row: Ok(Some(&ready))
            }),
            vec![
                ("Deployed", Some(true), "Available".into(), String::new()),
                ("Ready", Some(true), "Ready".into(), String::new())
            ]
        );
        assert_eq!(
            c(&PluginState::Running {
                available: true,
                row: Ok(Some(&refused))
            })[1],
            (
                "Ready",
                Some(false),
                "PluginRefused".into(),
                "hello.manifest.needs: kv is not granted".into()
            )
        );
        assert_eq!(
            c(&PluginState::Running {
                available: false,
                row: Ok(None)
            }),
            vec![
                (
                    "Deployed",
                    Some(false),
                    "DeploymentNotAvailable".into(),
                    "the plugin's Deployment has no available replica".into()
                ),
                (
                    "Ready",
                    Some(false),
                    "PluginNotReady".into(),
                    "the Daemon has no row for it yet".into()
                )
            ]
        );
        assert_eq!(
            c(&PluginState::Running {
                available: true,
                row: Err("the Daemon is unavailable: x".into())
            })[1]
                .2,
            "DaemonUnavailable"
        );
        assert_eq!(
            c(&PluginState::Blocked {
                reason: "SecretMissing",
                message: "spec.secrets.password: Secret s has no key k".into()
            })[1],
            (
                "Ready",
                Some(false),
                "SecretMissing".into(),
                "spec.secrets.password: Secret s has no key k".into()
            )
        );
    }
}
