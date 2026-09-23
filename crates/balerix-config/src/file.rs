//! The on-disk fleet file (spec §5): the three-level form the user writes.

use std::collections::BTreeMap;
use std::path::Path;

use balerix_api::{API_VERSION, GitSettings};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ConfigError;

const KIND: &str = "Fleet";

/// A parsed but unresolved fleet file. Settings layers are raw values.
/// `Serialize` so the file round-trips as the JSON object a plugin sends
/// the daemon (Spec L §3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetFile {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Fleet-level settings layer.
    #[serde(default = "empty_object")]
    pub defaults: Value,
    #[serde(default)]
    pub crews: BTreeMap<String, CrewFile>,
}

/// One crew as written in the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    check_header(&file)?;
    Ok(file)
}

/// The same file as the JSON object a plugin sends the daemon (Spec L
/// §3.1). A shape error names the offending key's path, `file` for the
/// root, so the plugin author sees `crews.repo: missing field `repo``
/// rather than a bare serde message.
pub fn from_value(value: &Value) -> Result<FleetFile, ConfigError> {
    let file: FleetFile = serde_path_to_error::deserialize(value).map_err(|e| {
        let message = e.inner().to_string();
        let mut path = e.path().to_string();
        // `deny_unknown_fields` reports the offending key itself as the
        // path's last segment (`serde_path_to_error` captures it while
        // trying to match it against the struct's fields); the message
        // already names it, so the path we surface is the struct's, one
        // segment shorter. This couples to serde's error wording;
        // `from_value_names_the_offending_key` is the regression guard.
        if message.starts_with("unknown field") {
            path = match path.rfind('.') {
                Some(i) => path[..i].to_string(),
                None => String::new(),
            };
        }
        // No field in this schema is array-shaped, so a `[N]` segment —
        // wherever it appears in the path, not only at the start — means
        // the value there wasn't the object/string/etc. we expected and
        // serde fell back to positional access instead. Truncate at the
        // first `[` so the path we surface is the containing struct's
        // (e.g. `crews.c.git[0]` becomes `crews.c.git`), not a
        // fabricated index.
        if let Some(i) = path.find('[') {
            path.truncate(i);
        }
        ConfigError::Invalid {
            path: if path.is_empty() || path == "." {
                "file".to_string()
            } else {
                path
            },
            message,
        }
    })?;
    check_header(&file)?;
    Ok(file)
}

fn check_header(file: &FleetFile) -> Result<(), ConfigError> {
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
    Ok(())
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

    /// Spec L §3.1: the file crosses the plugin → daemon boundary as JSON;
    /// `from_value` is `parse` for that shape, with the same header checks.
    #[test]
    fn a_file_round_trips_through_json_and_from_value_checks_the_header() {
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: payments\ndefaults:\n  tools: { node: \"22.11.0\" }\ncrews:\n  backend:\n    repo: acme/api\n    ref: develop\n    git: { push: false, auth: none }\n    agents:\n      alice: { branch: feature/x }\n";
        let parsed = parse(yaml).unwrap();
        let v = serde_json::to_value(&parsed).unwrap();
        assert_eq!(v["crews"]["backend"]["ref"], "develop");
        assert_eq!(
            v["crews"]["backend"]["agents"]["alice"]["branch"],
            "feature/x"
        );
        assert_eq!(from_value(&v).unwrap(), parsed);
        let mut no_name = v.clone();
        no_name.as_object_mut().unwrap().remove("name");
        let nameless = from_value(&no_name).unwrap();
        assert_eq!(nameless.name, None);
        assert!(
            serde_json::to_value(&nameless)
                .unwrap()
                .get("name")
                .is_none(),
            "an absent name is not serialized as null"
        );
        let mut bad_kind = v.clone();
        bad_kind["kind"] = json!("Crew");
        assert_eq!(
            from_value(&bad_kind).unwrap_err().to_string(),
            "kind: expected \"Fleet\", got \"Crew\""
        );
        let mut bad_version = v;
        bad_version["apiVersion"] = json!("balerix/v2");
        assert_eq!(
            from_value(&bad_version).unwrap_err().to_string(),
            "apiVersion: expected \"balerix/v1\", got \"balerix/v2\""
        );
    }

    #[test]
    fn from_value_names_the_offending_key() {
        let e = from_value(&json!({
            "apiVersion": "balerix/v1", "kind": "Fleet",
            "crews": { "c": { "repo": "o/r", "nope": 1 } }
        }))
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("crews.c: unknown field `nope`"), "{e}");
        let e = from_value(&json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "extra": 1 }))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("file: unknown field `extra`"), "{e}");
        let e = from_value(&json!([1])).unwrap_err().to_string();
        assert!(e.starts_with("file: "), "{e}");
        let e = from_value(
            &json!({ "apiVersion": "balerix/v1", "kind": "Fleet", "crews": { "c": {} } }),
        )
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("crews.c: missing field `repo`"), "{e}");
        // A `[N]` segment can land mid-path too, not only at the start:
        // a crew (or one of its nested blocks) given an array instead of
        // an object.
        let e = from_value(&json!({
            "apiVersion": "balerix/v1", "kind": "Fleet",
            "crews": { "c": [1] }
        }))
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("crews.c: "), "{e}");
        let e = from_value(&json!({
            "apiVersion": "balerix/v1", "kind": "Fleet",
            "crews": { "c": { "repo": "o/r", "git": [1] } }
        }))
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("crews.c.git: "), "{e}");
    }
}
