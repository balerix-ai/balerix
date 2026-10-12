//! Whose an object is (Spec O §26): every object the operator finds by
//! name is judged by its labels before it is used, applied over or
//! deleted. A name says where to look; the labels say whether what is
//! there is the one the operator made for this owner.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use serde_json::Value;

use super::plugin::PLUGIN_LABEL;

pub const COMPONENT_LABEL: &str = "balerix.ai/component";
pub const DAEMON_LABEL: &str = "balerix.ai/daemon";
pub const FLEET_LABEL: &str = "balerix.ai/fleet";
pub const CREW_LABEL: &str = "balerix.ai/crew";
pub const AGENT_LABEL: &str = "balerix.ai/agent";

/// The labels that say whose an object is. `None`: the label must be
/// absent.
pub type Identity = BTreeMap<String, Option<String>>;

/// The identity an object with these labels carries: its component, and
/// the fleet, crew, agent and plugin it is for, each present or absent. An
/// object with neither a fleet nor a plugin label is a Daemon's own, and
/// the Daemon's label identifies it too; on any other it does not: a
/// Plugin may be listed by another Daemon and keeps its objects, and a
/// Fleet's are named by the Fleet. The agent claim adds it back
/// (`controllers::agent`, §26.1).
pub fn identity(labels: &BTreeMap<String, String>) -> Identity {
    let mut id: Identity = [
        COMPONENT_LABEL,
        FLEET_LABEL,
        CREW_LABEL,
        AGENT_LABEL,
        PLUGIN_LABEL,
    ]
    .iter()
    .map(|k| (k.to_string(), labels.get(*k).cloned()))
    .collect();
    if id[FLEET_LABEL].is_none() && id[PLUGIN_LABEL].is_none() {
        id.insert(DAEMON_LABEL.to_string(), labels.get(DAEMON_LABEL).cloned());
    }
    id
}

/// `identity` of a label set written as JSON (`common::labels`). Values
/// that are not strings are ignored.
pub fn identity_of(labels: &Value) -> Identity {
    let map: BTreeMap<String, String> = labels
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect();
    identity(&map)
}

/// What an object found by name is to the owner looking for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Its labels are the identity.
    Ours,
    /// Some identity labels are missing, none disagrees, and the owner
    /// is its controller owner (an owner reference with `controller:
    /// true`): an object an earlier operator made.
    /// It is adopted: the labels are added, then it is ours.
    Unlabelled,
    /// Anything else, with why: it is left alone.
    Foreign(String),
}

