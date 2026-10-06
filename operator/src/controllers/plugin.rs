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
use kube::api::{DeleteParams, ListParams, PostParams, Preconditions};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, ResourceExt};
use serde_json::Value;

use super::daemon::{authority_and_token, read_issued, read_secret_string};
use super::{
    Context, Error, api_in, apply, bounded, error_policy, patch_status, reconciled, report,
};
use crate::api::{Daemon, Plugin, PluginStatus as PluginObjectStatus};
use crate::desired::common::{MANAGER, conditions};
use crate::desired::names;
use crate::desired::plugin::{
    Listing, PluginInputs, PluginState, check_name, grant, inject_secrets, listing,
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

pub async fn read_token(
    ctx: &Context,
    namespace: &str,
    plugin: &str,
) -> Result<Option<String>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(secrets
        .get_opt(&names::plugin_token(plugin))
        .await?
        .as_ref()
        .and_then(|s| read_secret_string(s, "token")))
}

pub async fn read_serving(
    ctx: &Context,
    namespace: &str,
    plugin: &str,
) -> Result<Option<Issued>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(read_issued(
        secrets
            .get_opt(&names::plugin_serving(plugin))
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
    let Some(authority) = read_issued(
        secrets.get_opt(&names::authority(daemon)).await?.as_ref(),
        "ca.crt",
        "ca.key",
    ) else {
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
    let token = match read_token(ctx, namespace, name).await? {
        Some(t) => t,
        None => pki::new_token(),
    };
    let serving = match read_serving(ctx, namespace, name).await? {
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
    apply(&ctx.client, &objects.token).await?;
    apply(&ctx.client, &objects.serving).await?;
    if let Some(claim) = &objects.claim {
        let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
        // a claim is immutable once bound: created when absent
        if claims.get_opt(&claim.name_any()).await?.is_none() {
            claims.create(&PostParams::default(), claim).await?;
        }
    }
    let deployment = apply(&ctx.client, &objects.deployment).await?;
    apply(&ctx.client, &objects.service).await?;
    apply(&ctx.client, &objects.policy).await?;
    let available = deployment
        .status
        .and_then(|s| s.available_replicas)
        .unwrap_or(0)
        >= 1;
    Ok(Ok(available))
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
