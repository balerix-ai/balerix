# Spec M part 2: Delivery Confirmation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A reaction says what the plugin knows: 📨 when `send_text` returned `Ok`, 👍 when the agent's `UserPromptSubmit` carries that text, and a thread note when neither comes within the window, with the clock waiting for the current turn to end. The tracker is a pure module in `balerix-plugin-common`; matrix adopts it here and the GitHub plugin (part 3) reuses it.

**Architecture:** `common::delivery::Deliveries<M>` keeps, per agent, a FIFO of prompts sent and not yet confirmed (each with the plugin's marker `M`, its send time and its window) plus a mid-turn bit; `on_event` confirms on a whitespace-normalised match and reports skipped older prompts, `expire` reports the ones past their window. `common::config` gains the `90s`/`5m`/`2h`/`0` duration grammar. Matrix gets a `SENT` mark, a `Tick` command from a five-second interval, a `confirmWindow` agent setting and the two-step reactions; its answer path (👍 on the keys, ✅ on the recorded answer) is untouched.

**Tech Stack:** Rust 1.98 (edition 2024), tokio (paused time in tests), serde, insta, cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-09-22-balerix-m-github-plugin-design.md` §4.2 (`confirmWindow`), §8.7, §13 (M-15, #85). Part 1 is `2026-09-27-balerix-m1-restricted-surface.md`; part 3 is `2026-09-27-balerix-m3-github-plugin.md`. Independent of part 1; part 3 needs both.

## Global Constraints

- Run cargo through mise; each plugin project is its own workspace: `mise run plugin common` and `mise run plugin matrix` must pass at the end of every task that touches them. `mise run check` is unaffected (no core crate changes) but run it once at the end.
- No new dependencies: common already has `tokio` with `sync` and `time`; matrix already has everything.
- `unsafe_code = "forbid"`; clippy `all` warn, `unwrap_used`/`expect_used` warn outside tests; common has `missing_docs = "warn"`, so every new `pub` item carries a doc comment.
- Time inside the tracker and the matrix actor is `tokio::time::Instant`, never `std::time::Instant`, so `#[tokio::test(start_paused = true)]` and `tokio::time::advance` drive the window in tests.
- The exact strings: matrix's new mark is `📨`; the notice is `**not confirmed by <agent> after <n>s**: Claude did not take the prompt (a dialog may be open, or the text may have been swallowed)`.
- Matching is equality after trimming both ends and collapsing each run of whitespace to one space, nothing looser.
- Commit after every task. The PR title is `feat(common): delivery confirmation, adopted by matrix as 📨 then 👍 (Spec M §8.7, #85)`; it touches `plugins/common/` and `plugins/matrix/`, so it releases common and matrix (AGENTS.md).

## Review Focus

1. **A prompt submitted at the terminal by the operator while a thread prompt is pending, with different text,** must not confirm the thread prompt nor mark it unconfirmed: it is ignored, and the mid-turn bit is set. Test: `an_unrelated_submit_is_ignored_but_starts_a_turn` in Task 2.
2. **A pending prompt on an agent that dies mid-turn** (no `Stop` ever comes, `SessionEnd` or a `Dead` phase instead) must still expire: `SessionEnd` clears the bit, and `Deactivate` forgets the agent. Test: `session_end_clears_the_turn_so_expiry_can_run` in Task 2, `deactivate_forgets_pending_prompts_silently` in Task 4.
3. **A body that is only whitespace, or empty after trimming,** must still be sent (matrix routes it today) but never tracked: an empty normalised text would match the next empty submit, which never comes. Test: `a_blank_body_is_sent_but_not_tracked` in Task 4.
4. **A window of `0` on the agent** must never note and must still confirm. Test: `a_zero_window_never_notes_and_still_confirms` in Task 4.
5. **More than 64 prompts pending on one agent** (Claude stuck, the operator keeps typing) must not grow without bound: the oldest is evicted and reported unconfirmed. Test: `the_queue_is_bounded_and_evicts_the_oldest` in Task 2.

---

## File Structure

**Created**

- `plugins/common/src/delivery.rs` — the pure tracker.

**Modified**

- `plugins/common/src/config.rs` — `parse_duration`, `deserialize_duration`.
- `plugins/common/src/metrics.rs` — `Shared.deliveries`.
- `plugins/common/src/lib.rs`, `README.md` — the module and its one-paragraph description.
- `plugins/matrix/src/config.rs` — `AgentConfig.confirm_window`.
- `plugins/matrix/src/matrix.rs` — `SENT`.
- `plugins/matrix/src/actor.rs` — `Command::Tick`, `TICK`, the tracker, the two-step reactions, the notice; tests.
- `plugins/matrix/src/main.rs` — the interval task.
- `scripts/verify-matrix.sh`, `ARCHITECTURE.md`, the Spec M file (§16 part 2).

---

### Task 1: The duration grammar (`common::config`)

**Files:**
- Modify: `plugins/common/src/config.rs`.

**Interfaces:**
- Produces:
  - `pub fn parse_duration(path: &str, s: &str) -> Result<Duration, ConfigError>`: `90`, `90s`, `5m`, `2h`, `0`.
  - `pub fn deserialize_duration<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Duration, D::Error>` for `#[serde(deserialize_with)]`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module of `plugins/common/src/config.rs`:

```rust
    #[test]
    fn durations_take_seconds_minutes_hours_and_zero() {
        use std::time::Duration;
        assert_eq!(parse_duration("w", "90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("w", "30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("w", "5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("w", "2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("w", " 0 ").unwrap(), Duration::ZERO);
        for bad in ["", "s", "5d", "-1", "1.5s", "5 m"] {
            let e = parse_duration("idleTimeout", bad).unwrap_err();
            assert_eq!(e.path, "idleTimeout", "{bad:?}");
            assert_eq!(
                e.message,
                format!("invalid duration {bad:?} (use 30s, 5m, 2h or 0)"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn deserialize_duration_reads_the_same_grammar_with_the_field_path() {
        use std::time::Duration;
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct S {
            #[serde(deserialize_with = "deserialize_duration")]
            confirm_window: Duration,
        }
        let s: S = deserialize(&json!({ "confirmWindow": "45s" })).unwrap();
        assert_eq!(s.confirm_window, Duration::from_secs(45));
        let e = deserialize::<S>(&json!({ "confirmWindow": "soon" })).unwrap_err();
        assert_eq!(e.path, "confirmWindow");
        assert!(e.message.contains("invalid duration \"soon\""), "{e}");
        let e = deserialize::<S>(&json!({ "confirmWindow": 30 })).unwrap_err();
        assert_eq!(e.path, "confirmWindow");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd plugins/common && mise x -- cargo nextest run duration`
Expected: FAIL to compile, `parse_duration` not found.

- [ ] **Step 3: Implement**

Add to `plugins/common/src/config.rs` (after `validate_key_delay`):

```rust
/// A duration as the plugins' config writes it (Spec M §4.1): `90`
/// (seconds), `90s`, `5m`, `2h`, or `0`. `path` is the config key the
/// error names.
pub fn parse_duration(path: &str, s: &str) -> Result<std::time::Duration, ConfigError> {
    let invalid = || ConfigError {
        path: path.to_string(),
        message: format!("invalid duration {s:?} (use 30s, 5m, 2h or 0)"),
    };
    let t = s.trim();
    let (digits, unit) = match t.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => t.split_at(i),
        None => (t, "s"),
    };
    if digits.is_empty() {
        return Err(invalid());
    }
    let n: u64 = digits.parse().map_err(|_| invalid())?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(invalid()),
    };
    Ok(std::time::Duration::from_secs(n * mult))
}

/// `#[serde(deserialize_with = "deserialize_duration")]`: the grammar of
/// `parse_duration` on a string field. The path in the error is the
/// field's, added by `deserialize`'s `serde_path_to_error`.
pub fn deserialize_duration<'de, D>(d: D) -> Result<std::time::Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    parse_duration("", &s).map_err(|e| serde::de::Error::custom(e.message))
}
```

(`use serde::Deserialize;` is already imported for `Secret`; if not, add it.) Note the `path` inside `deserialize_duration` is `""`: `ConfigError`'s `Display` prints just the message when the path is empty, and `deserialize` supplies the field path.

- [ ] **Step 4: Run the tests**

Run: `cd plugins/common && mise x -- cargo nextest run config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add plugins/common/src/config.rs
git commit -m "feat(common): the 30s/5m/2h/0 duration grammar for plugin config (Spec M §4.1)"
```

---

### Task 2: `common::delivery`

**Files:**
- Create: `plugins/common/src/delivery.rs`
- Modify: `plugins/common/src/lib.rs`, `plugins/common/src/metrics.rs`, `plugins/common/README.md`.

**Interfaces:**
- Consumes: `balerix_api::HookEvent`.
- Produces (all `pub`, in `balerix_plugin_common::delivery`):
  - `pub const MAX_PENDING: usize = 64;`
  - `pub enum Kind { Prompt, Command }`, `pub fn classify(text: &str) -> Kind`.
  - `pub fn normalize(text: &str) -> String`.
  - `pub struct Outcome<M> { pub confirmed: Option<M>, pub skipped: Vec<M> }` (`Default`).
  - `pub struct Deliveries<M>` (`Default`), with:
    - `pub fn sent(&mut self, agent: &str, text: &str, marker: M, now: Instant, window: Duration) -> Option<M>` (the evicted oldest when past `MAX_PENDING`).
    - `pub fn on_event(&mut self, event: &HookEvent, now: Instant) -> Outcome<M>`.
    - `pub fn expire(&mut self, now: Instant) -> Vec<M>`.
    - `pub fn forget(&mut self, agent: &str) -> Vec<M>`.
    - `pub fn pending(&self, agent: &str) -> usize`.
  - `Shared.deliveries: IntCounterVec` (`deliveries_total{outcome}`; outcomes `confirmed`, `unconfirmed`, `command`).

- [ ] **Step 1: Write the module with its failing tests**

Create `plugins/common/src/delivery.rs`:

```rust
//! Delivery confirmation (Spec M §8.7, #85). `send_text`'s `Ok` says the
//! daemon typed the body into the pane and pressed Enter; it does not say
//! Claude took a prompt. The agent's own `UserPromptSubmit` carries the
//! text Claude took, and that is the proof. This tracker is pure: the
//! plugin feeds it what it sent and every hook event, and executes what
//! it answers (a "sent" mark, a confirmation, a note).
//!
//! Text typed mid-turn is queued by Claude and submitted only after the
//! current turn's `Stop`, so a pending prompt's clock starts at the later
//! of its send and the agent's last `Stop`, and never runs while the
//! agent is mid-turn.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use balerix_api::HookEvent;
use serde_json::Value;
use tokio::time::Instant;

