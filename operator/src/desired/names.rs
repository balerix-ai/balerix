//! Every name the operator gives an object, and every directory on the
//! shared volume (Spec O §8.1). One place, so a controller that looks an
//! object up and the function that made it cannot disagree.

use super::common::DAEMON_PORT;

pub fn daemon(daemon: &str) -> String {
    format!("balerix-{daemon}")
}
pub fn state_claim(daemon: &str) -> String {
    format!("balerix-{daemon}-state")
}
pub fn shared_claim(daemon: &str) -> String {
    format!("balerix-{daemon}-shared")
}
/// The authority's Secret (`ca.crt`, `ca.key`) and its ConfigMap (`ca.crt`).
pub fn authority(daemon: &str) -> String {
    format!("balerix-{daemon}-ca")
}
pub fn serving(daemon: &str) -> String {
    format!("balerix-{daemon}-tls")
}
pub fn admin(daemon: &str) -> String {
    format!("balerix-{daemon}-admin")
}
pub fn daemon_pool_job(daemon: &str) -> String {
    format!("balerix-{daemon}-pool")
}
/// `status.endpoint`, and the `daemon_url` of every agent bundle.
pub fn endpoint(namespace: &str, daemon_name: &str) -> String {
    format!(
        "https://{}.{namespace}.svc:{DAEMON_PORT}",
        daemon(daemon_name)
    )
}
pub fn fleet_pool_job(fleet: &str) -> String {
    format!("{fleet}-pool")
}
pub fn crew(fleet: &str, crew: &str) -> String {
    format!("{fleet}-{crew}")
}
pub fn sync_job(fleet: &str, crew: &str) -> String {
    format!("{fleet}-{crew}-sync")
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
    format!("{agent}-harvest")
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
