//! The plugin side of the host protocol (plugins spec §4.1, §4.2, §7).
//! `Env` reads the four `BALERIX_*` variables the daemon sets through the
//! nono profile (plugins spec §5.1); `host` is the plugin → daemon half,
//! `plugin` the daemon → plugin half, `testing` a fake daemon for plugin
//! unit tests. Nothing here reads the process environment except
//! `Env::from_process`, so plugins stay testable with an injected one.

use std::fmt;
use std::path::PathBuf;

pub mod auth;
pub mod host;
pub mod metrics;
pub mod plugin;
pub mod testing;

pub use host::{Attach, AttachRead, AttachWrite, CloseReason, FleetWatch, Host};
pub use metrics::Metrics;
pub use plugin::{Plugin, bind, router, run, serve};

/// The four `BALERIX_*` variables the daemon sets through the nono profile
/// (plugins spec §5.1).
#[derive(Clone, PartialEq, Eq)]
pub struct Env {
    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub api_url: String,
    pub name: String,
    pub token: String,
    pub scratch: PathBuf,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("api_url", &self.api_url)
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("scratch", &self.scratch)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdkError {
    #[error("environment: {0} is not set")]
    MissingEnv(&'static str),
    #[error("daemon: {0}")]
    Transport(String),
    #[error("daemon: HTTP {status}: {message}")]
    Status { status: u16, message: String },
    #[error("listen: {0}")]
    Bind(String),
    #[error("configure: {0}")]
    Configure(String),
    #[error("metrics: {0}")]
    Metrics(String),
}

impl Env {
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, SdkError> {
        let var = |k: &'static str| {
            get(k)
                .filter(|v| !v.trim().is_empty())
                .ok_or(SdkError::MissingEnv(k))
        };
        Ok(Self {
            api_url: var("BALERIX_API_URL")?
                .trim()
                .trim_end_matches('/')
                .to_string(),
            name: var("BALERIX_PLUGIN_NAME")?,
            token: var("BALERIX_PLUGIN_TOKEN")?,
            scratch: PathBuf::from(var("BALERIX_PLUGIN_SCRATCH")?),
        })
    }

    /// The one place the SDK reads the real process environment.
    pub fn from_process() -> Result<Self, SdkError> {
        Self::from_env(|k| std::env::var(k).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| owned.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
    }

    const FULL: &[(&str, &str)] = &[
        ("BALERIX_API_URL", "http://127.0.0.1:7643/"),
        ("BALERIX_PLUGIN_NAME", "web"),
        ("BALERIX_PLUGIN_TOKEN", "tok-secret"),
        ("BALERIX_PLUGIN_SCRATCH", "/s/plugins/web/scratch"),
    ];

    #[test]
    fn env_reads_the_four_variables_and_names_the_missing_one() {
        let e = Env::from_env(env_of(FULL)).unwrap();
        assert_eq!(e.api_url, "http://127.0.0.1:7643", "trailing slash trimmed");
        assert_eq!(e.name, "web");
        assert_eq!(e.token, "tok-secret");
        assert_eq!(e.scratch, std::path::Path::new("/s/plugins/web/scratch"));
        for missing in [
            "BALERIX_API_URL",
            "BALERIX_PLUGIN_NAME",
            "BALERIX_PLUGIN_TOKEN",
            "BALERIX_PLUGIN_SCRATCH",
        ] {
            let vars: Vec<(&str, &str)> = FULL
                .iter()
                .copied()
                .filter(|(k, _)| *k != missing)
                .collect();
            let err = Env::from_env(env_of(&vars)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("environment: {missing} is not set")
            );
        }
        let dbg = format!("{e:?}");
        assert!(
            dbg.contains("web") && !dbg.contains("tok-secret") && dbg.contains("<redacted>"),
            "{dbg}"
        );
        let host = Host::new(e).unwrap();
        let dbg = format!("{host:?}");
        assert!(!dbg.contains("tok-secret"), "{dbg}");
    }
}
