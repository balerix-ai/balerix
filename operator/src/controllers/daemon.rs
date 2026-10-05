//! The Daemon controller (Spec O §5.1, §21.2): mints what is missing
//! (authority, serving certificate, admin token), applies the objects
//! `desired::daemon` makes, runs the pool Job under the Job rule, and
//! writes status, asking `/readyz` once the StatefulSet has a ready pod.

use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::PostParams;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, ResourceExt};

use super::jobs::ensure_job;
use super::{
    Context, Error, api_in, apply, bounded, error_policy, patch_status, reconciled, report,
};
use crate::api::Daemon;
use crate::desired::common::{Cond, conditions};
use crate::desired::daemon::{
    DaemonObserved, Material, NOT_AFTER_ANNOTATION, daemon_objects, daemon_secrets, daemon_status,
    version_ok,
};
use crate::desired::names;
use crate::pki::{self, Issued};

pub async fn controller(ctx: Arc<Context>, watches: &kube::Client, namespace: Option<&str>) {
    let client = watches;
    Controller::new(
        api_in::<Daemon>(client, namespace),
        watcher::Config::default(),
    )
    .owns(
        api_in::<StatefulSet>(client, namespace),
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
        api_in::<ConfigMap>(client, namespace),
        watcher::Config::default(),
    )
    .owns(api_in::<Job>(client, namespace), watcher::Config::default())
    .owns(
        api_in::<NetworkPolicy>(client, namespace),
        watcher::Config::default(),
    )
    .run(bounded(reconcile), error_policy, ctx.clone())
    .for_each(|r| async move { report("daemon", r) })
    .await;
}

/// One key of a Secret as text (`data` is already base64-decoded).
pub fn read_secret_string(secret: &Secret, key: &str) -> Option<String> {
    let bytes = secret.data.as_ref()?.get(key)?;
    String::from_utf8(bytes.0.clone()).ok()
}

fn read_issued(secret: Option<&Secret>, cert_key: &str, key_key: &str) -> Option<Issued> {
    let secret = secret?;
    let not_after = secret
        .annotations()
        .get(NOT_AFTER_ANNOTATION)?
        .parse()
        .ok()?;
    Some(Issued {
        cert_pem: read_secret_string(secret, cert_key)?,
        key_pem: read_secret_string(secret, key_key)?,
        not_after,
    })
}

/// The authority's certificate (from the ConfigMap) and the admin token:
/// what a client of this Daemon is built from.
pub async fn authority_and_token(
    ctx: &Context,
    namespace: &str,
    daemon: &str,
) -> Result<(String, String), Error> {
    let configmaps: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let authority = configmaps
        .get_opt(&names::authority(daemon))
        .await?
        .and_then(|c| c.data?.get("ca.crt").cloned())
        .ok_or_else(|| Error::Missing(format!("Daemon {daemon} has no authority ConfigMap yet")))?;
    let token = secrets
        .get_opt(&names::admin(daemon))
        .await?
        .as_ref()
        .and_then(|s| read_secret_string(s, "token"))
        .ok_or_else(|| Error::Missing(format!("Daemon {daemon} has no admin Secret yet")))?;
    Ok((authority, token))
}

