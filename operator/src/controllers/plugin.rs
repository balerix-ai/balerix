//! The Plugin controller (Spec O §5.5, §23.4): a Plugin named in one
//! Daemon's `spec.plugins` gets its Secrets, claim, Deployment, Service and
//! NetworkPolicy, each owned by it; unlisted, it owns nothing. Its `Ready`
//! is the Daemon's row for it, polled on the Fleet period.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, ListParams, Preconditions};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, ResourceExt};
use serde_json::Value;

use super::daemon::{authority_and_token, daemon_identity, read_issued, read_secret_string};
use super::owned::{apply_read, observe, observe_desired};
use super::{Context, Error, api_in, bounded, error_policy, patch_status, reconciled, report};
use crate::api::{Daemon, Plugin, PluginStatus as PluginObjectStatus};
use crate::desired::common::{MANAGER, conditions, labels};
use crate::desired::names;
use crate::desired::ownership::{Identity, identity_of};
use crate::desired::plugin::{
    Listing, PLUGIN_LABEL, PluginInputs, PluginState, check_name, grant, inject_secrets, listing,
    plugin_conditions, plugin_objects,
};
use crate::pki::{self, Issued};

pub async fn controller(ctx: Arc<Context>, watches: &kube::Client, namespace: Option<&str>) {
    let client = watches;
    Controller::new(
        api_in::<Plugin>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<Deployment>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<Service>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<Secret>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<NetworkPolicy>(client, namespace),
        watcher::Config::default(),
    )
    .run(bounded(reconcile), error_policy, ctx.clone())
    .for_each(|r| async move { report("plugin", r) })
    .await;
}

/// `spec.config` with `spec.secrets` injected, or why it cannot be. A
/// Secret the operator manages (a Daemon's authority, its admin token, a
/// plugin's token) is never one: whoever may edit a Plugin could ship it
/// to any image.
pub async fn read_config(
    ctx: &Context,
    namespace: &str,
    plugin: &Plugin,
) -> Result<Result<Value, (&'static str, String)>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let mut values = BTreeMap::new();
    for (key, r) in &plugin.spec.secrets {
        let Some(secret) = secrets.get_opt(&r.secret_name).await? else {
            return Ok(Err((
                "SecretMissing",
                format!(
                    "spec.secrets.{key}: Secret {} does not exist",
                    r.secret_name
                ),
            )));
        };
        if secret
            .labels()
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str)
            == Some(MANAGER)
        {
            return Ok(Err((
                "InvalidSpec",
                format!(
                    "spec.secrets.{key}: Secret {} is managed by the operator",
                    r.secret_name
                ),
            )));
        }
        let Some(value) = read_secret_string(&secret, &r.key) else {
            return Ok(Err((
                "SecretMissing",
                format!(
                    "spec.secrets.{key}: Secret {} has no key {}",
                    r.secret_name, r.key
                ),
            )));
        };
        values.insert(key.clone(), value);
    }
    Ok(inject_secrets(&plugin.spec.config, &values).map_err(|e| ("InvalidSpec", e)))
}

/// What a Plugin's own objects carry (§26): its component and its name.
fn plugin_identity(plugin: &Plugin) -> Identity {
    identity_of(&labels("", "plugin", &[(PLUGIN_LABEL, &plugin.name_any())]))
}

/// The plugin's token, from its Secret when that is the Plugin's own
/// (§26): another object by the name is a `Conflict`.
pub async fn read_token(
    ctx: &Context,
    namespace: &str,
    plugin: &Plugin,
) -> Result<Option<String>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(observe(
        &secrets,
        &names::plugin_token(&plugin.name_any()),
        &plugin_identity(plugin),
        plugin.uid().as_deref(),
    )
    .await?
    .as_ref()
    .and_then(|s| read_secret_string(s, "token")))
}

/// The plugin's serving certificate, from its Secret when that is the
/// Plugin's own (§26).
pub async fn read_serving(
    ctx: &Context,
    namespace: &str,
    plugin: &Plugin,
) -> Result<Option<Issued>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(read_issued(
        observe(
            &secrets,
            &names::plugin_serving(&plugin.name_any()),
            &plugin_identity(plugin),
            plugin.uid().as_deref(),
        )
        .await?
        .as_ref(),
        "tls.crt",
        "tls.key",
    ))
}

