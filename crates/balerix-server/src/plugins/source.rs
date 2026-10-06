//! Where the Daemon's plugins come from (Spec O §23.2): a seam with two
//! adapters. The registry, the chain, the client, the proxy and the
//! `plugin-host` routes sit above it and do not care which.

use std::path::PathBuf;
use std::sync::Arc;

use crate::kube::DeclaredPlugins;
use crate::plugins::{PluginHost, PluginHostConfig};

/// What `Daemon::start` is given.
pub enum PluginSetup {
    /// One machine: `plugins.yaml` and the reserved fleet (today).
    Packages(PluginHostConfig),
    /// Kubernetes mode (Spec O §23.2): the operator's list; hellos persist
    /// under `state_dir` (`<state>/plugins`), managed fleet requests
    /// under `managed_dir` (`<state>/managed`, §23.3).
    Declared {
        state_dir: PathBuf,
        managed_dir: PathBuf,
    },
}

/// What the Daemon holds. A closed enum, not a trait object: two
/// adapters, async methods, no boxing.
pub enum PluginSource {
    Packages(Arc<PluginHost>),
    Declared(Arc<DeclaredPlugins>),
}

impl PluginSource {
    pub fn packages(&self) -> Option<&Arc<PluginHost>> {
        match self {
            Self::Packages(h) => Some(h),
            Self::Declared(_) => None,
        }
    }

    pub fn declared(&self) -> Option<&Arc<DeclaredPlugins>> {
        match self {
            Self::Declared(d) => Some(d),
            Self::Packages(_) => None,
        }
    }
}
