//! Objects found by name, judged before they are used, applied over or
//! deleted (Spec O §26). `observe` reads one and says whether it is the
//! owner's; an object an earlier operator made without the identity
//! labels, and that the owner holds an owner reference to, gets them
//! here. Anything else is `Error::Conflict` and is left alone: not read
//! from, not patched, not deleted.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{FieldsV1, ManagedFieldsEntry};
use kube::api::{DeleteParams, Patch, PatchParams, PostParams, Preconditions};
use kube::{Api, Client, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::Error;
use crate::desired::common::{DesiredError, MANAGER};
use crate::desired::ownership::{Identity, Standing, identity, standing};

/// The identity a desired object carries in its own labels.
pub fn identity_of<K: Resource>(object: &K) -> Identity {
    identity(object.meta().labels.as_ref().unwrap_or(&BTreeMap::new()))
}

/// The uid of a desired object's controller owner: the owner whose
/// reference adopts an unlabelled object of an earlier operator.
pub fn owner_uid<K: Resource>(object: &K) -> Option<String> {
    object
        .owner_references()
        .iter()
        .find(|o| o.controller == Some(true))
        .map(|o| o.uid.clone())
}

/// The `Conflict` an object found by name that is not the owner's makes.
pub fn conflict<K>(name: &str, why: &str) -> Error
where
    K: Resource,
    K::DynamicType: Default,
{
    Error::Conflict(format!(
        "{} {name} exists and was not made for this object ({why}): it is left alone; rename this object, or remove that one",
        K::kind(&K::DynamicType::default())
    ))
}

/// The object called `name`, if there is one and it is the owner's:
/// `Ok(None)` when absent, `Err(Error::Conflict)` when it is someone
/// else's. `owner` is the uid whose owner reference adopts an unlabelled
/// object; adopting adds the identity's labels, the read object's uid a
/// precondition of the patch.
pub async fn observe<K>(
    api: &Api<K>,
    name: &str,
    want: &Identity,
    owner: Option<&str>,
) -> Result<Option<K>, Error>
where
    K: Resource + Clone + std::fmt::Debug + DeserializeOwned,
    K::DynamicType: Default,
{
    let Some(object) = api.get_opt(name).await? else {
        return Ok(None);
    };
    let labels = object.labels().clone();
    match standing(want, &labels, object.owner_references(), owner) {
        Standing::Ours => Ok(Some(object)),
        Standing::Foreign(why) => Err(conflict::<K>(name, &why)),
        Standing::Unlabelled => {
            let add: BTreeMap<&String, &String> = want
                .iter()
                .filter_map(|(k, v)| Some((k, v.as_ref()?)))
                .collect();
            tracing::info!(kind = %K::kind(&K::DynamicType::default()), name, "adopting an object an earlier operator made: adding its labels");
            let adopted = api
                .patch(
                    name,
                    &merge_params(),
                    &Patch::Merge(serde_json::json!({
                        "metadata": { "uid": object.uid(), "labels": add }
                    })),
                )
                .await?;
            Ok(Some(adopted))
        }
    }
}

/// `observe` for a desired object: its own name, labels and owner.
pub async fn observe_desired<K>(api: &Api<K>, desired: &K) -> Result<Option<K>, Error>
where
    K: Resource + Clone + std::fmt::Debug + DeserializeOwned,
    K::DynamicType: Default,
{
    observe(
        api,
        &desired.name_any(),
        &identity_of(desired),
        owner_uid(desired).as_deref(),
    )
    .await
}

/// Deletes `object`, the one read and judged, and not one made under its
/// name since: its uid is a precondition. Gone already, or replaced, is
/// done.
pub async fn delete_read<K>(api: &Api<K>, object: &K, params: DeleteParams) -> Result<(), Error>
where
    K: Resource + Clone + std::fmt::Debug + DeserializeOwned,
{
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: object.uid(),
            resource_version: None,
        }),
        ..params
    };
    match api.delete(&object.name_any(), &params).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Server-side apply, forced, under the operator's field manager (§5),
