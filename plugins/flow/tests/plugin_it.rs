//! The flow plugin through the SDK harness (plugins spec §17.3, §17.7):
//! every call crosses the wire as the daemon's would.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use balerix_api::{Key, KeyStep, PluginAction};
use balerix_plugin_flow::plugin::{FlowPlugin, Nudger, Stored};
use balerix_plugin_sdk::testing::{FakeHost, Harness, event, metric};
use balerix_plugin_sdk::{Env, Host};
use serde_json::{Value, json};
use tokio::time::Instant;

const ALICE: &str = "e2e/c/alice";

fn config() -> Value {
    json!({
        "initial": "working",
        "states": {
            "working": { "on": [
                { "event": "PreToolUse", "match": { "/tool_input/command": "rm -rf.*" },
                  "respond": { "decision": "block", "reason": "flow: no recursive deletes" } },
                { "event": "Stop", "goto": "review", "send": { "text": "flow says: run the tests" } }
            ] },
            "review": { "on": [ { "event": "Stop", "goto": "done", "action": "stop" } ] },
            "done": {}
        }
    })
}

async fn world() -> (FakeHost, Env, Harness) {
    let fake = FakeHost::start("tok", json!({}), vec![]).await;
    let env = fake.env("flow", std::path::Path::new("/s"));
    let plugin = FlowPlugin::new(Host::new(env.clone()).unwrap()).unwrap();
    let h = Harness::start(&env, plugin).await;
    (fake, env, h)
}

fn rm() -> balerix_api::HookEvent {
    event(
        ALICE,
        "PreToolUse",
        json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf /tmp/x" } }),
    )
}

fn stop() -> balerix_api::HookEvent {
    event(ALICE, "Stop", json!({ "stop_hook_active": false }))
}

#[tokio::test]
async fn activation_validates_and_a_rejection_carries_the_path() {
    let (fake, _, h) = world().await;
    assert_eq!(h.activate(ALICE, config()).await, Ok(()));
    assert_eq!(
        fake.kv_json(&FlowPlugin::state_key(ALICE)),
        Some(
            json!({ "state": "working", "config": balerix_plugin_flow::config::config_hash(&config()) })
        )
    );
    let mut bad = config();
    bad["states"]["working"]["on"][0]["match"]["/tool_input/command"] = json!("[");
    assert_eq!(
        h.activate("e2e/c/bob", bad).await,
        Err("states.working.on[0].match./tool_input/command: unclosed character class".into())
    );
    assert_eq!(
        fake.kv_json(&FlowPlugin::state_key("e2e/c/bob")),
        None,
        "nothing stored for a rejected agent"
    );
}

#[tokio::test]
async fn a_verdict_is_merged_over_the_response_so_far_and_carries_actions() {
    let (_, _, h) = world().await;
    h.activate(ALICE, config()).await.unwrap();
    let v = h
        .intercept_with(rm(), json!({ "decision": "allow", "other": 1 }), 900)
        .await;
    assert_eq!(
        v.response,
        json!({ "decision": "block", "reason": "flow: no recursive deletes", "other": 1 })
    );
    assert!(v.actions.is_empty());
    // a command the rule does not match passes through
    let v = h
        .intercept(event(
            ALICE,
            "PreToolUse",
            json!({ "tool_input": { "command": "ls" } }),
        ))
        .await;
    assert_eq!(v.response, json!({}));

    let v = h.intercept(stop()).await;
    assert_eq!(v.response, json!({}));
    assert_eq!(
        v.actions,
        vec![PluginAction::SendText {
            text: "flow says: run the tests".into(),
            submit: true
        }]
    );
    let v = h.intercept(stop()).await;
    assert_eq!(v.actions, vec![PluginAction::Stop]);
    let v = h.intercept(stop()).await;
    assert!(v.actions.is_empty(), "done has no rules");
}

#[tokio::test]
async fn transitions_move_the_gauge_count_and_persist() {
    let (fake, _, h) = world().await;
    h.activate(ALICE, config()).await.unwrap();
    let labels = [("fleet", "e2e"), ("crew", "c"), ("agent", "alice")];
    let m = h.metrics().await;
    assert!(m.starts_with("# HELP balerix_plugin_flow_"), "{m}");
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_state",
            &[("agent", "alice"), ("state", "working")]
        ),
        Some(1.0)
    );

    h.intercept(stop()).await;
    let m = h.metrics().await;
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_state",
            &[("agent", "alice"), ("state", "review")]
        ),
        Some(1.0)
    );
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_state",
            &[("agent", "alice"), ("state", "working")]
        ),
        None,
        "the previous state's series is removed"
    );
    let mut t = labels.to_vec();
    t.extend([("from", "working"), ("to", "review")]);
    assert_eq!(
        metric(&m, "balerix_plugin_flow_transitions_total", &t),
        Some(1.0)
    );
    let stored: Stored =
        serde_json::from_value(fake.kv_json(&FlowPlugin::state_key(ALICE)).unwrap()).unwrap();
    assert_eq!(stored.state, "review");
}

