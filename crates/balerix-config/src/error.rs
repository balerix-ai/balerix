//! Errors from parsing, merging and resolving fleet configuration.

use std::path::PathBuf;

/// Anything that can go wrong before a spec reaches the server. `Invalid`
/// and `Fleet` messages start with the config path of the offending value.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_norway::Error),
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
    #[error(transparent)]
    Fleet(#[from] balerix_core::FleetError),
}