/// Prompts pending per agent beyond which the oldest is evicted and
/// reported unconfirmed: a stuck agent must not grow this without bound.
pub const MAX_PENDING: usize = 64;

/// What a body is to Claude's input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Prose: a `UserPromptSubmit` will carry it if Claude takes it.
    Prompt,
    /// A slash command: most fire no hook, so "sent" is all a plugin can
    /// honestly say. Never tracked.
    Command,
}

/// A body whose first non-blank character is `/` is a command.
pub fn classify(text: &str) -> Kind {
    if text.trim_start().starts_with('/') {
        Kind::Command
    } else {
        Kind::Prompt
    }
}

/// Trimmed at both ends, each run of whitespace one space: what two
/// texts must agree on to be the same prompt.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What one hook event settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome<M> {
    /// The prompt this `UserPromptSubmit` took.
    pub confirmed: Option<M>,
    /// Older prompts it passed over: Claude submits queued text in order,
    /// so a skipped one was swallowed. Unconfirmed at once.
    pub skipped: Vec<M>,
}

impl<M> Default for Outcome<M> {
    fn default() -> Self {
        Self {
            confirmed: None,
            skipped: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct Pending<M> {
    text: String,
    marker: M,
    sent_at: Instant,
    window: Duration,
}

#[derive(Debug)]
struct AgentState<M> {
    pending: VecDeque<Pending<M>>,
    in_turn: bool,
    last_stop: Option<Instant>,
}

impl<M> Default for AgentState<M> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            in_turn: false,
            last_stop: None,
        }
    }
}

