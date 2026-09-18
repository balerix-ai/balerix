# Balerix — Spec J: answering Claude's questions from a Matrix thread

**Date:** 2026-09-18
**Status:** Approved in brainstorm 2026-09-18
**Scope:** the matrix plugin renders an `AskUserQuestion` dialog in the agent's
thread, matches a thread reply to the dialog's options, echoes what it
selected, and answers the dialog with paced keystrokes. One change to shared
crates carries it: a new plugin action, `send_keys`, through `balerix-api`,
`balerix-core`, `balerix-runtime` and `balerix-server`.

---

## 1. Problem

When Claude calls `AskUserQuestion` the agent stops and waits. The matrix
thread shows only `**needs you:** Claude needs your permission`, which is the
`Notification` payload's whole message. The operator cannot see the question
or the options from their phone.

Replying today is worse than useless. A thread reply becomes
`SendText { submit: true }`, which types the body and presses Enter. The dialog
ignores letters, and the Enter selects the highlighted first option. Measured
against claude 2.1.272: the reply `purple please` answered **Red**. In a
two-question dialog, the Enter that follows a digit silently answered the
second question as well.

## 2. What was measured

Every rule in §6 comes from a run against the pinned `claude` (2.1.272) in
tmux, checked against the `answers` map in the `PostToolUse` payload.

| Observation | Result |
|---|---|
| `PreToolUse` for `AskUserQuestion` carries `tool_input.questions[]`: `question`, `header`, `multiSelect`, `options[]` of `label` and `description` | every run |
| `Notification` with `notification_type: "permission_prompt"` follows about six seconds later with no detail | every run |
| `PostToolUse` carries `tool_response.answers`, a map from question text to the recorded answer; multi-select labels are joined with `", "` | every run |
| Escape declines; Claude sees "User declined to answer questions"; no `PostToolUse` fires | every run |
| The highlight starts on row 1 for every question, and on `Submit answers` on the review screen | every dialog opened |
| Keys with no pause between them: a key is dropped at a question transition and the dialog wedges | 0 of 1, and 1 earlier failure |
| Down and Enter, 50 ms apart, two single-select questions, four sessions running at once | 6 of 6 |
| The same at 200 ms | 6 of 6 |
| Multi-select, toggling with Enter, leaving through the in-list `Submit` row | 4 of 4 |
| Multi-select plus free text, same exit | 4 of 4 |
| Multi-select question followed by a single-select, leaving through the in-list `Next` row | 4 of 4 |
| Free text on the first of two questions | 3 of 3 |
| Free text on a single question: Down to `Type something`, type, Enter | 4 of 4 |
| Down and Enter sent to an idle prompt that has history | no prompt submitted, 10 keys |

Tab is avoided on purpose. From an option row it jumps to the review tab; from
the free-text row it moves to the in-list `Submit` row. Counting rows with Down
has one behaviour everywhere.

A `PermissionRequest` hook can also answer the dialog with no keystrokes, and
did so exactly in one run. It needs a hook held open for minutes, which the
interceptor chain's 1.5 s fail-open budget forbids. §11 defers it.

