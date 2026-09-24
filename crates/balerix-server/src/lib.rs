//! The daemon (Phase 3 spec §3): a registry of per-fleet actors over the
//! Phase 2 reconciler, a file store with an encrypted secrets vault, the
//! HTTP API, hook ingress and metrics. Depends on `balerix-core` and
//! `balerix-api` only; the binary wires the runtime adapters in.

pub mod actor;
pub mod api;
pub mod attach;
pub mod auth;
pub mod daemon;
pub mod fsutil;
pub mod hooks;
pub mod lifecycle;
pub mod metrics;
pub mod plugin_api;
pub mod plugins;
pub mod proxy;
pub mod sessions;
pub mod store;
pub mod system_pool;
pub mod testing;
pub mod vault;
pub mod watch;

pub use actor::{FleetHandle, Msg, Ports, READY_EVENT, SecretIndex, Shared};
pub use api::{ApiError, router, serve};
pub use attach::{CLOSE_ERROR, CLOSE_NORMAL, CLOSE_UNSUPPORTED, bridge};
pub use auth::{RateLimiter, bearer, constant_time_eq};
pub use daemon::{
    ApplyMode, Caller, Daemon, DaemonError, DaemonHandler, HEALTH_INTERVAL, HelloObserver,
};
pub use hooks::{ParsedEvent, parse_event};
pub use lifecycle::{
    LifecycleError, ServerPaths, load_or_create_token, read_endpoint, read_pid, remove_if_exists,
    write_endpoint, write_pid,
};
pub use metrics::Metrics;
pub use plugins::{
    ActivationRow, ObserverQueue, PluginAddr, PluginClient, PluginError, PluginEventHandler,
    PluginHost, PluginHostConfig, PluginInfo, PluginKv, PluginRegistry,
};
pub use store::FileFleetStore;
pub use system_pool::{SystemPoolConfig, SystemPoolState};
pub use vault::{Vault, VaultError, random_hex};
pub use watch::{PING_INTERVAL, serve_watch};
