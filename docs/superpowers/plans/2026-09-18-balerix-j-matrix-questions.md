# Spec J: Answering Claude's Questions from a Matrix Thread — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The matrix plugin shows an `AskUserQuestion` dialog in the agent's thread, matches a reply to its options, echoes the selection, and answers with paced keystrokes through a new `send_keys` plugin action.

**Architecture:** A generic `send_keys` action (named keys and literal text, paced) goes through `balerix-api`, the `AgentRunner` port, `TmuxRunner` and the daemon; the runtime knows nothing about Claude's dialog. All dialog knowledge lives in one pure module of the matrix plugin, `question.rs` (parse, match, plan), guarded by a model-based property test and a by-hand run against the real `claude`. The actor keeps a small per-agent state machine whose `Open` state is mirrored to the plugin's KV.

**Tech Stack:** Rust 2024 (1.98), serde, tokio, tmux 3.7c, `balerix-plugin-sdk` (`Host`, `FakeHost`), insta snapshots, proptest 1.11.0, bash + jq for the by-hand script.

**Spec:** `docs/superpowers/specs/2026-09-18-balerix-j-matrix-questions-design.md`. Read it first; §2's table is the measured behaviour every key rule comes from.

## Global Constraints

- Run cargo through mise: `mise x -- cargo …` or a `mise run` task. Never bare `cargo`.
- The core workspace gate is `mise run check`. The plugin gate is `mise run plugin matrix`. A plugin change is **not** caught by `check`.
- The matrix plugin is a standalone project (`plugins/matrix/`, its own `Cargo.lock`). Run its tests with `--manifest-path plugins/matrix/Cargo.toml` and `CARGO_TARGET_DIR=plugins/matrix/target`. Commit `plugins/matrix/Cargo.lock` when it changes.
- New Cargo dependencies are exact versions with the reason in the commit. This plan adds exactly one: `proptest = "1.11.0"` to the matrix plugin's dev-dependencies (the workspace's version).
- `unsafe` is forbidden. Library crates return `thiserror` errors; messages that name config start with the config path.
- insta snapshots: read the `.snap.new`, compare with the expected text in this plan, then `mise x -- cargo insta accept`. Never blind-accept.
- Constants, verbatim from the spec: `DEFAULT_KEY_DELAY_MS = 100`, `MIN_KEY_DELAY_MS = 20`, `MAX_KEY_DELAY_MS = 500`, `MAX_KEY_STEPS = 64`, `MAX_KEY_TEXT = 1024`, `MAX_KEY_SEQUENCE_MS = 8000`.
- Key allowlist, verbatim: `up`, `down`, `enter`, `escape`. The key plan uses only Down, Enter, Escape and text. Never Tab.
- Commit messages are Conventional Commits. Changelogs are generated from the PR title; do not edit any `CHANGELOG.md`.
- Integration-test temp roots live under `target/tmp`, never `/tmp`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/balerix-api/src/protocol.rs` (modify) | `Key`, `KeyStep`, `PluginAction::SendKeys`, the constants, `PluginAction::validate` |
| `crates/balerix-api/src/lib.rs` (modify) | re-exports |
| `docs/plugin-protocol/action-send-keys.json` (create) | conformance fixture |
| `docs/plugin-protocol.md` (modify) | §3 action list, §6 fixture count |
| `crates/balerix-plugin-sdk/tests/conformance.rs` (modify) | replays the new fixture |
| `crates/balerix-core/src/ports.rs` (modify) | `AgentRunner::send_keys` |
| `crates/balerix-core/src/fakes.rs` (modify) | `FakeRunner::send_keys` |
| `crates/balerix-runtime/src/tmux.rs` (modify) | `send_keys`, the per-agent send lock, trailing-semicolon handling |
| `crates/balerix-runtime/tests/tmux_it.rs` (modify) | real-tmux tests |
| `crates/balerix-server/src/daemon.rs` (modify) | `execute_action` arm, validation |
| `crates/balerix-server/tests/events_it.rs` (modify) | route answers 400 for an invalid action |
| `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md` (modify) | the clauses the spec names |
| `plugins/matrix/src/question.rs` (create) | pure: `parse`, `match_reply`, `plan`, `describe`, `recorded_matches` |
| `plugins/matrix/src/pending.rs` (create) | per-agent question state and its KV mirror |
| `plugins/matrix/src/render.rs` (modify) | `question_message` |
| `plugins/matrix/src/config.rs` (modify) | `keyDelayMs` |
| `plugins/matrix/src/matrix.rs` (modify) | the `CONFIRMED` reaction |
| `plugins/matrix/src/actor.rs` (modify) | event side and inbound side of the flow |
| `plugins/matrix/examples/question_plan.rs` (create) | prints a plan as JSON for the by-hand script |
| `scripts/verify-questions.sh` (create), `mise.toml` (modify) | the by-hand check against the real `claude` |

---

### Task 1: `send_keys` in `balerix-api`, with its fixture

**Files:**
- Modify: `crates/balerix-api/src/protocol.rs`
- Modify: `crates/balerix-api/src/lib.rs` (the `pub use protocol::{…}` list)
- Create: `docs/plugin-protocol/action-send-keys.json`
- Modify: `crates/balerix-plugin-sdk/tests/conformance.rs` (next to the `fx["action"]` block, around line 221)
- Modify: `docs/plugin-protocol.md` (§3 actions list, §6 "twenty-two fixtures")

**Interfaces:**
- Produces:
  - `pub enum Key { Up, Down, Enter, Escape }` — `Copy`, serde `snake_case`.
  - `pub enum KeyStep { Key(Key), Text(String) }` — wire `{"key":"down"}` or `{"text":"…"}`.
  - `PluginAction::SendKeys { steps: Vec<KeyStep>, delay_ms: u64 }`, label `"send_keys"`.
  - `PluginAction::validate(&self) -> Result<(), String>`.
  - The six constants from Global Constraints, all `pub const` in `protocol.rs`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module of `crates/balerix-api/src/protocol.rs`:

```rust
    #[test]
    fn send_keys_round_trips_and_defaults_its_delay() {
        let wire = json!({
            "action": "send_keys",
            "steps": [{ "key": "down" }, { "text": "teal-ish" }, { "key": "enter" }],
            "delay_ms": 150
        });
        let a: PluginAction = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            a,
            PluginAction::SendKeys {
                steps: vec![
                    KeyStep::Key(Key::Down),
                    KeyStep::Text("teal-ish".into()),
                    KeyStep::Key(Key::Enter),
                ],
                delay_ms: 150,
            }
        );
        assert_eq!(a.label(), "send_keys");
        assert_eq!(serde_json::to_value(&a).unwrap(), wire);

        let defaulted: PluginAction = serde_json::from_value(
            json!({ "action": "send_keys", "steps": [{ "key": "escape" }] }),
        )
        .unwrap();
        assert_eq!(
            defaulted,
            PluginAction::SendKeys {
                steps: vec![KeyStep::Key(Key::Escape)],
                delay_ms: DEFAULT_KEY_DELAY_MS,
            }
        );
    }

    #[test]
    fn send_keys_rejects_what_the_allowlist_does_not_name() {
        let bad = [
            json!({ "action": "send_keys", "steps": [{ "key": "tab" }] }),
            json!({ "action": "send_keys", "steps": [{ "key": "down", "text": "x" }] }),
            json!({ "action": "send_keys", "steps": [{}] }),
            json!({ "action": "send_keys", "steps": [{ "key": "down" }], "x": 1 }),
            json!({ "action": "send_keys" }),
        ];
        for v in bad {
            assert!(
                serde_json::from_value::<PluginAction>(v.clone()).is_err(),
                "{v}"
            );
        }
    }

    #[test]
    fn validate_bounds_steps_delay_text_and_the_whole_sequence() {
        let keys = |n: usize, delay_ms: u64| PluginAction::SendKeys {
            steps: vec![KeyStep::Key(Key::Down); n],
            delay_ms,
        };
        let text = |t: &str| PluginAction::SendKeys {
            steps: vec![KeyStep::Text(t.into())],
            delay_ms: DEFAULT_KEY_DELAY_MS,
        };
        assert_eq!(keys(1, 100).validate(), Ok(()));
        assert_eq!(keys(MAX_KEY_STEPS, 100).validate(), Ok(()));
        assert!(keys(0, 100).validate().unwrap_err().starts_with("steps:"));
        assert!(
            keys(MAX_KEY_STEPS + 1, 100)
                .validate()
                .unwrap_err()
                .starts_with("steps:")
        );
        assert!(keys(1, 19).validate().unwrap_err().starts_with("delay_ms:"));
        assert!(keys(1, 501).validate().unwrap_err().starts_with("delay_ms:"));
        assert_eq!(keys(16, 500).validate(), Ok(()));
        assert!(
            keys(17, 500)
                .validate()
                .unwrap_err()
                .starts_with("steps: 17 steps at 500 ms")
        );
        assert_eq!(text("teal-ish; really").validate(), Ok(()));
        assert!(text("").validate().unwrap_err().starts_with("steps[0].text:"));
        assert!(
            text(&"x".repeat(MAX_KEY_TEXT + 1))
                .validate()
                .unwrap_err()
                .starts_with("steps[0].text:")
        );
        assert!(
            text("two\nlines")
                .validate()
                .unwrap_err()
                .contains("control character")
        );
        assert!(text("esc\u{1b}[B").validate().is_err());
        assert_eq!(PluginAction::Stop.validate(), Ok(()));
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `mise x -- cargo nextest run -p balerix-api send_keys validate_bounds`
Expected: compile error, `cannot find type KeyStep`.

- [ ] **Step 3: Implement**

In `crates/balerix-api/src/protocol.rs`, below `CHAIN_BUDGET_MS`:

```rust
/// `send_keys`: the pause after each step when the action names none.
pub const DEFAULT_KEY_DELAY_MS: u64 = 100;
/// The floor is the point of the action (Spec J §2): keys sent with no
/// pause are dropped at a dialog transition.
pub const MIN_KEY_DELAY_MS: u64 = 20;
pub const MAX_KEY_DELAY_MS: u64 = 500;
pub const MAX_KEY_STEPS: usize = 64;
/// Bytes in one `text` step.
pub const MAX_KEY_TEXT: usize = 1024;
/// `steps × delay_ms` may not pass this: the SDK's `Host::action` gives up
/// after 10 s, and a plugin that timed out mid-sequence cannot know what
/// state the dialog is in.
pub const MAX_KEY_SEQUENCE_MS: u64 = 8000;

/// The keys `send_keys` may press. A closed set: the runner matches each to
/// a fixed tmux key name, so nothing from the wire reaches tmux as a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Up,
    Down,
    Enter,
    Escape,
}

/// One step of a `send_keys`: `{"key": …}` or `{"text": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStep {
    Key(Key),
    Text(String),
}
```

Add the variant to `PluginAction`, after `SendText`:

```rust
    SendKeys {
        steps: Vec<KeyStep>,
        delay_ms: u64,
    },
```

Add the arm to the hand-written `Deserialize`, after the `"send_text"` arm:

```rust
            "send_keys" => {
                reject_extra(&["action", "steps", "delay_ms"])?;
                let steps = obj
                    .get("steps")
                    .cloned()
                    .ok_or_else(|| D::Error::custom("missing field `steps`"))?;
                let steps: Vec<KeyStep> =
                    serde_json::from_value(steps).map_err(D::Error::custom)?;
                let delay_ms = match obj.get("delay_ms") {
                    Some(v) => serde_json::from_value(v.clone()).map_err(D::Error::custom)?,
                    None => DEFAULT_KEY_DELAY_MS,
                };
                Ok(PluginAction::SendKeys { steps, delay_ms })
            }
```

Extend `label()` with `PluginAction::SendKeys { .. } => "send_keys",` and add below it, inside the same `impl`:

```rust
    /// What serde cannot say: the bounds of a `send_keys` (Spec J §4.1).
    /// The message starts with the field, for the daemon's 400.
    pub fn validate(&self) -> Result<(), String> {
        let PluginAction::SendKeys { steps, delay_ms } = self else {
            return Ok(());
        };
        if steps.is_empty() || steps.len() > MAX_KEY_STEPS {
            return Err(format!(
                "steps: expected 1 to {MAX_KEY_STEPS} steps, got {}",
                steps.len()
            ));
        }
        if !(MIN_KEY_DELAY_MS..=MAX_KEY_DELAY_MS).contains(delay_ms) {
            return Err(format!(
                "delay_ms: expected {MIN_KEY_DELAY_MS} to {MAX_KEY_DELAY_MS}, got {delay_ms}"
            ));
        }
        let total = steps.len() as u64 * delay_ms;
        if total > MAX_KEY_SEQUENCE_MS {
            return Err(format!(
                "steps: {} steps at {delay_ms} ms is {total} ms, over the {MAX_KEY_SEQUENCE_MS} ms limit",
                steps.len()
            ));
        }
        for (i, step) in steps.iter().enumerate() {
            let KeyStep::Text(text) = step else { continue };
            if text.is_empty() || text.len() > MAX_KEY_TEXT {
                return Err(format!(
                    "steps[{i}].text: expected 1 to {MAX_KEY_TEXT} bytes, got {}",
                    text.len()
                ));
            }
            if text.chars().any(char::is_control) {
                return Err(format!(
                    "steps[{i}].text: a control character is not allowed"
                ));
            }
        }
        Ok(())
    }
```

In `crates/balerix-api/src/lib.rs`, add to the `pub use protocol::{…}` list: `DEFAULT_KEY_DELAY_MS, Key, KeyStep, MAX_KEY_DELAY_MS, MAX_KEY_SEQUENCE_MS, MAX_KEY_STEPS, MAX_KEY_TEXT, MIN_KEY_DELAY_MS`.

Note: a serde externally tagged enum already rejects `{}` and an object with two keys for `KeyStep`, which is why the second test passes without more code.

- [ ] **Step 4: Run the tests**

Run: `mise x -- cargo nextest run -p balerix-api`
Expected: PASS. Build only this crate (`-p balerix-api`): `balerix-server` has a `match` on `PluginAction` that stays non-exhaustive until Task 3.

- [ ] **Step 5: The conformance fixture**

Create `docs/plugin-protocol/action-send-keys.json`:

```json
{
  "route": "POST /v1/plugin-host/agents/payments/backend/bob/actions",
  "direction": "plugin-to-daemon",
  "request": {
    "action": "send_keys",
    "steps": [{ "key": "down" }, { "key": "down" }, { "text": "teal-ish" }, { "key": "enter" }],
    "delay_ms": 100
  },
  "status": 200,
  "response": {}
}
```

In `crates/balerix-plugin-sdk/tests/conformance.rs`, directly after the block that sends `fx["action"]` and asserts `fake.actions()[0]`, add:

```rust
    let keys: PluginAction =
        serde_json::from_value(fx["action-send-keys"]["request"].clone()).unwrap();
    assert_eq!(keys.validate(), Ok(()));
    host.action("payments/backend/bob", &keys).await.unwrap();
    assert_eq!(
        fake.actions()[1],
        ("payments/backend/bob".to_string(), keys)
    );
```

In `docs/plugin-protocol.md` §3, extend the actions list to:

```markdown
- `{ "action": "send_text", "text": <string>, "submit": <bool> }`
  (`action.json`)
- `{ "action": "send_keys", "steps": [<step>…], "delay_ms": <int> }`
  (`action-send-keys.json`). A step is `{ "key": "up" | "down" | "enter" |
  "escape" }` or `{ "text": <string> }`. The daemon pauses `delay_ms`
  (20 to 500, default 100) after every step. At most 64 steps, at most
  8000 ms in all, a `text` of 1 to 1024 bytes with no control character;
  anything else answers 400 naming the field. The call returns when the
  last step has been sent.
- `{ "action": "restart" }`
- `{ "action": "stop" }`
```

and in §6 change `twenty-two fixtures` to `twenty-three fixtures`.

- [ ] **Step 6: Run the SDK conformance test**

Run: `mise x -- cargo nextest run -p balerix-plugin-sdk --test conformance`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/balerix-api crates/balerix-plugin-sdk/tests/conformance.rs docs/plugin-protocol docs/plugin-protocol.md
git commit -m "feat(api): send_keys, a paced key sequence as a plugin action (Spec J §4.1)"
```

(The core workspace does not build as a whole until Task 3 adds the daemon's match arm. Tasks 1 to 3 are one reviewable unit; if a pre-commit hook runs `mise run check`, commit Tasks 1 to 3 together at the end of Task 3 instead.)

---

### Task 2: the `send_keys` port, the fake, and the tmux adapter

**Files:**
- Modify: `crates/balerix-core/src/ports.rs` (the `AgentRunner` trait, line ~245)
- Modify: `crates/balerix-core/src/fakes.rs` (`impl AgentRunner for FakeRunner`, line ~399, and its tests)
- Modify: `crates/balerix-runtime/src/tmux.rs` (`TmuxRunner` struct, `new`, `send_text`, new `send_keys`)
- Test: `crates/balerix-runtime/tests/tmux_it.rs`

**Interfaces:**
- Consumes: `balerix_api::{Key, KeyStep}` from Task 1 (`balerix-core` already depends on `balerix-api`).
- Produces: `fn send_keys(&self, agent: &AgentId, steps: &[KeyStep], delay: std::time::Duration) -> Result<(), RunnerError>;` on `AgentRunner`. `FakeRunner` records the call as `send_keys f/c/a [down,down,"teal-ish",enter] delay=100ms`.

- [ ] **Step 1: Write the failing fake test**

In the `tests` module of `crates/balerix-core/src/fakes.rs`, after `send_text_is_recorded_and_failable`:

```rust
    #[test]
    fn send_keys_is_recorded_and_failable() {
        use balerix_api::{Key, KeyStep};
        let r = FakeRunner::default();
        let steps = [
            KeyStep::Key(Key::Down),
            KeyStep::Key(Key::Down),
            KeyStep::Text("teal-ish".into()),
            KeyStep::Key(Key::Enter),
        ];
        r.send_keys(&id("f/c/a"), &steps, std::time::Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            r.calls(),
            vec!["send_keys f/c/a [down,down,\"teal-ish\",enter] delay=100ms"]
        );
        r.fail_next("send_keys", "f/c/a [escape] delay=20ms", "no window");
        assert!(
            r.send_keys(
                &id("f/c/a"),
                &[KeyStep::Key(Key::Escape)],
                std::time::Duration::from_millis(20)
            )
            .is_err()
        );
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `mise x -- cargo nextest run -p balerix-core send_keys_is_recorded`
Expected: compile error, no method `send_keys`.

- [ ] **Step 3: The port and the fake**

In `crates/balerix-core/src/ports.rs`, after `send_text` in `AgentRunner`:

```rust
    /// Presses each step in order, pausing `delay` after every one (Spec J
    /// §4.2). Returns when the last step has been sent.
    fn send_keys(
        &self,
        agent: &AgentId,
        steps: &[balerix_api::KeyStep],
        delay: std::time::Duration,
    ) -> Result<(), RunnerError>;
```

In `crates/balerix-core/src/fakes.rs`, after `FakeRunner::send_text`:

```rust
    fn send_keys(
        &self,
        agent: &AgentId,
        steps: &[balerix_api::KeyStep],
        delay: std::time::Duration,
    ) -> Result<(), RunnerError> {
        let shown: Vec<String> = steps
            .iter()
            .map(|s| match s {
                balerix_api::KeyStep::Key(k) => format!("{k:?}").to_lowercase(),
                balerix_api::KeyStep::Text(t) => format!("{t:?}"),
            })
            .collect();
        self.check(
            "send_keys",
            &format!("{agent} [{}] delay={}ms", shown.join(","), delay.as_millis()),
        )
    }
```

Run: `mise x -- cargo nextest run -p balerix-core send_keys_is_recorded`
Expected: PASS.

- [ ] **Step 4: Write the failing tmux tests**

Append to `crates/balerix-runtime/tests/tmux_it.rs`. Both follow the file's existing pattern (a pane whose script is `cat`, read back from a log); `cat -v` makes the arrow visible.

```rust
/// What `cat_v_pane` hands back. `_root` and `_server` are held for their
/// `Drop`: the temp root removes itself, so it must outlive the pane.
struct CatPane {
    r: TmuxRunner,
    id: AgentId,
    stdin_log: std::path::PathBuf,
    _server: KillServer,
    _root: balerix_runtime::testing::TempRoot,
}

/// A `cat -v` pane writing to `stdin.log`.
fn cat_v_pane(label: &str) -> Option<CatPane> {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return None;
    };
    let root = support::temp_root(label);
    let socket = format!("balerix-test-{label}-{}", std::process::id());
    let guard = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket);
    let id: AgentId = "f/c/a".parse().unwrap();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let stdin_log = agent_dir.join("stdin.log");
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec cat -v >> {}\n", stdin_log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script,
    };
    r.ensure_crew(&id.crew_ref()).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| {
        matches!(
            r.observe(&id.fleet).unwrap().get(&id),
            Some(ProcessState::Running { .. })
        )
    });
    std::thread::sleep(Duration::from_millis(300));
    Some(CatPane {
        r,
        id,
        stdin_log,
        _server: guard,
        _root: root,
    })
}