## 3. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| J-1 | Answers are delivered as **keystrokes**, counted from the known starting row, paced. | The operator's decision. Measured reliable when paced (§2). No hook is held, so the chain's fail-open contract is untouched. |
| J-2 | A new plugin action, **`send_keys`**, carries an ordered list of steps and a delay. Keys are an allowlist: `up`, `down`, `enter`, `escape`. | `send_text` cannot press an arrow key. An allowlist keeps a room reply from reaching control sequences the design never needed. |
| J-3 | The runtime stays ignorant of Claude's dialog. The matrix plugin owns the key plan. | `send_keys` is generic and testable against `cat`. The dialog's layout changes with `claude` versions; that coupling belongs in one plugin module, not in a port. |
| J-4 | Reply matching is **deterministic**: number, exact label, unique prefix, unique word. Ambiguity is refused with the candidates listed. | A wrong guess answers a question on the operator's behalf. No fuzzy distance, no model. |
| J-5 | The plugin **always echoes** its selection in the thread. It asks for a `yes` first only when the match was inexact or the answer is free text. | The operator's decisions: always show what was selected; do not cost an exact answer a second round trip. |
| J-6 | After submission the plugin compares `PostToolUse`'s `answers` with what it intended. Equal: a ✅ reaction on the echo. Different: a loud message. | Counting cannot see the review screen. This is the ground truth, and it is free. |
| J-7 | While a question is open, a reply is **never** passed to `send_text`. | That path is the silent wrong answer in §1. |
| J-8 | The open question is **persisted in the plugin's KV**, beside the thread record. `Confirming` and `Sent` are not. | Without it, a plugin restart mid-question reopens §1's hazard: the reply would go to `send_text`. Refusing replies until the agent's next event was considered and rejected, because an idle agent emits none and every ordinary nudge after a restart would be refused. |
| J-9 | Pane verification (reading the highlighted row before Enter) is deferred. | It passed 6 of 6, but it needs pane capture and a parser for Claude's screen in the runtime. `send_keys` runs in the runner, so it can be added later without a protocol change. |

J-1 and J-2 stay inside Spec G's G-1: the room remains a notify-and-nudge
surface. Nothing is intercepted and no permission is granted from the room.

## 4. The `send_keys` action

### 4.1 Wire shape (`balerix-api`, `docs/plugin-protocol.md` §3)

```json
{ "action": "send_keys",
  "steps": [ { "key": "down" }, { "key": "down" }, { "text": "teal-ish" }, { "key": "enter" } ],
  "delay_ms": 100 }
```

```rust
pub enum PluginAction {
    SendText { text: String, submit: bool },
    SendKeys { steps: Vec<KeyStep>, delay_ms: u64 },
    Restart,
    Stop,
}

pub enum KeyStep { Key(Key), Text(String) }   // {"key": …} or {"text": …}
pub enum Key { Up, Down, Enter, Escape }      // snake_case on the wire
```

`delay_ms` defaults to `DEFAULT_KEY_DELAY_MS = 100` when absent. The
hand-written `Deserialize` gains a `send_keys` arm that rejects unknown fields
like the others, and a step object with both or neither of `key` and `text`.

Validation is a method on the action, `PluginAction::validate`, called by the
daemon before execution and answering 400 with a message that names the field:

- `steps` holds 1 to `MAX_KEY_STEPS = 64` entries.
- `delay_ms` is within `20..=500`. The floor is the point of the action: the
  unpaced failure in §2 must not be reachable by configuration.
- A `text` step is 1 to 1024 bytes and contains no control character. A newline
  would be an Enter the plan did not count.

- `steps.len() × delay_ms` is at most `MAX_KEY_SEQUENCE_MS = 8000`. The SDK's
  `Host::action` gives up after 10 s, and a plugin that timed out while the
  keys kept arriving could not tell what state the dialog was in. At the
  default delay that is all 64 steps; at 500 ms it is 16.

The runner is busy for the length of the sequence, on a blocking thread, as
`send_text` already is.

### 4.2 Port and adapter

```rust
// balerix-core, AgentRunner
fn send_keys(&self, agent: &AgentId, steps: &[KeyStep], delay: Duration) -> Result<(), RunnerError>;
```

`TmuxRunner` sends each step as its own tmux command and sleeps `delay` after
each: `send-keys -t <target> Down` for a key, `send-keys -t <target> -l -- <text>`
for text. The key names are a fixed match on `Key`, never a string from the wire.

tmux reads a `;` that ends an argument as a command separator and drops it
(measured: `a;` arrives as `a`). A text step therefore sends its trailing
semicolons separately, each as the escaped argument `\;`, which arrives as
`;`. `send_text`'s short path has the same flaw today; fixing it is outside
this spec and is reported separately.

**Per-agent send lock.** A paced sequence lasts up to seconds. A `send_text`
from the flow plugin landing in the middle of it would corrupt both.
`TmuxRunner` takes a per-agent mutex for the whole of `send_keys` and
`send_text`. The lock map is keyed by `AgentId` and entries are never removed;
agents are few.

