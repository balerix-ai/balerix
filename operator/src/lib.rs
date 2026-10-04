//! The balerix operator (Spec O §4, §5). `api` is the five kinds;
//! `desired` the pure functions from observed objects to the objects the
//! operator applies; `pki` the per-Daemon authority; `daemon_client` the
//! Daemon's admin API. `controllers` ties them to a cluster (§21).

pub mod api;
pub mod controllers;
pub mod daemon_client;
pub mod desired;
pub mod pki;