/// Spec J §4.2: steps arrive in order, a key as its escape sequence and
/// text literally (a trailing `;` included: tmux drops an unescaped one),
/// and the call takes at least one delay per step.
#[test]
fn send_keys_arrives_in_order_paced_with_semicolons_intact() {
    use balerix_api::{Key, KeyStep};
    let Some(pane) = cat_v_pane("keys") else {
        return;
    };
    // Named bindings, not `..`: a field left unbound is dropped at once,
    // which would kill the tmux server before the first key.
    let CatPane {
        r,
        id,
        stdin_log,
        _server,
        _root,
    } = pane;
    let steps = [
        KeyStep::Key(Key::Down),
        KeyStep::Key(Key::Up),
        KeyStep::Text("a;;".into()),
        KeyStep::Text("x\;".into()),
        KeyStep::Text("-l mid;dle".into()),
        KeyStep::Key(Key::Enter),
    ];
    let started = Instant::now();
    r.send_keys(&id, &steps, Duration::from_millis(50)).unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(50 * steps.len() as u64),
        "{:?}",
        started.elapsed()
    );
    wait_for(|| std::fs::read_to_string(&stdin_log).is_ok_and(|s| s.ends_with('\n')));
    let got = std::fs::read_to_string(&stdin_log).unwrap();
    // an arrow is ESC [ B or ESC O B depending on the pane's cursor-key mode
    let arrows = got.replace("^[O", "^[[");
    assert_eq!(arrows, "^[[B^[[Aa;;x\;-l mid;dle\n", "{got:?}");
    r.stop_crew(&id.crew_ref()).unwrap();
}

/// The per-agent send lock: a `send_text` that starts while a paced
/// `send_keys` is running lands after it, never inside it.
#[test]
fn a_send_text_never_lands_inside_a_running_send_keys() {
    use balerix_api::KeyStep;
    let Some(pane) = cat_v_pane("keylock") else {
        return;
    };
    let CatPane {
        r,
        id,
        stdin_log,
        _server,
        _root,
    } = pane;
    let r = std::sync::Arc::new(r);
    let steps: Vec<KeyStep> = (0..10).map(|i| KeyStep::Text(format!("k{i}"))).collect();
    let (r2, id2) = (r.clone(), id.clone());
    let keys = std::thread::spawn(move || {
        r2.send_keys(&id2, &steps, Duration::from_millis(40)).unwrap();
    });
    std::thread::sleep(Duration::from_millis(100));
    r.send_text(&id, "TEXT", true).unwrap();
    keys.join().unwrap();
    wait_for(|| std::fs::read_to_string(&stdin_log).is_ok_and(|s| s.ends_with('\n')));
    let got = std::fs::read_to_string(&stdin_log).unwrap();
    assert_eq!(got, "k0k1k2k3k4k5k6k7k8k9TEXT\n", "{got:?}");
    r.stop_crew(&id.crew_ref()).unwrap();
}
```

- [ ] **Step 5: Run them and watch them fail**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test tmux_it send_keys a_send_text_never`
Expected: compile error, `TmuxRunner` does not implement `send_keys`.

- [ ] **Step 6: Implement the adapter**

In `crates/balerix-runtime/src/tmux.rs`:

Add to the imports: `use std::collections::HashMap;` and `use std::sync::{Arc, Mutex};` (keep the existing `std::sync::atomic` import). `Key` and `KeyStep` are written as `balerix_api::Key` and `balerix_api::KeyStep`; `balerix-runtime` already depends on `balerix-api`.

Give the runner its lock map (no other code constructs `TmuxRunner` with a struct literal; `new` is the only constructor):

```rust
pub struct TmuxRunner {
    pub tmux: PathBuf,
    pub socket: String,
    /// One lock per agent, held for the whole of a `send_text` or a
    /// `send_keys` (Spec J §4.2): a paced key sequence lasts seconds, and
    /// another send landing inside it would corrupt both. Entries are never
    /// removed; agents are few.
    sends: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}
```

```rust
    pub fn new(tmux: PathBuf, socket: impl Into<String>) -> Self {
        Self {
            tmux,
            socket: socket.into(),
            sends: Mutex::new(HashMap::new()),
        }
    }

    fn send_lock(&self, agent: &AgentId) -> Arc<Mutex<()>> {
        self.sends
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(agent.to_string())
            .or_default()
            .clone()
    }

    /// `send-keys -l`, with the one thing `-l` does not make literal: tmux
    /// reads a `;` ending an argument as a command separator and drops it
    /// (`a;` arrives as `a`). Trailing semicolons go separately, each as the
    /// escaped argument `\;`, which arrives as `;`.
    fn send_literal(&self, id: &str, target: &str, text: &str) -> Result<(), RunnerError> {
        let body = text.trim_end_matches(';');
        if !body.is_empty() {
            self.run(id, &["send-keys", "-t", target, "-l", "--", body])?;
        }
        for _ in 0..(text.len() - body.len()) {
            self.run(id, &["send-keys", "-t", target, "-l", "--", "\;"])?;
        }
        Ok(())
    }
```

At the top of the existing `send_text` body, take the lock (leave the rest of `send_text` exactly as it is; its trailing-semicolon flaw is out of scope and reported separately):

```rust
        let lock = self.send_lock(agent);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
```

Add `send_keys` to `impl AgentRunner for TmuxRunner`, after `send_text`:

```rust
    fn send_keys(
        &self,
        agent: &AgentId,
        steps: &[balerix_api::KeyStep],
        delay: Duration,
    ) -> Result<(), RunnerError> {
        let lock = self.send_lock(agent);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let id = agent.to_string();
        let target = Self::window_target(agent);
        for step in steps {
            match step {
                balerix_api::KeyStep::Key(key) => {
                    // a fixed name per variant: nothing from the wire is a key name
                    let name = match key {
                        balerix_api::Key::Up => "Up",
                        balerix_api::Key::Down => "Down",
                        balerix_api::Key::Enter => "Enter",
                        balerix_api::Key::Escape => "Escape",
                    };
                    self.run(&id, &["send-keys", "-t", &target, name])?;
                }
                balerix_api::KeyStep::Text(text) => self.send_literal(&id, &target, text)?,
            }
            std::thread::sleep(delay);
        }
        Ok(())
    }
```