`FakeRunner` in `balerix-core` records `send_keys f/c/a [down,down,"teal-ish",enter] delay=100ms`
and is failable like `send_text`.

### 4.3 Daemon

`Daemon::execute_action` gains the arm, mirroring `SendText`:
`spawn_blocking` around `runner.send_keys`. Metrics need nothing new; `label()`
returns `send_keys`. The `actions` need in the manifest already gates the route.

## 5. Rendering the question (`plugins/matrix/src/render.rs`)

A new pure function renders `tool_input` as markdown:

```
**question** · Color

Which color?

1. **Red** — A warm, bold color
2. **Green** — A calm, natural color
3. **Blue** — A cool, serene color

Reply with a number or a label. `other: …` gives your own answer, `skip` declines.
```

Several questions are titled `question 1 of 2`, `question 2 of 2` with their
headers, and the footer says one line per question, in order. A multi-select question says "choose any, separated
by commas". A payload that does not parse as questions renders as today's
`running AskUserQuestion` line and opens no pending state.

**When it posts.** `PreToolUse` is not in the default event set (G-3). An
`AskUserQuestion` `PreToolUse` is posted when the agent's set wants
`Notification` **or** `PreToolUse`: it is the detailed form of "needs you".

**Tracking is unconditional.** The open question is recorded whether or not
it is posted: J-7's protection must not depend on the event filter.

**Suppression.** While a question is open for an agent, a `Notification` whose
`notification_type` is `permission_prompt` is not posted. It says nothing the
question has not.

## 6. Matching and the key plan (`plugins/matrix/src/question.rs`, new, pure)

No I/O in this module. Three functions carry the feature.

### 6.1 `parse(tool_input) -> Option<Vec<Question>>`

`Question { text, header, multi_select, options: Vec<Opt { label, description }> }`.
`None` for anything that is not at least one question, each with at least one option.

### 6.2 `match_reply(&[Question], reply) -> Result<Matched, Refusal>`

**Splitting.** One question: the whole reply is its answer. Several: one
non-empty line per question, in order; or lines of `header: answer`, in any
order, where the header matches case-insensitively. A wrong line count, or an
unknown or repeated header, is a refusal that says what was expected.

**Within a multi-select answer**, items are separated by commas.

**Per item, the ladder**, first rung that produces a result wins:

1. A number `1..=n`: that option. *Exact.*
2. `other: <text>`: free text. *Inexact*, so it is always confirmed.
3. The label, compared after normalising: case-folded, trimmed, punctuation and
   repeated whitespace removed. *Exact.*
4. The normalised item is a prefix of exactly one normalised label. *Inexact.*
5. Every word of the item appears in exactly one label. *Inexact.*

Rungs 4 and 5 matching two or more options refuse with the candidates. No rung
matching refuses with the option list. A single-select item matching is one
option; a multi-select answer with a repeated option refuses.

The result is `Matched::Answers { selections: Vec<Selection>, exact: bool }`,
where `Selection { options: Vec<usize>, other: Option<String> }` holds the
chosen options, ascending, and the free text if any. `exact` is true only when
every item came from rung 1 or 3.

`skip` alone, in any case, is `Matched::Skip`: one Escape, exact.

### 6.3 `plan(&[Question], &[Selection]) -> Vec<KeyStep>`

Rows of a question: options `1..=n`, then `Type something` at `n+1`, then, on a
multi-select question only, `Submit` or `Next` at `n+2`. The cursor starts at
row 1. `down(k)` is `k` Down steps.

- **Single-select, option `i`:** `down(i-1)`, Enter.
- **Single-select, free text:** `down(n)`, Text, Enter.
- **Multi-select:** visit the chosen options in ascending order; for each,
  Down to it and Enter to toggle. If there is free text, Down to row `n+1` and
  Text. Then Down to row `n+2` and Enter.
- **After the last question:** a dialog with one single-select question is
  already submitted. Every other dialog is now on the review screen with
  `Submit answers` highlighted: one more Enter.

The plan never sends Up or Tab. `Key::Up` stays in the allowlist for J-9.