/// Unlisted: the objects a listing made are deleted (§23.4). Only those
/// this Plugin owns: an object of the same name that someone else made is
/// left alone.
async fn remove_owned(
    ctx: &Context,
    namespace: &str,
    plugin: &str,
    uid: &str,
) -> Result<(), Error> {
    async fn release<K>(api: Api<K>, name: &str, owner: &str) -> Result<(), Error>
    where
        K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        let Some(object) = api.get_opt(name).await? else {
            return Ok(());
        };
        if !object.owner_references().iter().any(|o| o.uid == owner) {
            return Ok(());
        }
        // the object read, not one made under its name since
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: object.uid(),
                resource_version: None,
            }),
            ..DeleteParams::default()
        };
        match api.delete(name, &params).await {
            Ok(_) => Ok(()),
            // gone, or replaced by one that is not ours
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    let c = &ctx.client;
    release(
        Api::<Deployment>::namespaced(c.clone(), namespace),
        &names::plugin(plugin),
        uid,
    )
    .await?;
    release(
        Api::<Service>::namespaced(c.clone(), namespace),
        plugin,
        uid,
    )
    .await?;
    release(
        Api::<NetworkPolicy>::namespaced(c.clone(), namespace),
        &names::plugin(plugin),
        uid,
    )
    .await?;
    release(
        Api::<Secret>::namespaced(c.clone(), namespace),
        &names::plugin_token(plugin),
        uid,
    )
    .await?;
    release(
        Api::<Secret>::namespaced(c.clone(), namespace),
        &names::plugin_serving(plugin),
        uid,
    )
    .await?;
    release(
        Api::<PersistentVolumeClaim>::namespaced(c.clone(), namespace),
        &names::plugin_scratch(plugin),
        uid,
    )
    .await?;
    Ok(())
}

pub async fn reconcile(plugin: Arc<Plugin>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = plugin.namespace().unwrap_or_default();
    let name = plugin.name_any();
    let daemons = Api::<Daemon>::namespaced(ctx.client.clone(), &namespace)
        .list(&ListParams::default())
        .await?
        .items;
    let rows;
    let state = match listing(&name, &daemons) {
        Listing::None => {
            let uid = plugin
                .uid()
                .ok_or_else(|| Error::Missing(format!("Plugin {name} has no metadata.uid")))?;
            remove_owned(&ctx, &namespace, &name, &uid).await?;
            PluginState::NotListed
        }
        Listing::Many(ds) => PluginState::ListedTwice(ds),
        Listing::One(daemon) => match listed(&ctx, &plugin, &namespace, &name, &daemon).await? {
            Err((reason, message)) => PluginState::Blocked { reason, message },
            Ok(available) => {
                rows = row_of(&ctx, &namespace, &daemon).await;
                PluginState::Running {
                    available,
                    row: rows
                        .as_ref()
                        .map(|rs| rs.iter().find(|r| r.name == name))
                        .map_err(Clone::clone),
                }
            }
        },
    };
    let old = plugin
        .status
        .as_ref()
        .map_or(&[][..], |s| s.conditions.as_slice());
    let status = PluginObjectStatus {
        observed_generation: plugin.metadata.generation,
        conditions: conditions(
            old,
            &plugin_conditions(&state),
            plugin.metadata.generation,
            &ctx.k8s_now(),
        ),
    };
    patch_status(&ctx.client, plugin.as_ref(), &status).await?;
    reconciled(&ctx, plugin.as_ref());
    Ok(Action::requeue(ctx.run.fleet_period))
}