- [ ] **Step 7: Run the runtime tests**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test tmux_it`
Expected: PASS, including every pre-existing `send_text` test (the lock must not change them).

- [ ] **Step 8: Commit**

```bash
git add crates/balerix-core crates/balerix-runtime
git commit -m "feat(runtime): AgentRunner::send_keys over tmux, with a per-agent send lock (Spec J §4.2)"
```

---

### Task 3: the daemon executes and validates `send_keys`

**Files:**
- Modify: `crates/balerix-server/src/daemon.rs` (`execute_action`, line ~708; tests module)
- Modify: `crates/balerix-server/tests/events_it.rs` (`host_routes_are_gated_by_needs_and_kv_and_actions_work`)
- Modify: `docs/THREAT-MODEL.md` (the "Interceptors fail open" bullet and the G-5 bullet)

**Interfaces:**
- Consumes: `PluginAction::SendKeys`, `PluginAction::validate` (Task 1); `AgentRunner::send_keys` (Task 2).
- Produces: `POST …/actions` with a `send_keys` body runs it; an invalid one answers 400 `{ "error": "<field>: …" }` and never reaches the runner.

- [ ] **Step 1: Write the failing daemon test**

In the `tests` module of `crates/balerix-server/src/daemon.rs`, after `events_run_the_chain_and_actions_reach_the_runner_and_the_actor`:

```rust
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_keys_reaches_the_runner_and_an_invalid_one_never_does() {
        use balerix_api::{Key, KeyStep};
        let w = world().await;
        let agent: AgentId = "f/c/a".parse().unwrap();
        let ok = PluginAction::SendKeys {
            steps: vec![KeyStep::Key(Key::Down), KeyStep::Key(Key::Enter)],
            delay_ms: 20,
        };
        w.daemon.execute_action(&agent, &ok, Some("matrix")).await.unwrap();
        assert!(
            w.h.runner
                .calls()
                .contains(&"send_keys f/c/a [down,enter] delay=20ms".to_string()),
            "{:?}",
            w.h.runner.calls()
        );

        let before = w.h.runner.calls().len();
        let too_fast = PluginAction::SendKeys {
            steps: vec![KeyStep::Key(Key::Enter)],
            delay_ms: 0,
        };
        let err = w
            .daemon
            .execute_action(&agent, &too_fast, Some("matrix"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DaemonError::Invalid(m) if m.starts_with("delay_ms:")),
            "{err:?}"
        );
        assert_eq!(w.h.runner.calls().len(), before, "never reached the runner");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `mise x -- cargo nextest run -p balerix-server send_keys_reaches`
Expected: compile error, non-exhaustive patterns: `SendKeys` not covered in `execute_action`.

- [ ] **Step 3: Implement**

In `Daemon::execute_action`, make validation the first statement, before the metrics lines, so a refused action is not counted as run:

```rust
        action.validate().map_err(DaemonError::Invalid)?;
```

and add the arm after `PluginAction::SendText`:

```rust
            PluginAction::SendKeys { steps, delay_ms } => {
                let runner = self.ports.runner.clone();
                let (id, steps) = (agent.clone(), steps.clone());
                let delay = std::time::Duration::from_millis(*delay_ms);
                tokio::task::spawn_blocking(move || runner.send_keys(&id, &steps, delay))
                    .await
                    .map_err(|e| DaemonError::Internal(e.to_string()))?
                    .map_err(|e| DaemonError::Internal(e.to_string()))
            }
```

`DaemonError::Invalid` already maps to 400 in `crates/balerix-server/src/api.rs`.

- [ ] **Step 4: The route test**

In `crates/balerix-server/tests/events_it.rs`, inside `host_routes_are_gated_by_needs_and_kv_and_actions_work`, directly after the `host.action("f/c/a", &PluginAction::Restart)` block and its `wait_for`, add:

```rust
    let e = host
        .action(
            "f/c/a",
            &PluginAction::SendKeys {
                steps: vec![balerix_api::KeyStep::Key(balerix_api::Key::Enter)],
                delay_ms: 5,
            },
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("delay_ms"), "{e}");
```

- [ ] **Step 5: Run the server tests, then the whole core gate**

Run: `mise x -- cargo nextest run -p balerix-server send_keys_reaches host_routes_are_gated`
Expected: PASS.

Run: `mise run check`
Expected: PASS. The core workspace builds as a whole again from this task on.

- [ ] **Step 6: The threat model**

In `docs/THREAT-MODEL.md`, in the bullet that begins `**Interceptors fail open**`, replace the sentence `A plugin with `actions` can stop, restart or type into any agent it is active for;` with:

```markdown
A plugin with `actions` can stop, restart, type into, or press Up, Down, Enter and Escape in any agent it is active for — no other key: `send_keys` takes a closed `Key` enum the runner matches to fixed tmux key names, and a `text` step refuses control characters, so a step cannot smuggle an escape sequence (`crates/balerix-api/src/protocol.rs::PluginAction::validate`, Spec J §4);
```

In the bullet that begins `**Matrix room membership is the plugin's access control (Spec G-5).**`, after the sentence ending `push rights.`, insert:

```markdown
Since Spec J a room member can also answer an agent's `AskUserQuestion` dialog from the thread; an answer is strictly narrower than the prompt a member could already send.
```

- [ ] **Step 7: Commit**

```bash
git add crates/balerix-server docs/THREAT-MODEL.md
git commit -m "feat(server): execute and validate send_keys; an invalid one answers 400 (Spec J §4.3)"
```

---

### Task 4: `question.rs` — parse the dialog and match a reply

**Files:**
- Create: `plugins/matrix/src/question.rs`
- Modify: `plugins/matrix/src/lib.rs` (add `pub mod question;` after `pub mod plugin;`)

**Interfaces:**
- Consumes: `balerix_api::MAX_KEY_TEXT` (Task 1).
- Produces, all `pub` in `crate::question`:
  - `const TOOL: &str = "AskUserQuestion"`
  - `struct Opt { label: String, description: String }`
  - `struct Question { text: String, header: String, multi_select: bool, options: Vec<Opt> }` with `fn name(&self) -> &str`
  - `struct Selection { options: Vec<usize>, other: Option<String> }` — `options` 0-based, ascending, unique
  - `enum Matched { Answers { selections: Vec<Selection>, exact: bool }, Skip }`
  - `struct Refusal(pub String)` — markdown, shown in the thread
  - `fn parse(tool_input: &Value) -> Option<Vec<Question>>`
  - `fn match_reply(questions: &[Question], reply: &str) -> Result<Matched, Refusal>`

All commands in Tasks 4 to 9 run from the repository root with the plugin's manifest:

```bash
T="CARGO_TARGET_DIR=plugins/matrix/target mise x -- cargo test --manifest-path plugins/matrix/Cargo.toml"
```

- [ ] **Step 1: Write the failing tests**

Create `plugins/matrix/src/question.rs` with only the tests and the fixtures they share:

```rust
//! `AskUserQuestion` (Spec J §6): parse the dialog, match a thread reply to
//! its options, plan the keystrokes. Pure: no I/O and no clock, so every
//! rule is testable, and the rules are the ones Spec J §2 measured.

#[cfg(test)]
pub(crate) mod fixtures {
    use serde_json::{Value, json};

    pub fn color() -> Value {
        json!({ "question": "Which color?", "header": "Color", "multiSelect": false,
                "options": [
                    { "label": "Red", "description": "A warm, bold color" },
                    { "label": "Green", "description": "A calm, natural color" },
                    { "label": "Blue", "description": "A cool, serene color" } ] })
    }

    pub fn size() -> Value {
        json!({ "question": "Which size?", "header": "Size", "multiSelect": false,
                "options": [
                    { "label": "Small", "description": "" },
                    { "label": "Medium", "description": "" },
                    { "label": "Large", "description": "" } ] })
    }

    pub fn colors_multi() -> Value {
        let mut q = color();
        q["question"] = json!("Which colors?");
        q["header"] = json!("Colors");
        q["multiSelect"] = json!(true);
        q
    }

    pub fn input(questions: &[Value]) -> Value {
        json!({ "questions": questions })
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use serde_json::json;

    fn parsed(questions: &[serde_json::Value]) -> Vec<Question> {
        parse(&input(questions)).unwrap()
    }

    fn answers(questions: &[Question], reply: &str) -> (Vec<Selection>, bool) {
        match match_reply(questions, reply) {
            Ok(Matched::Answers { selections, exact }) => (selections, exact),
            other => panic!("{reply:?}: {other:?}"),
        }
    }

    fn option(i: usize) -> Selection {
        Selection {
            options: vec![i],
            other: None,
        }
    }

    fn refusal(questions: &[Question], reply: &str) -> String {
        match match_reply(questions, reply) {
            Err(Refusal(reason)) => reason,
            other => panic!("{reply:?} was not refused: {other:?}"),
        }
    }

    #[test]
    fn parse_reads_questions_and_refuses_anything_else() {
        let q = parsed(&[color()]);
        assert_eq!(q.len(), 1);
        assert_eq!((q[0].text.as_str(), q[0].header.as_str()), ("Which color?", "Color"));
        assert!(!q[0].multi_select);
        assert_eq!(q[0].options[2].label, "Blue");
        assert_eq!(q[0].options[0].description, "A warm, bold color");
        assert_eq!(q[0].name(), "Color");
        assert!(parsed(&[colors_multi()])[0].multi_select);

        let mut headless = color();
        headless["header"] = json!("");
        assert_eq!(parsed(&[headless])[0].name(), "Which color?");

        for bad in [
            json!({}),
            json!({ "questions": [] }),
            json!({ "questions": [{ "question": "Q?", "options": [] }] }),
            json!({ "questions": [{ "question": "Q?", "options": [{ "label": " " }] }] }),
            json!({ "questions": [{ "options": [{ "label": "A" }] }] }),
            json!({ "questions": "no" }),
        ] {
            assert_eq!(parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_ladder_number_label_prefix_word() {
        let q = parsed(&[color()]);
        assert_eq!(answers(&q, "2"), (vec![option(1)], true));
        assert_eq!(answers(&q, " 3. "), (vec![option(2)], true));
        assert_eq!(answers(&q, "BLUE!"), (vec![option(2)], true));
        assert_eq!(answers(&q, "gre"), (vec![option(1)], false), "a unique prefix is inexact");

        let mut long = color();
        long["options"][0]["label"] = json!("Red (Recommended)");
        long["options"][1]["label"] = json!("Dark green");
        let q = parsed(&[long]);
        assert_eq!(answers(&q, "green"), (vec![option(1)], false), "a unique word");
        assert_eq!(answers(&q, "red"), (vec![option(0)], false), "a prefix of a longer label");
        assert_eq!(answers(&q, "Red (recommended)"), (vec![option(0)], true));
    }

    #[test]
    fn ambiguity_and_no_match_are_refused_with_what_to_do() {
        let mut close = color();
        close["options"][0]["label"] = json!("Green tea");
        close["options"][1]["label"] = json!("Green apple");
        let q = parsed(&[close]);
        let r = refusal(&q, "green");
        assert!(r.contains("1. Green tea") && r.contains("2. Green apple"), "{r}");

        let q = parsed(&[color()]);
        let r = refusal(&q, "purple please");
        assert!(r.contains("matches nothing") && r.contains("1. Red, 2. Green, 3. Blue"), "{r}");
        assert!(refusal(&q, "7").contains("no option 7"));
        assert!(refusal(&q, "0").contains("no option 0"));
        assert!(refusal(&q, "   ").contains("no answer"));
    }

    #[test]
    fn other_is_free_text_and_never_exact() {
        let q = parsed(&[color()]);
        assert_eq!(
            answers(&q, "Other: teal-ish, really"),
            (
                vec![Selection {
                    options: vec![],
                    other: Some("teal-ish, really".into())
                }],
                false
            )
        );
        assert!(refusal(&q, "other:").contains("needs some text"));
        assert!(refusal(&q, "red, other: pink").contains("one answer"));
        assert!(refusal(&q, "other: two\nlines").contains("one line"));
        assert!(refusal(&q, &format!("other: {}", "x".repeat(1025))).contains("1024"));
    }

    #[test]
    fn multi_select_takes_a_comma_list_in_any_order() {
        let q = parsed(&[colors_multi()]);
        assert_eq!(
            answers(&q, "blue, 1"),
            (
                vec![Selection {
                    options: vec![0, 2],
                    other: None
                }],
                true
            )
        );
        assert_eq!(
            answers(&q, "2, other: a bit of gold"),
            (
                vec![Selection {
                    options: vec![1],
                    other: Some("a bit of gold".into())
                }],
                false
            )
        );
        assert!(refusal(&q, "red, 1").contains("twice"));
    }

    #[test]
    fn several_questions_take_lines_in_order_or_header_lines_in_any_order() {
        let q = parsed(&[color(), size()]);
        assert_eq!(answers(&q, "blue\n\n2\n"), (vec![option(2), option(1)], true));
        assert_eq!(answers(&q, "size: large\nCOLOR: 1"), (vec![option(0), option(2)], true));
        assert!(refusal(&q, "blue").contains("2 questions need 2 lines"));
        assert!(refusal(&q, "color: red\ncolor: blue").contains("twice"));
        assert!(refusal(&q, "color: red\nsize: 2\nsize: 3").contains("twice"));
        // a header line for one question only falls back to positional, which
        // then refuses on the count
        assert!(refusal(&q, "color: red").contains("2 questions need 2 lines"));
    }

    #[test]
    fn skip_declines() {
        let q = parsed(&[color(), size()]);
        assert_eq!(match_reply(&q, " Skip "), Ok(Matched::Skip));
    }
}
```

Add `pub mod question;` to `plugins/matrix/src/lib.rs`.

- [ ] **Step 2: Run them and watch them fail**

Run: `eval "$T" question`
Expected: compile errors, `cannot find function parse`.

- [ ] **Step 3: Implement**

Insert into `plugins/matrix/src/question.rs`, between the module doc comment and `mod fixtures`:

```rust
use std::collections::BTreeSet;

use balerix_api::MAX_KEY_TEXT;
use serde_json::Value;

/// The tool whose `PreToolUse` opens a question.
pub const TOOL: &str = "AskUserQuestion";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opt {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Verbatim: it is the key of `PostToolUse`'s `answers` map.
    pub text: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<Opt>,
}

impl Question {
    /// What the thread calls this question: its header, else its text.
    pub fn name(&self) -> &str {
        if self.header.is_empty() {
            &self.text
        } else {
            &self.header
        }
    }
}

/// One question's answer. `options` is 0-based, ascending and unique. A
/// single-select holds exactly one option or `other`; a multi-select holds
/// at least one of either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub options: Vec<usize>,
    pub other: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matched {
    /// `exact` is true only when every item was a number or a whole label.
    Answers {
        selections: Vec<Selection>,
        exact: bool,
    },
    Skip,
}

/// Why a reply was not matched, as markdown for the thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

/// `tool_input` as questions; `None` for any other shape (Spec J §6.1).
pub fn parse(tool_input: &Value) -> Option<Vec<Question>> {
    let questions: Vec<Question> = tool_input
        .get("questions")?
        .as_array()?
        .iter()
        .map(|q| {
            let options: Vec<Opt> = q
                .get("options")?
                .as_array()?
                .iter()
                .map(|o| {
                    let label = o.get("label")?.as_str()?.trim().to_string();
                    (!label.is_empty()).then(|| Opt {
                        label,
                        description: text_of(o, "description"),
                    })
                })
                .collect::<Option<_>>()?;
            if options.is_empty() {
                return None;
            }
            Some(Question {
                text: q.get("question")?.as_str()?.to_string(),
                header: text_of(q, "header"),
                multi_select: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                options,
            })
        })
        .collect::<Option<_>>()?;
    (!questions.is_empty()).then_some(questions)
}

fn text_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Case-folded, punctuation to spaces, whitespace collapsed.
fn normalise(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn options_list(q: &Question) -> String {
    q.options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("{}. {}", i + 1, o.label))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A thread reply against the open questions (Spec J §6.2).
pub fn match_reply(questions: &[Question], reply: &str) -> Result<Matched, Refusal> {
    let reply = reply.trim();
    if reply.eq_ignore_ascii_case("skip") {
        return Ok(Matched::Skip);
    }
    let parts = split(questions, reply)?;
    let mut exact = true;
    let mut selections = Vec::with_capacity(questions.len());
    for (q, part) in questions.iter().zip(parts) {
        let (selection, e) = match_answer(q, part)?;
        exact &= e;
        selections.push(selection);
    }
    Ok(Matched::Answers { selections, exact })
}

/// One part of the reply per question, in question order.
fn split<'a>(questions: &[Question], reply: &'a str) -> Result<Vec<&'a str>, Refusal> {
    if questions.len() == 1 {
        return Ok(vec![reply]);
    }
    let lines: Vec<&str> = reply
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // `header: answer` lines in any order — only when every line is one and
    // they cover more than a single question.
    let by_header: Option<Vec<(usize, &str)>> = lines
        .iter()
        .map(|line| {
            let (head, rest) = line.split_once(':')?;
            let head = normalise(head);
            let i = questions
                .iter()
                .position(|q| !q.header.is_empty() && normalise(&q.header) == head)?;
            Some((i, rest.trim()))
        })
        .collect();
    if let Some(pairs) = by_header.filter(|p| p.len() > 1) {
        let mut parts: Vec<Option<&str>> = vec![None; questions.len()];
        for (i, rest) in pairs {
            if parts[i].replace(rest).is_some() {
                return Err(Refusal(format!(
                    "**{}** is answered twice.",
                    questions[i].name()
                )));
            }
        }
        return parts
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                p.ok_or_else(|| Refusal(format!("no answer for **{}**.", questions[i].name())))
            })
            .collect();
    }
    if lines.len() != questions.len() {
        return Err(Refusal(format!(
            "{n} questions need {n} lines, one answer per line in order; got {}.",
            lines.len(),
            n = questions.len()
        )));
    }
    Ok(lines)
}

/// `other:` at the start of the answer or right after a comma takes the
/// rest of the answer, commas included.
fn split_other(part: &str) -> (&str, Option<&str>) {
    const MARK: &str = "other:";
    let lower = part.to_ascii_lowercase(); // same byte offsets as `part`
    let mut from = 0;
    while let Some(found) = lower[from..].find(MARK) {
        let at = from + found;
        let before = part[..at].trim_end();
        if before.is_empty() || before.ends_with(',') {
            return (
                before.trim_end_matches(',').trim_end(),
                Some(part[at + MARK.len()..].trim()),
            );
        }
        from = at + MARK.len();
    }
    (part, None)
}

fn match_answer(q: &Question, part: &str) -> Result<(Selection, bool), Refusal> {
    let (listed, other) = split_other(part);
    let other = match other {
        Some(text) => Some(check_other(q, text)?),
        None => None,
    };
    let items: Vec<&str> = if q.multi_select {
        listed
            .split(',')
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .collect()
    } else if listed.trim().is_empty() {
        Vec::new()
    } else {
        vec![listed.trim()]
    };
    // free text is never exact: a typo must not become an answer unasked
    let mut exact = other.is_none();
    let mut options = BTreeSet::new();
    for item in items {
        let (i, e) = match_item(q, item)?;
        exact &= e;
        if !options.insert(i) {
            return Err(Refusal(format!(
                "**{}** names {} twice.",
                q.name(),
                q.options[i].label
            )));
        }
    }
    let options: Vec<usize> = options.into_iter().collect();
    let chosen = options.len() + usize::from(other.is_some());
    if chosen == 0 {
        return Err(Refusal(format!(
            "no answer given for **{}**. Options: {}",
            q.name(),
            options_list(q)
        )));
    }
    if !q.multi_select && chosen > 1 {
        return Err(Refusal(format!(
            "**{}** takes one answer: an option or `other:`, not both.",
            q.name()
        )));
    }
    Ok((Selection { options, other }, exact))
}

fn check_other(q: &Question, text: &str) -> Result<String, Refusal> {
    if text.is_empty() {
        return Err(Refusal(format!(
            "`other:` needs some text for **{}**.",
            q.name()
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(Refusal(format!(
            "`other:` for **{}** must be one line.",
            q.name()
        )));
    }
    if text.len() > MAX_KEY_TEXT {
        return Err(Refusal(format!(
            "`other:` for **{}** is limited to {MAX_KEY_TEXT} bytes.",
            q.name()
        )));
    }
    Ok(text.to_string())
}

/// The ladder: number, whole label, unique prefix, unique word. The `bool`
/// is whether the rung was exact.
fn match_item(q: &Question, item: &str) -> Result<(usize, bool), Refusal> {
    let digits = item.trim().trim_end_matches(['.', ')', ':']);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        return match digits.parse::<usize>() {
            Ok(n) if (1..=q.options.len()).contains(&n) => Ok((n - 1, true)),
            _ => Err(Refusal(format!(
                "**{}** has no option {digits}. Options: {}",
                q.name(),
                options_list(q)
            ))),
        };
    }
    let want = normalise(item);
    if want.is_empty() {
        return Err(Refusal(format!(
            "no answer given for **{}**. Options: {}",
            q.name(),
            options_list(q)
        )));
    }
    let labels: Vec<String> = q.options.iter().map(|o| normalise(&o.label)).collect();
    if let Some(i) = labels.iter().position(|l| *l == want) {
        return Ok((i, true));
    }
    let unique = |hits: Vec<usize>| match hits.as_slice() {
        [] => None,
        [i] => Some(Ok((*i, false))),
        many => Some(Err(Refusal(format!(
            "\"{}\" could be {} in **{}**. Reply with the number.",
            item.trim(),
            many.iter()
                .map(|i| format!("{}. {}", i + 1, q.options[*i].label))
                .collect::<Vec<_>>()
                .join(" or "),
            q.name()
        )))),
    };
    let by_prefix = (0..labels.len())
        .filter(|&i| labels[i].starts_with(&want))
        .collect();
    if let Some(result) = unique(by_prefix) {
        return result;
    }
    let words: Vec<&str> = want.split(' ').collect();
    let by_words = (0..labels.len())
        .filter(|&i| words.iter().all(|w| labels[i].split(' ').any(|l| l == *w)))
        .collect();
    if let Some(result) = unique(by_words) {
        return result;
    }
    Err(Refusal(format!(
        "\"{}\" matches nothing in **{}**. Options: {}",
        item.trim(),
        q.name(),
        options_list(q)
    )))
}
```

- [ ] **Step 4: Run the tests**

Run: `eval "$T" question`
Expected: PASS, 7 tests. Then `mise run plugin matrix` must pass (clippy denies warnings in the plugin too).

- [ ] **Step 5: Commit**

```bash
git add plugins/matrix/src/question.rs plugins/matrix/src/lib.rs
git commit -m "feat(matrix): parse AskUserQuestion and match a reply to its options (Spec J §6.1, §6.2)"
```

---

### Task 5: `question.rs` — the key plan, the echo text, and the model that guards them

**Files:**
- Modify: `plugins/matrix/src/question.rs`
- Modify: `plugins/matrix/Cargo.toml` (`[dev-dependencies]`), `plugins/matrix/Cargo.lock`

**Interfaces:**
- Consumes: Task 4's types; `balerix_api::{Key, KeyStep}`.
- Produces, `pub` in `crate::question`:
  - `fn plan(questions: &[Question], selections: &[Selection]) -> Vec<KeyStep>`
  - `fn skip_plan() -> Vec<KeyStep>` — one Escape
  - `fn describe(questions: &[Question], selections: &[Selection]) -> String` — `Color → Blue · Size → Medium`
  - `fn describe_recorded(questions: &[Question], answers: &Value) -> String`
  - `fn recorded_matches(questions: &[Question], selections: &[Selection], answers: &Value) -> bool`

- [ ] **Step 1: Add the dev-dependency**

In `plugins/matrix/Cargo.toml` under `[dev-dependencies]`, add:

```toml
# The model-based test of the key plan (Spec J §10). Same exact version as
# the core workspace's.
proptest = "1.11.0"
```

- [ ] **Step 2: Write the failing tests**

Add to the `tests` module of `plugins/matrix/src/question.rs`:

```rust
    use balerix_api::{Key, KeyStep};

    /// `D` Down, `E` Enter, `T` the text step `t`.
    fn keys(pattern: &str, t: &str) -> Vec<KeyStep> {
        pattern
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| match c {
                'D' => KeyStep::Key(Key::Down),
                'E' => KeyStep::Key(Key::Enter),
                'T' => KeyStep::Text(t.to_string()),
                other => panic!("{other}"),
            })
            .collect()
    }

    fn sel(options: &[usize], other: Option<&str>) -> Selection {
        Selection {
            options: options.to_vec(),
            other: other.map(str::to_string),
        }
    }

    /// Each case is a row of Spec J §2's table: the sequence that was
    /// measured against the real `claude`.
    #[test]
    fn the_plan_is_the_measured_sequence() {
        let one = parsed(&[color()]);
        assert_eq!(plan(&one, &[sel(&[1], None)]), keys("DE", ""));
        assert_eq!(plan(&one, &[sel(&[0], None)]), keys("E", ""));
        assert_eq!(plan(&one, &[sel(&[], Some("teal"))]), keys("DDD T E", "teal"));

        let two = parsed(&[color(), size()]);
        assert_eq!(
            plan(&two, &[sel(&[2], None), sel(&[1], None)]),
            keys("DDE DE E", "")
        );
        assert_eq!(
            plan(&two, &[sel(&[], Some("teal")), sel(&[2], None)]),
            keys("DDD T E  DDE  E", "teal")
        );

        let multi = parsed(&[colors_multi()]);
        assert_eq!(plan(&multi, &[sel(&[0, 2], None)]), keys("E DDE DDE E", ""));
        assert_eq!(
            plan(&multi, &[sel(&[1], Some("gold"))]),
            keys("DE DD T DE E", "gold")
        );
        assert_eq!(plan(&multi, &[sel(&[], Some("gold"))]), keys("DDD T DE E", "gold"));

        let mixed = parsed(&[colors_multi(), size()]);
        assert_eq!(
            plan(&mixed, &[sel(&[0, 2], None), sel(&[1], None)]),
            keys("E DDE DDE  DE  E", "")
        );
        assert_eq!(skip_plan(), vec![KeyStep::Key(Key::Escape)]);
    }

    #[test]
    fn describe_and_the_recorded_answers() {
        let q = parsed(&[colors_multi(), size()]);
        let chosen = [sel(&[0, 2], Some("gold")), sel(&[1], None)];
        assert_eq!(describe(&q, &chosen), "Colors → Red, Blue, gold · Size → Medium");

        let same = json!({ "Which colors?": "Blue, gold, Red", "Which size?": "Medium" });
        assert!(recorded_matches(&q, &chosen, &same), "multi-select compares as a set");
        let differs = json!({ "Which colors?": "Red, Blue, gold", "Which size?": "Small" });
        assert!(!recorded_matches(&q, &chosen, &differs));
        assert!(!recorded_matches(&q, &chosen, &json!({ "Which size?": "Medium" })));
        assert!(!recorded_matches(&q, &chosen, &json!(null)));
        assert_eq!(
            describe_recorded(&q, &differs),
            "Colors → Red, Blue, gold · Size → Small"
        );
        assert_eq!(
            describe_recorded(&q, &json!({ "Which size?": "Small" })),
            "Colors → (nothing) · Size → Small"
        );
    }

    /// The dialog as Spec J §2 measured it: what each key does. `plan` is
    /// correct when, fed to this, it records exactly the selections and ends
    /// submitted. The model is only as true as `mise run verify-questions`.
    struct Dialog<'a> {
        questions: &'a [Question],
        /// The question on screen; `questions.len()` is the review screen.
        at: usize,
        row: usize,
        picked: Vec<Selection>,
        submitted: bool,
        /// A key landed somewhere the plan must never reach.
        broken: Option<String>,
    }

    impl<'a> Dialog<'a> {
        fn new(questions: &'a [Question]) -> Self {
            Self {
                questions,
                at: 0,
                row: 0,
                picked: vec![sel(&[], None); questions.len()],
                submitted: false,
                broken: None,
            }
        }

        fn advance(&mut self) {
            self.at += 1;
            self.row = 0;
            let lone_single = self.questions.len() == 1 && !self.questions[0].multi_select;
            if self.at == self.questions.len() && lone_single {
                self.submitted = true; // no review screen for one single-select
            }
        }

        fn press(&mut self, step: &KeyStep) {
            if self.submitted || self.broken.is_some() {
                self.broken = Some(format!("{step:?} after the end"));
                return;
            }
            if self.at == self.questions.len() {
                // review: `Submit answers` is highlighted
                match step {
                    KeyStep::Key(Key::Enter) => self.submitted = true,
                    other => self.broken = Some(format!("{other:?} on the review screen")),
                }
                return;
            }
            // copy the `&'a [Question]` out so `q` does not borrow `self`
            let questions = self.questions;
            let q = &questions[self.at];
            let n = q.options.len();
            let row = self.row;
            match step {
                KeyStep::Key(Key::Down) => self.row += 1,
                KeyStep::Text(t) if self.row == n => self.picked[self.at].other = Some(t.clone()),
                KeyStep::Key(Key::Enter) if self.row < n && q.multi_select => {
                    let options = &mut self.picked[self.at].options;
                    match options.iter().position(|o| *o == row) {
                        // Enter toggles
                        Some(i) => {
                            options.remove(i);
                        }
                        None => options.push(row),
                    }
                }
                KeyStep::Key(Key::Enter) if self.row < n => {
                    self.picked[self.at].options = vec![self.row];
                    self.advance();
                }
                // single-select: Enter on the typed text submits it
                KeyStep::Key(Key::Enter)
                    if self.row == n && !q.multi_select && self.picked[self.at].other.is_some() =>
                {
                    self.advance()
                }
                // multi-select: row n+1 is `Submit` or `Next`
                KeyStep::Key(Key::Enter) if self.row == n + 1 && q.multi_select => self.advance(),
                other => self.broken = Some(format!("{other:?} at row {} of {}", self.row, q.name())),
            }
        }
    }

    use proptest::prelude::*;

    fn dialogs() -> impl Strategy<Value = (Vec<Question>, Vec<Selection>)> {
        prop::collection::vec((any::<bool>(), 2usize..=4, any::<u8>(), any::<bool>()), 1..=4)
            .prop_map(|specs| {
                specs
                    .into_iter()
                    .enumerate()
                    .map(|(qi, (multi, n, bits, other))| {
                        let q = Question {
                            text: format!("Q{qi}?"),
                            header: format!("H{qi}"),
                            multi_select: multi,
                            options: (0..n)
                                .map(|i| Opt {
                                    label: format!("opt{i}"),
                                    description: String::new(),
                                })
                                .collect(),
                        };
                        let s = if multi {
                            let options: Vec<usize> =
                                (0..n).filter(|i| bits >> i & 1 == 1).collect();
                            let other = (other || options.is_empty()).then(|| "free".to_string());
                            Selection { options, other }
                        } else if other {
                            sel(&[], Some("free"))
                        } else {
                            sel(&[bits as usize % n], None)
                        };
                        (q, s)
                    })
                    .unzip()
            })
    }

    proptest! {
        #[test]
        fn the_plan_drives_the_model_to_exactly_the_selections((questions, selections) in dialogs()) {
            let steps = plan(&questions, &selections);
            prop_assert!(steps.len() <= balerix_api::MAX_KEY_STEPS, "{} steps", steps.len());
            let mut dialog = Dialog::new(&questions);
            for step in &steps {
                dialog.press(step);
            }
            prop_assert_eq!(&dialog.broken, &None);
            prop_assert!(dialog.submitted, "not submitted: {:?}", steps);
            prop_assert_eq!(dialog.picked, selections);
        }
    }