## 7. The actor (`plugins/matrix/src/actor.rs`)

### 7.1 State, per agent

`Open` is written to KV when entered and removed when left for `None` (J-8);
the other states live in memory.

```
None
 └─ PreToolUse(AskUserQuestion), parsed ──▶ Open { questions }
Open ── reply, exact match ──────────────▶ Sent { intended, echo_event_id }
Open ── reply, inexact match ────────────▶ Confirming { matched, echo_event_id }
Confirming ── "yes" / "y" ───────────────▶ Sent
Confirming ── "no" / "n" ────────────────▶ Open
Confirming ── any other reply ───────────▶ matched again as a fresh answer
any ── PostToolUse(AskUserQuestion) | Stop | UserPromptSubmit | SessionStart | SessionEnd ──▶ None
```

### 7.2 A reply while a question is open

The inbound filter of Spec G §9.1 runs first, unchanged. Then, instead of §9.2's
`send_text`:

- **Refusal:** the refused reaction, and a thread message with the reason and
  the option list or candidates. Nothing is sent to the agent.
- **Exact match:** post the echo, `**answering** Color → Blue · Size → Medium`,
  then `send_keys`. On success, the ACK reaction on the operator's message and
  state `Sent`. On failure, Spec G's existing failure message and reaction, and
  state returns to `Open`.
- **Inexact match:** post the echo as a question,
  `**I read that as** Color → Blue · Size → Medium. Reply **yes** to send.`
  State `Confirming`. `yes` then follows the exact path without a second echo.
- **`skip`:** echo `**declining the question**`, then one Escape.
- **In `Sent`:** refused with "an answer is already on its way".

### 7.3 Ground truth

On `PostToolUse` for `AskUserQuestion`, read `tool_response.answers`.

- State was `Sent` and the recorded answers equal the intended ones: a ✅
  reaction on the echo message. Multi-select compares as a set of labels.
- State was `Sent` and they differ: `**recorded answer differs** — Claude
  recorded Size → Small, you chose Medium. Tell the agent if that matters.`
- State was `Open` or `Confirming`: someone answered at the terminal. Post
  `**answered at the terminal** Color → Blue`.

### 7.4 After a plugin restart

A persisted open question resumes as `Open`. `Confirming` and `Sent` resume as
`Open` too: the operator answers again, which is the safe direction.

The persisted state can be stale if the clearing event was lost while the
plugin was down. A reply is then matched against a question that is gone, and
its plan lands on an idle prompt. That is bounded: a plan holds only Down,
Enter and typed text; Down and Enter at an idle prompt submit nothing (§2);
and a free-text plan types the text and submits it as a prompt, which is what
`send_text` would have done with that reply. No `PostToolUse` follows, so the
echo never gains its ✅, and the agent's next clearing event repairs the state.

### 7.5 Config

One per-agent key, next to `events`: `keyDelayMs` (camelCase, like the daemon
block), default 100, validated to `20..=500` at `activate` with the config
path first in the error.

A plan whose length times the delay exceeds `MAX_KEY_SEQUENCE_MS`, or that has
more than `MAX_KEY_STEPS` steps, is refused in the thread before anything is
sent.

## 8. Failure and metrics

- `send_keys` failing (window gone, tmux error) is reported in the thread like a
  failed `send_text` today; `errors{kind="send_keys"}`.
- `inbound_total` gains outcomes: `answered`, `confirm_asked`, `confirmed`,
  `answer_refused`, `skipped`.
- A new counter, `answers_mismatched_total`. It should stay at zero; a non-zero
  value after a `claude` bump is the signal that the dialog's layout changed.

## 9. Security

`docs/THREAT-MODEL.md` changes in two places.

- The sentence "a plugin with `actions` can stop, restart or type into any
  agent it is active for" gains: and press Up, Down, Enter and Escape. No other
  key is reachable: `Key` is a closed enum matched to fixed tmux key names, and
  a `text` step rejects control characters, so a step cannot smuggle an escape
  sequence.
