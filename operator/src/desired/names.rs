//! Every name the operator gives an object, and every directory on the
//! shared volume (Spec O §8.1). One place, so a controller that looks an
//! object up and the function that made it cannot disagree.

use super::common::{DAEMON_PORT, hash};

/// The longest name Kubernetes copies into a label value: a Job's name
/// goes into its pods' `job-name` label.
const LABEL_MAX: usize = 63;

/// `base` + `suffix` (the suffix begins with `-`) as a Job name. Within 63
/// characters it is that; beyond, `base` is cut and an 8-hex hash of the
/// whole `base` keeps two long bases apart: `<cut>-<hash><suffix>`.
fn job_name(base: &str, suffix: &str) -> String {
    if base.len() + suffix.len() <= LABEL_MAX {
        return format!("{base}{suffix}");
    }
    let digest = hash(&serde_json::Value::String(base.to_string()));
    let room = LABEL_MAX - suffix.len() - 1 - 8;
    // names are DNS labels, so ASCII: a byte cut is a character cut
    let cut = base[..room.min(base.len())].trim_end_matches('-');
    format!("{cut}-{}{suffix}", &digest[..8])
}

/// `balerix-<daemon><suffix>`, bounded as a Job name is: a Daemon's name
/// reaches labels (`balerix.ai/daemon`) and pod names, so it may not make
/// a value over 63 characters (§21.5).
fn daemon_derived(daemon: &str, suffix: &str) -> String {
    job_name(&format!("balerix-{daemon}"), suffix)
}

pub fn daemon(daemon: &str) -> String {
    daemon_derived(daemon, "")
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
    fn the_remove_job_is_named_and_bounded() {
        assert_eq!(remove_job("f", "c"), "f-c-remove");
        let l = "e".repeat(63);
        assert!(remove_job(&l, &l).len() <= 63);
        assert!(remove_job(&l, &l).ends_with("-remove"));
    }
}