```

- [ ] **Step 3: Run them and watch them fail**

Run: `eval "$T" question`
Expected: compile errors, `cannot find function plan`.

- [ ] **Step 4: Implement**

Change the import at the top of `question.rs` to `use balerix_api::{Key, KeyStep, MAX_KEY_TEXT};` and add after `match_item`:

```rust
/// The keystrokes that answer the dialog (Spec J §6.3). Rows of a question:
/// the options, then `Type something`, then — multi-select only — `Submit`
/// or `Next`. The cursor starts on the first row of every question. Only
/// Down, Enter and text: Tab behaves differently from different rows.
pub fn plan(questions: &[Question], selections: &[Selection]) -> Vec<KeyStep> {
    fn down(steps: &mut Vec<KeyStep>, count: usize) {
        steps.extend(std::iter::repeat_n(KeyStep::Key(Key::Down), count));
    }
    let enter = KeyStep::Key(Key::Enter);
    let mut steps = Vec::new();
    for (q, s) in questions.iter().zip(selections) {
        let n = q.options.len();
        if q.multi_select {
            let mut row = 0;
            for &option in &s.options {
                down(&mut steps, option - row);
                row = option;
                steps.push(enter.clone()); // toggles
            }
            if let Some(text) = &s.other {
                down(&mut steps, n - row);
                row = n;
                steps.push(KeyStep::Text(text.clone())); // typing ticks the row
            }
            down(&mut steps, n + 1 - row);
            steps.push(enter.clone()); // `Submit` or `Next`
        } else if let Some(text) = &s.other {
            down(&mut steps, n);
            steps.push(KeyStep::Text(text.clone()));
            steps.push(enter.clone());
        } else {
            down(&mut steps, s.options[0]);
            steps.push(enter.clone());
        }
    }
    // Everything but a lone single-select question ends on the review
    // screen, with `Submit answers` highlighted.
    let lone_single = questions.len() == 1 && !questions[0].multi_select;
    if !lone_single {
        steps.push(enter);
    }
    steps
}