/// The prompts sent and not yet confirmed, per agent. `M` is the
/// plugin's marker for a sent prompt (a message id, a comment id).
#[derive(Debug)]
pub struct Deliveries<M> {
    agents: HashMap<String, AgentState<M>>,
}

impl<M> Default for Deliveries<M> {
    fn default() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }
}

impl<M> Deliveries<M> {
    /// Registers a prompt typed into `agent`'s pane at `now`. `window` is
    /// how long it may go unconfirmed before `expire` reports it; zero
    /// means never. Answers the oldest pending prompt when this one
    /// pushed the queue past `MAX_PENDING`; the caller reports it
    /// unconfirmed. A body that normalises to nothing is not tracked.
    pub fn sent(
        &mut self,
        agent: &str,
        text: &str,
        marker: M,
        now: Instant,
        window: Duration,
    ) -> Option<M> {
        let text = normalize(text);
        if text.is_empty() {
            return None;
        }
        let state = self.agents.entry(agent.to_string()).or_default();
        state.pending.push_back(Pending {
            text,
            marker,
            sent_at: now,
            window,
        });
        if state.pending.len() > MAX_PENDING {
            return state.pending.pop_front().map(|p| p.marker);
        }
        None
    }

    /// One hook event. `UserPromptSubmit` starts a turn and, when its
    /// `prompt` matches a pending prompt, confirms the oldest such and
    /// skips everything older. `Stop` and `SessionEnd` end the turn.
    pub fn on_event(&mut self, event: &HookEvent, now: Instant) -> Outcome<M> {
        let Some(state) = self.agents.get_mut(&event.agent) else {
            return Outcome::default();
        };
        match event.name.as_str() {
            "UserPromptSubmit" => {
                state.in_turn = true;
                let prompt = normalize(
                    event
                        .payload
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
                let Some(i) = state.pending.iter().position(|p| p.text == prompt) else {
                    return Outcome::default();
                };
                let skipped = state.pending.drain(..i).map(|p| p.marker).collect();
                Outcome {
                    confirmed: state.pending.pop_front().map(|p| p.marker),
                    skipped,
                }
            }
            "Stop" | "SessionEnd" => {
                state.in_turn = false;
                state.last_stop = Some(now);
                Outcome::default()
            }
            _ => Outcome::default(),
        }
    }

    /// Every pending prompt whose window has passed since the later of
    /// its send and the agent's last `Stop`, on agents not mid-turn.
    /// Removed as they are answered.
    pub fn expire(&mut self, now: Instant) -> Vec<M> {
        let mut out = Vec::new();
        for state in self.agents.values_mut() {
            if state.in_turn {
                continue;
            }
            let last_stop = state.last_stop;
            let mut kept = VecDeque::new();
            for p in state.pending.drain(..) {
                let start = last_stop.map_or(p.sent_at, |s| s.max(p.sent_at));
                if !p.window.is_zero() && now.duration_since(start) >= p.window {
                    out.push(p.marker);
                } else {
                    kept.push_back(p);
                }
            }
            state.pending = kept;
        }
        out
    }

    /// Drops everything held for `agent`, answering the markers so the
    /// caller may report or ignore them.
    pub fn forget(&mut self, agent: &str) -> Vec<M> {
        self.agents
            .remove(agent)
            .map(|s| s.pending.into_iter().map(|p| p.marker).collect())
            .unwrap_or_default()
    }

    /// How many prompts `agent` has pending.
    pub fn pending(&self, agent: &str) -> usize {
        self.agents.get(agent).map_or(0, |s| s.pending.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::event;
    use serde_json::json;

    const W: Duration = Duration::from_secs(30);

    fn submit(text: &str) -> HookEvent {
        event("f/c/a", "UserPromptSubmit", json!({ "prompt": text }))
    }

    fn stop() -> HookEvent {
        event("f/c/a", "Stop", json!({}))
    }

    #[test]
    fn a_slash_command_is_a_command_and_prose_is_a_prompt() {
        assert_eq!(classify("/exit"), Kind::Command);
        assert_eq!(classify("  /compact "), Kind::Command);
        assert_eq!(classify("run /tmp/x"), Kind::Prompt);
        assert_eq!(classify(""), Kind::Prompt);
    }

    #[test]
    fn normalize_trims_and_collapses_whitespace() {
        assert_eq!(normalize("  run\n\n the\t tests  "), "run the tests");
        assert_eq!(normalize(" \n "), "");
    }

    #[test]
    fn an_exact_and_a_whitespace_differing_submit_confirm() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        assert_eq!(d.sent("f/c/a", "run the tests", 1, now, W), None);
        let o = d.on_event(&submit("run the tests"), now);
        assert_eq!(o, Outcome { confirmed: Some(1), skipped: vec![] });
        assert_eq!(d.pending("f/c/a"), 0);

        d.sent("f/c/a", "run\n  the tests\n", 2, now, W);
        let o = d.on_event(&submit("  run the   tests"), now);
        assert_eq!(o.confirmed, Some(2));
    }

    #[test]
    fn an_unrelated_submit_is_ignored_but_starts_a_turn() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "run the tests", 1, now, W);
        let o = d.on_event(&submit("something the operator typed"), now);
        assert_eq!(o, Outcome::default());
        assert_eq!(d.pending("f/c/a"), 1);
        // mid-turn: the window does not run
        assert!(d.expire(now + Duration::from_secs(600)).is_empty());
    }

