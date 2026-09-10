//! The step (plugins spec §17.3): given the compiled config, the current
//! state and one event, which rule fires and what follows. Pure; the
//! plugin applies the transition and the KV write.

use balerix_api::{HookEvent, PluginAction};
use serde_json::Value;

use crate::config::{Compiled, CompiledRule, RuleAction};

/// What one event does in one state.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Index of the rule that fired in the state's `on` list.
    pub fired: Option<usize>,
    /// The verdict: `so_far` with the rule's `respond` keys replaced.
    pub response: Value,
    /// `send` first, then `action`.
    pub actions: Vec<PluginAction>,
    /// The rule's `goto`, if any — a self-transition included.
    pub next: Option<String>,
}

/// The payload value at a JSON pointer as text: strings as they are,
/// anything else (numbers, booleans, `null`, objects) as its JSON text.
/// `None` when the pointer resolves to nothing (§17.3).
pub fn text_at(payload: &Value, pointer: &str) -> Option<String> {
    payload.pointer(pointer).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// The event name equals the rule's and every `match` entry holds.
pub fn rule_matches(rule: &CompiledRule, event: &HookEvent) -> bool {
    rule.event == event.name
        && rule
            .matches
            .iter()
            .all(|(pointer, re)| text_at(&event.payload, pointer).is_some_and(|s| re.is_match(&s)))
}

pub fn step(compiled: &Compiled, state: &str, event: &HookEvent, so_far: Value) -> Step {
    let Some((i, rule)) = compiled.states.get(state).and_then(|rules| {
        rules
            .iter()
            .enumerate()
            .find(|(_, r)| rule_matches(r, event))
    }) else {
        return Step {
            fired: None,
            response: so_far,
            actions: Vec::new(),
            next: None,
        };
    };
    let mut response = match so_far {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    for (k, v) in &rule.respond {
        response.insert(k.clone(), v.clone());
    }
    let mut actions = Vec::new();
    if let Some(send) = &rule.send {
        actions.push(PluginAction::SendText {
            text: send.text.clone(),
            submit: send.submit,
        });
    }
    match rule.action {
        Some(RuleAction::Stop) => actions.push(PluginAction::Stop),
        Some(RuleAction::Restart) => actions.push(PluginAction::Restart),
        None => {}
    }
    Step {
        fired: Some(i),
        response: Value::Object(response),
        actions,
        next: rule.goto.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::compile;
    use balerix_api::PluginAction;
    use balerix_plugin_sdk::testing::event;
    use serde_json::json;

    fn compiled() -> Compiled {
        compile(&json!({
            "initial": "working",
            "states": {
                "working": { "on": [
                    { "event": "PreToolUse", "match": { "/tool_input/command": "rm -rf.*", "/tool_name": "Bash" },
                      "respond": { "decision": "block", "reason": "no" } },
                    { "event": "PreToolUse", "match": { "/tool_input/n": "3" }, "respond": { "n": "three" } },
                    { "event": "PreToolUse", "respond": { "decision": "allow" } },
                    { "event": "Stop", "goto": "review", "send": { "text": "tests", "submit": false }, "action": "restart" },
                    { "event": "SessionEnd", "goto": "working" }
                ] },
                "review": { "on": [ { "event": "Stop", "goto": "done", "action": "stop" } ] },
                "done": {}
            }
        }))
        .unwrap()
    }

    #[test]
    fn text_at_reads_strings_and_stringifies_the_rest() {
        let p = json!({ "a": "x", "n": 3, "b": true, "o": { "k": [1] }, "z": null });
        assert_eq!(text_at(&p, "/a").as_deref(), Some("x"));
        assert_eq!(text_at(&p, "/n").as_deref(), Some("3"));
        assert_eq!(text_at(&p, "/b").as_deref(), Some("true"));
        assert_eq!(text_at(&p, "/o").as_deref(), Some("{\"k\":[1]}"));
        assert_eq!(text_at(&p, "/o/k/0").as_deref(), Some("1"));
        assert_eq!(text_at(&p, "/z").as_deref(), Some("null"));
        assert_eq!(text_at(&p, "/missing"), None);
        assert_eq!(text_at(&p, "/a/deeper"), None);
    }

    #[test]
    fn the_first_rule_whose_event_and_every_match_hold_fires() {
        let c = compiled();
        let blocked = event(
            "f/c/a",
            "PreToolUse",
            json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf /" } }),
        );
        let s = step(&c, "working", &blocked, json!({}));
        assert_eq!(s.fired, Some(0));
        assert_eq!(s.response, json!({ "decision": "block", "reason": "no" }));
        assert!(s.actions.is_empty() && s.next.is_none());

        // same command, different tool: rule 0 needs every entry
        let other_tool = event(
            "f/c/a",
            "PreToolUse",
            json!({ "tool_name": "Write", "tool_input": { "command": "rm -rf /" } }),
        );
        assert_eq!(step(&c, "working", &other_tool, json!({})).fired, Some(2));

        // a number is matched through its JSON text
        let n = event("f/c/a", "PreToolUse", json!({ "tool_input": { "n": 3 } }));
        let s = step(&c, "working", &n, json!({}));
        assert_eq!(s.fired, Some(1));
        assert_eq!(s.response, json!({ "n": "three" }));

        // a missing pointer never matches, so the catch-all fires
        let bare = event("f/c/a", "PreToolUse", json!({}));
        assert_eq!(step(&c, "working", &bare, json!({})).fired, Some(2));
    }

    #[test]
    fn no_rule_means_pass_through() {
        let c = compiled();
        let e = event("f/c/a", "Notification", json!({}));
        let s = step(&c, "working", &e, json!({ "keep": 1 }));
        assert_eq!(s.fired, None);
        assert_eq!(s.response, json!({ "keep": 1 }));
        assert!(s.actions.is_empty() && s.next.is_none());
        // a state with no rules
        let s = step(&c, "done", &event("f/c/a", "Stop", json!({})), json!({}));
        assert_eq!(s.fired, None);
        // an unknown state (never produced by the plugin) also passes through
        let s = step(
            &c,
            "nowhere",
            &event("f/c/a", "Stop", json!({})),
            json!({ "x": 1 }),
        );
        assert_eq!((s.fired, s.response), (None, json!({ "x": 1 })));
    }

    #[test]
    fn respond_replaces_top_level_keys_only() {
        let c = compiled();
        let e = event(
            "f/c/a",
            "PreToolUse",
            json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf /" } }),
        );
        let so_far = json!({ "decision": "allow", "reason": { "nested": true }, "other": 1 });
        let s = step(&c, "working", &e, so_far);
        assert_eq!(
            s.response,
            json!({ "decision": "block", "reason": "no", "other": 1 })
        );
        // a non-object so far (the daemon never sends one) is replaced, not merged into
        let s = step(&c, "working", &e, json!(7));
        assert_eq!(s.response, json!({ "decision": "block", "reason": "no" }));
    }

    #[test]
    fn send_action_and_goto_are_carried() {
        let c = compiled();
        let s = step(&c, "working", &event("f/c/a", "Stop", json!({})), json!({}));
        assert_eq!(s.fired, Some(3));
        assert_eq!(s.response, json!({}));
        assert_eq!(
            s.actions,
            vec![
                PluginAction::SendText {
                    text: "tests".into(),
                    submit: false
                },
                PluginAction::Restart
            ],
            "send before action"
        );
        assert_eq!(s.next.as_deref(), Some("review"));
        let s = step(&c, "review", &event("f/c/a", "Stop", json!({})), json!({}));
        assert_eq!(s.actions, vec![PluginAction::Stop]);
        assert_eq!(s.next.as_deref(), Some("done"));
        // a self-transition is still a transition
        let s = step(
            &c,
            "working",
            &event("f/c/a", "SessionEnd", json!({})),
            json!({}),
        );
        assert_eq!(s.next.as_deref(), Some("working"));
    }

    mod props {
        use super::*;
        use balerix_api::HookEvent;
        use proptest::prelude::*;

        const EVENTS: [&str; 3] = ["PreToolUse", "Stop", "Notification"];
        const PATTERNS: [&str; 4] = ["x", "x.*", ".*", "[0-9]+"];
        const VALUES: [&str; 4] = ["x", "xyz", "42", ""];
        const STATES: [&str; 3] = ["a", "b", "c"];

        fn rule() -> impl Strategy<Value = Value> {
            (
                0..3usize,
                proptest::option::of((0..4usize, 0..4usize)),
                proptest::option::of(0..3usize),
                proptest::option::of(proptest::collection::btree_map("[a-c]", 0..3u8, 0..3)),
            )
                .prop_map(|(ev, m, goto, respond)| {
                    let mut r = json!({ "event": EVENTS[ev] });
                    if let Some((ptr, pat)) = m {
                        let pointer = format!("/k{ptr}");
                        r["match"] = json!({ pointer: PATTERNS[pat] });
                    }
                    if let Some(g) = goto {
                        r["goto"] = json!(STATES[g]);
                    }
                    if let Some(resp) = respond {
                        r["respond"] = json!(resp);
                    }
                    r
                })
        }

        fn config() -> impl Strategy<Value = Value> {
            (
                0..3usize,
                proptest::collection::vec(proptest::collection::vec(rule(), 0..4), 3),
            )
                .prop_map(|(initial, rules)| {
                    let mut states = serde_json::Map::new();
                    for (name, on) in STATES.iter().zip(rules) {
                        states.insert(name.to_string(), json!({ "on": on }));
                    }
                    json!({ "initial": STATES[initial], "states": states })
                })
        }

        fn payload() -> impl Strategy<Value = Value> {
            proptest::collection::btree_map(0..4usize, 0..4usize, 0..4).prop_map(|m| {
                let mut p = serde_json::Map::new();
                for (k, v) in m {
                    p.insert(format!("k{k}"), json!(VALUES[v]));
                }
                Value::Object(p)
            })
        }

        /// The obvious matcher, written independently of `step`.
        fn naive_first(c: &Compiled, state: &str, e: &HookEvent) -> Option<usize> {
            let rules = c.states.get(state)?;
            rules.iter().position(|r| {
                r.event == e.name
                    && r.matches.iter().all(|(ptr, re)| {
                        e.payload
                            .pointer(ptr)
                            .map(|v| match v {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .is_some_and(|s| re.is_match(&s))
                    })
            })
        }

        proptest! {
            #[test]
            fn step_fires_the_first_matching_rule_and_lands_in_a_declared_state(
                cfg in config(), state in 0..3usize, ev in 0..3usize, p in payload()
            ) {
                let c = compile(&cfg).unwrap();
                let e = event("f/c/a", EVENTS[ev], p);
                let s = step(&c, STATES[state], &e, json!({}));
                prop_assert_eq!(s.fired, naive_first(&c, STATES[state], &e));
                if let Some(next) = &s.next {
                    prop_assert!(c.states.contains_key(next));
                }
                prop_assert!(s.response.is_object());
                if let Some(i) = s.fired {
                    let rule = &c.states[STATES[state]][i];
                    for (k, v) in &rule.respond {
                        prop_assert_eq!(&s.response[k], v);
                    }
                    prop_assert_eq!(s.next.as_deref(), rule.goto.as_deref());
                }
            }
        }
    }
}