/// over nothing or over the owner's own object: a forced apply over
/// another's would take it over (§26).
pub async fn apply<K>(client: &Client, object: &K) -> Result<K, Error>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + DeserializeOwned
        + Serialize,
    K::DynamicType: Default,
{
    let namespace = object.namespace().ok_or_else(|| {
        Error::Missing(format!("{} has no metadata.namespace", object.name_any()))
    })?;
    let api: Api<K> = Api::namespaced(client.clone(), &namespace);
    let read = observe_desired(&api, object).await?;
    apply_read(&api, object, read.as_ref()).await
}

/// `object` written over what a read found (`read`, judged the owner's
/// or absent), and over nothing else (§26.1). Found absent: a plain
/// create, so an object made under the name since is a 409. Found: a
/// forced server-side apply carrying the read object's uid, which the API
/// server refuses for an object deleted and made again since.
pub async fn apply_read<K>(api: &Api<K>, object: &K, read: Option<&K>) -> Result<K, Error>
where
    K: Resource + Clone + std::fmt::Debug + DeserializeOwned + Serialize,
    K::DynamicType: Default,
{
    let Some(read) = read else {
        let params = PostParams {
            field_manager: Some(MANAGER.to_string()),
            ..PostParams::default()
        };
        let created = api.create(&params, object).await?;
        // the first apply starts field tracking: the API server lists
        // what the create set under `before-first-apply`, folded in next
        let mut object = object.clone();
        object.meta_mut().uid = created.uid();
        let applied = api
            .patch(
                &object.name_any(),
                &PatchParams::apply(MANAGER).force(),
                &Patch::Apply(&object),
            )
            .await?;
        return to_apply(api, applied).await;
    };
    let read = to_apply(api, read.clone()).await?;
    let mut object = object.clone();
    object.meta_mut().uid = read.uid();
    match api
        .patch(
            &object.name_any(),
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&object),
        )
        .await
    {
        Ok(written) => Ok(written),
        // a controller reference someone else set, to another owner: the
        // operator neither removes it nor adds its own beside it
        Err(kube::Error::Api(e))
            if e.code == 422 && e.message.contains("Only one reference can have Controller") =>
        {
            Err(conflict::<K>(
                &object.name_any(),
                "it has a controller owner reference to another owner, set by someone else",
            ))
        }
        Err(e) => Err(e.into()),
    }
}