/// Declining the dialog: Claude sees "User declined to answer questions".
pub fn skip_plan() -> Vec<KeyStep> {
    vec![KeyStep::Key(Key::Escape)]
}

fn answer_text(q: &Question, s: &Selection) -> String {
    s.options
        .iter()
        .map(|&i| q.options[i].label.clone())
        .chain(s.other.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `Color → Blue · Size → Medium`, for the echo.
pub fn describe(questions: &[Question], selections: &[Selection]) -> String {
    questions
        .iter()
        .zip(selections)
        .map(|(q, s)| format!("{} → {}", q.name(), answer_text(q, s)))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The same line from `PostToolUse`'s `tool_response.answers`.
pub fn describe_recorded(questions: &[Question], answers: &Value) -> String {
    questions
        .iter()
        .map(|q| {
            let got = answers
                .get(&q.text)
                .and_then(Value::as_str)
                .unwrap_or("(nothing)");
            format!("{} → {got}", q.name())
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Whether Claude recorded what was intended (Spec J §7.3). A multi-select
/// answer is its labels joined with `", "`, in Claude's order, so it is
/// compared as a set.
pub fn recorded_matches(questions: &[Question], selections: &[Selection], answers: &Value) -> bool {
    questions.iter().zip(selections).all(|(q, s)| {
        let Some(got) = answers.get(&q.text).and_then(Value::as_str) else {
            return false;
        };
        let want = answer_text(q, s);
        let set = |t: &str| t.split(", ").map(str::to_string).collect::<BTreeSet<_>>();
        got == want || (q.multi_select && set(got) == set(&want))
    })
}
```

`plan` indexes `s.options[0]` for a single-select without `other`. `match_reply` guarantees that shape; `plan` is only ever called with its output.

- [ ] **Step 5: Run the tests**

Run: `eval "$T" question`
Expected: PASS, 10 tests, the property test included (256 cases).

- [ ] **Step 6: Commit**

```bash
git add plugins/matrix/src/question.rs plugins/matrix/Cargo.toml plugins/matrix/Cargo.lock
git commit -m "feat(matrix): the key plan for a question, guarded by a model of the dialog (Spec J §6.3)

Adds proptest 1.11.0 to the plugin's dev-dependencies for the model-based
test; the same exact version the core workspace uses."
```

---

### Task 6: render the question

**Files:**
- Modify: `plugins/matrix/src/render.rs`
- Create (by insta): `plugins/matrix/src/snapshots/balerix_plugin_matrix__render__tests__a_question_*.snap`

**Interfaces:**
- Consumes: `crate::question::Question` (Task 4) and its test fixtures.
- Produces: `pub fn question_message(questions: &[Question]) -> String` in `crate::render`.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `plugins/matrix/src/render.rs`:

```rust
    use crate::question::{self, fixtures};

    fn questions(list: &[serde_json::Value]) -> Vec<question::Question> {
        question::parse(&fixtures::input(list)).unwrap()
    }

    #[test]
    fn a_question_lists_its_numbered_options() {
        insta::assert_snapshot!(question_message(&questions(&[fixtures::color()])));
    }

    #[test]
    fn a_question_set_numbers_the_questions_and_marks_a_multi_select() {
        insta::assert_snapshot!(question_message(&questions(&[
            fixtures::colors_multi(),
            fixtures::size()
        ])));
    }

    #[test]
    fn an_unparsable_question_falls_back_to_the_generic_tool_line() {
        let e = event(
            "f/c/a",
            "PreToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_input": { "questions": [] } }),
        );
        assert_eq!(event_message(&e), "running `AskUserQuestion`");
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `eval "$T" render`
Expected: compile error, `cannot find function question_message`.

- [ ] **Step 3: Implement**

In `plugins/matrix/src/render.rs`, add `use crate::question::Question;` to the imports and, after `event_message`:

```rust
/// An `AskUserQuestion` dialog for the thread (Spec J §5). Paragraphs are
/// separated by blank lines: a lone newline is not a break in markdown.
pub fn question_message(questions: &[Question]) -> String {
    let many = questions.len() > 1;
    let mut paragraphs = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let mut title = if many {
            format!("**question {} of {}**", i + 1, questions.len())
        } else {
            "**question**".to_string()
        };
        if !q.header.is_empty() {
            title.push_str(&format!(" · {}", q.header));
        }
        paragraphs.push(title);
        let mut ask = q.text.trim().to_string();
        if q.multi_select {
            ask.push_str(" *(choose any, separated by commas)*");
        }
        paragraphs.push(ask);
        paragraphs.push(
            q.options
                .iter()
                .enumerate()
                .map(|(n, o)| {
                    if o.description.is_empty() {
                        format!("{}. **{}**", n + 1, o.label)
                    } else {
                        format!("{}. **{}** — {}", n + 1, o.label, o.description)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    paragraphs.push(if many {
        "Reply with one line per question, in order: a number or a label. \
         `other: …` gives your own answer, `skip` declines."
            .to_string()
    } else {
        "Reply with a number or a label. `other: …` gives your own answer, `skip` declines."
            .to_string()
    });
    paragraphs.join("\n\n")
}
```

- [ ] **Step 4: Review and accept the snapshots**

Run: `eval "$T" render` (the two snapshot tests fail and write `.snap.new`).

Read each `.snap.new` under `plugins/matrix/src/snapshots/`. The first must be exactly:

```
**question** · Color

Which color?

1. **Red** — A warm, bold color
2. **Green** — A calm, natural color
3. **Blue** — A cool, serene color

Reply with a number or a label. `other: …` gives your own answer, `skip` declines.
```

The second must be exactly:

```
**question 1 of 2** · Colors

Which colors? *(choose any, separated by commas)*

1. **Red** — A warm, bold color
2. **Green** — A calm, natural color
3. **Blue** — A cool, serene color

**question 2 of 2** · Size

Which size?

1. **Small**
2. **Medium**
3. **Large**

Reply with one line per question, in order: a number or a label. `other: …` gives your own answer, `skip` declines.
```

Only then: `(cd plugins/matrix && CARGO_TARGET_DIR=target mise x -- cargo insta accept)`.

Run: `eval "$T" render`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add plugins/matrix/src/render.rs plugins/matrix/src/snapshots
git commit -m "feat(matrix): render an AskUserQuestion dialog with numbered options (Spec J §5)"
```

---

### Task 7: `keyDelayMs`, the ✅ reaction, and the open-question store

**Files:**
- Modify: `plugins/matrix/src/config.rs` (`AgentConfig`, `parse_agent`, tests)
- Modify: `plugins/matrix/src/matrix.rs` (the reaction constants, line ~11)
- Create: `plugins/matrix/src/pending.rs`
- Modify: `plugins/matrix/src/lib.rs` (add `pub mod pending;` after `pub mod matrix;`)

**Interfaces:**
- Consumes: `crate::question::{self, Question, Selection}`; `balerix_plugin_sdk::Host` (`kv_put(key, bytes, false)`, `kv_get`, `kv_delete`, `kv_list(prefix)`); `balerix_api::{DEFAULT_KEY_DELAY_MS, MIN_KEY_DELAY_MS, MAX_KEY_DELAY_MS}`.
- Produces:
  - `AgentConfig.key_delay_ms: u64` — wire name `keyDelayMs`, default 100, validated 20 to 500.
  - `pub const CONFIRMED: &str = "✅";` in `crate::matrix`.
  - In `crate::pending`:
    - `pub const QUESTION_PREFIX: &str = "question/";`
    - `pub enum Stage { Open, Confirming { selections: Vec<Selection>, echo: Option<String> }, Sent { selections: Option<Vec<Selection>>, echo: Option<String> } }` — `Sent.selections` is `None` for a `skip`.
    - `pub struct OpenQuestion { pub questions: Vec<Question>, pub stage: Stage }`
    - `pub struct Questions` with `load(host) -> Result<Self, SdkError>`, `get(&self, agent) -> Option<&OpenQuestion>`, `is_open(&self, agent) -> bool`, `set_stage(&mut self, agent, Stage)`, `async open(&mut self, host, agent, tool_input: &Value, questions: Vec<Question>)`, `async clear(&mut self, host, agent) -> Option<OpenQuestion>`. `open` and `clear` log a KV failure and do not return it: memory is the protection, KV is the restart mirror.

- [ ] **Step 1: Write the failing config test**

In the `tests` module of `plugins/matrix/src/config.rs`:

```rust
    #[test]
    fn key_delay_defaults_to_100_and_is_bounded() {
        assert_eq!(parse_agent(&json!({})).unwrap().key_delay_ms, 100);
        assert_eq!(parse_agent(&json!({ "keyDelayMs": 250 })).unwrap().key_delay_ms, 250);
        for bad in [19, 501] {
            let e = parse_agent(&json!({ "keyDelayMs": bad })).unwrap_err();
            assert_eq!(e.path, "keyDelayMs");
            assert!(e.message.contains("20 to 500"), "{e}");
        }
        assert!(parse_agent(&json!({ "key_delay_ms": 100 })).is_err(), "camelCase only");
    }
```

Run: `eval "$T" key_delay`
Expected: FAIL, no field `key_delay_ms`.

- [ ] **Step 2: Implement the config key and the reaction**

In `plugins/matrix/src/config.rs`, import `use balerix_api::{DEFAULT_KEY_DELAY_MS, HOOK_EVENTS, MAX_KEY_DELAY_MS, MIN_KEY_DELAY_MS};` (replacing the `HOOK_EVENTS` import), add the field and its default:

```rust
pub struct AgentConfig {
    pub enabled: bool,
    pub events: Vec<String>,
    pub phases: bool,
    /// The pause after each key when answering a question (Spec J §7.5).
    #[serde(rename = "keyDelayMs")]
    pub key_delay_ms: u64,
}
```

```rust
            phases: true,
            key_delay_ms: DEFAULT_KEY_DELAY_MS,
```

and in `parse_agent`, before `Ok(c)`:

```rust
    if !(MIN_KEY_DELAY_MS..=MAX_KEY_DELAY_MS).contains(&c.key_delay_ms) {
        return Err(ConfigError {
            path: "keyDelayMs".into(),
            message: format!(
                "expected {MIN_KEY_DELAY_MS} to {MAX_KEY_DELAY_MS} milliseconds, got {}",
                c.key_delay_ms
            ),
        });
    }
```

In `plugins/matrix/src/matrix.rs`, beside `ACK`, `REFUSED` and `FAILED`:

```rust
/// On the plugin's own echo: Claude recorded exactly what was chosen.
pub const CONFIRMED: &str = "✅";
```

Run: `eval "$T" key_delay`
Expected: PASS.

- [ ] **Step 3: Write the failing store tests**

Create `plugins/matrix/src/pending.rs` with the tests only, and add `pub mod pending;` to `lib.rs`:

```rust
//! The question each agent is waiting on (Spec J §7.1). `Open` is mirrored
//! to the daemon's KV (J-8): without it a plugin restart mid-question would
//! send the operator's reply down `send_text` again, which is the silent
//! wrong answer this feature exists to remove.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::fixtures::{color, input};
    use balerix_plugin_sdk::testing::FakeHost;
    use serde_json::json;

    async fn host() -> (FakeHost, Host) {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        (fake, host)
    }

    #[tokio::test]
    async fn an_open_question_is_mirrored_to_kv_and_survives_a_reload() {
        let (fake, host) = host().await;
        let tool_input = input(&[color()]);
        let questions = question::parse(&tool_input).unwrap();
        let mut q = Questions::default();
        assert!(!q.is_open("f/c/a"));
        q.open(&host, "f/c/a", &tool_input, questions.clone()).await;
        assert!(q.is_open("f/c/a"));
        assert_eq!(fake.kv_json("question/f/c/a"), Some(tool_input));

        q.set_stage(
            "f/c/a",
            Stage::Sent {
                selections: None,
                echo: None,
            },
        );
        let reloaded = Questions::load(&host).await.unwrap();
        assert_eq!(
            reloaded.get("f/c/a"),
            Some(&OpenQuestion {
                questions,
                stage: Stage::Open
            }),
            "only `Open` is persisted; the operator answers again"
        );
    }

    #[tokio::test]
    async fn clear_returns_the_question_and_removes_the_mirror() {
        let (fake, host) = host().await;
        let tool_input = input(&[color()]);
        let mut q = Questions::default();
        q.open(&host, "f/c/a", &tool_input, question::parse(&tool_input).unwrap())
            .await;
        let was = q.clear(&host, "f/c/a").await.unwrap();
        assert_eq!(was.stage, Stage::Open);
        assert!(!q.is_open("f/c/a"));
        assert!(fake.kv_json("question/f/c/a").is_none());
        assert!(q.clear(&host, "f/c/a").await.is_none(), "nothing open, nothing to do");
    }

    #[tokio::test]
    async fn a_record_that_no_longer_parses_is_ignored_on_load() {
        let (_fake, host) = host().await;
        host.kv_put("question/f/c/a", br#"{"questions":[]}"#, false)
            .await
            .unwrap();
        host.kv_put("question/f/c/b", b"\xff", false).await.unwrap();
        let q = Questions::load(&host).await.unwrap();
        assert!(!q.is_open("f/c/a") && !q.is_open("f/c/b"));
    }
}
```

Run: `eval "$T" pending`
Expected: compile errors, `cannot find type Questions`.

- [ ] **Step 4: Implement the store**

Insert into `plugins/matrix/src/pending.rs`, between the module doc comment and `mod tests`:

```rust
use std::collections::HashMap;

use balerix_plugin_sdk::{Host, SdkError};
use serde_json::Value;

use crate::question::{self, Question, Selection};

pub const QUESTION_PREFIX: &str = "question/";

pub fn question_key(agent: &str) -> String {
    format!("{QUESTION_PREFIX}{agent}")
}

/// Where an open question stands (Spec J §7.1). Event ids are the plugin's
/// own echo message, kept for the ✅ that follows `PostToolUse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Open,
    /// An inexact match was echoed; waiting for `yes`.
    Confirming {
        selections: Vec<Selection>,
        echo: Option<String>,
    },
    /// Keys were sent. `selections` is `None` for a `skip`.
    Sent {
        selections: Option<Vec<Selection>>,
        echo: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenQuestion {
    pub questions: Vec<Question>,
    pub stage: Stage,
}

#[derive(Debug, Default)]
pub struct Questions {
    open: HashMap<String, OpenQuestion>,
}

impl Questions {
    /// Every mirrored question resumes as `Open`. A record that no longer
    /// parses is skipped, not an error: it must not stop the plugin loading.
    pub async fn load(host: &Host) -> Result<Self, SdkError> {
        let mut questions = Self::default();
        for key in host.kv_list(QUESTION_PREFIX).await? {
            let Some(bytes) = host.kv_get(&key).await? else {
                continue;
            };
            let parsed = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| question::parse(&v));
            match parsed {
                Some(list) => {
                    questions.open.insert(
                        key[QUESTION_PREFIX.len()..].to_string(),
                        OpenQuestion {
                            questions: list,
                            stage: Stage::Open,
                        },
                    );
                }
                None => tracing::warn!("matrix: bad question record {key}"),
            }
        }
        Ok(questions)
    }

    pub fn get(&self, agent: &str) -> Option<&OpenQuestion> {
        self.open.get(agent)
    }

    pub fn is_open(&self, agent: &str) -> bool {
        self.open.contains_key(agent)
    }

    pub fn set_stage(&mut self, agent: &str, stage: Stage) {
        if let Some(open) = self.open.get_mut(agent) {
            open.stage = stage;
        }
    }

    /// Memory first, unlike `Maps`: the record in memory is what keeps a
    /// reply off `send_text`, so it must exist even when the KV write fails.
    pub async fn open(
        &mut self,
        host: &Host,
        agent: &str,
        tool_input: &Value,
        questions: Vec<Question>,
    ) {
        self.open.insert(
            agent.to_string(),
            OpenQuestion {
                questions,
                stage: Stage::Open,
            },
        );
        let bytes = tool_input.to_string().into_bytes();
        if let Err(e) = host.kv_put(&question_key(agent), &bytes, false).await {
            tracing::warn!("matrix: mirroring the question for {agent}: {e}");
        }
    }

    /// No KV call when nothing is open: this runs on every `Stop`.
    pub async fn clear(&mut self, host: &Host, agent: &str) -> Option<OpenQuestion> {
        let was = self.open.remove(agent)?;
        if let Err(e) = host.kv_delete(&question_key(agent)).await {
            tracing::warn!("matrix: clearing the question for {agent}: {e}");
        }
        Some(was)
    }
}
```

- [ ] **Step 5: Run the tests and the plugin gate**

Run: `eval "$T" -- pending key_delay` (several filters go after `--`)
Expected: PASS.

Run: `mise run plugin matrix`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add plugins/matrix/src/config.rs plugins/matrix/src/matrix.rs plugins/matrix/src/pending.rs plugins/matrix/src/lib.rs
git commit -m "feat(matrix): keyDelayMs, and the open question mirrored to KV (Spec J §7.1, §7.5, J-8)"
```

---

### Task 8: the actor, event side — open, suppress, clear, ground truth

**Files:**
- Modify: `plugins/matrix/src/actor.rs` (`Counters`, `Actor`, `load`, `handle`'s `Deactivate`, `on_event`, `react`; tests)

**Interfaces:**
- Consumes: `crate::pending::{Questions, Stage}`, `crate::question`, `crate::render::question_message`, `crate::matrix::CONFIRMED`.
- Produces (private to the actor, used by Task 9):
  - field `questions: Questions` on `Actor`
  - `Counters.answers_mismatched: IntCounter` (`answers_mismatched_total`)
  - `async fn react_to(&self, room: &str, event_id: &str, key: &str)`
  - test helpers `color_asks(agent, session)`, `asks(agent, session, questions)`, `with_question_thread()` returning `(FakeHost, FakePort, Actor<FakePort>, String /*room*/, String /*root*/)` with the agent `f/c/alice` activated for `["Notification", "Stop"]`.

Event ids in these tests: `FakePort` mints `<prefix><n>:fake` on its nth `create_room` or `send` (reactions mint nothing). After `with_question_thread` the room is `!room1:fake` and the thread root `$evt2:fake`, so the question message is `$evt3:fake`.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `plugins/matrix/src/actor.rs`, after the `with_thread` helper:

```rust
    use crate::matrix::CONFIRMED;
    use crate::pending::Stage;
    use crate::question::fixtures::{color, size};

    fn asks(agent: &str, session: &str, questions: &[serde_json::Value]) -> HookEvent {
        during(
            agent,
            session,
            "PreToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_input": { "questions": questions } }),
        )
    }

    fn answered(agent: &str, session: &str, answers: serde_json::Value) -> HookEvent {
        during(
            agent,
            session,
            "PostToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_response": { "answers": answers } }),
        )
    }

    /// `with_thread`, for an agent that posts `Notification` and `Stop`.
    async fn with_question_thread() -> (FakeHost, FakePort, Actor<FakePort>, String, String) {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification", "Stop"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        let room = match port.calls().last() {
            Some(Call::Send { room, .. }) => room.clone(),
            other => panic!("expected a root send, got {other:?}"),
        };
        port.take_calls();
        (fake, port, a, room, "$evt2:fake".to_string())
    }

    #[tokio::test]
    async fn a_question_is_posted_in_the_thread_and_the_permission_line_is_suppressed() {
        let (fake, port, mut a, _room, root) = with_question_thread().await;
        a.handle(Command::Events(vec![
            asks("f/c/alice", "s1", &[color()]),
            during(
                "f/c/alice",
                "s1",
                "Notification",
                json!({ "notification_type": "permission_prompt", "message": "Claude needs your permission" }),
            ),
        ]))
        .await;
        let sent = sends(&port.calls());
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0.as_deref(), Some(root.as_str()));
        assert!(sent[0].1.starts_with("**question** · Color"), "{}", sent[0].1);
        assert!(sent[0].1.contains("3. **Blue**"));
        assert!(fake.kv_json("question/f/c/alice").is_some(), "mirrored to KV");

        // with no question open the same notification posts as before
        a.handle(Command::Events(vec![during("f/c/alice", "s1", "Stop", json!({}))]))
            .await;
        port.take_calls();
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Notification",
            json!({ "notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash" }),
        )]))
        .await;
        assert!(sends(&port.calls())[0].1.contains("needs your permission to use Bash"));
    }

    #[tokio::test]
    async fn a_question_is_tracked_even_when_the_event_filter_hides_it() {
        let (fake, port, mut a, _room, _root) = with_thread().await; // events: []
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        assert!(sends(&port.calls()).is_empty(), "nothing posted");
        assert!(a.questions.is_open("f/c/alice"), "but J-7's protection is on");
        assert!(fake.kv_json("question/f/c/alice").is_some());
    }

    #[tokio::test]
    async fn every_clearing_event_closes_the_question() {
        for (name, payload) in [
            ("Stop", json!({})),
            ("UserPromptSubmit", json!({ "prompt": "x" })),
            ("SessionEnd", json!({ "reason": "clear" })),
            ("SessionStart", json!({ "source": "clear" })),
        ] {
            let (fake, _port, mut a, _room, _root) = with_question_thread().await;
            a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
                .await;
            assert!(a.questions.is_open("f/c/alice"));
            a.handle(Command::Events(vec![during("f/c/alice", "s1", name, payload)]))
                .await;
            assert!(!a.questions.is_open("f/c/alice"), "{name}");
            assert!(fake.kv_json("question/f/c/alice").is_none(), "{name}");
        }
        let (_fake, _port, mut a, _room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        a.handle(deactivate("f/c/alice")).await;
        assert!(!a.questions.is_open("f/c/alice"), "deactivate");
    }

    #[tokio::test]
    async fn an_answer_given_at_the_terminal_is_reported() {
        let (_fake, port, mut a, _room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color(), size()])]))
            .await;
        port.take_calls();
        a.handle(Command::Events(vec![answered(
            "f/c/alice",
            "s1",
            json!({ "Which color?": "Blue", "Which size?": "Medium" }),
        )]))
        .await;
        assert_eq!(
            sends(&port.calls())[0].1,
            "**answered at the terminal** Color → Blue · Size → Medium"
        );
        assert!(!a.questions.is_open("f/c/alice"));
    }

    #[tokio::test]
    async fn a_sent_answer_is_confirmed_on_the_echo_or_reported_when_it_differs() {
        let chosen = vec![crate::question::Selection {
            options: vec![2],
            other: None,
        }];
        let sent = |echo: &str| Stage::Sent {
            selections: Some(chosen.clone()),
            echo: Some(echo.to_string()),
        };

        let (_fake, port, mut a, room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        a.questions.set_stage("f/c/alice", sent("$echo:fake"));
        port.take_calls();
        a.handle(Command::Events(vec![answered(
            "f/c/alice",
            "s1",
            json!({ "Which color?": "Blue" }),
        )]))
        .await;
        assert_eq!(
            port.calls(),
            vec![Call::React {
                room: room.clone(),
                event_id: "$echo:fake".into(),
                key: CONFIRMED.into()
            }],
            "a reaction, not another message"
        );

        let (_fake, port, mut a, _room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        a.questions.set_stage("f/c/alice", sent("$echo:fake"));
        port.take_calls();
        a.handle(Command::Events(vec![answered(
            "f/c/alice",
            "s1",
            json!({ "Which color?": "Red" }),
        )]))
        .await;
        let body = &sends(&port.calls())[0].1;
        assert!(body.starts_with("**recorded answer differs**"), "{body}");
        assert!(body.contains("Color → Red") && body.contains("Color → Blue"), "{body}");
        assert_eq!(a.counters.answers_mismatched.get(), 1);
    }

    #[tokio::test]
    async fn an_open_question_survives_an_actor_restart() {
        let (fake, _port, mut a, _room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        drop(a);
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        let mut again = Actor::new(
            host,
            FakePort::new("@balerix:example.org"),
            counters(),
            Health::new(),
        );
        again.load().await;
        assert!(again.questions.is_open("f/c/alice"));
    }
```

(`Call` already derives `PartialEq`, so `assert_eq!` on `port.calls()` works.)

Also extend `every_metric_family_carries_the_plugin_prefix`: add `c.answers_mismatched.inc();` before `m.render()` and `"balerix_plugin_matrix_answers_mismatched_total",` to the family list.

- [ ] **Step 2: Run them and watch them fail**

Run: `eval "$T" actor`
Expected: compile errors, no field `questions` on `Actor`.

- [ ] **Step 3: Implement**

In `plugins/matrix/src/actor.rs`:

Imports: add `CONFIRMED` to the `crate::matrix::{…}` list, and
```rust
use crate::pending::{Questions, Stage};
use crate::question;
```

`Counters`: add the field `pub answers_mismatched: IntCounter,` and in `Counters::new`:

```rust
            answers_mismatched: metrics.int_counter(
                "answers_mismatched_total",
                "Answers Claude recorded differently from what the thread chose",
            )?,
```

`Actor`: add the field, below `maps`:

```rust
    /// The `AskUserQuestion` each agent is waiting on (Spec J §7).
    questions: Questions,
```

and `questions: Questions::default(),` in `Actor::new`.

`load`, after the `Maps::load` match:

```rust
        match Questions::load(&self.host).await {
            Ok(questions) => self.questions = questions,
            Err(e) => tracing::warn!("matrix: loading questions: {e}"),
        }
```

`handle`, in `Command::Deactivate`, after `self.agents.remove(&agent);`:

```rust
                self.questions.clear(&self.host, &agent).await;
```

`on_event`, directly after the `if !config.enabled { return; }` block:

```rust
        // Spec J §7.1: these prove no dialog is on screen any more. Done
        // before the thread logic, which returns early for `SessionStart`.
        if matches!(
            event.name.as_str(),
            "Stop" | "UserPromptSubmit" | "SessionStart" | "SessionEnd"
        ) {
            self.questions.clear(&self.host, &event.agent).await;
        }
```

`on_event`, directly before `if !config.wants(&event.name) { return; }`:

```rust
        if self.on_question_event(&config, &room, &event).await {
            return;
        }
```

New methods on `Actor`, after `on_event`:

```rust
    /// The `AskUserQuestion` lifecycle (Spec J §5, §7.3). `true` when the
    /// event was handled here and must not also post as a generic line.
    async fn on_question_event(
        &mut self,
        config: &AgentConfig,
        room: &str,
        event: &HookEvent,
    ) -> bool {
        let is_question =
            event.payload.get("tool_name").and_then(Value::as_str) == Some(question::TOOL);
        // The question is the detailed form of "needs you", so either event
        // being wanted shows it. Tracking below never depends on this (J-7).
        let shown = config.wants("Notification") || config.wants("PreToolUse");
        let root = self.maps.thread(&event.agent).map(|t| t.root.clone());
        match event.name.as_str() {
            "PreToolUse" if is_question => {
                let input = event
                    .payload
                    .get("tool_input")
                    .cloned()
                    .unwrap_or(Value::Null);
                let Some(questions) = question::parse(&input) else {
                    return false; // posts as `running AskUserQuestion`, as before
                };
                let body = render::question_message(&questions);
                self.questions
                    .open(&self.host, &event.agent, &input, questions)
                    .await;
                if shown {
                    if let Some(root) = root {
                        self.send(room, Some(&root), &body, "question").await;
                    }
                }
                true
            }
            "Notification"
                if self.questions.is_open(&event.agent)
                    && event
                        .payload
                        .get("notification_type")
                        .and_then(Value::as_str)
                        == Some("permission_prompt") =>
            {
                true // says nothing the question has not
            }
            "PostToolUse" if is_question => {
                let Some(open) = self.questions.clear(&self.host, &event.agent).await else {
                    return false;
                };
                let answers = event
                    .payload
                    .pointer("/tool_response/answers")
                    .cloned()
                    .unwrap_or(Value::Null);
                let recorded = question::describe_recorded(&open.questions, &answers);
                match open.stage {
                    Stage::Sent {
                        selections: Some(selections),
                        echo,
                    } => {
                        if question::recorded_matches(&open.questions, &selections, &answers) {
                            if let Some(echo) = echo {
                                self.react_to(room, &echo, CONFIRMED).await;
                            }
                        } else {
                            // Posted whatever the filter says: it answers
                            // the operator's own action.
                            self.counters.answers_mismatched.inc();
                            let body = format!(
                                "**recorded answer differs** — Claude recorded {recorded}; \
                                 you chose {}. Tell the agent if that matters.",
                                question::describe(&open.questions, &selections)
                            );
                            if let Some(root) = root {
                                self.send(room, Some(&root), &body, "question").await;
                            }
                        }
                    }
                    Stage::Sent {
                        selections: None, ..
                    } => {}
                    Stage::Open | Stage::Confirming { .. } => {
                        if shown {
                            if let Some(root) = root {
                                let body = format!("**answered at the terminal** {recorded}");
                                self.send(room, Some(&root), &body, "question").await;
                            }
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }
```

Replace `react` with a pair, so a reaction can target the plugin's own message:

```rust
    async fn react(&self, message: &Inbound, key: &str) {
        self.react_to(&message.room, &message.event_id, key).await;
    }

    async fn react_to(&self, room: &str, event_id: &str, key: &str) {
        if let Err(e) = self.port.react(room, event_id, key).await {
            self.counters.errors.with_label_values(&["react"]).inc();
            tracing::warn!("matrix: reacting to {event_id}: {e}");
        }
    }
```

If clippy's `collapsible_if` fires on the nested `if shown { if let Some(root) … }`, collapse it into a let-chain (`if shown && let Some(root) = root {`), which edition 2024 allows.

- [ ] **Step 4: Run the tests**

Run: `eval "$T" actor`
Expected: PASS, every pre-existing actor test included.

- [ ] **Step 5: Commit**

```bash
git add plugins/matrix/src/actor.rs
git commit -m "feat(matrix): post the question, suppress the bare permission line, report the recorded answer (Spec J §5, §7.3)"
```

---

### Task 9: the actor, inbound side — echo, confirm, send the keys

**Files:**
- Modify: `plugins/matrix/src/actor.rs` (`on_inbound`, new `on_answer` and `deliver`; tests)
- Modify: `ARCHITECTURE.md` (the `balerix-plugin-matrix` bullet, line ~60)

**Interfaces:**
- Consumes: Task 8's `questions` field, `react_to`, and test helpers; `question::{match_reply, plan, skip_plan, describe, Matched, Refusal}`; `PluginAction::SendKeys` and `validate` (Task 1); `AgentConfig.key_delay_ms` (Task 7).
- Produces: a thread reply while a question is open never becomes `SendText`.

Event ids in these tests: root `$evt2:fake`, question `$evt3:fake`, so the first message the actor sends in answer to a reply (the echo, or a refusal) is `$evt4:fake`.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `plugins/matrix/src/actor.rs`:

```rust
    use balerix_api::{Key, KeyStep};

    /// A thread with `questions` open; calls so far are cleared.
    async fn asked(
        questions: &[serde_json::Value],
    ) -> (FakeHost, FakePort, Actor<FakePort>, String, String) {
        let (fake, port, mut a, room, root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", questions)]))
            .await;
        port.take_calls();
        (fake, port, a, room, root)
    }

    async fn reply(a: &mut Actor<FakePort>, room: &str, root: &str, body: &str) {
        a.handle(Command::Inbound(inbound(
            room,
            Some(root),
            "@rahul:example.org",
            body,
        )))
        .await;
    }

    fn down_enter(downs: usize) -> Vec<KeyStep> {
        let mut steps = vec![KeyStep::Key(Key::Down); downs];
        steps.push(KeyStep::Key(Key::Enter));
        steps
    }

    #[tokio::test]
    async fn an_exact_reply_is_echoed_then_sent_as_keys_and_never_as_text() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "3").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: down_enter(2),
                delay_ms: 100,
            }]
        );
        assert_eq!(
            sends(&port.calls()),
            vec![(Some(root.clone()), "**answering** Color → Blue".to_string())]
        );
        assert_eq!(reactions(&port.calls()), vec![ACK.to_string()]);
        assert_eq!(
            a.questions.get("f/c/alice").unwrap().stage,
            Stage::Sent {
                selections: Some(vec![crate::question::Selection {
                    options: vec![2],
                    other: None
                }]),
                echo: Some("$evt4:fake".into()),
            }
        );

        // the recorded answer then lands as a ✅ on that echo
        port.take_calls();
        a.handle(Command::Events(vec![answered(
            "f/c/alice",
            "s1",
            json!({ "Which color?": "Blue" }),
        )]))
        .await;
        assert_eq!(
            port.calls(),
            vec![Call::React {
                room,
                event_id: "$evt4:fake".into(),
                key: CONFIRMED.into()
            }]
        );
    }

    #[tokio::test]
    async fn the_agents_key_delay_is_used() {
        let (fake, _port, mut a, room, root) = asked(&[color()]).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: crate::config::parse_agent(
                &json!({ "events": ["Notification"], "keyDelayMs": 250 }),
            )
            .unwrap(),
        })
        .await;
        reply(&mut a, &room, &root, "1").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: down_enter(0),
                delay_ms: 250,
            }]
        );
    }

    #[tokio::test]
    async fn an_inexact_reply_asks_first_and_yes_sends_it() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "gre").await;
        assert!(fake.actions_for("f/c/alice").is_empty(), "nothing typed yet");
        assert_eq!(
            sends(&port.calls())[0].1,
            "**I read that as** Color → Green. Reply **yes** to send."
        );
        assert!(matches!(
            a.questions.get("f/c/alice").unwrap().stage,
            Stage::Confirming { .. }
        ));

        port.take_calls();
        reply(&mut a, &room, &root, " Yes ").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: down_enter(1),
                delay_ms: 100,
            }]
        );
        assert!(sends(&port.calls()).is_empty(), "no second echo");
        assert_eq!(
            a.questions.get("f/c/alice").unwrap().stage,
            Stage::Sent {
                selections: Some(vec![crate::question::Selection {
                    options: vec![1],
                    other: None
                }]),
                echo: Some("$evt4:fake".into()),
            },
            "the ✅ goes on the echo that asked"
        );
    }

    #[tokio::test]
    async fn no_drops_the_confirmation_and_another_reply_replaces_it() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "gre").await;
        reply(&mut a, &room, &root, "no").await;
        assert_eq!(a.questions.get("f/c/alice").unwrap().stage, Stage::Open);
        assert!(fake.actions_for("f/c/alice").is_empty());

        reply(&mut a, &room, &root, "gre").await;
        port.take_calls();
        reply(&mut a, &room, &root, "blue").await; // not yes/no: a fresh answer
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: down_enter(2),
                delay_ms: 100,
            }]
        );
        assert_eq!(sends(&port.calls())[0].1, "**answering** Color → Blue");
    }

    #[tokio::test]
    async fn prose_is_refused_and_nothing_reaches_the_agent() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "purple please").await;
        assert!(fake.actions_for("f/c/alice").is_empty(), "the §1 hazard");
        assert_eq!(reactions(&port.calls()), vec![REFUSED.to_string()]);
        let body = &sends(&port.calls())[0].1;
        assert!(body.contains("matches nothing") && body.contains("1. Red"), "{body}");
        assert_eq!(a.questions.get("f/c/alice").unwrap().stage, Stage::Open);
    }

    #[tokio::test]
    async fn skip_declines_with_one_escape() {
        let (fake, port, mut a, room, root) = asked(&[color(), size()]).await;
        reply(&mut a, &room, &root, "skip").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: vec![KeyStep::Key(Key::Escape)],
                delay_ms: 100,
            }]
        );
        assert_eq!(sends(&port.calls())[0].1, "**declining the question**");
        assert_eq!(
            a.questions.get("f/c/alice").unwrap().stage,
            Stage::Sent {
                selections: None,
                echo: Some("$evt4:fake".into()),
            }
        );
    }

    #[tokio::test]
    async fn one_reply_answers_several_questions() {
        let (fake, port, mut a, room, root) = asked(&[color(), size()]).await;
        reply(&mut a, &room, &root, "blue\nmedium").await;
        let mut steps = down_enter(2);
        steps.extend(down_enter(1));
        steps.push(KeyStep::Key(Key::Enter)); // the review screen
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps,
                delay_ms: 100,
            }]
        );
        assert_eq!(
            sends(&port.calls())[0].1,
            "**answering** Color → Blue · Size → Medium"
        );
    }

    #[tokio::test]
    async fn a_reply_while_keys_are_on_their_way_is_refused() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "1").await;
        port.take_calls();
        reply(&mut a, &room, &root, "2").await;
        assert_eq!(fake.actions_for("f/c/alice").len(), 1, "only the first");
        assert_eq!(reactions(&port.calls()), vec![REFUSED.to_string()]);
        assert!(sends(&port.calls())[0].1.contains("already on its way"));
    }

    #[tokio::test]
    async fn a_failed_send_keys_is_reported_and_the_question_stays_open() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        fake.fail_actions(Some("no window"));
        reply(&mut a, &room, &root, "1").await;
        assert_eq!(reactions(&port.calls()), vec![FAILED.to_string()]);
        let bodies = sends(&port.calls());
        assert!(bodies[1].1.starts_with("**not delivered to f/c/alice:**"), "{bodies:?}");
        assert_eq!(a.questions.get("f/c/alice").unwrap().stage, Stage::Open);
    }

    #[tokio::test]
    async fn with_no_question_open_a_reply_is_still_a_prompt() {
        let (fake, _port, mut a, room, root) = with_question_thread().await;
        reply(&mut a, &room, &root, "3").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendText {
                text: "3".into(),
                submit: true,
            }]
        );
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `eval "$T" actor`
Expected: FAIL. `an_exact_reply_…` shows a `SendText { text: "3" }` where `SendKeys` was expected: the hazard of Spec J §1, reproduced.

- [ ] **Step 3: Implement**

In `on_inbound`, directly after the `stale_thread` block and before `let action = PluginAction::SendText {`:

```rust
        // Spec J-7: while a question is open a reply is an answer, never a
        // prompt. `send_text` here would type the body into the dialog and
        // its Enter would pick whatever row is highlighted.
        if self.questions.is_open(&agent) {
            self.on_answer(&agent, &root, &message).await;
            return;
        }
```

New methods on `Actor`, after `on_inbound`:

```rust
    /// A thread reply while `agent` has a question open (Spec J §7.2).
    async fn on_answer(&mut self, agent: &str, root: &str, message: &Inbound) {
        let count = |outcome: &str| self.counters.inbound.with_label_values(&[outcome]).inc();
        let Some(open) = self.questions.get(agent).cloned() else {
            return;
        };
        match &open.stage {
            Stage::Sent { .. } => {
                count("answer_refused");
                let body = "an answer is already on its way; wait for the agent.";
                self.send(&message.room, Some(root), body, "question").await;
                self.react(message, REFUSED).await;
                return;
            }
            Stage::Confirming { selections, echo } => {
                match message.body.trim().to_ascii_lowercase().as_str() {
                    "yes" | "y" => {
                        count("confirmed");
                        let (selections, echo) = (selections.clone(), echo.clone());
                        self.deliver(agent, root, message, &open.questions, Some(selections), echo)
                            .await;
                        return;
                    }
                    "no" | "n" => {
                        self.questions.set_stage(agent, Stage::Open);
                        self.react(message, ACK).await;
                        return;
                    }
                    _ => {} // anything else is a fresh answer, matched below
                }
            }
            Stage::Open => {}
        }

        match question::match_reply(&open.questions, &message.body) {
            Err(question::Refusal(reason)) => {
                count("answer_refused");
                self.questions.set_stage(agent, Stage::Open);
                self.send(&message.room, Some(root), &reason, "question").await;
                self.react(message, REFUSED).await;
            }
            Ok(question::Matched::Skip) => {
                let echo = self
                    .send(&message.room, Some(root), "**declining the question**", "question")
                    .await;
                self.deliver(agent, root, message, &open.questions, None, echo)
                    .await;
            }
            Ok(question::Matched::Answers { selections, exact }) => {
                let chosen = question::describe(&open.questions, &selections);
                if exact {
                    let body = format!("**answering** {chosen}");
                    let echo = self.send(&message.room, Some(root), &body, "question").await;
                    self.deliver(agent, root, message, &open.questions, Some(selections), echo)
                        .await;
                } else {
                    count("confirm_asked");
                    let body = format!("**I read that as** {chosen}. Reply **yes** to send.");
                    let echo = self.send(&message.room, Some(root), &body, "question").await;
                    self.questions
                        .set_stage(agent, Stage::Confirming { selections, echo });
                }
            }
        }
    }

    /// Sends the keys. `selections` is `None` for a `skip`.
    async fn deliver(
        &mut self,
        agent: &str,
        root: &str,
        message: &Inbound,
        questions: &[question::Question],
        selections: Option<Vec<question::Selection>>,
        echo: Option<String>,
    ) {
        let count = |outcome: &str| self.counters.inbound.with_label_values(&[outcome]).inc();
        let steps = match &selections {
            Some(selections) => question::plan(questions, selections),
            None => question::skip_plan(),
        };
        let delay_ms = self
            .agents
            .get(agent)
            .map(|c| c.key_delay_ms)
            .unwrap_or(balerix_api::DEFAULT_KEY_DELAY_MS);
        let action = PluginAction::SendKeys { steps, delay_ms };
        // The daemon would refuse it; say why here, before anything is sent.
        if let Err(reason) = action.validate() {
            count("answer_refused");
            self.questions.set_stage(agent, Stage::Open);
            let body = format!(
                "this answer needs more keystrokes than can be sent from here ({reason}); \
                 answer at the terminal."
            );
            self.send(&message.room, Some(root), &body, "question").await;
            self.react(message, REFUSED).await;
            return;
        }
        match self.host.action(agent, &action).await {
            Ok(()) => {
                count(if selections.is_some() { "answered" } else { "skipped" });
                self.questions
                    .set_stage(agent, Stage::Sent { selections, echo });
                self.react(message, ACK).await;
            }
            Err(e) => {
                count("send_failed");
                self.counters.errors.with_label_values(&["send_keys"]).inc();
                self.questions.set_stage(agent, Stage::Open);
                let body = format!("**not delivered to {agent}:** {e}");
                self.send(&message.room, Some(root), &body, "notice").await;
                self.react(message, FAILED).await;
            }
        }
    }
```

`count` borrows `self.counters` immutably; if the borrow checker objects to it living across a `&mut self` call, replace each `count("x")` with the expression it stands for, `self.counters.inbound.with_label_values(&["x"]).inc()`. `on_inbound` already uses the closure the same way, so follow whatever compiles there.

- [ ] **Step 4: Run the tests and the plugin gate**

Run: `eval "$T" actor`
Expected: PASS.

Run: `mise run plugin matrix`
Expected: PASS.

- [ ] **Step 5: ARCHITECTURE.md**

Replace the `balerix-plugin-matrix` bullet's first sentence so it reads:

```markdown
- `balerix-plugin-matrix` — the third in-tree plugin: a Matrix room per
  crew and a thread per agent session, with thread replies coming back as
  `send_text` (Spec G), and an `AskUserQuestion` dialog shown in the thread
  and answered from it with paced `send_keys` (Spec J). Everything the
  plugin knows about Claude's dialog is in `question.rs`; bumping `claude`
  means running `mise run verify-questions`. Its `matrix-sdk` tree is larger
  than the rest of the repository put together, which is why plugins
  stopped being workspace members.
```

- [ ] **Step 6: Commit**

```bash
git add plugins/matrix/src/actor.rs ARCHITECTURE.md
git commit -m "feat(matrix): answer a question from the thread: echo, confirm the inexact, send the keys (Spec J §7.2)"
```

---

### Task 10: `verify-questions` — the model checked against the real `claude`

The property test in Task 5 proves `plan` against a *model* of the dialog. This task is what keeps the model honest: it drives the pinned `claude` with the plans `question.rs` really produces and compares what Claude recorded. It needs a logged-in `claude`, so it is by hand, like `verify-claude`.

**Files:**
- Create: `plugins/matrix/examples/question_plan.rs`
- Create: `scripts/verify-questions.sh` (mode 0755)
- Modify: `mise.toml` (a task, next to `[tasks.verify-claude]`)
- Modify: `AGENTS.md` (the task list and the Gotchas)

**Interfaces:**
- Consumes: `balerix_plugin_matrix::question::{parse, match_reply, plan, skip_plan, describe, Matched, Refusal}`.
- Produces: `question_plan <tool_input.json> <reply>` prints `{"steps":[…],"exact":<bool>,"chosen":"…"}` on stdout, or exits 1 with `refused: …` on stderr. The `balerix` binary cannot depend on plugin code (AGENTS.md), which is why this is an example of the plugin's own project.

- [ ] **Step 1: The example**

Create `plugins/matrix/examples/question_plan.rs`:

```rust
//! `question_plan <tool_input.json> <reply>`: what the matrix plugin would
//! do with `reply` while that `AskUserQuestion` is open, as JSON. Used by
//! `scripts/verify-questions.sh` to drive the real `claude` with the
//! plugin's own key plans (Spec J §10).

use balerix_plugin_matrix::question::{self, Matched, Refusal};
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(reply)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: question_plan <tool_input.json> <reply>");
    };
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let questions = question::parse(&input)
        .ok_or_else(|| anyhow::anyhow!("{path}: not an AskUserQuestion tool_input"))?;
    let out = match question::match_reply(&questions, &reply) {
        Ok(Matched::Answers { selections, exact }) => json!({
            "steps": question::plan(&questions, &selections),
            "exact": exact,
            "chosen": question::describe(&questions, &selections),
        }),
        Ok(Matched::Skip) => json!({
            "steps": question::skip_plan(),
            "exact": true,
            "chosen": "skip",
        }),
        Err(Refusal(reason)) => anyhow::bail!("refused: {reason}"),
    };
    println!("{out}");
    Ok(())
}
```

Run:

```bash
CARGO_TARGET_DIR=plugins/matrix/target mise x -- cargo build -q --manifest-path plugins/matrix/Cargo.toml --example question_plan
printf '%s' '{"questions":[{"question":"Which color?","header":"Color","multiSelect":false,"options":[{"label":"Red","description":""},{"label":"Green","description":""}]}]}' > target/tmp/q.json
plugins/matrix/target/debug/examples/question_plan target/tmp/q.json green
```

Expected: `{"chosen":"Color → Green","exact":true,"steps":[{"key":"down"},{"key":"enter"}]}`

- [ ] **Step 2: The script**

Create `scripts/verify-questions.sh`:

```bash
#!/usr/bin/env bash
# The by-hand check for Spec J: the real pinned `claude`, in tmux, answered
# with the key plans the matrix plugin's `question.rs` produces, and the
# recorded answers read back from the PostToolUse hook.
#
# What it settles: that the dialog still behaves as Spec J §2 measured. The
# property test in plugins/matrix/src/question.rs proves the plans against a
# model of the dialog; this proves the model. Run it after bumping `claude`
# in mise.toml.
#
# Your real $HOME stays, for claude's credentials. Everything else lives
# under target/tmp/verify-questions, wiped every run. Costs a few short
# model turns. Not part of any CI tier.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$REPO/target/tmp/verify-questions"
WORK="$ROOT/work"
SOCKET="balerix-verify-questions-$$"
DELAY="${BALERIX_KEY_DELAY:-0.1}"
PLAN="$REPO/plugins/matrix/target/debug/examples/question_plan"

say() { printf '%s\n' "$*"; }
t() { tmux -L "$SOCKET" "$@"; }
pane() { t capture-pane -p -t s 2>/dev/null; }
lines() { if [ -f "$1" ]; then grep -c . "$1"; else echo 0; fi; }

cleanup() { t kill-server >/dev/null 2>&1; }
trap cleanup EXIT

wait_for() { # wait_for <seconds> <command…>: until the command succeeds
  local deadline=$((SECONDS + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then return 1; fi
    sleep 0.25
  done
}
pane_has() { pane | grep -q -- "$1"; }
pane_idle() { ! pane | grep -q 'to navigate\|esc to interrupt'; }
more_lines() { [ "$(lines "$1")" -gt "$2" ]; }

play() { # play <plan.json>: one tmux command per step, paced
  local step key
  while IFS= read -r step; do
    key=$(jq -r '.key // empty' <<<"$step")
    case "$key" in
      up) t send-keys -t s Up ;;
      down) t send-keys -t s Down ;;
      enter) t send-keys -t s Enter ;;
      escape) t send-keys -t s Escape ;;
      "") t send-keys -t s -l -- "$(jq -r '.text' <<<"$step")" ;;
      *) say "unknown key: $key"; return 1 ;;
    esac
    sleep "$DELAY"
  done < <(jq -c '.steps[]' "$1")
}

rm -rf "$ROOT"
mkdir -p "$WORK/.claude"
git -C "$WORK" init -q
PRE="$ROOT/pre.log"
POST="$ROOT/post.log"
jq -n --arg pre "cat >> $PRE; echo >> $PRE" --arg post "cat >> $POST; echo >> $POST" '{
  hooks: {
    PreToolUse:  [{ matcher: "AskUserQuestion", hooks: [{ type: "command", command: $pre }] }],
    PostToolUse: [{ matcher: "AskUserQuestion", hooks: [{ type: "command", command: $post }] }]
  } }' > "$WORK/.claude/settings.json"

