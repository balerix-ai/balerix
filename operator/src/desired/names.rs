//! Every name the operator gives an object, and every directory on the
//! shared volume (Spec O §8.1). One place, so a controller that looks an
//! object up and the function that made it cannot disagree.

use super::common::{DAEMON_PORT, hash};

/// The longest name Kubernetes copies into a label value: a Job's name
/// goes into its pods' `job-name` label.
const LABEL_MAX: usize = 63;
/// The longest StatefulSet name: its pods carry
/// `controller-revision-hash: <name>-<10 characters>`, a label value.
const STATEFULSET_MAX: usize = LABEL_MAX - 11;

/// `base` + `suffix` (the suffix begins with `-`, or is empty) within
/// `max` characters: unchanged when it fits; beyond, `base` is cut and an
/// 8-hex hash of the whole `base` keeps two long bases apart:
/// `<cut>-<hash><suffix>`.
fn bounded(base: &str, suffix: &str, max: usize) -> String {
    if base.len() + suffix.len() <= max {
        return format!("{base}{suffix}");
    }
    let digest = hash(&serde_json::Value::String(base.to_string()));
    let room = max - suffix.len() - 1 - 8;
    // names are DNS labels, so ASCII: a byte cut is a character cut
    let cut = base[..room.min(base.len())].trim_end_matches('-');
    format!("{cut}-{}{suffix}", &digest[..8])
}

/// `base` + `suffix` as a Job name, bounded to a label value (§20.5).
fn job_name(base: &str, suffix: &str) -> String {
    bounded(base, suffix, LABEL_MAX)
}

/// `balerix-<daemon><suffix>`, bounded as a Job name is: a Daemon's name
/// reaches labels and pod names, so it may not make a value over 63
/// characters (§21.5).
fn daemon_derived(daemon: &str, suffix: &str) -> String {
    job_name(&format!("balerix-{daemon}"), suffix)
}

/// The `balerix.ai/daemon` label value of every object a Daemon's
/// children carry, and of every selector that finds them: the Daemon's
/// name, bounded as a Job name is when it is over 63 characters.
pub fn daemon_label(daemon: &str) -> String {
    job_name(daemon, "")
}

/// The StatefulSet and its Service: bounded to 52 characters, so its
/// pods' `controller-revision-hash` fits a label value.
pub fn daemon(daemon: &str) -> String {
    bounded(&format!("balerix-{daemon}"), "", STATEFULSET_MAX)
}
pub fn state_claim(daemon: &str) -> String {
    daemon_derived(daemon, "-state")
}
pub fn shared_claim(daemon: &str) -> String {
    daemon_derived(daemon, "-shared")
}
/// The authority's Secret (`ca.crt`, `ca.key`) and its ConfigMap (`ca.crt`).
pub fn authority(daemon: &str) -> String {
    daemon_derived(daemon, "-ca")
}
pub fn serving(daemon: &str) -> String {
    daemon_derived(daemon, "-tls")
}
pub fn admin(daemon: &str) -> String {
    daemon_derived(daemon, "-admin")
}
pub fn daemon_pool_job(daemon: &str) -> String {
    daemon_derived(daemon, "-pool")
}
/// `status.endpoint`, and the `daemon_url` of every agent bundle.
pub fn endpoint(namespace: &str, daemon_name: &str) -> String {
    format!(
        "https://{}.{namespace}.svc:{DAEMON_PORT}",
        daemon(daemon_name)
    )
}

/// A plugin's Deployment and NetworkPolicy (§23.4); its Service is the
/// plugin's own name, the host of its url.
pub fn plugin(plugin: &str) -> String {
    job_name(&format!("balerix-plugin-{plugin}"), "")
}
pub fn plugin_token(plugin: &str) -> String {
    job_name(&format!("balerix-plugin-{plugin}"), "-token")
}
pub fn plugin_serving(plugin: &str) -> String {
    job_name(&format!("balerix-plugin-{plugin}"), "-tls")
}
pub fn plugin_scratch(plugin: &str) -> String {
    job_name(&format!("balerix-plugin-{plugin}"), "-scratch")
}
pub fn plugin_url(namespace: &str, plugin: &str) -> String {
    format!(
        "https://{plugin}.{namespace}.svc:{}",
        crate::desired::plugin::PLUGIN_PORT
    )
}

pub fn fleet_pool_job(fleet: &str) -> String {
    job_name(fleet, "-pool")
}
pub fn crew(fleet: &str, crew: &str) -> String {
    format!("{fleet}-{crew}")
}
pub fn sync_job(fleet: &str, crew: &str) -> String {
    job_name(&format!("{fleet}-{crew}"), "-sync")
}
/// The Agent object, its claim, its pod and its NetworkPolicy.
pub fn agent(fleet: &str, crew: &str, agent: &str) -> String {
    format!("{fleet}-{crew}-{agent}")
}
pub fn bundle(agent: &str) -> String {
    format!("{agent}-bundle")
}
pub fn token(agent: &str) -> String {
    format!("{agent}-token")
}
pub fn harvest_job(agent: &str) -> String {
    job_name(agent, "-harvest")
}
/// The cleanup Job of a Fleet deleted with `retain: None` (§21.2).
pub fn remove_job(fleet: &str, crew: &str) -> String {
    job_name(&format!("{fleet}-{crew}"), "-remove")
}

