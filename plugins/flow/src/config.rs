//! The `plugins.flow` block (plugins spec §8.1, §17.2): typed, validated
//! with config-path errors, compiled into anchored regexes. Pure.

use std::collections::BTreeMap;

use balerix_api::HOOK_EVENTS;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// `RegexBuilder::size_limit` for every `match` pattern (§8.1).
pub const REGEX_SIZE_LIMIT: usize = 10 * 1024;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowConfig {
    pub initial: String,
    pub states: BTreeMap<String, StateConfig>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StateConfig {
    pub on: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub event: String,
    /// JSON pointer into the event payload → full-match regex.
    #[serde(default, rename = "match")]
    pub matches: BTreeMap<String, String>,
    #[serde(default)]
    pub goto: Option<String>,
    /// Top-level keys replace those of the chain's response so far.
    #[serde(default)]
    pub respond: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub send: Option<Send>,
    #[serde(default)]
    pub action: Option<RuleAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Send {
    pub text: String,
    #[serde(default = "default_true")]
    pub submit: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Stop,
    Restart,
}

/// One line, config path first: `states.working.on[1].goto: no state
/// "foo" declared`. An empty path (a top-level serde error) prints the
/// message alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl ConfigError {
    fn at(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// A config with its regexes compiled and its states checked.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub initial: String,
    pub states: BTreeMap<String, Vec<CompiledRule>>,
    /// `config_hash` of the block this was compiled from.
    pub hash: String,
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub event: String,
    pub matches: Vec<(String, Regex)>,
    pub goto: Option<String>,
    pub respond: serde_json::Map<String, Value>,
    pub send: Option<Send>,
    pub action: Option<RuleAction>,
}

/// Typed parse with the path of the failing element. serde's own
/// messages are trimmed to their first clause (`unknown field `foo``,
/// not `…, expected one of …`).
pub fn parse(config: &Value) -> Result<FlowConfig, ConfigError> {
    serde_path_to_error::deserialize(config.clone()).map_err(|e| {
        let path = match e.path().to_string() {
            p if p == "." => String::new(),
            p => p,
        };
        let inner = e.into_inner().to_string();
        let message = inner
            .split(", expected one of")
            .next()
            .unwrap_or(&inner)
            .split(", expected `")
            .next()
            .unwrap_or(&inner)
            .to_string();
        ConfigError { path, message }
    })
}

/// sha256 of the canonical JSON text (serde_json's `Value` keeps its
/// object keys sorted, so key order in the source does not matter).
pub fn config_hash(config: &Value) -> String {
    hex::encode(Sha256::digest(config.to_string().as_bytes()))
}

/// `regex` syntax errors span several lines (the pattern, a caret, then
/// `error: …`); a config error is one line, so keep the last line.
fn one_line(e: regex::Error) -> String {
    match e {
        regex::Error::Syntax(s) => s
            .lines()
            .last()
            .unwrap_or("")
            .trim_start_matches("error: ")
            .to_string(),
        other => other.to_string(),
    }
}

/// Parse, validate every reference and event, compile every regex.
pub fn compile(config: &Value) -> Result<Compiled, ConfigError> {
    let parsed = parse(config)?;
    if !parsed.states.contains_key(&parsed.initial) {
        return Err(ConfigError::at(
            "initial",
            format!("no state {:?} declared", parsed.initial),
        ));
    }
    let mut states = BTreeMap::new();
    for (name, state) in &parsed.states {
        let mut rules = Vec::with_capacity(state.on.len());
        for (i, rule) in state.on.iter().enumerate() {
            let at = |field: &str| format!("states.{name}.on[{i}].{field}");
            if !HOOK_EVENTS.contains(&rule.event.as_str()) {
                return Err(ConfigError::at(
                    at("event"),
                    format!("unknown event {:?}", rule.event),
                ));
            }
            if let Some(goto) = &rule.goto
                && !parsed.states.contains_key(goto)
            {
                return Err(ConfigError::at(
                    at("goto"),
                    format!("no state {goto:?} declared"),
                ));
            }
            let mut matches = Vec::with_capacity(rule.matches.len());
            for (pointer, pattern) in &rule.matches {
                let path = at(&format!("match.{pointer}"));
                if !pointer.starts_with('/') {
                    return Err(ConfigError::at(
                        path,
                        "not a JSON pointer (must start with \"/\")",
                    ));
                }
                let re = RegexBuilder::new(&format!("^(?:{pattern})$"))
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .map_err(|e| ConfigError::at(path, one_line(e)))?;
                matches.push((pointer.clone(), re));
            }
            rules.push(CompiledRule {
                event: rule.event.clone(),
                matches,
                goto: rule.goto.clone(),
                respond: rule.respond.clone().unwrap_or_default(),
                send: rule.send.clone(),
                action: rule.action,
            });
        }
        states.insert(name.clone(), rules);
    }
    Ok(Compiled {
        initial: parsed.initial,
        states,
        hash: config_hash(config),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example() -> Value {
        json!({
            "initial": "working",
            "states": {
                "working": { "on": [
                    { "event": "PreToolUse",
                      "match": { "/tool_input/command": "rm -rf.*" },
                      "respond": { "decision": "block", "reason": "no recursive deletes" } },
                    { "event": "Stop", "goto": "review",
                      "send": { "text": "Run the tests and fix any failures.", "submit": true } }
                ] },
                "review": { "on": [ { "event": "Stop", "goto": "done", "action": "stop" } ] },
                "done": {}
            }
        })
    }

    #[test]
    fn the_spec_example_parses_and_compiles() {
        let c = parse(&example()).unwrap();
        assert_eq!(c.initial, "working");
        assert_eq!(c.states.len(), 3);
        let r = &c.states["working"].on[0];
        assert_eq!(r.event, "PreToolUse");
        assert_eq!(r.matches["/tool_input/command"], "rm -rf.*");
        assert_eq!(r.respond.as_ref().unwrap()["decision"], "block");
        assert!(r.goto.is_none() && r.send.is_none() && r.action.is_none());
        let s = c.states["working"].on[1].send.as_ref().unwrap();
        assert_eq!(
            (s.text.as_str(), s.submit),
            ("Run the tests and fix any failures.", true)
        );
        assert_eq!(c.states["review"].on[0].action, Some(RuleAction::Stop));
        assert!(
            c.states["done"].on.is_empty(),
            "`done: {{}}` is a state with no rules"
        );

        let compiled = compile(&example()).unwrap();
        assert_eq!(compiled.initial, "working");
        let (ptr, re) = &compiled.states["working"][0].matches[0];
        assert_eq!(ptr, "/tool_input/command");
        assert!(re.is_match("rm -rf /tmp/x"));
        assert!(
            !re.is_match("echo rm -rf"),
            "full match: anchored at both ends"
        );
        assert_eq!(compiled.hash.len(), 64);
        assert_eq!(compiled.hash, config_hash(&example()));
    }

    #[test]
    fn send_submit_defaults_to_true() {
        let c = parse(&json!({
            "initial": "a",
            "states": { "a": { "on": [ { "event": "Stop", "send": { "text": "go" } } ] } }
        }))
        .unwrap();
        assert!(c.states["a"].on[0].send.as_ref().unwrap().submit);
    }

    #[test]
    fn errors_carry_the_config_path() {
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({ "initial": "foo", "states": { "a": {} } }),
                "initial: no state \"foo\" declared",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Foo" } ] } } }),
                "states.a.on[0].event: unknown event \"Foo\"",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop" }, { "event": "Stop", "goto": "x" } ] } } }),
                "states.a.on[1].goto: no state \"x\" declared",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "match": { "tool_input": "x" } } ] } } }),
                "states.a.on[0].match.tool_input: not a JSON pointer (must start with \"/\")",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "match": { "/x": "[" } } ] } } }),
                "states.a.on[0].match./x: unclosed character class",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "foo": 1 } ] } } }),
                "states.a.on[0].foo: unknown field `foo`",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "respond": "block" } ] } } }),
                "states.a.on[0].respond: invalid type: string \"block\", expected a map",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "action": "explode" } ] } } }),
                "states.a.on[0].action: unknown variant `explode`",
            ),
            (json!({ "states": { "a": {} } }), "missing field `initial`"),
            (
                json!({ "initial": "a", "states": { "a": {} }, "extra": true }),
                "extra: unknown field `extra`",
            ),
            (
                json!("nope"),
                "invalid type: string \"nope\", expected struct FlowConfig",
            ),
        ];
        for (input, want) in cases {
            let got = compile(&input).unwrap_err().to_string();
            assert_eq!(got, want, "for {input}");
        }
    }

    #[test]
    fn an_oversized_regex_is_rejected_with_the_path() {
        let huge = "a{1,5000}".to_string();
        let err = compile(&json!({
            "initial": "a",
            "states": { "a": { "on": [ { "event": "Stop", "match": { "/x": huge } } ] } }
        }))
        .unwrap_err();
        assert_eq!(err.path, "states.a.on[0].match./x");
        assert!(
            err.message.contains("size limit"),
            "one line, mentions the limit: {}",
            err.message
        );
        assert!(!err.to_string().contains('\n'));
    }

    #[test]
    fn the_hash_is_canonical() {
        let a = json!({ "initial": "a", "states": { "a": {} } });
        let b = json!({ "states": { "a": {} }, "initial": "a" });
        assert_eq!(
            config_hash(&a),
            config_hash(&b),
            "key order does not matter"
        );
        let c = json!({ "initial": "a", "states": { "a": { "on": [] } } });
        assert_ne!(
            config_hash(&a),
            config_hash(&c),
            "an explicit empty `on` is a different document"
        );
    }
}