/// `object` with the `Update` entries a create left in `managedFields`
/// folded into the operator's `Apply` entry, as client-go's `csaupgrade`
/// does. The API server tracks no fields until an object's first apply,
/// and then lists every field it already had under `before-first-apply`;
/// an `Update` entry keeps co-owning its fields through every later
/// apply, so a field the desired object drops would never go, and a
/// leftover's old controller reference would stay beside a new one.
/// Written with the object's uid as a precondition; unchanged when there
/// is nothing to fold.
async fn to_apply<K>(api: &Api<K>, object: K) -> Result<K, Error>
where
    K: Resource + Clone + std::fmt::Debug + DeserializeOwned + Serialize,
    K::DynamicType: Default,
{
    let mut object = object;
    // never applied (made by a create, an earlier pass ended before its
    // first apply): an apply of its identity alone starts the tracking
    if object.managed_fields().is_empty() {
        let whole = serde_json::to_value(&object).map_err(DesiredError::from)?;
        let identity = serde_json::json!({
            "apiVersion": whole["apiVersion"],
            "kind": whole["kind"],
            "metadata": {
                "name": object.name_any(),
                "namespace": object.namespace(),
                "uid": object.uid(),
            },
        });
        object = api
            .patch(
                &object.name_any(),
                &PatchParams::apply(MANAGER).force(),
                &Patch::Apply(&identity),
            )
            .await?;
    }
    // the whole list is replaced: under the resourceVersion read too, so
    // another manager's write in between is not lost but re-read
    let uid = object.uid();
    for _ in 0..FOLD_TRIES {
        let entries = object.meta().managed_fields.clone().unwrap_or_default();
        let Some(folded) = fold_updates(&entries) else {
            return Ok(object);
        };
        let written = api
            .patch(
                &object.name_any(),
                &merge_params(),
                &Patch::Merge(serde_json::json!({
                    "metadata": {
                        "uid": uid,
                        "resourceVersion": object.resource_version(),
                        "managedFields": folded,
                    }
                })),
            )
            .await;
        match written {
            Ok(written) => return Ok(written),
            Err(kube::Error::Api(e)) if e.code == 409 => {
                let again = api.get(&object.name_any()).await?;
                if again.uid() != uid {
                    return Err(conflict::<K>(
                        &object.name_any(),
                        "it was deleted and made again while being written",
                    ));
                }
                object = again;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Waiting(format!(
        "{} {}: its field ownership changed under {FOLD_TRIES} writes in a row; retried",
        K::kind(&K::DynamicType::default()),
        object.name_any()
    )))
}

/// How many times the fold is written over a fresh read before the
/// reconcile gives up and is retried.
const FOLD_TRIES: usize = 3;

/// The operator's field manager for its merge patches (an attempts or
/// back-off annotation, adoption labels, a released claim), apart from
/// the apply manager (§26.1). `fold_updates` folds only `MANAGER`'s
/// `Update` entries and `before-first-apply`: a merge patch's fields
/// stay its own, so the next apply does not drop them.
pub const PATCH_MANAGER: &str = "balerix-operator-patch";

/// `PatchParams` for a merge patch: `PATCH_MANAGER`, never the default,
/// whose manager the API server derives from the client's user agent.
pub fn merge_params() -> PatchParams {
    PatchParams {
        field_manager: Some(PATCH_MANAGER.to_string()),
        ..PatchParams::default()
    }
}

/// The manager the API server names for the fields an object had before
/// its first server-side apply.
pub const BEFORE_FIRST_APPLY: &str = "before-first-apply";

fn own_entry(e: &ManagedFieldsEntry, operation: &str) -> bool {
    e.manager.as_deref() == Some(MANAGER)
        && e.operation.as_deref() == Some(operation)
        && e.subresource.as_deref().unwrap_or("").is_empty()
}

/// An `Update` entry to fold: the operator's own (a create under its
/// field manager), or `before-first-apply`.
fn foldable(e: &ManagedFieldsEntry) -> bool {
    own_entry(e, "Update")
        || (e.manager.as_deref() == Some(BEFORE_FIRST_APPLY)
            && e.operation.as_deref() == Some("Update")
            && e.subresource.as_deref().unwrap_or("").is_empty())
}

/// `entries` with the foldable `Update` entries merged into the
/// operator's `Apply` entry (made from the first when there is none);
/// `None` when there is none to fold. Status is never folded in, and a
/// status subresource's entry is left as it is: an apply never writes
/// status.
pub fn fold_updates(entries: &[ManagedFieldsEntry]) -> Option<Vec<ManagedFieldsEntry>> {
    let updates: Vec<&ManagedFieldsEntry> = entries.iter().filter(|e| foldable(e)).collect();
    let first = updates.first()?;
    let mut apply = entries
        .iter()
        .find(|e| own_entry(e, "Apply"))
        .cloned()
        .unwrap_or_else(|| ManagedFieldsEntry {
            manager: Some(MANAGER.to_string()),
            operation: Some("Apply".to_string()),
            fields_v1: None,
            ..(*first).clone()
        });
    let mut fields = apply
        .fields_v1
        .take()
        .map(|f| f.0)
        .unwrap_or_else(|| serde_json::json!({}));
    for update in &updates {
        if let Some(f) = &update.fields_v1 {
            let mut f = f.0.clone();
            if let Some(o) = f.as_object_mut() {
                o.remove("f:status");
            }
            union(&mut fields, &f);
        }
    }
    apply.fields_v1 = Some(FieldsV1(fields));
    apply.fields_type = Some("FieldsV1".to_string());
    let mut out: Vec<ManagedFieldsEntry> = entries
        .iter()
        .filter(|e| !foldable(e) && !own_entry(e, "Apply"))
        .cloned()
        .collect();
    out.push(apply);
    Some(out)
}

/// `into` with every path of `from` added: field sets are tries of
/// objects whose leaves are `{}`.
fn union(into: &mut serde_json::Value, from: &serde_json::Value) {
    let (Some(into), Some(from)) = (into.as_object_mut(), from.as_object()) else {
        return;
    };
    for (k, v) in from {
        match into.get_mut(k) {
            Some(existing) => union(existing, v),
            None => {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::json;

    use super::*;

    fn entry(
        manager: &str,
        operation: &str,
        subresource: Option<&str>,
        fields: serde_json::Value,
    ) -> ManagedFieldsEntry {
        ManagedFieldsEntry {
            api_version: Some("v1".into()),
            fields_type: Some("FieldsV1".into()),
            fields_v1: Some(FieldsV1(fields)),
            manager: Some(manager.into()),
            operation: Some(operation.into()),
            subresource: subresource.map(Into::into),
            time: None,
        }
    }

    #[test]
    fn the_operators_update_entries_fold_into_its_apply_entry() {
        let entries = vec![
            entry(
                MANAGER,
                "Update",
                None,
                json!({ "f:data": { "f:a": {}, "f:b": {} } }),
            ),
            entry(
                MANAGER,
                "Apply",
                None,
                json!({ "f:data": { "f:a": {} }, "f:metadata": { "f:labels": {} } }),
            ),
            entry(MANAGER, "Update", Some("status"), json!({ "f:status": {} })),
            entry(
                "kubectl",
                "Update",
                None,
                json!({ "f:data": { "f:c": {} } }),
            ),
        ];
        let folded = fold_updates(&entries).unwrap();
        assert_eq!(folded.len(), 3);
        let apply = folded.iter().find(|e| own_entry(e, "Apply")).unwrap();
        assert_eq!(
            apply.fields_v1.as_ref().unwrap().0,
            json!({ "f:data": { "f:a": {}, "f:b": {} }, "f:metadata": { "f:labels": {} } })
        );
        // another manager's, and the status entry, are kept as they were
        assert!(folded.contains(&entries[2]) && folded.contains(&entries[3]));
        // none to fold: nothing to write
        assert!(fold_updates(&entries[1..]).is_none());
        // no Apply entry yet: the Update becomes it
        let made = fold_updates(&entries[..1]).unwrap();
        assert_eq!(made.len(), 1);
        assert!(own_entry(&made[0], "Apply"));
        assert_eq!(made[0].fields_v1, entries[0].fields_v1);
        // what an object had before its first apply is folded in too,
        // without its status
        let first_apply = vec![
            entry(MANAGER, "Apply", None, json!({ "f:data": { "f:a": {} } })),
            entry(
                BEFORE_FIRST_APPLY,
                "Update",
                None,
                json!({ "f:data": { "f:a": {}, "f:b": {} }, "f:status": { "f:phase": {} } }),
            ),
            entry(
                "unknown",
                "Update",
                None,
                json!({ "f:metadata": { "f:annotations": {} } }),
            ),
        ];
        let folded = fold_updates(&first_apply).unwrap();
        assert_eq!(folded.len(), 2);
        assert_eq!(
            folded
                .iter()
                .find(|e| own_entry(e, "Apply"))
                .unwrap()
                .fields_v1
                .as_ref()
                .unwrap()
                .0,
            json!({ "f:data": { "f:a": {}, "f:b": {} } })
        );
        assert!(
            folded.contains(&first_apply[2]),
            "another manager's is kept"
        );
        // the operator's merge patches are their own manager: never folded
        let patched = vec![entry(
            PATCH_MANAGER,
            "Update",
            None,
            json!({ "f:metadata": { "f:annotations": { "f:balerix.ai/attempts": {} } } }),
        )];
        assert!(fold_updates(&patched).is_none());
    }
}