say "building the plugin's question_plan example"
CARGO_TARGET_DIR="$REPO/plugins/matrix/target" cargo build -q \
  --manifest-path "$REPO/plugins/matrix/Cargo.toml" --example question_plan ||
  { say "cargo build failed"; exit 2; }

say "starting $(claude --version 2>/dev/null || echo claude) in tmux"
t new-session -d -s s -x 110 -y 40 -c "$WORK" claude
wait_for 30 pane_has '❯' || { say "claude never showed a prompt"; pane | tail -n 15; exit 2; }
sleep 1
if pane_has 'trust this folder'; then
  t send-keys -t s Down
  sleep 0.5
  t send-keys -t s Enter
  sleep 3
  wait_for 30 pane_has '❯' || { say "no prompt after trusting the folder"; exit 2; }
fi
sleep 1

COLOR="'Which color?' header 'Color' options Red, Green, Blue"
SIZE="'Which size?' header 'Size' options Small, Medium, Large"
COLORS="'Which colors?' header 'Colors' options Red, Green, Blue"
fails=0

run_case() { # run_case <name> <what to ask for> <reply> <expected answers JSON, or "declined">
  local name=$1 ask=$2 reply=$3 want=$4 before_pre before_post got
  before_pre=$(lines "$PRE")
  before_post=$(lines "$POST")
  t send-keys -t s -l -- "Use the AskUserQuestion tool exactly once, with exactly this: $ask. Give every option a two-word description. After I answer, reply with only: ok"
  t send-keys -t s Enter
  if ! wait_for 90 more_lines "$PRE" "$before_pre" || ! wait_for 30 pane_has 'to navigate'; then
    say "FAIL $name: the dialog never appeared"
    fails=$((fails + 1))
    return
  fi
  sleep 1
  tail -n 1 "$PRE" | jq '.tool_input' > "$ROOT/$name.input.json"
  if ! "$PLAN" "$ROOT/$name.input.json" "$reply" > "$ROOT/$name.plan.json"; then
    say "FAIL $name: question_plan refused the reply"
    t send-keys -t s Escape
    fails=$((fails + 1))
    return
  fi
  play "$ROOT/$name.plan.json"
  if [ "$want" = declined ]; then
    if wait_for 30 pane_has 'declined to answer'; then say "PASS $name"; else
      say "FAIL $name: claude did not report a declined question"
      fails=$((fails + 1))
    fi
  elif wait_for 20 more_lines "$POST" "$before_post"; then
    got=$(tail -n 1 "$POST" | jq -S -c '.tool_response.answers')
    if [ "$got" = "$(jq -S -c . <<<"$want")" ]; then say "PASS $name: $got"; else
      say "FAIL $name: recorded $got, wanted $want (keys: $(jq -c '.steps' "$ROOT/$name.plan.json"))"
      fails=$((fails + 1))
    fi
  else
    say "FAIL $name: never submitted (keys: $(jq -c '.steps' "$ROOT/$name.plan.json"))"
    pane | grep -v '^\s*$' | tail -n 12
    t send-keys -t s Escape
    fails=$((fails + 1))
  fi
  wait_for 60 pane_idle
  sleep 3
}