- The G-5 accepted risk gains a clause: a room member can now answer an agent's
  question as well as prompt it. The boundary is unchanged, and an answer is
  strictly narrower than a prompt, which room members already have.

Reply bodies are matched against option labels that came from the agent.
Labels are data: they are normalised and compared, never interpolated into a
key name or a command. The echo interpolates labels into a Matrix message body,
which goes through the existing 4000-character cut.

## 10. Testing

- **Conformance:** a new fixture, `docs/plugin-protocol/action-send-keys.json`,
  replayed by the SDK's conformance test.
- **`balerix-api`:** serde round trips for `send_keys`; rejects unknown fields,
  an unknown key name, a step with both `key` and `text`; `validate` boundaries
  for step count, delay and text.
- **`balerix-core`:** `FakeRunner` records and fails `send_keys`.
- **`balerix-runtime` (`tmux_it`):** against a pane running `cat -v` into a log,
  as the paste tests do: the steps arrive in order, Down arrives as an arrow
  escape and text arrives literally, and the elapsed time is at least
  `(steps - 1) × delay`. A second test runs `send_keys` and `send_text` to the
  same agent from two threads and asserts the log holds one whole then the
  other, never interleaved.
- **`balerix-server`:** `execute_action` reaches the runner; an invalid action
  answers 400 and never reaches it.
- **`question.rs`, example tests:** every row of §2's table as a plan; every
  rung of the ladder; every refusal.
- **`question.rs`, model-based property test.** A small model of the dialog
  encodes §6.3's rows and transitions. For generated questions and selections,
  feeding `plan`'s steps to the model yields exactly the selections, and ends
  submitted. This is what guards the counting. It needs `proptest` in the matrix
  plugin's dev-dependencies, at the workspace's exact version; a new plugin
  dependency, stated here as AGENTS.md asks.
- **`render.rs`:** insta snapshots for one question, several, multi-select, and
  an unparsable payload.
- **Actor, with `FakeHost`:** the question posts when only `Notification` is
  wanted; `permission_prompt` is suppressed while open; an exact reply echoes
  then sends keys and no `send_text`; an inexact reply asks, and `yes` sends;
  prose is refused and sends nothing; `PostToolUse` equal, different, and
  terminal-answered; every clearing event; an open question surviving a
  restart of the actor over the same `FakeHost` KV.
- **`mise run verify-questions`, by hand, not in CI.** A script beside
  `verify-claude.sh` that drives the real pinned `claude` through §2's dialog
  shapes using the key plans `question.rs` produces, and checks
  `PostToolUse`'s `answers`. The plans come from
  `plugins/matrix/examples/question_plan.rs`, which prints `match_reply` plus
  `plan` as JSON for a `tool_input` and a reply; the `balerix` binary cannot
  depend on plugin code (AGENTS.md), so it is an example of the plugin's own
  project. The model in the property test is only as true as this run. AGENTS.md gains a gotcha: bumping `claude` in `mise.toml` means
  running it.

## 11. Deliberately deferred

- **Pane verification before Enter (J-9).**
- **Answering through a held `PermissionRequest` hook.** Exact and keystroke
  free, but it needs a long-held hook path beside the 1.5 s chain and
  `PermissionRequest` in `HOOK_EVENTS`.
- **Ordinary tool permission prompts.** Their options appear in no hook payload.
- **`send_keys` in the flow plugin's rules.** Nothing asks for it.

## 12. Done when

- `mise run check`, `mise run test-it` and `mise run plugin matrix` pass.
- `mise run verify-questions` passes against the pinned `claude` for every
  dialog shape in §2.
- Against a real homeserver: a question appears in the thread with its
  options; `2` answers it and the echo gains ✅; `gre` asks for a `yes`;
  `purple please` is refused and the agent's dialog is untouched; a
  two-question dialog is answered from one reply.
- `docs/plugin-protocol.md` §3, `docs/THREAT-MODEL.md` and `ARCHITECTURE.md`
  describe `send_keys` and the question flow. Changelogs are written by the
  release PR from the pull request's Conventional Commit title
  (`docs/RELEASING.md`), so none is edited by hand.
