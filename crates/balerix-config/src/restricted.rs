//! The restricted settings surface for a plugin-applied fleet file (Spec
//! M §12.1): the file cannot choose what its agents run as. Checked on
//! each raw layer before merging, so the host's own `settings.json` and
//! the operator's `fleetDefaults`, which may carry these keys, are never
//! read here.

use serde_json::Value;

use crate::ConfigError;

/// Keys a plugin-applied file may not set at any layer.
pub const REFUSED_KEYS: [&str; 4] = ["claude.binary", "claude.args", "env", "sandbox"];

/// Keys inside `claude.settings` a plugin-applied file may not set: the
/// two that redirect where credentials go. An open set that drifts with
/// Claude Code releases; reviewed when the pinned `claude` moves.
pub const REFUSED_SETTINGS: [&str; 2] = ["env", "apiKeyHelper"];

const WHY: &str = "not allowed in a plugin-applied fleet file; the host's default applies";

/// Refuses `layer` (a `defaults`, crew `defaults` or agent block, already
/// known to be a mapping or null) when it carries a refused key,
/// present with any value including null. The message names
/// `<path>.<key>`.
pub fn check_layer(path: &str, layer: &Value) -> Result<(), ConfigError> {
    let refused = |key: &str| ConfigError::Invalid {
        path: format!("{path}.{key}"),
        message: WHY.to_string(),
    };
    for key in REFUSED_KEYS {
        if layer
            .pointer(&format!("/{}", key.replace('.', "/")))
            .is_some()
        {
            return Err(refused(key));
        }
    }
    for key in REFUSED_SETTINGS {
        if layer.pointer(&format!("/claude/settings/{key}")).is_some() {
            return Err(refused(&format!("claude.settings.{key}")));
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