run_case single "ONE single-select question $COLOR" "2" \
  '{"Which color?":"Green"}'
run_case single-other "ONE single-select question $COLOR" "other: teal-ish" \
  '{"Which color?":"teal-ish"}'
run_case two "TWO single-select questions in the same call: $COLOR; and $SIZE" $'blue\nmedium' \
  '{"Which color?":"Blue","Which size?":"Medium"}'
run_case two-other "TWO single-select questions in the same call: $COLOR; and $SIZE" $'other: teal-ish\nlarge' \
  '{"Which color?":"teal-ish","Which size?":"Large"}'
run_case multi "ONE question with multiSelect true, $COLORS" "red, blue" \
  '{"Which colors?":"Red, Blue"}'
run_case multi-other "ONE question with multiSelect true, $COLORS" "green, other: a bit of gold" \
  '{"Which colors?":"Green, a bit of gold"}'
run_case mixed "TWO questions in the same call: first, with multiSelect true, $COLORS; second, single-select, $SIZE" $'red, blue\nmedium' \
  '{"Which colors?":"Red, Blue","Which size?":"Medium"}'
run_case skip "ONE single-select question $COLOR" "skip" declined

if [ "$fails" -eq 0 ]; then
  say "verify-questions: every dialog shape answered as planned"
else
  say "verify-questions: $fails case(s) failed — the dialog no longer matches Spec J §2; fix plugins/matrix/src/question.rs (plan and the test model together)"
  exit 1
