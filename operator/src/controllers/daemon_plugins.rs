//! The Daemon controller's plugin half (Spec O §23.4): the list it sends
//! on every reconcile, `PluginsReady` from the Daemon's rows, and the
//! managed requests written as Fleets.

use kube::api::{DeleteParams, ListParams};
use kube::{Api, ResourceExt};

use super::plugin::{read_config, read_serving, read_token};
use super::{Context, Error, apply};
use crate::api::{Daemon, Fleet, Plugin};
use crate::daemon_client::DaemonClient;
use crate::desired::common::Cond;
use crate::desired::plugin::{
    MANAGED_BY_LABEL, PluginInputs, declared, grant, managed_fleet, plugins_ready, retain_of,
};

/// Builds and sends `PUT /v1/plugins` from `spec.plugins` in order, then
/// judges `PluginsReady` from `GET /v1/plugins`. A plugin with no Plugin,
/// or whose token or serving Secret is not made yet, is left out of this
/// pass: neither has managed requests to lose (deleting a Plugin collects
/// its Fleets with it). A plugin listed by another Daemon too, or whose
/// grant or config cannot be built, holds the whole list back: the Daemon
/// drops the requests of any plugin a list leaves out (§23.7), so sending
/// without it would delete its Fleets over a second listing or a deleted
/// Secret. Returns the names sent, `None` when nothing was.
pub async fn send_list(
    ctx: &Context,
    daemon: &Daemon,
    namespace: &str,
    client: &DaemonClient,
) -> Result<(Cond, Option<Vec<String>>), Error> {
    let name = daemon.name_any();
    let plugins: Api<Plugin> = Api::namespaced(ctx.client.clone(), namespace);
    let others: Vec<Daemon> = Api::<Daemon>::namespaced(ctx.client.clone(), namespace)
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|d| d.name_any() != name)
        .collect();
    let (mut missing, mut entries) = (Vec::new(), Vec::new());
    for p in &daemon.spec.plugins {
        if others.iter().any(|d| d.spec.plugins.contains(p)) {
            let message = format!("Plugin {p} is listed by another Daemon too");
            return Ok((
                Cond::no("PluginsReady", "PluginListedTwice", &message),
                None,
            ));
        }
        let Some(plugin) = plugins.get_opt(p).await? else {
            missing.push(p.clone());
            continue;
        };
        let (Some(token), Some(serving)) = (
            read_token(ctx, namespace, p).await?,
            read_serving(ctx, namespace, p).await?,
        ) else {
            continue;
        };
        let config = match grant(&plugin.spec) {
            Err(e) => Err(("InvalidSpec", e)),
            Ok(_) => read_config(ctx, namespace, &plugin).await?,
        };
        let config = match config {
            Ok(c) => c,
            Err((reason, message)) => {
                return Ok((
                    Cond::no("PluginsReady", reason, &format!("{p}: {message}")),
                    None,
                ));
            }
        };
        entries.push(declared(
            &plugin,
            namespace,
            &PluginInputs {
                token,
                serving,
                config,
            },
        )?);
    }
    let sent: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    client
        .declare_plugins(&balerix_api::DeclaredPlugins { plugins: entries })
        .await?;
    let rows = client.plugins().await?;
    Ok((
        plugins_ready(&daemon.spec.plugins, &missing, &[], &rows),
        Some(sent),
    ))
}

/// Each live request of a plugin sent this pass becomes a Fleet labelled
/// for it. A down request, or a sent plugin's Fleet with no live request,
/// deletes the Fleet; so does a plugin dropped from `spec.plugins`. A
/// plugin still listed but left out of this pass (its Secrets not made)
/// keeps its Fleets. A Fleet without the label, or with another plugin's,
/// is never written: a `FleetConflict` Event instead. A request that
/// cannot be made a Fleet, or that the API server refuses, is a
/// `ManagedFleetRefused` Event on its Plugin and the rest go on.
pub async fn write_managed(
    ctx: &Context,
    daemon: &Daemon,
    namespace: &str,
    client: &DaemonClient,
    sent: &[String],
) -> Result<(), Error> {
    let name = daemon.name_any();
    let fleets: Api<Fleet> = Api::namespaced(ctx.client.clone(), namespace);
    let plugins: Api<Plugin> = Api::namespaced(ctx.client.clone(), namespace);
    let rows = client.managed_fleets().await?;
    for row in rows.iter().filter(|r| sent.contains(&r.plugin)) {
        let Some(plugin) = plugins.get_opt(&row.plugin).await? else {
            continue;
        };
        let written = write_one(ctx, &fleets, row, &plugin, &name).await;
        refused(ctx, Some(&plugin), &row.name, written).await?;
    }
    let ours = fleets
        .list(&ListParams::default().labels(MANAGED_BY_LABEL))
        .await?
        .items;
    for f in ours.iter().filter(|f| f.spec.daemon == name) {
        let plugin = f
            .labels()
            .get(MANAGED_BY_LABEL)
            .cloned()
            .unwrap_or_default();
        let dropped = !daemon.spec.plugins.contains(&plugin);
        let live = rows
            .iter()
            .any(|r| r.name == f.name_any() && r.plugin == plugin && r.down.is_none());
        if dropped || (sent.contains(&plugin) && !live) {
            let deleted = delete_fleet(&fleets, &f.name_any()).await;
            let owner = plugins.get_opt(&plugin).await?;
            refused(ctx, owner.as_ref(), &f.name_any(), deleted).await?;
        }
    }
    Ok(())
}

/// One managed request written: applied, or downed and deleted.
async fn write_one(
    ctx: &Context,
    fleets: &Api<Fleet>,
    row: &balerix_api::ManagedFleet,
    plugin: &Plugin,
    daemon: &str,
) -> Result<(), Error> {
    let current = fleets.get_opt(&row.name).await?;
    if let Some(f) = &current
        && f.labels().get(MANAGED_BY_LABEL) != Some(&row.plugin)
    {
        let note = format!(
            "Fleet {} exists and is not managed by plugin {}: left as it is",
            row.name, row.plugin
        );
        ctx.warn(plugin, "FleetConflict", note).await;
        return Ok(());
    }
    let mut desired = managed_fleet(row, plugin, daemon)?;
    match &row.down {
        None => {
            apply(&ctx.client, &desired).await?;
        }
        Some(down) if current.is_some() => {
            desired.spec.retain = retain_of(down);
            apply(&ctx.client, &desired).await?;
            delete_fleet(fleets, &row.name).await?;
        }
        Some(_) => {}
    }
    Ok(())
}

/// A request's own failure (one `managed_fleet` cannot build, or a 4xx
/// from the API server) is a Warning on its Plugin and the pass goes on;
/// anything else (transport, 5xx) fails the reconcile.
async fn refused(
    ctx: &Context,
    plugin: Option<&Plugin>,
    fleet: &str,
    result: Result<(), Error>,
) -> Result<(), Error> {
    let why = match result {
        Ok(()) => return Ok(()),
        Err(Error::Desired(e)) => e.to_string(),
        Err(Error::Kube(kube::Error::Api(e))) if (400..500).contains(&e.code) => e.to_string(),
        Err(e) => return Err(e),
    };
    let note = format!("managed Fleet {fleet}: {why}");
    match plugin {
        Some(p) => ctx.warn(p, "ManagedFleetRefused", note).await,
        None => tracing::warn!(fleet, "{note}"),
    }
    Ok(())
}

async fn delete_fleet(fleets: &Api<Fleet>, name: &str) -> Result<(), Error> {
    match fleets.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