#[tokio::test]
async fn a_restart_resumes_the_stored_state() {
    let (_, env, mut h) = world().await;
    h.activate(ALICE, config()).await.unwrap();
    h.intercept(stop()).await; // working → review
    h.restart(FlowPlugin::new(Host::new(env.clone()).unwrap()).unwrap())
        .await;
    // the daemon re-sends activate after hello with the same config
    h.activate(ALICE, config()).await.unwrap();
    let m = h.metrics().await;
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_state",
            &[("agent", "alice"), ("state", "review")]
        ),
        Some(1.0)
    );
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_transitions_total",
            &[("agent", "alice")]
        ),
        None,
        "counters start empty on a fresh process"
    );
    let v = h.intercept(stop()).await;
    assert_eq!(
        v.actions,
        vec![PluginAction::Stop],
        "review's rule, not working's"
    );
}

#[tokio::test]
async fn deactivate_and_a_changed_config_reset_to_initial() {
    let (fake, _, h) = world().await;
    h.activate(ALICE, config()).await.unwrap();
    h.intercept(stop()).await; // → review

    // deactivate: entry and key gone, gauge series gone, events pass through
    h.deactivate(ALICE).await;
    assert_eq!(fake.kv_json(&FlowPlugin::state_key(ALICE)), None);
    assert_eq!(
        metric(
            &h.metrics().await,
            "balerix_plugin_flow_state",
            &[("agent", "alice")]
        ),
        None
    );
    let v = h.intercept_with(rm(), json!({ "k": 1 }), 900).await;
    assert_eq!(
        v.response,
        json!({ "k": 1 }),
        "an unknown agent passes through"
    );

    // activate again: back at initial
    h.activate(ALICE, config()).await.unwrap();
    assert_eq!(
        fake.kv_json(&FlowPlugin::state_key(ALICE)).unwrap()["state"],
        "working"
    );
    h.intercept(stop()).await; // → review

    // a changed config arrives as an `activate` in place, no `deactivate`
    // before it (§16.2): the hash no longer matches, so initial again
    let mut changed = config();
    changed["states"]["review"]["on"][0]["action"] = json!("restart");
    h.activate(ALICE, changed.clone()).await.unwrap();
    let stored = fake.kv_json(&FlowPlugin::state_key(ALICE)).unwrap();
    assert_eq!(stored["state"], "working");
    assert_eq!(
        stored["config"],
        balerix_plugin_flow::config::config_hash(&changed)
    );
}

#[tokio::test]
async fn activation_fails_loudly_when_kv_is_unavailable() {
    let fake = FakeHost::start("tok", json!({}), vec![]).await;
    let mut env = fake.env("flow", std::path::Path::new("/s"));
    env.token = "wrong".into(); // every kv call is a 401
    let plugin = FlowPlugin::new(Host::new(env.clone()).unwrap()).unwrap();
    let good = fake.env("flow", std::path::Path::new("/s"));
    let h = Harness::start(&good, plugin).await;
    let err = h.activate(ALICE, config()).await.unwrap_err();
    assert!(err.starts_with("kv: daemon: HTTP 401"), "{err}");
}

#[tokio::test]
async fn health_is_always_ok() {
    let (_, _, h) = world().await;
    assert_eq!(h.health().await, Ok(()));
}

/// A rule that sends the first prompt the moment the session starts (#100).
fn first_prompt_config() -> Value {
    json!({
        "initial": "starting",
        "states": {
            "starting": { "on": [
                { "event": "SessionStart", "goto": "working",
                  "send": { "text": "flow says: read the issue", "submit": true } }
            ] },
            "working": {}
        }
    })
}

fn session_start() -> balerix_api::HookEvent {
    event(ALICE, "SessionStart", json!({ "source": "startup" }))
}

fn submitted(text: &str) -> balerix_api::HookEvent {
    event(ALICE, "UserPromptSubmit", json!({ "prompt": text }))
}

fn enter() -> PluginAction {
    PluginAction::SendKeys {
        steps: vec![KeyStep::Key(Key::Enter)],
        delay_ms: balerix_api::DEFAULT_KEY_DELAY_MS,
    }
}

async fn nudged_world() -> (FakeHost, Harness, Nudger) {
    let fake = FakeHost::start("tok", json!({}), vec![]).await;
    let env = fake.env("flow", std::path::Path::new("/s"));
    let plugin = FlowPlugin::new(Host::new(env.clone()).unwrap()).unwrap();
    let nudger = plugin.nudger();
    let h = Harness::start(&env, plugin).await;
    (fake, h, nudger)
}