fi
```

Then: `chmod +x scripts/verify-questions.sh` and `mise x -- shellcheck scripts/verify-questions.sh` (must be clean; `mise run lint` runs shellcheck).

- [ ] **Step 3: The task and the docs**

In `mise.toml`, after the `[tasks.verify-claude]` block:

```toml
[tasks.verify-questions]
description = "By-hand Spec J check: drives the real pinned claude through every AskUserQuestion dialog shape with the matrix plugin's own key plans and checks the recorded answers (scratch under target/tmp/verify-questions; your HOME kept for credentials; a few short model turns). Run after bumping claude. Not part of any CI tier"
run = "scripts/verify-questions.sh"
```

In `AGENTS.md`, after the `verify-matrix` task bullet:

```markdown
- `verify-questions` — Spec J's manual check (`scripts/verify-questions.sh`):
  the real pinned `claude` in tmux, answered with the key plans the matrix
  plugin's `question.rs` produces, the recorded answers read back from the
  `PostToolUse` hook. Needs a logged-in `claude`. Not part of any CI tier.
```

and in `## Gotchas`, after the bullet about the embedded default tool table:

```markdown
- The matrix plugin answers `AskUserQuestion` by counting rows and pressing
  Down and Enter (`plugins/matrix/src/question.rs::plan`). That encodes
  Claude Code's dialog layout, which no API promises. The property test
  there proves `plan` against a *model* of the dialog; only
  `mise run verify-questions` proves the model. Bump `claude` in `mise.toml`
  and you run it. Keys sent with no pause are dropped at a question
  transition, which is why `send_keys` has a 20 ms floor; and never use Tab
  in a plan: it goes to different places from different rows.
```

- [ ] **Step 4: Run it**

Run: `mise run verify-questions`
Expected: eight `PASS` lines, then `verify-questions: every dialog shape answered as planned`. A `FAIL` here is a real finding about `plan`, not a flaky script: stop and report it with the printed keys and recorded answers rather than adjusting expectations.

If no logged-in `claude` is available in the executing environment, do not skip silently: record in the final report that this step was not run, and leave it as the one open item for the operator.

- [ ] **Step 5: Commit**

```bash
git add plugins/matrix/examples/question_plan.rs scripts/verify-questions.sh mise.toml AGENTS.md
git commit -m "test(matrix): verify-questions, the key plans against the real claude (Spec J §10)"
```

---

### Task 11: the gates, and the pull request

**Files:** none new.

- [ ] **Step 1: Every gate the spec names**

```bash
mise run check
mise run test-it
mise run plugins
```

Expected: all PASS. `plugins` runs flow, web and matrix: flow and web build against the changed `balerix-api`, so they are part of this change's blast radius even though neither file was touched.

- [ ] **Step 2: Walk the spec's "Done when" list**

Open `docs/superpowers/specs/2026-09-18-balerix-j-matrix-questions-design.md` §12 and check each line against what was built. The real-homeserver checks need `MATRIX_HOMESERVER`, `MATRIX_USER_ID`, `MATRIX_PASSWORD` and `MATRIX_INVITE`; without them, say so in the report rather than claiming them.

- [ ] **Step 3: Open the pull request**

Title (it becomes the changelog line for both the core and the matrix release units): `feat(matrix): show and answer Claude's questions from the thread (Spec J)`

The body names the new `send_keys` action, links the spec, lists which verification steps were run and which were not, and notes the separately reported `send_text` flaw: a body ending in `;` loses it on the short `send-keys -l` path, because tmux reads a trailing `;` as a command separator.