/// The objects of a Plugin `daemon` lists; `Ok(available)` once applied.
async fn listed(
    ctx: &Context,
    plugin: &Plugin,
    namespace: &str,
    name: &str,
    daemon: &str,
) -> Result<Result<bool, (&'static str, String)>, Error> {
    // before anything is made: a name the Service or the Daemon refuses
    // would fail part-way through
    if let Err(e) = check_name(name) {
        return Ok(Err(("InvalidSpec", e)));
    }
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let authority = match observe(
        &secrets,
        &names::authority(daemon),
        &daemon_identity(daemon),
        None,
    )
    .await
    {
        Ok(found) => found,
        Err(Error::Conflict(m)) => return Ok(Err(("Conflict", m))),
        Err(e) => return Err(e),
    };
    let Some(authority) = read_issued(authority.as_ref(), "ca.crt", "ca.key") else {
        return Ok(Err((
            "WaitingForDaemon",
            format!("Daemon {daemon} has no authority yet"),
        )));
    };
    if let Err(e) = grant(&plugin.spec) {
        return Ok(Err(("InvalidSpec", e)));
    }
    let config = match read_config(ctx, namespace, plugin).await? {
        Ok(c) => c,
        Err(blocked) => return Ok(Err(blocked)),
    };
    let now = ctx.now();
    let plugin_names = pki::plugin_names(namespace, name);
    let token = match read_token(ctx, namespace, plugin).await {
        Ok(Some(t)) => t,
        Ok(None) => pki::new_token(),
        Err(Error::Conflict(m)) => return Ok(Err(("Conflict", m))),
        Err(e) => return Err(e),
    };
    let serving = match read_serving(ctx, namespace, plugin).await {
        Err(Error::Conflict(m)) => return Ok(Err(("Conflict", m))),
        other => other?,
    };
    let serving = match serving {
        Some(s)
            if pki::verifies(&authority.cert_pem, &s.cert_pem, &plugin_names[0], now)
                && !pki::needs_renewal(s.not_after, now) =>
        {
            s
        }
        _ => {
            tracing::info!(plugin = %name, "issuing the plugin's serving certificate");
            pki::issue_serving(&authority, namespace, daemon, &plugin_names, now)?
        }
    };
    let inputs = PluginInputs {
        token,
        serving,
        config,
    };
    let objects = match plugin_objects(plugin, daemon, &inputs) {
        Ok(o) => o,
        Err(e) => return Ok(Err(("InvalidSpec", e.to_string()))),
    };
    match apply_objects(ctx, namespace, &objects).await {
        Ok(deployment) => {
            let available = deployment
                .status
                .and_then(|s| s.available_replicas)
                .unwrap_or(0)
                >= 1;
            Ok(Ok(available))
        }
        Err(Error::Conflict(m)) => Ok(Err(("Conflict", m))),
        Err(e) => Err(e),
    }
}

/// Each object over nothing or the Plugin's own (§26); the Deployment
/// as applied. Every name is judged before any object is written, so
/// one in `Conflict` stops the Deployment too, and the policy is written
/// before the Deployment it covers.
async fn apply_objects(
    ctx: &Context,
    namespace: &str,
    objects: &crate::desired::plugin::PluginObjects,
) -> Result<Deployment, Error> {
    let c = &ctx.client;
    let secrets: Api<Secret> = Api::namespaced(c.clone(), namespace);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), namespace);
    let deployments: Api<Deployment> = Api::namespaced(c.clone(), namespace);
    let services: Api<Service> = Api::namespaced(c.clone(), namespace);
    let policies: Api<NetworkPolicy> = Api::namespaced(c.clone(), namespace);
    let token = observe_desired(&secrets, &objects.token).await?;
    let serving = observe_desired(&secrets, &objects.serving).await?;
    let claim = match &objects.claim {
        Some(claim) => Some(observe_desired(&claims, claim).await?),
        None => None,
    };
    let deployment = observe_desired(&deployments, &objects.deployment).await?;
    let service = observe_desired(&services, &objects.service).await?;
    let policy = observe_desired(&policies, &objects.policy).await?;
    apply_read(&secrets, &objects.token, token.as_ref()).await?;
    apply_read(&secrets, &objects.serving, serving.as_ref()).await?;
    // a claim is immutable once bound: created when absent
    if let (Some(wanted), Some(None)) = (&objects.claim, &claim) {
        apply_read(&claims, wanted, None).await?;
    }
    apply_read(&policies, &objects.policy, policy.as_ref()).await?;
    apply_read(&services, &objects.service, service.as_ref()).await?;
    apply_read(&deployments, &objects.deployment, deployment.as_ref()).await
}

/// The Daemon's rows, or why it could not be asked.
async fn row_of(
    ctx: &Context,
    namespace: &str,
    daemon: &str,
) -> Result<Vec<balerix_api::PluginStatus>, String> {
    let (authority, token) = authority_and_token(ctx, namespace, daemon)
        .await
        .map_err(|e| e.to_string())?;
    let client = ctx
        .daemon_client(
            namespace,
            daemon,
            &names::endpoint(namespace, daemon),
            &authority,
            &token,
        )
        .map_err(|e| e.to_string())?;
    client.plugins().await.map_err(|e| e.to_string())
}