/// Judges an object's `labels` and `owners` against `identity`. `owner`
/// is the uid of the owner looking, whose controller owner reference
/// makes an unlabelled object its own; `None` adopts nothing.
pub fn standing(
    identity: &Identity,
    labels: &BTreeMap<String, String>,
    owners: &[OwnerReference],
    owner: Option<&str>,
) -> Standing {
    let mut missing = Vec::new();
    for (key, want) in identity {
        match (want, labels.get(key)) {
            (Some(w), Some(h)) if w == h => {}
            (None, None) => {}
            (Some(_), None) => missing.push(key.as_str()),
            (_, Some(h)) => return Standing::Foreign(format!("its label {key} is {h}")),
        }
    }
    if missing.is_empty() {
        return Standing::Ours;
    }
    match owner {
        Some(uid)
            if owners
                .iter()
                .any(|o| o.uid == uid && o.controller == Some(true)) =>
        {
            Standing::Unlabelled
        }
        _ => Standing::Foreign(format!(
            "it has no label {} and no controller owner reference to this owner",
            missing.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::json;

    use super::*;
    use crate::desired::common::labels;

    fn map(v: Value) -> BTreeMap<String, String> {
        serde_json::from_value(v).unwrap()
    }

    fn owner(uid: &str) -> OwnerReference {
        OwnerReference {
            api_version: "balerix.ai/v1alpha1".into(),
            kind: "Agent".into(),
            name: "f-c-a".into(),
            uid: uid.into(),
            controller: Some(true),
            ..OwnerReference::default()
        }
    }

    fn agent_labels(fleet: &str, crew: &str, agent: &str) -> Value {
        labels(
            "default",
            "agent",
            &[
                (FLEET_LABEL, fleet),
                (CREW_LABEL, crew),
                (AGENT_LABEL, agent),
            ],
        )
    }

    #[test]
    fn the_same_labels_are_ours_on_any_daemon() {
        let id = identity_of(&agent_labels("f", "c", "a"));
        let mut theirs = map(agent_labels("f", "c", "a"));
        assert_eq!(standing(&id, &theirs, &[], None), Standing::Ours);
        // the Daemon's label is not part of a Fleet object's identity
        theirs.insert(DAEMON_LABEL.into(), "other".into());
        assert_eq!(standing(&id, &theirs, &[], None), Standing::Ours);
    }

    #[test]
    fn a_different_or_extra_identity_label_is_foreign() {
        let id = identity_of(&agent_labels("f", "c", "a"));
        // the same name joined from other parts
        let other = map(agent_labels("f", "c-a", ""));
        assert!(matches!(
            standing(&id, &other, &[owner("u")], Some("u")),
            Standing::Foreign(why) if why.contains(AGENT_LABEL) || why.contains(CREW_LABEL)
        ));
        // the same agent's object of another component
        let token = map(labels(
            "default",
            "token",
            &[(FLEET_LABEL, "f"), (CREW_LABEL, "c"), (AGENT_LABEL, "a")],
        ));
        assert_eq!(
            standing(&id, &token, &[], None),
            Standing::Foreign("its label balerix.ai/component is token".into())
        );
        // a label the identity wants absent
        let mut extra = map(agent_labels("f", "c", "a"));
        extra.insert(PLUGIN_LABEL.into(), "p".into());
        assert!(matches!(
            standing(&id, &extra, &[], None),
            Standing::Foreign(why) if why.contains(PLUGIN_LABEL)
        ));
    }

    #[test]
    fn the_daemon_label_counts_only_where_there_is_no_fleet_or_plugin_label() {
        let daemon_pool = identity_of(&labels("d", "pool", &[]));
        assert_eq!(daemon_pool[DAEMON_LABEL], Some("d".to_string()));
        assert_eq!(daemon_pool[FLEET_LABEL], None);
        let fleet_pool = map(labels("d", "pool", &[(FLEET_LABEL, "f")]));
        assert_eq!(
            standing(&daemon_pool, &fleet_pool, &[], None),
            Standing::Foreign("its label balerix.ai/fleet is f".into())
        );
        let other_daemon = map(labels("e", "pool", &[]));
        assert!(matches!(
            standing(&daemon_pool, &other_daemon, &[], None),
            Standing::Foreign(_)
        ));
        // and the Fleet's identity does not hold the Daemon's label
        let id = identity_of(&labels("d", "pool", &[(FLEET_LABEL, "f")]));
        assert!(!id.contains_key(DAEMON_LABEL));
    }

    #[test]
    fn missing_labels_are_adopted_only_through_the_owners_reference() {
        let id = identity_of(&agent_labels("f", "c", "a"));
        let bare = map(json!({ "app": "x" }));
        assert_eq!(
            standing(&id, &bare, &[owner("agent-uid")], Some("agent-uid")),
            Standing::Unlabelled
        );
        assert!(matches!(
            standing(&id, &bare, &[owner("someone-else")], Some("agent-uid")),
            Standing::Foreign(why) if why.contains("no controller owner reference")
        ));
        assert!(matches!(
            standing(&id, &bare, &[owner("agent-uid")], None),
            Standing::Foreign(_)
        ));
        // a reference that is not the controller's adopts nothing
        let plain = OwnerReference {
            controller: None,
            ..owner("agent-uid")
        };
        assert!(matches!(
            standing(&id, &bare, &[plain], Some("agent-uid")),
            Standing::Foreign(why) if why.contains("no controller owner reference")
        ));
        // some present and agreeing, the rest missing: still adoptable
        let partial = map(json!({ COMPONENT_LABEL: "agent", FLEET_LABEL: "f" }));
        assert_eq!(
            standing(&id, &partial, &[owner("agent-uid")], Some("agent-uid")),
            Standing::Unlabelled
        );
        // one disagreeing is never adopted, whatever the references say
        let wrong = map(json!({ COMPONENT_LABEL: "token" }));
        assert!(matches!(
            standing(&id, &wrong, &[owner("agent-uid")], Some("agent-uid")),
            Standing::Foreign(_)
        ));
    }
}