pub fn vol_daemon_pool() -> String {
    "pools/daemon".to_string()
}
pub fn vol_fleet_pool(fleet: &str) -> String {
    format!("fleets/{fleet}/pool")
}
pub fn vol_crew_pool(fleet: &str, crew: &str) -> String {
    format!("fleets/{fleet}/crews/{crew}/pool")
}
pub fn vol_crew_repo(fleet: &str, crew: &str) -> String {
    format!("fleets/{fleet}/crews/{crew}/repo")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_job_name_is_bounded_to_a_label_and_unchanged_when_it_fits() {
        // exactly 63: unchanged
        let fits = "a".repeat(63 - "-harvest".len());
        assert_eq!(harvest_job(&fits), format!("{fits}-harvest"));
        assert_eq!(harvest_job(&fits).len(), 63);

        // one more: bounded, ends with the suffix, stable, distinct
        let long = "a".repeat(63 - "-harvest".len() + 1);
        let name = harvest_job(&long);
        assert!(name.len() <= 63 && name.ends_with("-harvest"), "{name}");
        assert_eq!(name, harvest_job(&long));
        let shared = "b".repeat(62);
        assert_ne!(
            harvest_job(&format!("{shared}x")),
            harvest_job(&format!("{shared}y"))
        );

        // a cut that lands after a `-` does not double it
        let dashed = format!(
            "{}-{}",
            "c".repeat(63 - "-harvest".len() - 1 - 8 - 1),
            "d".repeat(20)
        );
        let name = harvest_job(&dashed);
        assert!(!name.contains("--") && name.len() <= 63, "{name}");

        // the other Job names share the bound
        let l = "e".repeat(63);
        for n in [fleet_pool_job(&l), sync_job(&l, &l), daemon_pool_job(&l)] {
            assert!(n.len() <= 63, "{n}");
            assert!(n.starts_with(|c: char| c.is_ascii_alphanumeric()), "{n}");
        }
        assert!(sync_job(&l, &l).ends_with("-sync"));
        assert!(fleet_pool_job(&l).ends_with("-pool"));
        assert!(daemon_pool_job(&l).ends_with("-pool"));
    }

    #[test]
    fn daemon_derived_names_are_bounded_like_job_names() {
        let long = "d".repeat(80);
        for n in [
            daemon(&long),
            state_claim(&long),
            shared_claim(&long),
            authority(&long),
            serving(&long),
            admin(&long),
            daemon_pool_job(&long),
        ] {
            assert!(n.len() <= 63, "{n}");
            assert!(n.starts_with("balerix-"), "{n}");
        }
        assert!(state_claim(&long).ends_with("-state"));
        assert_ne!(state_claim(&long), shared_claim(&long));
        // a short name is unchanged
        assert_eq!(daemon("default"), "balerix-default");
        assert_eq!(admin("default"), "balerix-default-admin");
    }

    #[test]
    fn the_statefulset_name_leaves_room_for_its_revision_hash() {
        // 52 fits: `<name>-<10 characters>` is a 63-character label value
        let fits = "s".repeat(52 - "balerix-".len());
        assert_eq!(daemon(&fits), format!("balerix-{fits}"));
        let over = "s".repeat(52 - "balerix-".len() + 1);
        let name = daemon(&over);
        assert!(name.len() <= 52 && name.starts_with("balerix-"), "{name}");
        assert_eq!(format!("{name}-0123456789").len(), 63);
        assert_ne!(daemon(&over), daemon(&format!("{over}x")));
        // the endpoint follows the Service's name
        assert!(endpoint("ns", &over).starts_with(&format!("https://{name}.ns.svc:")));
    }

    #[test]
    fn the_daemon_label_is_the_name_within_a_label_value() {
        assert_eq!(daemon_label("default"), "default");
        let fits = "l".repeat(63);
        assert_eq!(daemon_label(&fits), fits);
        let long = "l".repeat(64);
        let label = daemon_label(&long);
        assert!(label.len() <= 63, "{label}");
        assert!(
            label.ends_with(|c: char| c.is_ascii_alphanumeric()),
            "{label}"
        );
        assert_eq!(label, daemon_label(&long));
        assert_ne!(label, daemon_label(&format!("{long}x")));
    }

    #[test]
    fn the_remove_job_is_named_and_bounded() {
        assert_eq!(remove_job("f", "c"), "f-c-remove");
        let l = "e".repeat(63);
        assert!(remove_job(&l, &l).len() <= 63);
        assert!(remove_job(&l, &l).ends_with("-remove"));
    }
}