    #[test]
    fn a_later_prompt_taken_first_skips_the_earlier_one() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "first", 1, now, W);
        d.sent("f/c/a", "second", 2, now, W);
        d.sent("f/c/a", "third", 3, now, W);
        let o = d.on_event(&submit("second"), now);
        assert_eq!(o, Outcome { confirmed: Some(2), skipped: vec![1] });
        assert_eq!(d.pending("f/c/a"), 1, "the third still waits");
    }

    #[test]
    fn expiry_counts_from_the_send_when_the_agent_is_idle() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, W);
        assert!(d.expire(now + Duration::from_secs(29)).is_empty());
        assert_eq!(d.expire(now + Duration::from_secs(30)), vec![1]);
        assert_eq!(d.pending("f/c/a"), 0);
    }

    #[test]
    fn expiry_waits_for_the_turn_to_end_then_counts_from_the_stop() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.on_event(&submit("earlier work"), now); // mid-turn (nothing pending yet)
        d.sent("f/c/a", "x", 1, now, W);
        d.on_event(&submit("earlier work"), now);
        assert!(d.expire(now + Duration::from_secs(600)).is_empty(), "mid-turn");
        let stopped = now + Duration::from_secs(600);
        d.on_event(&stop(), stopped);
        assert!(d.expire(stopped + Duration::from_secs(29)).is_empty());
        assert_eq!(d.expire(stopped + Duration::from_secs(30)), vec![1]);
    }

    #[test]
    fn session_end_clears_the_turn_so_expiry_can_run() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.on_event(&submit("earlier"), now);
        d.sent("f/c/a", "x", 1, now, W);
        d.on_event(&event("f/c/a", "SessionEnd", json!({})), now);
        assert_eq!(d.expire(now + W), vec![1]);
    }

    #[test]
    fn a_zero_window_never_expires_and_still_confirms() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        assert!(d.expire(now + Duration::from_secs(86_400)).is_empty());
        assert_eq!(d.on_event(&submit("x"), now).confirmed, Some(1));
    }

    #[test]
    fn a_blank_body_is_not_tracked() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        assert_eq!(d.sent("f/c/a", " \n ", 1, now, W), None);
        assert_eq!(d.pending("f/c/a"), 0);
        assert_eq!(d.on_event(&submit(""), now), Outcome::default());
    }

    #[test]
    fn the_queue_is_bounded_and_evicts_the_oldest() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        for i in 0..MAX_PENDING {
            assert_eq!(d.sent("f/c/a", &format!("p{i}"), i, now, W), None);
        }
        assert_eq!(d.sent("f/c/a", "one more", MAX_PENDING, now, W), Some(0));
        assert_eq!(d.pending("f/c/a"), MAX_PENDING);
    }

    #[test]
    fn forget_drops_an_agent_and_answers_its_markers() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, W);
        d.sent("f/c/b", "y", 2, now, W);
        assert_eq!(d.forget("f/c/a"), vec![1]);
        assert_eq!(d.forget("f/c/a"), Vec::<u32>::new());
        assert_eq!(d.pending("f/c/b"), 1);
        assert_eq!(d.on_event(&event("f/c/a", "Stop", json!({})), now), Outcome::default());
    }
}
```

- [ ] **Step 2: Register the module and run**

`plugins/common/src/lib.rs`: add, after the `answer` line and with the same one-line doc style:

```rust
/// Delivery confirmation: 📨 on `Ok`, 👍 on the matching `UserPromptSubmit` (Spec M §8.7).
pub mod delivery;
```

Run: `cd plugins/common && mise x -- cargo nextest run delivery`
Expected: PASS (every test in the module).

- [ ] **Step 3: The metric family**

`plugins/common/src/metrics.rs`: add to `Shared`:

```rust
    /// `deliveries_total{outcome}`: `confirmed`, `unconfirmed`, `command`
    /// (Spec M §8.7).
    pub deliveries: IntCounterVec,
