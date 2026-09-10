//! The on-disk fleet file (spec §5): the three-level form the user writes.

use std::collections::BTreeMap;
use std::path::Path;

use balerix_api::{API_VERSION, GitSettings};
use serde::Deserialize;
use serde_json::Value;

use crate::ConfigError;

const KIND: &str = "Fleet";

/// A parsed but unresolved fleet file. Settings layers are raw values.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetFile {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Fleet-level settings layer.
    #[serde(default = "empty_object")]
    pub defaults: Value,
    #[serde(default)]
    pub crews: BTreeMap<String, CrewFile>,
}

/// One crew as written in the file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrewFile {
    pub repo: String,
    #[serde(rename = "ref", default = "default_ref")]
    pub git_ref: String,
    #[serde(default)]
    pub git: GitSettings,
    /// Crew-level settings layer.
    #[serde(default = "empty_object")]
    pub defaults: Value,
    /// Agent name → agent-level settings layer.
    #[serde(default)]
    pub agents: BTreeMap<String, Value>,
}

/// Parses YAML text and checks `apiVersion` / `kind`.
pub fn parse(yaml: &str) -> Result<FleetFile, ConfigError> {
    let file: FleetFile = serde_norway::from_str(yaml)?;
    if file.api_version != API_VERSION {
        return Err(ConfigError::Invalid {
            path: "apiVersion".to_string(),
            message: format!("expected {API_VERSION:?}, got {:?}", file.api_version),
        });
    }
    if file.kind != KIND {
        return Err(ConfigError::Invalid {
            path: "kind".to_string(),
            message: format!("expected {KIND:?}, got {:?}", file.kind),
        });
    }
    Ok(file)
}

/// Reads and parses a fleet file from disk.
pub fn read(path: &Path) -> Result<FleetFile, ConfigError> {
    let yaml = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&yaml)
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn default_ref() -> String {
    "main".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    const MINIMAL: &str =
        "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  backend:\n    repo: acme/api\n";

    #[test]
    fn parses_minimal_file_with_defaults() {
        let f = parse(MINIMAL).unwrap();
        assert_eq!(f.api_version, "balerix/v1");
        assert_eq!(f.kind, "Fleet");
        assert_eq!(f.name, None);
        assert_eq!(f.defaults, json!({}));
        let crew = &f.crews["backend"];
        assert_eq!(crew.repo, "acme/api");
        assert_eq!(crew.git_ref, "main");
        assert!(crew.git.push);
        assert_eq!(crew.defaults, json!({}));
        assert!(crew.agents.is_empty());
    }

    #[test]
    fn keeps_settings_layers_as_raw_values() {
        let yaml = r#"
apiVersion: balerix/v1
kind: Fleet
name: payments
defaults:
  tools: { node: "22.11.0" }
crews:
  backend:
    repo: acme/api
    ref: develop
    git: { push: false, auth: none }
    defaults:
      tools: { python: "3.12.8" }
    agents:
      alice: {}
      bob:
        claude: { settings: { model: opus } }
        tools: { node: null }
"#;
        let f = parse(yaml).unwrap();
        assert_eq!(f.name.as_deref(), Some("payments"));
        assert_eq!(f.defaults, json!({"tools": {"node": "22.11.0"}}));
        let crew = &f.crews["backend"];
        assert_eq!(crew.git_ref, "develop");
        assert!(!crew.git.push);
        assert_eq!(crew.defaults, json!({"tools": {"python": "3.12.8"}}));
        assert_eq!(crew.agents["alice"], json!({}));
        assert_eq!(
            crew.agents["bob"],
            json!({"claude": {"settings": {"model": "opus"}}, "tools": {"node": null}})
        );
    }

    #[test]
    fn rejects_wrong_api_version_and_kind() {
        let err = parse("apiVersion: balerix/v2\nkind: Fleet\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "apiVersion: expected \"balerix/v1\", got \"balerix/v2\""
        );
        let err = parse("apiVersion: balerix/v1\nkind: Crew\n").unwrap_err();
        assert_eq!(err.to_string(), "kind: expected \"Fleet\", got \"Crew\"");
    }

    #[test]
    fn rejects_unknown_keys_on_balerix_owned_structs() {
        let err =
            parse("apiVersion: balerix/v1\nkind: Fleet\ncrew:\n  backend:\n    repo: acme/api\n")
                .unwrap_err();
        assert!(err.to_string().contains("unknown field `crew`"), "{err}");
        let err = parse("apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  backend:\n    repo: acme/api\n    agent: {}\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `agent`"), "{err}");
    }

    #[test]
    fn requires_repo_per_crew() {
        let err =
            parse("apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  backend: {}\n").unwrap_err();
        assert!(err.to_string().contains("missing field `repo`"), "{err}");
    }

    #[test]
    fn read_reports_the_path_on_io_error() {
        let err = read(std::path::Path::new("/definitely/not/here.yaml")).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("failed to read /definitely/not/here.yaml"),
            "{err}"
        );
    }
}
