//! Host defaults (spec §5): the client's own Claude settings and
//! credentials, used as the bottom layer and the credential bundle.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use balerix_api::CredentialBundle;
use serde_json::{Map, Value};

use crate::ConfigError;

/// Keys lifted from `~/.claude.json` into the credential bundle.
const ACCOUNT_KEYS: &[&str] = &["oauthAccount", "hasCompletedOnboarding"];

/// Where the host keeps Claude and gh state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPaths {
    pub claude_dir: PathBuf,
    pub claude_json: PathBuf,
    pub gh_hosts: PathBuf,
}

impl HostPaths {
    /// Resolves from the real environment.
    pub fn discover() -> Result<Self, ConfigError> {
        let home = std::env::home_dir().ok_or_else(|| ConfigError::Invalid {
            path: "host".to_string(),
            message: "cannot determine the home directory".to_string(),
        })?;
        Ok(Self::from_env(&home, |k| std::env::var_os(k)))
    }

    /// Pure resolution: `CLAUDE_CONFIG_DIR`, `GH_CONFIG_DIR`, `XDG_CONFIG_HOME`
    /// are honoured in that order of specificity, then dotfiles under `home`.
    pub fn from_env(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let claude_dir = env("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let gh_dir = env("GH_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| env("XDG_CONFIG_HOME").map(|x| PathBuf::from(x).join("gh")))
            .unwrap_or_else(|| home.join(".config").join("gh"));
        Self {
            claude_dir,
            claude_json: home.join(".claude.json"),
            gh_hosts: gh_dir.join("hosts.yml"),
        }
    }
}

/// What was found on the host. Absent files are `None`, never errors.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostDefaults {
    pub claude_settings: Option<Value>,
    pub credentials: CredentialBundle,
}

/// Reads host defaults from `paths`.
pub fn load(paths: &HostPaths) -> Result<HostDefaults, ConfigError> {
    let claude_settings = read_json_if_exists(&paths.claude_dir.join("settings.json"))?;
    let claude_credentials = read_json_if_exists(&paths.claude_dir.join(".credentials.json"))?;
    let claude_account = read_json_if_exists(&paths.claude_json)?.map(|v| pick(&v, ACCOUNT_KEYS));
    let gh_token = read_gh_token(&paths.gh_hosts)?;
    Ok(HostDefaults {
        claude_settings,
        credentials: CredentialBundle {
            claude_credentials,
            claude_account,
            gh_token,
        },
    })
}

fn read_if_exists(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn read_json_if_exists(path: &Path) -> Result<Option<Value>, ConfigError> {
    read_if_exists(path)?
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| ConfigError::Invalid {
                path: path.display().to_string(),
                message: format!("invalid JSON: {e}"),
            })
        })
        .transpose()
}

/// `github.com.oauth_token`, else `github.com.users.<user>.oauth_token`.
fn read_gh_token(path: &Path) -> Result<Option<String>, ConfigError> {
    let Some(text) = read_if_exists(path)? else {
        return Ok(None);
    };
    let hosts: Value = serde_norway::from_str(&text).map_err(|e| ConfigError::Invalid {
        path: path.display().to_string(),
        message: format!("invalid YAML: {e}"),
    })?;
    let gh = &hosts["github.com"];
    let direct = gh["oauth_token"].as_str();
    let via_user = gh["user"]
        .as_str()
        .and_then(|u| gh["users"][u]["oauth_token"].as_str());
    Ok(direct.or(via_user).map(str::to_string))
}

fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    if let Value::Object(m) = v {
        for k in keys {
            if let Some(val) = m.get(*k) {
                out.insert((*k).to_string(), val.clone());
            }
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ffi::OsString;
    use std::fs;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn from_env_defaults_to_dotfiles_under_home() {
        let p = HostPaths::from_env(Path::new("/h"), no_env);
        assert_eq!(p.claude_dir, PathBuf::from("/h/.claude"));
        assert_eq!(p.claude_json, PathBuf::from("/h/.claude.json"));
        assert_eq!(p.gh_hosts, PathBuf::from("/h/.config/gh/hosts.yml"));
    }

    #[test]
    fn from_env_honours_claude_config_dir_gh_config_dir_and_xdg() {
        let env = |k: &str| match k {
            "CLAUDE_CONFIG_DIR" => Some(OsString::from("/cc")),
            "GH_CONFIG_DIR" => Some(OsString::from("/ghc")),
            _ => None,
        };
        let p = HostPaths::from_env(Path::new("/h"), env);
        assert_eq!(p.claude_dir, PathBuf::from("/cc"));
        assert_eq!(p.gh_hosts, PathBuf::from("/ghc/hosts.yml"));

        let env = |k: &str| (k == "XDG_CONFIG_HOME").then(|| OsString::from("/xdg"));
        assert_eq!(
            HostPaths::from_env(Path::new("/h"), env).gh_hosts,
            PathBuf::from("/xdg/gh/hosts.yml")
        );
    }

    fn paths_in(dir: &Path) -> HostPaths {
        HostPaths {
            claude_dir: dir.join(".claude"),
            claude_json: dir.join(".claude.json"),
            gh_hosts: dir.join("gh/hosts.yml"),
        }
    }

    #[test]
    fn missing_files_yield_none_everywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let d = load(&paths_in(tmp.path())).unwrap();
        assert_eq!(d.claude_settings, None);
        assert_eq!(d.credentials, balerix_api::CredentialBundle::default());
    }

    #[test]
    fn loads_settings_credentials_account_fields_and_gh_token() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths_in(tmp.path());
        fs::create_dir_all(&p.claude_dir).unwrap();
        fs::create_dir_all(p.gh_hosts.parent().unwrap()).unwrap();
        fs::write(
            p.claude_dir.join("settings.json"),
            r#"{"model":"haiku","theme":"dark"}"#,
        )
        .unwrap();
        fs::write(
            p.claude_dir.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"fake-token"}}"#,
        )
        .unwrap();
        fs::write(&p.claude_json, r#"{"oauthAccount":{"emailAddress":"x@y.z"},"hasCompletedOnboarding":true,"numStartups":42}"#).unwrap();
        fs::write(
            &p.gh_hosts,
            "github.com:\n    oauth_token: gho_fake\n    user: someone\n",
        )
        .unwrap();

        let d = load(&p).unwrap();
        assert_eq!(
            d.claude_settings,
            Some(json!({"model": "haiku", "theme": "dark"}))
        );
        assert_eq!(
            d.credentials.claude_credentials,
            Some(json!({"claudeAiOauth": {"accessToken": "fake-token"}}))
        );
        assert_eq!(
            d.credentials.claude_account,
            Some(
                json!({"oauthAccount": {"emailAddress": "x@y.z"}, "hasCompletedOnboarding": true})
            )
        );
        assert_eq!(d.credentials.gh_token.as_deref(), Some("gho_fake"));
    }

    #[test]
    fn gh_token_falls_back_to_the_users_map() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths_in(tmp.path());
        fs::create_dir_all(p.gh_hosts.parent().unwrap()).unwrap();
        fs::write(&p.gh_hosts, "github.com:\n    users:\n        someone:\n            oauth_token: gho_user\n    user: someone\n").unwrap();
        assert_eq!(
            load(&p).unwrap().credentials.gh_token.as_deref(),
            Some("gho_user")
        );
    }

    #[test]
    fn malformed_json_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths_in(tmp.path());
        fs::create_dir_all(&p.claude_dir).unwrap();
        fs::write(p.claude_dir.join("settings.json"), "{not json").unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("settings.json"), "{err}");
    }
}