```

and in `new`:

```rust
            deliveries: metrics.int_counter_vec(
                "deliveries_total",
                "Prompts sent to an agent, by whether Claude took them",
                &["outcome"],
            )?,
```

In the test `every_family_carries_the_plugin_prefix` add `s.deliveries.with_label_values(&["confirmed"]).inc();` and, where it asserts the rendered names, `balerix_plugin_chat_deliveries_total`.

Run: `cd plugins/common && mise x -- cargo nextest run metrics`
Expected: PASS.

- [ ] **Step 4: README**

In `plugins/common/README.md`, after the paragraph beginning "Long bodies: `render::split(…)`", add:

```
Delivery: `delivery::Deliveries<M>` tracks the prompts a plugin sent with
`send_text` and pairs each with the agent's `UserPromptSubmit` (Spec M
§8.7). Mark the message "sent" on `Ok`, feed every hook event to
`on_event` and react "confirmed" on what it answers, feed a tick to
`expire` and post its note for what never came; `classify` tells a slash
command, which is never tracked, from a prompt.
```

- [ ] **Step 5: Lint and commit**

Run: `mise run plugin common`
Expected: PASS, no `missing_docs` warning.

```bash
git add plugins/common
git commit -m "feat(common): delivery confirmation, the pure tracker behind 📨 then 👍 (Spec M §8.7, #85)"
```

---

### Task 3: `confirmWindow` on the matrix agent config

**Files:**
- Modify: `plugins/matrix/src/config.rs:54-82` and its tests.

**Interfaces:**
- Produces: `AgentConfig.confirm_window: Duration` (YAML `confirmWindow`, default 30 s), `pub const DEFAULT_CONFIRM_WINDOW: Duration`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module of `plugins/matrix/src/config.rs`:

```rust
    #[test]
    fn confirm_window_defaults_to_30s_and_reads_the_duration_grammar() {
        use std::time::Duration;
        assert_eq!(parse_agent(&json!({})).unwrap().confirm_window, Duration::from_secs(30));
        assert_eq!(
            parse_agent(&json!({ "confirmWindow": "2m" })).unwrap().confirm_window,
            Duration::from_secs(120)
        );
        assert_eq!(
            parse_agent(&json!({ "confirmWindow": "0" })).unwrap().confirm_window,
            Duration::ZERO
        );
        assert_eq!(
            parse_agent(&json!({ "confirmWindow": "soon" })).unwrap_err().to_string(),
            "confirmWindow: invalid duration \"soon\" (use 30s, 5m, 2h or 0)"
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd plugins/matrix && mise x -- cargo nextest run confirm_window`
Expected: FAIL to compile.

- [ ] **Step 3: Add the field**

In `plugins/matrix/src/config.rs`: extend the `pub use balerix_plugin_common::config::{…}` line with `deserialize_duration`; add `pub const DEFAULT_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);` beside `DEFAULT_MAX_PARTS`; in `AgentConfig` add

```rust
    /// How long a routed prompt may go without the agent's
    /// `UserPromptSubmit` before the thread gets a note (Spec M §8.7);
    /// `0` disables the note, not the confirmation.
    #[serde(rename = "confirmWindow", deserialize_with = "deserialize_duration")]
    pub confirm_window: std::time::Duration,
```

and `confirm_window: DEFAULT_CONFIRM_WINDOW` in `Default`. (`#[serde(default)]` on the struct fills the field from `Default` when absent; `deserialize_with` applies when present.)

- [ ] **Step 4: Run and commit**

Run: `cd plugins/matrix && mise x -- cargo nextest run config`
Expected: PASS.

```bash
git add plugins/matrix/src/config.rs
git commit -m "feat(matrix): confirmWindow per agent (Spec M §4.2)"
```

---

### Task 4: The two-step reactions in the matrix actor

**Files:**
- Modify: `plugins/matrix/src/matrix.rs:10-17` (`SENT`), `src/actor.rs` (`Command`, `Counters`, `Actor`, `handle`, `on_event`, `on_inbound`, a new `unconfirmed`, tests), `src/main.rs` (the interval task).

**Interfaces:**
- Consumes: `delivery::{Deliveries, Kind, classify}`, `Shared.deliveries`, `AgentConfig.confirm_window`.
- Produces:
  - `matrix::SENT: &str = "📨"`.
  - `Command::Tick` and `pub const TICK: Duration = Duration::from_secs(5)` in `actor.rs`.
  - `struct Sent { room, root, event_id, agent }` (private, the tracker's marker).

- [ ] **Step 1: Write the failing tests**

In `plugins/matrix/src/actor.rs`'s `tests` module, first give `inbound` an id: change the helper to

```rust
    fn inbound(room: &str, root: Option<&str>, sender: &str, body: &str) -> Inbound {
        inbound_with_id(room, root, sender, body, "$msg:fake")
    }

    fn inbound_with_id(room: &str, root: Option<&str>, sender: &str, body: &str, id: &str) -> Inbound {
        Inbound {
            room: room.to_string(),
            event_id: id.to_string(),
            sender: sender.to_string(),
            thread_root: root.map(str::to_string),
            body: body.to_string(),
        }
    }

    fn submitted(agent: &str, text: &str) -> HookEvent {
        during(agent, "s1", "UserPromptSubmit", json!({ "prompt": text }))
    }

    fn notices(calls: &[Call]) -> Vec<String> {
        sends(calls)
            .into_iter()
            .filter(|(_, b)| b.starts_with("**not confirmed by"))
            .map(|(_, b)| b)
            .collect()
    }
```

Replace `a_thread_reply_becomes_a_submitted_send_text_and_is_acknowledged` with:

```rust
    #[tokio::test]
    async fn a_thread_reply_is_marked_sent_and_confirmed_when_claude_takes_it() {
        let (fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(&room, Some(&root), "@rahul:example.org", "run the tests")))
            .await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendText { text: "run the tests".into(), submit: true }]
        );
        assert_eq!(reactions(&port.calls()), vec![SENT.to_string()], "sent, not yet taken");

        a.handle(Command::Events(vec![submitted("f/c/alice", "run\n the  tests ")]))
            .await;
        assert_eq!(reactions(&port.calls()), vec![SENT.to_string(), ACK.to_string()]);
        assert!(notices(&port.calls()).is_empty());
        assert_eq!(
            port.calls().last(),
            Some(&Call::React { room, event_id: "$msg:fake".into(), key: ACK.into() }),
            "the 👍 lands on the operator's message"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_prompt_gets_a_note_after_the_window() {
        let (_fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(&room, Some(&root), "@rahul:example.org", "run the tests")))
            .await;
        tokio::time::advance(Duration::from_secs(29)).await;
        a.handle(Command::Tick).await;
        assert!(notices(&port.calls()).is_empty());
        tokio::time::advance(Duration::from_secs(1)).await;
        a.handle(Command::Tick).await;
        assert_eq!(
            notices(&port.calls()),
            vec!["**not confirmed by f/c/alice after 30s**: Claude did not take the prompt (a dialog may be open, or the text may have been swallowed)".to_string()]
        );
        assert_eq!(reactions(&port.calls()), vec![SENT.to_string()], "no 👍, no extra mark");
        a.handle(Command::Tick).await;
        assert_eq!(notices(&port.calls()).len(), 1, "noted once");
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_waits_for_the_turn_to_end() {
        let (_fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Events(vec![submitted("f/c/alice", "earlier work")])).await;
        a.handle(Command::Inbound(inbound(&room, Some(&root), "@rahul:example.org", "next")))
            .await;
        tokio::time::advance(Duration::from_secs(600)).await;
        a.handle(Command::Tick).await;
        assert!(notices(&port.calls()).is_empty(), "mid-turn: queued, not swallowed");
        a.handle(Command::Events(vec![during("f/c/alice", "s1", "Stop", json!({}))])).await;
        tokio::time::advance(Duration::from_secs(29)).await;
        a.handle(Command::Tick).await;
        assert!(notices(&port.calls()).is_empty());
        tokio::time::advance(Duration::from_secs(1)).await;
        a.handle(Command::Tick).await;
        assert_eq!(notices(&port.calls()).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slash_command_is_sent_and_never_confirmed_or_noted() {
        let (_fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(&room, Some(&root), "@rahul:example.org", "/compact")))
            .await;
        assert_eq!(reactions(&port.calls()), vec![SENT.to_string()]);
        tokio::time::advance(Duration::from_secs(3600)).await;
        a.handle(Command::Tick).await;
        assert!(notices(&port.calls()).is_empty());
        assert_eq!(a.counters.deliveries.with_label_values(&["command"]).get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_window_never_notes_and_still_confirms() {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: crate::config::parse_agent(&json!({ "confirmWindow": "0" })).unwrap(),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")])).await;
        let room = "!room1:fake".to_string();
        let root = minted_root(&port.take_calls(), 1);
        a.handle(Command::Inbound(inbound(&room, Some(&root), "@rahul:example.org", "x"))).await;
        tokio::time::advance(Duration::from_secs(86_400)).await;
        a.handle(Command::Tick).await;
        assert!(notices(&port.calls()).is_empty());
        a.handle(Command::Events(vec![submitted("f/c/alice", "x")])).await;
        assert_eq!(reactions(&port.calls()), vec![SENT.to_string(), ACK.to_string()]);
        let _ = fake;
    }

    #[tokio::test]
    async fn a_later_prompt_taken_first_notes_the_earlier_one() {
        let (_fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound_with_id(&room, Some(&root), "@rahul:example.org", "first", "$one:fake")))
            .await;
        a.handle(Command::Inbound(inbound_with_id(&room, Some(&root), "@rahul:example.org", "second", "$two:fake")))
            .await;
        a.handle(Command::Events(vec![submitted("f/c/alice", "second")])).await;
        assert_eq!(notices(&port.calls()).len(), 1, "the first was swallowed");
        assert_eq!(
            port.calls().last(),
            Some(&Call::React { room, event_id: "$two:fake".into(), key: ACK.into() })
        );
        assert_eq!(a.counters.deliveries.with_label_values(&["unconfirmed"]).get(), 1);
        assert_eq!(a.counters.deliveries.with_label_values(&["confirmed"]).get(), 1);
    }

    #[tokio::test]
    async fn a_blank_body_is_sent_but_not_tracked() {
        let (fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(&room, Some(&root), "@rahul:example.org", "   "))).await;
        assert_eq!(fake.actions_for("f/c/alice").len(), 1);
        assert_eq!(reactions(&port.calls()), vec![SENT.to_string()]);
        assert_eq!(a.deliveries.pending("f/c/alice"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn deactivate_forgets_pending_prompts_silently() {
        let (_fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(&room, Some(&root), "@rahul:example.org", "x"))).await;
        a.handle(deactivate("f/c/alice")).await;
        tokio::time::advance(Duration::from_secs(3600)).await;
        a.handle(Command::Tick).await;
        assert!(notices(&port.calls()).is_empty());
    }
```

Check the existing test at line ~2106 (`an_exact_reply_is_echoed_then_sent_as_keys_and_never_as_text`): it asserts `vec![ACK]` on an answer, which stays 👍 (answers go by `send_keys`, not tracked). Any other test asserting `ACK` right after an `Inbound` that is a prose prompt changes to `SENT`.

- [ ] **Step 2: Run to verify failure**

Run: `cd plugins/matrix && mise x -- cargo nextest run actor::tests`
Expected: FAIL to compile (`SENT`, `Command::Tick`, `deliveries`).

- [ ] **Step 3: Implement**

`plugins/matrix/src/matrix.rs`, with the other marks:

```rust
/// The daemon typed the reply into the pane (Spec M §8.7); `ACK` follows
/// once Claude takes it.
pub const SENT: &str = "📨";
```

`plugins/matrix/src/actor.rs`:

1. Imports: `use balerix_plugin_common::delivery::{self, Deliveries, Kind};`, `use tokio::time::Instant;`, and `SENT` in the `crate::matrix` import.
2. `pub const TICK: Duration = Duration::from_secs(5);` with a doc line ("How often `main` pushes `Command::Tick`, which runs delivery expiry (Spec M §8.7)").
3. `Command` gains `/// Runs delivery expiry; pushed by `main` every `TICK`.` `Tick,`.
4. `Counters` gains `pub deliveries: IntCounterVec,` filled from `shared.deliveries`.
5. The marker:

```rust
/// What a routed prompt is to the thread: where the 👍 or the note goes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sent {
    room: String,
    root: String,
    event_id: String,
    agent: String,
}
```

6. `Actor` gains `deliveries: Deliveries<Sent>,` (initialised `Deliveries::default()` in `new`).
7. In `handle`: `Command::Deactivate` adds `self.deliveries.forget(&agent);` (nothing reported: the agent is gone); a new arm `Command::Tick => self.on_tick().await,`.
8. New methods:

```rust
    async fn on_tick(&mut self) {
        for sent in self.deliveries.expire(Instant::now()) {
            self.unconfirmed(sent).await;
        }
    }

    /// Spec M §8.7: 📨 without 👍 plus this note is the picture.
    async fn unconfirmed(&self, sent: Sent) {
        self.counters
            .deliveries
            .with_label_values(&["unconfirmed"])
            .inc();
        let window = self
            .agents
            .get(&sent.agent)
            .map(|c| c.confirm_window)
            .unwrap_or(crate::config::DEFAULT_CONFIRM_WINDOW);
        let body = format!(
            "**not confirmed by {} after {}s**: Claude did not take the prompt (a dialog may be open, or the text may have been swallowed)",
            sent.agent,
            window.as_secs()
        );
        self.send(&sent.room, Some(&sent.root), &body, "notice").await;
    }
```

9. In `on_event`, right after the `let Some(config) = self.agents.get(…)` guard and before `track_question`:

```rust
        // Spec M §8.7: the agent's own `UserPromptSubmit` is the proof a
        // routed prompt was taken; like question tracking, it runs above
        // every early return.
        let outcome = self.deliveries.on_event(&event, Instant::now());
        for sent in outcome.skipped {
            self.unconfirmed(sent).await;
        }
        if let Some(sent) = outcome.confirmed {
            self.counters
                .deliveries
                .with_label_values(&["confirmed"])
                .inc();
            self.react_to(&sent.room, &sent.event_id, ACK).await;
        }
```

10. In `on_inbound`, the `Ok(())` arm becomes:

```rust
            Ok(()) => {
                count("routed");
                self.react(&message, SENT).await;
                match delivery::classify(&message.body) {
                    Kind::Command => {
                        self.counters
                            .deliveries
                            .with_label_values(&["command"])
                            .inc();
                    }
                    Kind::Prompt => {
                        let window = self
                            .agents
                            .get(&agent)
                            .map(|c| c.confirm_window)
                            .unwrap_or(crate::config::DEFAULT_CONFIRM_WINDOW);
                        let sent = Sent {
                            room: message.room.clone(),
                            root: root.clone(),
                            event_id: message.event_id.clone(),
                            agent: agent.clone(),
                        };
                        if let Some(evicted) =
                            self.deliveries
                                .sent(&agent, &message.body, sent, Instant::now(), window)
                        {
                            self.unconfirmed(evicted).await;
                        }
                    }
                }
            }
```

11. `plugins/matrix/src/main.rs`: after the `phase_watch` spawn:

```rust
        let tick_queue = plugin.queue();
        let ticker = tokio::spawn(async move {
            let mut interval = tokio::time::interval(balerix_plugin_matrix::actor::TICK);
            loop {
                interval.tick().await;
                tick_queue.push(Command::Tick);
            }
        });
```

and `ticker.abort();` beside `phase_watch.abort();`.

- [ ] **Step 4: Run the matrix suite**

Run: `mise run plugin matrix`
Expected: PASS. `tests/plugin_it.rs` is unaffected (it asserts the `send_text`, not the reaction); if it asserts `ACK` after the reply, change it to `SENT`.

- [ ] **Step 5: Commit**

```bash
git add plugins/matrix
git commit -m "feat(matrix): react 📨 on send and 👍 when Claude takes the prompt, a note when it never does (Spec M §8.7, #85)"
```

---

### Task 5: Documentation and the manual check

**Files:**
- Modify: `scripts/verify-matrix.sh` (the checklist), `ARCHITECTURE.md:71-79`, the Spec M file.

- [ ] **Step 1: The manual checklist**

In `scripts/verify-matrix.sh`, replace the bullet "replying inside a live thread reaches the agent and the reply is acknowledged with a reaction;" with:

```
  - replying inside a live thread reaches the agent: the reply gets 📨 at
    once and 👍 a moment later, when Claude takes it;
  - replying while the agent is mid-turn: 📨 at once, 👍 only after the
    turn ends and the queued text is submitted, and no note in between;
  - replying while a permission dialog is open (not a question): 📨, no
    👍, and after thirty seconds a "not confirmed by" note in the thread;
  - replying with a slash command (`/compact`): 📨 and nothing more;
```

- [ ] **Step 2: ARCHITECTURE and Spec M**

`ARCHITECTURE.md`, in the `balerix-plugin-matrix` paragraph, after "the matrix actor executes common's `answer` decisions." insert "A routed reply is 📨 when typed and 👍 when the agent's `UserPromptSubmit` carries it, through common's `delivery` tracker (Spec M §8.7)." In the `balerix-plugin-common` paragraph's list of modules, add `delivery` after `answer`.

Append to `docs/superpowers/specs/2026-09-22-balerix-m-github-plugin-design.md`:

```
## 17. Recorded at implementation, part 2 (delivery confirmation)

- `common::delivery::Deliveries<M>`: `sent` takes the window per prompt
  (`0` never expires) and evicts past `MAX_PENDING` (64) per agent,
  answering the evicted marker; a body that normalises to nothing is not
  tracked; `on_event` takes `now` for `Stop`/`SessionEnd`.
- `common::config::{parse_duration, deserialize_duration}` carry the
  `90s`/`5m`/`2h`/`0` grammar; `confirmWindow` is a `Duration` on the
  agent config.
- Matrix's `Tick` comes from a five-second `tokio::time::interval` in
  `main`; the note is posted once per prompt and nothing else reacts.
- `#85` closes with this part.
```

- [ ] **Step 3: Final check and commit**

Run: `mise run plugins && mise run check`
Expected: PASS.

```bash
git add scripts/verify-matrix.sh ARCHITECTURE.md docs
git commit -m "docs: delivery confirmation in the architecture, the matrix manual check and Spec M (#85)"
```

Open the PR with the title from Global Constraints; its body says "Closes #85".

---

## Done when

1. `mise run plugin common`, `mise run plugin matrix` and `mise run check` pass.
2. A thread reply gets 📨 on `Ok` and 👍 when the agent's `UserPromptSubmit` matches it after whitespace normalisation; a slash command gets 📨 only.
3. A prompt not taken within `confirmWindow` (counted from the later of its send and the agent's last `Stop`, never while mid-turn) gets the one thread note; `0` disables the note.
4. `deliveries_total{outcome}` counts `confirmed`, `unconfirmed` and `command`.
5. The GitHub plugin (part 3) can build on `delivery::Deliveries<M>` unchanged.