/// #100: a prompt sent on `SessionStart` reaches the composer while the
/// TUI is starting and loses its Enter, so an unconfirmed submitted send
/// gets Enter again every 5 s until `UserPromptSubmit` names it.
#[tokio::test]
async fn an_unconfirmed_submitted_send_gets_enter_again_until_confirmed() {
    let (fake, h, nudger) = nudged_world().await;
    h.activate(ALICE, first_prompt_config()).await.unwrap();
    let t0 = Instant::now();
    let v = h.intercept(session_start()).await;
    assert_eq!(
        v.actions,
        vec![PluginAction::SendText {
            text: "flow says: read the issue".into(),
            submit: true
        }]
    );
    // the prompt's clock starts inside `intercept`, a moment after `t0`
    nudger.tick(t0 + Duration::from_secs(4)).await;
    assert!(fake.actions_for(ALICE).is_empty(), "too early");
    nudger.tick(t0 + Duration::from_secs(6)).await;
    assert_eq!(fake.actions_for(ALICE), vec![enter()]);
    nudger.tick(t0 + Duration::from_secs(8)).await;
    assert_eq!(fake.actions_for(ALICE).len(), 1, "5 s between presses");
    nudger.tick(t0 + Duration::from_secs(12)).await;
    assert_eq!(fake.actions_for(ALICE), vec![enter(), enter()]);

    h.intercept(submitted("flow says: read the issue")).await;
    h.intercept(stop()).await;
    nudger.tick(t0 + Duration::from_secs(60)).await;
    assert_eq!(fake.actions_for(ALICE).len(), 2, "confirmed: no more");
}

#[tokio::test]
async fn a_send_left_in_the_composer_on_purpose_is_never_nudged() {
    let (fake, h, nudger) = nudged_world().await;
    let mut config = first_prompt_config();
    config["states"]["starting"]["on"][0]["send"]["submit"] = json!(false);
    h.activate(ALICE, config).await.unwrap();
    let t0 = Instant::now();
    h.intercept(session_start()).await;
    nudger.tick(t0 + Duration::from_secs(6)).await;
    nudger.tick(t0 + Duration::from_secs(60)).await;
    assert!(fake.actions_for(ALICE).is_empty());
}

#[tokio::test]
async fn a_send_typed_mid_turn_is_not_nudged_until_the_turn_ends() {
    let (fake, h, nudger) = nudged_world().await;
    let config = json!({
        "initial": "working",
        "states": { "working": { "on": [
            { "event": "PreToolUse", "send": { "text": "flow says: careful", "submit": true } }
        ] } }
    });
    h.activate(ALICE, config).await.unwrap();
    let t0 = Instant::now();
    h.intercept(submitted("something else")).await; // a turn is open
    h.intercept(event(ALICE, "PreToolUse", json!({ "tool_name": "Bash" })))
        .await;
    nudger.tick(t0 + Duration::from_secs(30)).await;
    assert!(
        fake.actions_for(ALICE).is_empty(),
        "Claude queues text typed mid-turn; an Enter would answer a dialog"
    );
    h.intercept(stop()).await;
    let t1 = Instant::now();
    nudger.tick(t1 + Duration::from_secs(6)).await;
    assert_eq!(fake.actions_for(ALICE), vec![enter()]);
}

#[tokio::test]
async fn deliveries_are_counted_and_a_refused_enter_is_an_error() {
    let (fake, h, nudger) = nudged_world().await;
    h.activate(ALICE, first_prompt_config()).await.unwrap();
    let t0 = Instant::now();
    h.intercept(session_start()).await;
    nudger.tick(t0 + Duration::from_secs(6)).await;
    let m = h.metrics().await;
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_deliveries_total",
            &[("outcome", "nudged")]
        ),
        Some(1.0)
    );
    fake.fail_actions(Some("window gone"));
    nudger.tick(t0 + Duration::from_secs(12)).await;
    let m = h.metrics().await;
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_deliveries_total",
            &[("outcome", "nudged")]
        ),
        Some(1.0)
    );
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_errors_total",
            &[("kind", "send_keys")]
        ),
        Some(1.0)
    );
    h.intercept(submitted("flow says: read the issue")).await;
    let m = h.metrics().await;
    assert_eq!(
        metric(
            &m,
            "balerix_plugin_flow_deliveries_total",
            &[("outcome", "confirmed")]
        ),
        Some(1.0)
    );
}

#[tokio::test]
async fn deactivate_forgets_what_was_pending() {
    let (fake, h, nudger) = nudged_world().await;
    h.activate(ALICE, first_prompt_config()).await.unwrap();
    let t0 = Instant::now();
    h.intercept(session_start()).await;
    h.deactivate(ALICE).await;
    nudger.tick(t0 + Duration::from_secs(6)).await;
    assert!(
        fake.actions_for(ALICE).is_empty(),
        "down: nothing to submit"
    );
}
