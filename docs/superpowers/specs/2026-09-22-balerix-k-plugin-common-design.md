# Balerix — Spec K: `balerix-plugin-common`

**Date:** 2026-09-22
**Status:** Approved in brainstorm 2026-09-22
**Scope:** a new published library crate, `balerix-plugin-common`, holding
the channel-independent half of the matrix plugin (rendering, the
question flow, config helpers, the actor queue, phase diffing) and the web
plugin's review message, so that the GitHub plugin (Spec M) and third-party
plugins build on them instead of copying them. Matrix and web change only
in where their code lives; their behaviour, wire surface and tests are
unchanged.

Spec L (the daemon side of plugin-managed fleets) and Spec M (the GitHub
plugin) are written with this crate in place. Build order is K, L, M.

---

## 1. Problem

The matrix plugin is 6 800 lines, and most of them know nothing about
Matrix. `render.rs` turns hook events into markdown and splits long bodies
at fence-aware line boundaries; `question.rs` parses an `AskUserQuestion`,
matches a reply to its options and plans the keystrokes; `pending.rs`
mirrors the open question to KV; `config.rs` holds `Secret`, `ConfigError`
and the path-carrying deserializer that every plugin re-implements
(`flow`, `web` and `matrix` each have one); `plugin.rs::phase_changes`
diffs two `fleets/watch` frames; the actor's `Queue`, `Health` and counters
are the shape every observer plugin needs.

The next plugin, GitHub, needs all of it, and the parts that are hardest to
get right — the J-5/J-7 guarantees of the answer flow — are exactly the
parts that must not be copied by hand. A third party writing a Slack or
Discord plugin needs the same.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| K-1 | A new crate, `balerix-plugin-common`, not additions to `balerix-plugin-sdk`. | The SDK is the host protocol; flow and web must not carry a markdown renderer and a question matcher they never call. |
| K-2 | **Toolkit plus a pure answer core** (approach B). Each plugin keeps its own actor; common owns the pure pieces and one decision function for the answer flow. | A generic conversation engine over a `ChatPort` (approach C) would force one model over two that differ in direction: matrix opens threads *from* sessions, GitHub opens sessions *from* issues, and GitHub edits a status comment where matrix appends. A pure decision function shares the guarantees without a port that has to fit both. |
| K-3 | Moved code moves with its tests and snapshots, unchanged. Matrix's actor tests are the regression gate. | The extraction is mechanical; the proof that nothing changed is a green `mise run plugin matrix` with no snapshot edits. |
| K-4 | The body limit is a parameter of `split`, not a constant. | Matrix's ceiling is 4 000 for a phone; GitHub's is the 65 536-character comment limit. |
| K-5 | Published on crates.io as its own release unit. | Third parties build plugins on it, so it is versioned and released like `balerix-api` and the SDK, which it depends on. |
| K-6 | Standalone project under `plugins/common/`, not a workspace member. | Spec H's rule: a plugin's dependency tree stays out of the daemon's feature resolution. Common depends on the SDK, which is a member, by path; the other direction never happens. |
| K-7 | The web plugin's review message moves here too. | Spec M renders a GitHub review into the same message shape; one renderer, one set of limits. |

## 3. Crate and modules

```
plugins/common/
  Cargo.toml        publish = true; balerix-api and balerix-plugin-sdk by
                    { version = "<exact>", path = "../../crates/…" };
                    plugins reach it by path = "../common"
  Cargo.lock
  clippy.toml
  deny.toml
  README.md         "build a plugin on it": the walkthrough (§8)
  CHANGELOG.md
  src/
    lib.rs
    render.rs       event, phase, thread-root and question messages; split
    question.rs     parse, match_reply, plan, describe, recorded checks
    pending.rs      Questions, Stage, OpenQuestion, the KV mirror
    answer.rs       NEW: the pure decision core of the answer flow (§4)
    config.rs       Secret, ConfigError, deserialize, EventFilter
    queue.rs        Queue, Health
    metrics.rs      Shared: the counter families every chat plugin has
    phases.rs       PhaseChange, phase_changes, run
    review.rs       Review, Comment, Side, limits, validate, render_message
    snapshots/      the insta snapshots that move with render and review
```

