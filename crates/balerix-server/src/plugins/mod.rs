//! Plugins in the daemon (plugins spec §2.1, §5.2): the declarative file,
//! package install, and the host that drives them as the `balerix` fleet.

pub mod activation;
pub mod chain;
pub mod client;
pub mod config;
pub mod host;
pub mod kv;
pub mod manifest;
pub mod materializer;
pub mod package;
pub mod registry;

use std::path::PathBuf;

pub use chain::{ObserverQueue, PluginEventHandler};
pub use client::{CallFailure, PluginClient};
pub use config::{Source, load_plugins_file, resolve_source};
pub use host::{PluginHost, PluginHostConfig};
pub use kv::{PluginKv, validate_key};
pub use manifest::read_manifest;
pub use materializer::{NullStore, PluginMaterializer};
pub use registry::{ActivationRow, PluginAddr, PluginInfo, PluginRegistry};

/// Every plugin failure, with the config path or file first.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    #[error("plugins.yaml: {path}: {message}")]
    Config { path: String, message: String },
    #[error(transparent)]
    Manifest(#[from] balerix_core::ManifestError),
    #[error("balerix-plugin.yaml: {0}")]
    ManifestParse(String),
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    /// The field is `url`, not `source`: thiserror treats a field named
    /// `source` as the underlying `Error`, which a `String` is not.
    #[error("{url}: {message}")]
    Fetch { url: String, message: String },
    #[error("package: {0}")]
    Package(String),
    #[error("digest mismatch (expected {expected}, got {got})")]
    Digest { expected: String, got: String },
    #[error("plugin {0:?} is still declared in plugins.yaml; remove it first")]
    StillDeclared(String),
    #[error("{0}")]
    Internal(String),
    /// An agent's `plugins.<name>` could not be activated; `path` is the
    /// config path (`crews.c.agents.a.plugins.flow`).
    #[error("{path}: {message}")]
    Activation { path: String, message: String },
    #[error("capability {0:?} not declared in balerix-plugin.yaml")]
    Capability(String),
    #[error("kv: invalid key: {0}")]
    KvKey(String),
    #[error("plugin is not active for agent {0}")]
    NotActive(String),
    #[error("{path}: {message}")]
    Kv { path: PathBuf, message: String },
}

impl PluginError {
    pub(crate) fn io(path: &std::path::Path, e: impl std::fmt::Display) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}

/// The wire label of a serializable enum value (e.g. `Capability::Kv` →
/// `"kv"`): shared by `metrics.rs` (label values) and `PluginError::Capability`.
pub(crate) fn wire_label<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}
