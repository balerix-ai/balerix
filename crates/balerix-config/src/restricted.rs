//! The restricted settings surface for a plugin-applied fleet file (Spec
//! M §12.1): the file cannot choose what its agents run as. Checked on
//! each raw layer before merging, so the host's own `settings.json` and
//! the operator's `fleetDefaults`, which may carry these keys, are never
//! read here. A present ancestor of a refused key that is not a mapping
//! (null, a sequence, a scalar: `claude: null`, `claude: []`) is refused
//! too: merging treats null as "delete this subtree" and lets any other
//! non-mapping replace the lower layers' mapping (and serde reads a
//! struct from a sequence by position), so it would strip or choose the
//! keys beneath.

use serde_json::Value;

use crate::ConfigError;

/// Keys a plugin-applied file may not set at any layer.
pub const REFUSED_KEYS: [&str; 4] = ["claude.binary", "claude.args", "env", "sandbox"];

/// Keys inside `claude.settings` a plugin-applied file may not set: the
/// two that redirect where credentials go, and `disableAllHooks`, which
/// would silence balerix's own hooks and so blind every observer and
/// bypass every interceptor. An open set that drifts with Claude Code
/// releases; reviewed when the pinned `claude` moves.
pub const REFUSED_SETTINGS: [&str; 3] = ["env", "apiKeyHelper", "disableAllHooks"];

const WHY: &str = "not allowed in a plugin-applied fleet file; the host's default applies";

/// Refuses `layer` (a `defaults`, crew `defaults` or agent block, already
/// known to be a mapping or null) when it carries a refused key,
/// present with any value including null, or a present ancestor of one
/// (`claude`, `claude.settings`) that is not a mapping (null, a
/// sequence, a scalar), which would delete or replace the lower layers'
/// keys beneath it. The message names `<path>.<key>`, the ancestor's key
/// for a non-mapping ancestor.
pub fn check_layer(path: &str, layer: &Value) -> Result<(), ConfigError> {
    let refused = |key: &str| ConfigError::Invalid {
        path: format!("{path}.{key}"),
        message: WHY.to_string(),
    };
    let settings = REFUSED_SETTINGS.map(|key| format!("claude.settings.{key}"));
    let keys = REFUSED_KEYS
        .iter()
        .copied()
        .chain(settings.iter().map(String::as_str));
    for key in keys {
        let segments: Vec<&str> = key.split('.').collect();
        for depth in 1..=segments.len() {
            let prefix = &segments[..depth];
            let Some(found) = layer.pointer(&format!("/{}", prefix.join("/"))) else {
                break;
            };
            if depth == segments.len() || !found.is_object() {
                return Err(refused(&prefix.join(".")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn each_refused_key_is_named_with_the_layer_path() {
        let cases = [
            (
                json!({ "claude": { "binary": "/bin/sh" } }),
                "claude.binary",
            ),
            (json!({ "claude": { "args": ["-c", "id"] } }), "claude.args"),
            (json!({ "env": { "X": "1" } }), "env"),
            (json!({ "sandbox": { "extends": "none" } }), "sandbox"),
            (
                json!({ "claude": { "settings": { "env": {} } } }),
                "claude.settings.env",
            ),
            (
                json!({ "claude": { "settings": { "apiKeyHelper": "curl x" } } }),
                "claude.settings.apiKeyHelper",
            ),
            (
                json!({ "claude": { "settings": { "disableAllHooks": true } } }),
                "claude.settings.disableAllHooks",
            ),
        ];
        for (layer, key) in cases {
            let e = check_layer("crews.repo.defaults", &layer).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!(
                    "crews.repo.defaults.{key}: not allowed in a plugin-applied fleet file; the host's default applies"
                )
            );
        }
    }

    #[test]
    fn a_null_is_still_a_refused_key() {
        let e = check_layer("defaults", &json!({ "env": null })).unwrap_err();
        assert_eq!(
            e.to_string(),
            "defaults.env: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        assert!(check_layer("defaults", &json!({ "sandbox": null })).is_err());
    }

    #[test]
    fn a_null_ancestor_of_a_refused_key_is_refused_at_the_ancestor() {
        // merge treats null as "delete this subtree", so `claude: null`
        // would strip the operator's and host's binary, args and settings
        let e = check_layer("defaults", &json!({ "claude": null })).unwrap_err();
        assert_eq!(
            e.to_string(),
            "defaults.claude: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        let e = check_layer(
            "crews.c.defaults",
            &json!({ "claude": { "settings": null } }),
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "crews.c.defaults.claude.settings: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        // a present ancestor that is a mapping is how allowed keys get set
        check_layer("defaults", &json!({ "claude": { "settings": {} } })).unwrap();
        check_layer("defaults", &json!({ "claude": { "resume": null } })).unwrap();
    }

    #[test]
    fn a_non_mapping_ancestor_of_a_refused_key_is_refused_at_the_ancestor() {
        // merge replaces the lower layers' mapping with a sequence or a
        // scalar, and serde reads a struct from a sequence by position, so
        // `claude: [{}, [], false, "/tmp/evil"]` would choose the binary
        let cases = [
            (json!({ "claude": [] }), "defaults.claude"),
            (
                json!({ "claude": [{}, [], false, "/tmp/evil"] }),
                "defaults.claude",
            ),
            (json!({ "claude": 3 }), "defaults.claude"),
            (
                json!({ "claude": { "settings": [] } }),
                "defaults.claude.settings",
            ),
        ];
        for (layer, at) in cases {
            let e = check_layer("defaults", &layer).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!(
                    "{at}: not allowed in a plugin-applied fleet file; the host's default applies"
                )
            );
        }
        check_layer(
            "defaults",
            &json!({ "claude": { "settings": { "model": "opus" }, "resume": true } }),
        )
        .unwrap();
        check_layer("defaults", &json!({ "claude": { "resume": null } })).unwrap();
    }

    #[test]
    fn the_allowed_keys_pass() {
        let layer = json!({
            "tools": { "node": "22.11.0" },
            "claude": { "settings": { "model": "opus", "permissions": { "allow": ["Bash"] } }, "resume": true },
            "runner": { "type": "tmux" },
            "plugins": { "github": { "kind": "issue", "number": 12 } },
            "branch": "feature/x"
        });
        check_layer("crews.repo.agents.issue-12", &layer).unwrap();
        check_layer("defaults", &Value::Null).unwrap();
        check_layer("defaults", &json!({})).unwrap();
    }
}