Dependencies: `balerix-api`, `balerix-plugin-sdk`, `serde`, `serde_json`,
`serde_path_to_error`, `thiserror`, `tracing`, `prometheus` (the SDK's
registry types), `tokio` (`sync`, `time`). Dev: `insta`, `proptest` (the
question plan's model test moves with `question.rs`). Every version exact,
each matching the core workspace's pin where the crate appears there.

`lib.rs` re-exports the modules; nothing is `pub(crate)` that a plugin
would need. Every `pub` item carries a doc comment: the published API is
what `cargo doc` renders.

### 3.1 What moves, by source

| From | To | Change |
|---|---|---|
| `matrix/src/render.rs` | `render.rs` | `split(text, limit, max_parts)`; `BODY_LIMIT` becomes matrix's own constant, passed in. `PhaseChange` moves to `phases.rs` and is re-exported here for the message. |
| `matrix/src/question.rs` | `question.rs` | none; `fixtures` stays `pub` for downstream tests |
| `matrix/src/pending.rs` | `pending.rs` | log lines lose the `matrix:` prefix; the plugin name is a `tracing` span the plugin opens |
| `matrix/src/config.rs`: `Secret`, `ConfigError`, `deserialize` | `config.rs` | `deserialize` becomes `pub fn deserialize<T>(config: &Value) -> Result<T, ConfigError>` |
| `matrix/src/config.rs`: `events`/`LIFECYCLE`/`wants` | `config::EventFilter` | `EventFilter { events: Vec<String> }` with `Default` (the curated four), `wants`, `validate() -> Result<(), ConfigError>` (unknown names, with index). `DEFAULT_EVENTS` and `LIFECYCLE` come along. |
| `matrix/src/config.rs`: `keyDelayMs` bounds | `config::validate_key_delay` | returns the same `ConfigError` |
| `matrix/src/actor.rs`: `Queue`, `Health` | `queue.rs` | none |
| `matrix/src/actor.rs`: `Counters` | `metrics::Shared` | the shared families only (§3.2); matrix keeps `rooms` and `threads_open` in its own struct beside a `Shared` |
| `matrix/src/plugin.rs`: `phase_changes`, `flatten` | `phases.rs` | plus `run` (§3.3) |
| `web/src/review.rs` | `review.rs` | `ReviewBody` is renamed `Review`; web keeps a type alias for its route |

Matrix's `AgentConfig` stays in matrix, built from `EventFilter` plus its
own `phases` and `key_delay_ms`; `parse_agent` calls the two validators.
Matrix's `DaemonConfig` stays in matrix.

### 3.2 `metrics::Shared`

```rust
pub struct Shared {
    pub events_dropped: IntCounter,          // events_dropped_total
    pub messages_sent: IntCounterVec,        // messages_sent_total{kind}
    pub inbound: IntCounterVec,              // inbound_total{outcome}
    pub errors: IntCounterVec,               // errors_total{kind}
    pub answers_mismatched: IntCounter,      // answers_mismatched_total
}
impl Shared { pub fn new(metrics: &Metrics) -> Result<Self, SdkError> }
```

Registered through the SDK's `Metrics`, so the family names carry the
plugin's prefix. The label values are the plugin's; the names in Spec G
§10 and Spec J §8 are the convention.

### 3.3 `phases::run`

```rust
pub async fn run(host: Host, sink: impl FnMut(Vec<PhaseChange>) + Send) -> Infallible
```

The `fleets/watch` loop matrix's `client.rs` runs today: keeps the previous
frame, calls `phase_changes`, hands non-empty results to `sink`, reconnects
when the socket drops. Matrix and GitHub both spawn it and push into their
queue from the sink.

## 4. `answer`: the pure decision core

The answer flow of Spec J §7.2 and §7.3, with no I/O and no port. Two
functions.

```rust
pub enum Reaction { Ack, Refused, Failed, Confirmed }

pub struct Decision {
    /// The message to post in the conversation first, if any.
    pub post: Option<String>,
    /// The action to send to the agent after `post` landed, if any.
    pub send: Option<PluginAction>,
    /// The stage to commit after `send` succeeded (or after `post` when
    /// there is no `send`).
    pub stage: Stage,
    /// The reaction on the operator's message once the above is done.
    pub react: Reaction,
    /// The `inbound_total` outcome label.
    pub outcome: &'static str,
}

/// Spec J §7.2 for one reply while `open` is the agent's question.
pub fn on_reply(open: &OpenQuestion, reply: &str, key_delay_ms: u64) -> Decision;

pub enum Verdict {
    /// Recorded answers equal the intended ones: react on the echo.
    Confirmed { echo: Option<String> },
    /// They differ: post this, count `answers_mismatched`.
    Mismatch { message: String },
    /// Nobody answered from here: post this when the question was shown.
    AnsweredAtTerminal { message: String },
    /// A `skip` was sent, or nothing to compare: do nothing.
    Nothing,
}

/// Spec J §7.3 for the `PostToolUse` that closes `open`.
pub fn on_closed(open: &OpenQuestion, answers: &Value) -> Verdict;
```

`on_reply` covers every arm of J §7.2: `Sent` refuses with "an answer is
already on its way"; `Confirming` takes `yes`/`y` (send the held plan),
`no`/`n` (back to `Open`, ack), or matches anything else as a fresh
answer; a refusal posts the reason; `skip` echoes "declining" and plans one
Escape; an exact match echoes and plans; an inexact match asks and enters
`Confirming`. A plan the daemon would refuse (`PluginAction::validate`) is
refused here first with J §7.5's message. The echo text, the confirm text
and the refusal texts are the strings matrix posts today, so matrix's
snapshots and actor tests hold.

**The executor contract**, stated in the module docs and honoured by
matrix and GitHub alike: post `post` first; if it did not land, send
nothing, commit `Stage::Open`, react `Failed` and count `send_failed`
(J-5: never send what the operator cannot see, never enter `Confirming` on
a reading nobody was shown). Otherwise send `send` if any; on success
commit `stage` (with the echo id filled in) and react `react`; on failure
post the daemon's error, commit `Stage::Open`, react `Failed`. Matrix's
`on_answer`, `deliver` and `echo_lost` collapse into that executor and its
`question::match_reply` call moves inside `on_reply`.

## 5. Release

A fifth release unit, `common`:

| unit | tag | contents |
|---|---|---|
| common | `balerix-plugin-common-v<ver>` | `balerix-plugin-common` on crates.io |

- `scripts/release/prepare.sh` knows the unit; `affected-units.sh` counts a
  change under `plugins/common/` for `common` and for every plugin whose
  manifest depends on it (matrix, web, github), as a change under
  `crates/balerix-plugin-sdk/` counts for every plugin today.
- `publish-crates.sh` publishes it with `cargo publish --locked` from
  `plugins/common/`, trusted publisher on crates.io, same bootstrap step as
  the SDK (`docs/RELEASING.md` §crates.io bootstrap).
- The manifest names the SDK and API by `{ version = "<exact>", path }`.
  `cargo publish` needs those versions on crates.io, so `prepare.sh`
  refuses to propose `common` while the SDK version it names has no
  `balerix-v<ver>` tag, with a message naming the core release to run
  first. A core release moves those two versions in common's manifest
  (and its lockfile), as it already refreshes every plugin's lockfile.
- In-tree plugins are `publish = false` and depend on common by plain
  `path = "../common"`, so no second ordering rule exists. A plugin's
  paths for release detection gain `plugins/common/**` when its manifest
  names common, so a common change proposes a release of every plugin
  built on it.
- `mise run plugin common` lints and tests it; `plugins` and CI's
  per-plugin jobs include it; `package-plugins` skips it (a library
  packages nothing). `scripts/plugin.sh` and `mise.toml` gain the entry;
  `release-test` gains a scenario for the ordering refusal.
- `docs/RELEASING.md` lists the unit and the ordering rule.

## 6. Changes to matrix and web

- `plugins/matrix/Cargo.toml` adds `balerix-plugin-common`; the moved
  modules become `pub use balerix_plugin_common::{…}` where matrix's tests
  or examples reach them (`examples/question_plan.rs` uses
  `balerix_plugin_common::question`). `matrix.rs`, `client.rs`,
  `routing.rs`, `session.rs`, `actor.rs`, `plugin.rs`, `main.rs` stay.
- The actor's answer path is rewritten as the §4 executor. Every actor
  test in `actor.rs` passes unchanged; the tests that moved with
  `question.rs`, `pending.rs`, `render.rs` and `config.rs` pass in common.
- `plugins/web/Cargo.toml` adds common; `review.rs` becomes the route's
  request type plus `use balerix_plugin_common::review::*`; its snapshots
  move.
- AGENTS.md gotchas that name a moved file are updated to the new path
  (`question.rs::plan`, the actor `Queue`).

## 7. Testing

- Every moved test moves with its module. `plugins/common/src/snapshots/`
  holds the moved insta files; no snapshot content changes.
- `answer.rs`: example tests for every arm of J §7.2 (refusal, exact, inexact,
  `yes`, `no`, other-while-confirming, skip, already-sent, plan too long)
  and of J §7.3 (confirmed, mismatch, at the terminal, skip), asserting the
  posted text, the action, the stage and the reaction.
- `config.rs`: `EventFilter` defaults, lifecycle always wanted, unknown
  event with its index; `validate_key_delay` bounds.
- `phases.rs`: the moved `phase_changes` tests; `run` against
  `FakeHost`'s watch.
- `review.rs`: the moved web tests.
- `mise run plugin matrix` and `mise run plugin web` green with no
  snapshot edits: the regression gate for K-3.
- `cargo package --no-verify` for common in the `lint` task, beside the SDK.
- `cargo deny` in `plugins/common`.

## 8. README: building a plugin on common

The crate README walks a third party through one observer plugin in about
a page: `Env::from_process`, `Host::new`, a `Plugin` impl that pushes into
a `Queue`, an actor that renders with `render::event_message`, splits with
`render::split`, tracks questions with `pending::Questions` and answers
them through `answer::on_reply` plus the executor contract, and registers
`metrics::Shared`. It links to `docs/plugin-protocol.md` for the wire
contract and names the SDK and API versions it pairs with.

## 9. Deliberately deferred

- A generic conversation actor (approach C). Revisit if a third chat
  plugin shows the two executors converging.
- Moving `flow`'s `ConfigError` onto common's. Flow has no other reason to
  depend on common.
- A `matrix`-style `DaemonConfig` base type. Two plugins is not a pattern.

## 10. Done when

1. `plugins/common` builds, `mise run plugin common` passes, `cargo deny`
   passes, `cargo package` succeeds.
2. `mise run plugin matrix` and `mise run plugin web` pass with no snapshot
   changes; `mise run verify-questions` passes against the pinned `claude`.
3. Matrix's actor uses `answer::on_reply` / `on_closed` and the executor
   contract; `on_answer`, `deliver` and `echo_lost` are gone.
4. The `common` release unit is in `prepare.sh`, `affected-units.sh`,
   `publish-crates.sh`, `release-test` and `docs/RELEASING.md`, with the
   ordering refusal tested.
5. The README walkthrough compiles as a doc test or an example.
6. `ARCHITECTURE.md` lists the crate; AGENTS.md paths are current.