/// Reads the material back, minting what is absent or due (§21.2), and
/// applies the four Secret-like objects. Returns the serving expiry.
async fn ensure_material(
    ctx: &Context,
    daemon: &Daemon,
    namespace: &str,
    name: &str,
) -> Result<i64, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let now = ctx.now();
    let read = read_issued(
        secrets.get_opt(&names::authority(name)).await?.as_ref(),
        "ca.crt",
        "ca.key",
    );
    // an authority whose key does not sign what its certificate verifies is
    // missing (§22.4): kept, every serving certificate it issued would fail
    // `verifies`, and be reissued on every reconcile
    let authority = match read {
        Some(a) if pki::authority_works(&a, namespace, name, now) => a,
        Some(_) => {
            tracing::warn!(daemon = %name, "the authority's key does not match its certificate: minting a new one");
            pki::new_authority(namespace, name, now)?
        }
        None => {
            tracing::info!(daemon = %name, "minting the authority");
            pki::new_authority(namespace, name, now)?
        }
    };
    // a serving certificate its authority cannot verify is missing (§21.2):
    // checked against the authority this reconcile holds, so one minted by
    // a reconcile dropped before it applied the serving Secret is followed
    let serving_names = pki::daemon_names(namespace, name);
    let existing = read_issued(
        secrets.get_opt(&names::serving(name)).await?.as_ref(),
        "tls.crt",
        "tls.key",
    );
    let renewal = match &existing {
        None => Some("absent"),
        Some(s) if !pki::verifies(&authority.cert_pem, &s.cert_pem, &serving_names[0], now) => {
            Some("unverified")
        }
        Some(s) if pki::needs_renewal(s.not_after, now) => Some("expiring"),
        Some(_) => None,
    };
    let serving = match (existing, renewal) {
        (Some(s), None) => s,
        (_, renewal) => {
            tracing::info!(daemon = %name, renewal, "issuing the serving certificate");
            pki::issue_serving(&authority, namespace, name, &serving_names, now)?
        }
    };
    let admin_token = match secrets
        .get_opt(&names::admin(name))
        .await?
        .as_ref()
        .and_then(|s| read_secret_string(s, "token"))
    {
        Some(t) => t,
        None => pki::new_token(),
    };
    let material = Material {
        authority: &authority,
        serving: &serving,
        admin_token: &admin_token,
    };
    let objects = daemon_secrets(daemon, &material)?;
    apply(&ctx.client, &objects.authority).await?;
    apply(&ctx.client, &objects.authority_config).await?;
    apply(&ctx.client, &objects.serving).await?;
    apply(&ctx.client, &objects.admin).await?;
    Ok(serving.not_after)
}

/// Replaces `cond`'s entry in `computed`, judged against the Daemon's
/// previous status: an unchanged status keeps its transition time, so a
/// repeated failure patches nothing.
fn with_condition(computed: &mut [Condition], daemon: &Daemon, cond: Cond, now: &Time) {
    let old = daemon
        .status
        .as_ref()
        .map_or(&[][..], |s| s.conditions.as_slice());
    let generation = daemon.metadata.generation;
    if let Some(slot) = computed.iter_mut().find(|c| c.type_ == cond.type_)
        && let Some(replacement) = conditions(old, &[cond], generation, now).pop()
    {
        *slot = replacement;
    }
}

pub async fn reconcile(daemon: Arc<Daemon>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = daemon.namespace().unwrap_or_default();
    let name = daemon.name_any();
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &namespace);
    let statefulsets: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &namespace);

    let (observed_pool, again) = if version_ok(&daemon, &ctx.cfg) {
        let not_after = ensure_material(&ctx, &daemon, &namespace, &name).await?;
        let objects = daemon_objects(&daemon, &ctx.cfg, not_after)?;
        // a claim is immutable once bound: created when absent, never re-applied
        for claim in &objects.claims {
            if claims.get_opt(&claim.name_any()).await?.is_none() {
                claims.create(&PostParams::default(), claim).await?;
            }
        }
        apply(&ctx.client, &objects.service).await?;
        apply(&ctx.client, &objects.statefulset).await?;
        apply(&ctx.client, &objects.policy).await?;
        let pool = ensure_job(ctx.as_ref(), daemon.as_ref(), objects.pool_job, None).await?;
        (pool.outcome, pool.again)
    } else {
        (crate::desired::common::JobOutcome::Absent, ctx.run.period)
    };

    let shared = claims.get_opt(&names::shared_claim(&name)).await?;
    let statefulset = statefulsets.get_opt(&names::daemon(&name)).await?;
    let observed = DaemonObserved {
        shared_claim: shared.as_ref(),
        statefulset: statefulset.as_ref(),
        pool: &observed_pool,
    };
    let mut status = daemon_status(&daemon, &ctx.cfg, &observed, &ctx.k8s_now());
    status.endpoint = Some(names::endpoint(&namespace, &name));
    // `Ready` additionally needs the daemon to answer (§21.2)
    if status
        .conditions
        .iter()
        .any(|c| c.type_ == "Ready" && c.status == "True")
    {
        let (authority, token) = authority_and_token(&ctx, &namespace, &name).await?;
        let client = ctx.daemon_client(
            &namespace,
            &name,
            &names::endpoint(&namespace, &name),
            &authority,
            &token,
        )?;
        if let Err(e) = client.ready().await {
            let not_ready = Cond::no("Ready", "DaemonNotReady", &e.to_string());
            with_condition(&mut status.conditions, &daemon, not_ready, &ctx.k8s_now());
        }
    }
    patch_status(&ctx.client, daemon.as_ref(), &status).await?;
    reconciled(&ctx, daemon.as_ref());
    Ok(Action::requeue(again.min(ctx.run.period)))
}
