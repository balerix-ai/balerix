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
pub mod tls;

pub use host::{Attach, AttachRead, AttachWrite, CloseReason, FleetWatch, Host};
pub use metrics::Metrics;
pub use plugin::{Plugin, bind, bind_to, parse_manifest, router, run, serve};

/// The four `BALERIX_*` variables the daemon sets through the nono profile
/// (plugins spec §5.1).
#[derive(Clone, PartialEq, Eq)]
pub struct Env {
    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub api_url: String,
    pub name: String,
    pub token: String,
    pub scratch: PathBuf,
    /// `BALERIX_CA_FILE` (Spec O §23.1): the one authority the client
    /// trusts; `api_url` must then be `https://`.
    pub ca: Option<PathBuf>,
    /// `BALERIX_PLUGIN_TLS_CERT` / `_KEY` (Spec O §23.1): the certificate
    /// and key a plugin in a pod serves with.
    pub tls: Option<(PathBuf, PathBuf)>,
    /// `BALERIX_PLUGIN_LISTEN` (Spec O §23.1): `127.0.0.1:0` on one machine,
    /// `0.0.0.0:7644` in a pod.
    pub listen: String,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("api_url", &self.api_url)
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("scratch", &self.scratch)
            .field("ca", &self.ca)
            .field("tls", &self.tls)
            .field("listen", &self.listen)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdkError {
    #[error("environment: {0} is not set")]
    MissingEnv(&'static str),
    #[error("environment: {0}")]
    Env(&'static str),
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
        let opt = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let var = |k: &'static str| opt(k).ok_or(SdkError::MissingEnv(k));
        // Spec O §23.1: a pod mounts the token; the file wins
        let token = match opt("BALERIX_PLUGIN_TOKEN_FILE") {
            Some(path) => std::fs::read_to_string(&path)
                .map_err(|e| SdkError::Transport(format!("{path}: {e}")))?
                .trim()
                .to_string(),
            None => var("BALERIX_PLUGIN_TOKEN")?,
        };
        let api_url = var("BALERIX_API_URL")?.trim_end_matches('/').to_string();
        let ca = opt("BALERIX_CA_FILE").map(PathBuf::from);
        if ca.is_some() && !api_url.starts_with("https://") {
            return Err(SdkError::Env(
                "BALERIX_CA_FILE is set, so BALERIX_API_URL must be https://",
            ));
        }
        refuse_https_without_ca(&api_url, ca.as_deref())?;
        let tls = match (
            opt("BALERIX_PLUGIN_TLS_CERT"),
            opt("BALERIX_PLUGIN_TLS_KEY"),
        ) {
            (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
            (None, None) => None,
            _ => {
                return Err(SdkError::Env(
                    "BALERIX_PLUGIN_TLS_CERT and BALERIX_PLUGIN_TLS_KEY go together",
                ));
            }
        };
        Ok(Self {
            api_url,
            name: var("BALERIX_PLUGIN_NAME")?,
            token,
            scratch: PathBuf::from(var("BALERIX_PLUGIN_SCRATCH")?),
            ca,
            tls,
            listen: opt("BALERIX_PLUGIN_LISTEN").unwrap_or_else(|| "127.0.0.1:0".into()),
        })
    }

    /// The one place the SDK reads the real process environment.
    pub fn from_process() -> Result<Self, SdkError> {
        Self::from_env(|k| std::env::var(k).ok())
    }
}

/// Spec O §23.1: without the authority file, reqwest would verify
/// `https://` against the system's roots and tungstenite `wss://` against
/// webpki's bundle; the daemon's one authority is the only trust allowed.
pub(crate) fn refuse_https_without_ca(
    api_url: &str,
    ca: Option<&std::path::Path>,
) -> Result<(), SdkError> {
    if ca.is_none() && api_url.starts_with("https://") {
        return Err(SdkError::Env(
            "BALERIX_API_URL is https://, so BALERIX_CA_FILE must be set",
        ));
    }
    Ok(())
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

    #[test]
    fn env_reads_the_file_inputs_and_holds_their_rules() {
        let dir = tempfile::tempdir().unwrap();
        let tok = dir.path().join("token");
        std::fs::write(&tok, "tok-from-file\n").unwrap();
        let ca = dir.path().join("ca.crt");
        let mut vars: Vec<(&'static str, String)> = vec![
            ("BALERIX_API_URL", "https://balerix.ns.svc:7643".into()),
            ("BALERIX_PLUGIN_NAME", "flow".into()),
            ("BALERIX_PLUGIN_TOKEN_FILE", tok.display().to_string()),
            ("BALERIX_PLUGIN_SCRATCH", "/scratch".into()),
            ("BALERIX_CA_FILE", ca.display().to_string()),
            ("BALERIX_PLUGIN_TLS_CERT", "/tls/tls.crt".into()),
            ("BALERIX_PLUGIN_TLS_KEY", "/tls/tls.key".into()),
            ("BALERIX_PLUGIN_LISTEN", "0.0.0.0:7644".into()),
        ];
        fn get(vars: &[(&'static str, String)]) -> impl Fn(&str) -> Option<String> + use<> {
            let vars = vars.to_vec();
            move |k: &str| vars.iter().find(|(kk, _)| *kk == k).map(|(_, v)| v.clone())
        }
        let e = Env::from_env(get(&vars)).unwrap();
        assert_eq!(e.token, "tok-from-file", "file wins, trimmed");
        assert_eq!(e.ca.as_deref(), Some(ca.as_path()));
        assert_eq!(e.listen, "0.0.0.0:7644");
        assert!(e.tls.is_some());
        // a CA with a plain-http daemon url is refused
        vars[0].1 = "http://balerix:7643".into();
        assert_eq!(
            Env::from_env(get(&vars)).unwrap_err().to_string(),
            "environment: BALERIX_CA_FILE is set, so BALERIX_API_URL must be https://"
        );
        vars[0].1 = "https://balerix.ns.svc:7643".into();
        // an https daemon url without the authority is refused: no public
        // roots stand in for it (Spec O §23.1)
        let without_ca: Vec<_> = vars
            .iter()
            .filter(|(k, _)| *k != "BALERIX_CA_FILE")
            .cloned()
            .collect();
        assert_eq!(
            Env::from_env(get(&without_ca)).unwrap_err(),
            SdkError::Env("BALERIX_API_URL is https://, so BALERIX_CA_FILE must be set")
        );
        // a certificate without its key is refused
        vars.retain(|(k, _)| *k != "BALERIX_PLUGIN_TLS_KEY");
        assert_eq!(
            Env::from_env(get(&vars)).unwrap_err().to_string(),
            "environment: BALERIX_PLUGIN_TLS_CERT and BALERIX_PLUGIN_TLS_KEY go together"
        );
    }

    #[test]
    fn env_without_the_new_variables_is_todays_env() {
        let e = Env::from_env(env_of(FULL)).unwrap();
        assert_eq!(
            (e.ca, e.tls, e.listen.as_str()),
            (None, None, "127.0.0.1:0")
        );
    }
}

/// A throwaway authority for TLS tests (Spec O §23.1); host.rs and the
/// plugin tests share it.
#[cfg(test)]
pub(crate) mod test_tls {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::path::{Path, PathBuf};

    /// (ca.crt, tls.crt, tls.key) for 127.0.0.1 in `dir`.
    pub(crate) fn authority(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca, ca_key);
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        let paths = (dir.join("ca.crt"), dir.join("tls.crt"), dir.join("tls.key"));
        std::fs::write(&paths.0, ca_cert.pem()).unwrap();
        std::fs::write(&paths.1, leaf.pem()).unwrap();
        std::fs::write(&paths.2, key.serialize_pem()).unwrap();
        paths
    }
}
