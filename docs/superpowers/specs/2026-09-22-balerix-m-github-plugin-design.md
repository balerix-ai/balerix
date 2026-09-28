# Balerix — Spec M: the GitHub plugin

**Date:** 2026-09-22
**Status:** Approved in brainstorm 2026-09-22; amended 2026-09-27 for
#64 (the restricted settings surface, §12.1 and §12.2) and #85 (delivery
confirmation, §8.7), approved in brainstorm 2026-09-27
**Scope:** a new plugin crate, `balerix-plugin-github`, that turns a
mention of a GitHub App on an issue or pull request into a running agent
whose session is that issue: Claude's turns and questions post as
comments, comments from collaborators become prompts or answers, a
submitted review on the PR reaches the agent as one message, and the fleet
the agent runs in is built from the repository's own `.balerix.yaml`.

Depends on Spec K (`balerix-plugin-common`) and Spec L (the `manage`
capability, owned fleets, the per-agent `branch`). Lands with two
daemon-side changes (§12.1, §12.2), one common module (§8.7) and its
adoption by matrix.

---

## 1. Problem

The matrix plugin gives an operator a phone-side view of agents they
started by hand. A team working in GitHub wants the opposite direction:
the conversation exists first, as an issue or a PR, and an agent should
join it on request, work in that repository under settings the repository
declares, report back where the conversation already is, and leave when
the issue closes. Nobody on the team should have to run `balerix up`, and
two people asking on two repositories should get two reproducible setups
from two files in version control.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| M-1 | **One fleet per repository, one crew, one agent per issue or PR.** Fleet `gh-<owner>-<repo>`, crew `repo`, agents `issue-<n>` / `pr-<n>`. | A crew is a repository; an agent is a session; the issue is the session. The plugin's fleet file is the repo's file plus the agents it adds, so `balerix up -f .balerix.yaml` on a laptop and the plugin produce the same agents. |
| M-2 | **Access is write permission on the repository**, checked per inbound. | A comment is a prompt to an agent holding the operator's credentials and push rights. On a public repo anyone can comment; `admin`/`maintain`/`write` is the boundary GitHub already maintains. Others are ignored silently. |
| M-3 | **Webhooks on the plugin's own listener**, HMAC-verified. | The daemon's mount is admin- or cookie-authenticated, so GitHub cannot deliver there. Webhooks are instant and the standard App shape; the operator fronts the port with a proxy or tunnel. |
| M-4 | **Agents push as the operator** (Spec L-3); only comments come from the App. | One long-lived token, no refresh machinery. Attribution of pushes to the bot is deferred. |
| M-5 | **The daemon resolves**; the plugin posts the unresolved file with its agents added (Spec L-2). | The plugin never sees host defaults or credentials, and one resolver means no drift. |
| M-6 | **`.balerix.yaml` is an ordinary fleet file** with one crew naming this repository, read from the **default branch** only. | No new format, and the file works with the CLI unchanged. A PR head must not be able to change the sandbox or tools of the agent reviewing it. |
| M-7 | **Turns as comments; status in one edited comment.** `Stop` and questions are comments; session start and end, notifications, phase changes and idle stops edit a single status comment. | Every comment emails watchers. The timeline should hold what a person reads; the status comment shows a stuck or dead agent without the noise. |
| M-8 | A mention starts a session; afterwards **every permitted comment on that issue is a message**. | The issue is the conversation; requiring the mention on every line is typing for nothing. |
| M-9 | A mention on a PR starts an agent on the **PR's head branch** (`branch`, Spec L-7); a **submitted review** renders as one message. | Pushes must reach the PR. A review is one act with many parts; the agent should read it whole, as the web plugin's review already arrives. |
| M-10 | A session ends on **close or merge**, and on an **idle timeout**; ending **removes the agent** from the fleet. | Removing frees the sandbox and tmux window; the branch survives in the crew cache (harvested from the clone at removal, Spec N §5) and seeds the clone on the next mention, which starts a fresh Claude session told where the earlier work is. Claude's own session does not survive removal (the home is deleted); that is the accepted cost of not holding idle sandboxes open. |
| M-11 | The actor is generic over a **`GitHubPort`** with a recording fake; the listener enqueues and answers 202. | Spec G-13 and G-11: the ordering rules are unit-tested without GitHub, and a slow API never stalls hook delivery. |
| M-12 | A thin `reqwest` client, not `octocrab`. | About ten endpoints; a small tree in a standalone project that already carries TLS. |
| M-13 | **A plugin-applied fleet file cannot choose what runs, its environment or its sandbox**: the daemon's resolver refuses `claude.binary`, `claude.args`, `env`, `sandbox` and, inside `claude.settings`, `env` and `apiKeyHelper`, at every layer (#64). The operator sets them per plugin in `plugins.yaml` (`fleetDefaults`). | The file this plugin applies is the repository's, written by anyone with write on the default branch. M-2 already lets such a person prompt an agent that holds the operator's credentials; letting the file pick the binary or widen the sandbox would let them run anything with those credentials, with neither Claude nor the sandbox in the loop. Enforced in the daemon, not the plugin, so the bound holds against the plugin too. |
| M-14 | **`PUT fleets/{name}` answers the record only to a plugin with `fleets`**, 204 otherwise (#64). | The record carries the resolved spec with the host's `settings.json` folded in; reading records is what `fleets` means. |
| M-15 | **A reaction says what the plugin knows**: "sent" on `send_text`'s `Ok`, confirmed on the agent's matching `UserPromptSubmit`, a note when neither comes in time; shared with matrix through common (#85). | `Ok` means the daemon typed into the pane. A reader takes ✓ as "the agent is on it", and an open dialog or a mid-turn queue makes that false. The hook the plugin already observes is the proof. |

## 3. Crate and modules

```
plugins/github/
  Cargo.toml       standalone; common, sdk, api by { version, path }
  src/
    main.rs        env, tracing, build, serve; failures print `github: …`, exit 1
    lib.rs
    config.rs      DaemonConfig (§4.1) and AgentConfig (§4.2)
    github.rs      the GitHubPort trait, `fake::FakePort`
    client.rs      the reqwest implementation: App JWT, installation tokens, endpoints
    webhook.rs     the axum listener: signature, dedupe, parse into WebhookEvent
    mention.rs     pure: mention detection, name sanitising
    repo_config.rs pure: `.balerix.yaml` validation and agent injection (§6)
    prompt.rs      pure: the first prompt and the review message (§8.2, §9)
    status.rs      pure: the status comment body (§8.3)
    session.rs     Session rows, KV mirror, reverse map (§5)
    actor.rs       the owned task: state plus the command loop
    plugin.rs      the SDK Plugin impl: validate and enqueue
    snapshots/
  package/
    balerix-plugin.yaml
    mise.toml
  tests/
    plugin_it.rs
```

Dependencies, each a deliberate commit: `reqwest` (`rustls-tls`, `json`),
`jsonwebtoken` (RS256 App JWT), `hmac` and `sha2` (webhook signature),
`serde_norway` (parse `.balerix.yaml`; the core's YAML crate),
`axum` (the listener; already in the SDK's tree), `serde`, `serde_json`,
`tokio`, `tracing`, `thiserror`, `anyhow` (main only). The JWT's `iat`
and `exp` come from `std::time::SystemTime`. Dev: `insta`.

Manifest:

```yaml
apiVersion: balerix/v1
kind: Plugin
name: github
version: 0.1.0
protocol: 1
start: serve
hooks:
  observe: [SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, SubagentStop, PreCompact]
  intercept: []
# fleets: readiness and phases. actions: send_text and send_keys.
# kv: session rows. manage: the fleet per repository (Spec L).
needs: [fleets, actions, kv, manage]
routes: false
```

No sandbox block: nono's default leaves outbound and listening open, and
the base profile grants `/etc` for root certificates. The listener binds
what `listen` says.

## 4. Configuration

### 4.1 Daemon level

```yaml
plugins:
  - name: github
    source: ./target/plugins/github
    secrets:
      privateKey: ../secrets/github-app.pem
      webhookSecret: ../secrets/github-webhook
    config:
      appId: 12345
      listen: 127.0.0.1:8787       # default; the operator's proxy or tunnel fronts it
      configPath: .balerix.yaml    # default; path in the repo, default branch
      idleTimeout: 2h              # default; 0 disables
      maxParts: 10                 # default; most comments one turn may become
```

`appId`, `privateKey` and `webhookSecret` are required; the two secrets
are `Secret` newtypes and may also be given as literals under `config`
(one source each, as matrix's password). `listen` is a socket address;
`idleTimeout` is `90s`/`5m`/`2h`/`0`; unknown keys are rejected with
their path. The plugin's process environment is nono's, so there is no
environment variable for the path; `configPath` is the knob.

### 4.2 Per agent

The agent's `plugins.github` block, merged fleet to crew to agent. The
plugin injects `kind` and `number` when it adds an agent (§6); the rest is
what the repository's `defaults` may set:

```yaml
defaults:
  plugins:
    github:
      events: [Notification, Stop]   # what reaches the status comment
      phases: true
      keyDelayMs: 100
      confirmWindow: 30s
```

`enabled` (default `true`), `events` (common's `EventFilter`; the default
set; `SessionStart`/`SessionEnd` always), `phases`, `keyDelayMs` as in
Spec J. `confirmWindow` (default `30s`, the `idleTimeout` grammar) is how
long a prompt may go unconfirmed before the note of §8.7; `0` disables
the note, not the confirmation. `kind` (`issue`|`pr`) and `number` are accepted and validated;
an agent activated without them is rejected (`plugins.github.number:
missing`), which is how a hand-written agent in `.balerix.yaml` is caught.

## 5. State

KV, one row per agent, `session/<fleet>/<agent>`:

```rust
struct Session {
    repo: String,               // owner/name, as GitHub spells it
    installation: u64,
    kind: Kind,                 // Issue | Pr
    number: u64,
    head: Option<String>,       // the PR's head ref
    status_comment: Option<u64>,
    session_id: Option<String>, // the live Claude session, once seen
    last_activity: u64,         // unix seconds
    closed: bool,
}
```

`Sessions` mirrors the map to KV like matrix's `Maps` (store first, then
memory) and derives `(repo, number) → agent` at load. The activation
config carries `kind` and `number` too, so at start a row missing from KV
is rebuilt from the daemon's `activate` calls with `status_comment` unset;
the first status edit then posts a new comment.

`repo/<fleet>` = `owner/name` records which repository a sanitised fleet
name stands for (§6).

In memory only: installation tokens with their expiry; the collaborator
permission cache (login, repo) → (permission, until), five minutes; the
delivery-id ring (last 4 096 ids); per-agent pending status edit; the
prompts awaiting confirmation (§8.7).

## 6. The repository's fleet file

On every session start the plugin reads `configPath` from the default
branch (`GET /repos/{o}/{r}/contents/{path}?ref=<default>`), decodes it,
parses it with `serde_norway` into a JSON value and checks, in order,
each failure posted on the issue with a `confused` reaction and nothing
applied:

1. `apiVersion: balerix/v1`, `kind: Fleet`.
2. Exactly one entry under `crews`, whose `repo` is this repository
   (`owner/name`, case-insensitive; a clone URL of it is accepted too).
   Its key is the crew name; `repo` is the conventional key.
3. No agent in the file whose name matches `issue-<n>` or `pr-<n>`: those
   names are the plugin's.

Then it sets `name` to the fleet name, defaults the crew's `ref` to the
repository's default branch when absent, and adds one entry per live
session under that crew's `agents`: `{ plugins: { github: { kind, number
} } }` plus `branch: <head>` for a PR. The result is the body of `PUT
plugin-host/fleets/<name>`; a 400 from the daemon is the resolver's
message, posted verbatim.

**Fleet name.** `gh-` + the sanitised `owner/name`: lower-cased, every
run of characters outside `[a-z0-9]` replaced by one `-`, trimmed of
`-`, cut to 63. `repo/<fleet>` in KV must be absent or equal to this
repository's `owner/name`; otherwise the mention is refused with
`fleet name gh-… already stands for <other>`. Renamed repositories keep
working through the same check until the operator clears the key.

**Change semantics.** The file is read at every start, so an edit on the
default branch takes effect at the next session start. That apply
re-resolves every agent in the fleet, and the reconciler restarts any
running agent whose resolved settings changed; the status comment of such
an agent gains `restarted: settings changed`. This is the reproducibility
the file exists for.

## 7. Ingress

`webhook.rs` serves `POST /webhook` on `listen`:

1. Read the raw body, 1 MiB cap (413 beyond).
2. Compute HMAC-SHA256 with `webhookSecret`, compare in constant time with
   `X-Hub-Signature-256`; 401 on a miss, counted `bad_signature`.
3. `X-GitHub-Event: ping` → 200. A `X-GitHub-Delivery` already in the ring
   → 202, counted `duplicate`. Otherwise parse by event name into
   `WebhookEvent` (below), push `Command::Webhook`, answer 202. Everything
   else is 202 and counted `ignored` with the event name.

```rust
enum WebhookEvent {
    IssueOpened   { repo, installation, number, author, body, title, url, is_pr: false },
    PrOpened      { repo, installation, number, author, body, title, url, head, head_repo, base },
    Comment       { repo, installation, number, author, comment_id, body, is_pr },
    Closed        { repo, installation, number, merged },
    ReviewSubmitted { repo, installation, number, author, review_id, state, body, commit },
}
```

`repo` is `repository.full_name`, `installation` is `installation.id`,
both from the verified payload. `author` carries `login` and `type`
(`User`/`Bot`). `issues.reopened` and `pull_request.reopened` are
ignored. `pull_request_review_comment` is not read: every inline comment
belongs to a review, and the review's `submitted` event follows.

A delivery missed while the plugin was down is not recovered here (§13).

## 8. The actor

```rust
enum Command {
    Configure(DaemonConfig),
    Activate { agent, config: AgentConfig },
    Deactivate { agent },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Webhook(WebhookEvent),
    Tick,                       // every five seconds (§8.7, §10)
}
```

Common's `Queue` (drop-oldest, counted); commands buffered until
`Configure`; `plugin.rs` validates and enqueues. State: the config, the
agents' `AgentConfig`, `Sessions`, common's `Questions`, the caches of
§5, and the `GitHubPort`.

### 8.1 Who is heard

`permitted(repo, author)` is false for `type: Bot`, for the App's own
login, and for anyone whose permission on the repo
(`GET /repos/{o}/{r}/collaborators/{login}/permission`) is not
`admin`, `maintain` or `write`; cached five minutes. Nothing is posted or
reacted for an unpermitted author.

### 8.2 Starting

A `Comment` or `IssueOpened`/`PrOpened` whose body mentions the App
(`@<slug>`, case-insensitive, at a word boundary, outside fenced code)
from a permitted author, on a number with no live session:

1. React `eyes` on the comment (or the issue body).
2. For a PR whose `head_repo` is not this repository: post `sessions on
   pull requests from forks are not supported`, react `confused`, stop.
3. Read and check the fleet file (§6); apply with the new agent added.
   On failure: post the message once, react `confused`, store nothing.
4. Post the status comment (§8.3) with `starting`; store the row with
   `last_activity = now`.

The first prompt is sent as `send_text` with submit on the agent's
`SessionStart` (the agent is ready then, not before). It is tracked by
§8.7 with the mentioning comment (or the issue, for an `IssueOpened`
start) as its marker, so the `eyes` of step 1 gains `+1` once Claude
takes it. Rendered by `prompt.rs`:

```
You are attached to <owner>/<repo> issue #12: <title>
<url>

Branch: balerix/gh-acme-payments/repo/issue-12 (from main)

<issue body>

---
@alice asked:
<the mentioning comment>
```

For a PR: `pull request #34`, `Branch: <head> (into <base>)`, and the PR
body. After an idle stop the prompt adds `Earlier work on this issue is
on branch <branch>; continue from it.` when the row existed before.

A mention while a session is live is an ordinary message (§8.4).

### 8.3 The status comment

One comment per session, posted at start and edited in place:

```
**balerix** · `gh-acme-payments/repo/issue-12` · session `0199aa11` · **ready**

- 14:02 session started (startup)
- 14:05 needs you: Claude needs your permission to use Bash
- 14:31 turn finished
- 16:31 stopped after 2h idle — mention @balerix to resume
```

The header names the agent, the short session id and the phase from the
fleets watch. The list holds the last twenty lines; a line is added for
`SessionStart`, `SessionEnd`, every event the agent's `events` wants
(rendered with common's `event_message`, first line only), phase changes
when `phases` is on, restarts, the idle stop and the close. Edits are
coalesced: a pending edit is flushed at most once per two seconds per
agent. A failed edit is counted and retried on the next change; a missing
comment id (or a 404, the comment was deleted) posts a new one.

### 8.4 Inbound while live

A permitted `Comment` on a number with a live, unclosed session:

- Question open (common's `Questions`): common's `answer::on_reply`,
  executed per Spec K §4 — post the echo as a comment, `send_keys`,
  commit the stage, react. Reactions: `Ack` → `+1`, `Refused` →
  `confused`, `Failed` → `-1`, `Confirmed` → `hooray` on the echo.
- Otherwise `send_text { text: body, submit: true }`. On `Ok`: `eyes`,
  and the comment is tracked by §8.7, which adds `+1` when Claude takes
  it. On failure: a comment `not delivered to <agent>: <error>` and `-1`.

A permitted comment on a closed session: react `confused` and post once
per session `this session has ended; mention @<slug> to start a new one`.
An unknown number, or a non-permitted author: nothing.

`last_activity` is updated on every inbound and every hook event; the row
is written back on the next `Tick` rather than on every event.

### 8.5 Outbound

- `Stop`: the payload's `last_assistant_message`, split with common's
  `split(text, 65_536, maxParts)`, each part its own comment, in order; a
  part that fails to post stops the rest (matrix's rule). An empty
  message posts nothing and adds a status line.
- Questions: Spec J §5 through common. The `PreToolUse` for
  `AskUserQuestion` posts `question_message` as a comment whose footer
  reads `Reply with a comment: a number or a label…`; tracking is
  unconditional; the first `permission_prompt` after a posted question is
  suppressed from the status comment. `PostToolUse` runs
  `answer::on_closed`: `Confirmed` reacts `hooray` on the echo,
  `Mismatch` and `AnsweredAtTerminal` post their message.
- Everything else goes to the status comment (§8.3). `SessionEnd` adds
  its line and nothing more: the row's `closed` is set only by §10, and an
  agent still declared in the fleet may emit again after a `SessionEnd`
  (an operator resuming it at the terminal), which the status comment
  then shows.

### 8.6 Phases

`phases::run` from common feeds `Phases`; only agents with a row are
considered. A phase change updates the status header and, when `phases`
is on, adds a line. `Dead` and `Failed` lines carry the daemon's message.

### 8.7 Delivery confirmation

As implemented (§17), `sent` takes each prompt's window and `expire`
takes only `now`; `on_event` records the turn for agents it has not seen
a send for. The note names its reason, and only expiry claims a
duration; a prompt skipped by a later one or evicted past `MAX_PENDING`
gets its own wording regardless of `confirmWindow`. See §17 for the
exact strings.

`send_text`'s `Ok` says the daemon typed the body into the pane and
pressed Enter. It does not say Claude took a prompt: a dialog may be
open, or the agent may be mid-turn and queue the text. The plugin
observes `UserPromptSubmit`, whose payload's `prompt` is the text Claude
took, and that is the proof (#85). Common gets `delivery.rs`, pure like
`answer.rs`, used by this plugin and by matrix:

- `Deliveries<M>` holds, per agent, a FIFO of prompts sent and not yet
  confirmed, each with the plugin's marker `M` (here a comment or issue
  id) and its send time, and one bit: whether the agent is mid-turn.
- `classify(text)`: a body whose first non-blank character is `/` is a
  slash command and is never tracked; most commands fire no hook, so
  "sent" is all the plugin can honestly say.
- `sent(agent, text, marker, now)` registers a prompt.
- `on_event(&HookEvent) -> Outcome`: a `UserPromptSubmit` whose `prompt`
  matches the oldest pending prompt with that text on that agent removes
  it and answers its marker as confirmed; any older pending prompts it
  passed over are answered as unconfirmed at once, since Claude submits
  queued text in order and a skipped one was swallowed. Matching is
  equality after trimming both ends and collapsing each run of
  whitespace to one space. A submit that matches nothing (the operator
  typed at the terminal) is ignored. `UserPromptSubmit` sets the agent
  mid-turn; `Stop` and `SessionEnd` clear it.
- `expire(now, window) -> Vec<M>` removes and answers every prompt whose
  `window` has passed since the later of its send time and the agent's
  last `Stop`. Text typed mid-turn is queued by Claude and submitted
  after the current turn ends, so the clock starts when the queue can
  drain, not when the text was typed.
- Nothing is persisted: a plugin restart loses the pending markers, and
  at worst a reaction is missing. Accepted.

Here: `eyes` on `Ok` means typed into the pane; the matching submit adds
`+1`; expiry adds `confused` on the comment and the status line `prompt
from @alice not confirmed after 30s`; a slash command gets `eyes` only.
`Tick` runs `expire` every five seconds with `confirmWindow` (§4.2) as
`window`; `0` skips `expire` and the note, and the confirmation still
reacts. Common's `Shared` gains `deliveries_total{outcome}` (`confirmed`,
`unconfirmed`, `command`).

Matrix adopts the same in this spec's plan: a new 📨 mark on `Ok`, its 👍
moved to the confirmation, a thread notice `not confirmed by <agent>
after 30s` on expiry with no further reaction (📨 without 👍 plus the
notice is the picture), `confirmWindow` in its agent block, and a `Tick`
command from a five-second interval its actor did not have. The flow
plugin's `send` step stays fire-and-forget (§14).

## 9. Pull request reviews

`ReviewSubmitted` from a permitted author on a PR with a live session:
fetch the review's comments (`GET /repos/{o}/{r}/pulls/{n}/reviews/{id}/comments`),
map each to common's `review::Comment` (`path`, `side` from `LEFT`/`RIGHT`,
`line` from `line` or `original_line`, `text` = the last line of
`diff_hunk`, `body`), and render:

```
Review by @bob: changes requested, at 3f9c2a1
<common review::render_message: comments in file/line order, then Overall>
```

Delivered as one `send_text` with submit; `+1` on the review is not
possible (GitHub reactions do not apply to reviews), so the status
comment gains `review from @bob delivered`. A review with neither body
nor comments is skipped. A review on a PR with no session is ignored;
a review while a question is open is held in memory and delivered when
the question clears, so it never reaches the dialog; a plugin restart in
between loses it, and the status comment says `review from @bob held`
until it is delivered.

## 10. Ending

- `Closed` (issue closed, PR closed or merged) with a live session:
  apply the fleet without the agent, final status line (`closed` or
  `merged`), row `closed = true`. The row stays for the "session has
  ended" reply and is dropped on `Deactivate`.
- `Tick`: every agent whose `last_activity` is older than `idleTimeout`
  (when non-zero) is ended the same way, status line `stopped after <t>
  idle — mention @<slug> to resume`, row kept with `closed = true`. A
  later start on a number whose row is closed is the resume case of §8.2.
- `Deactivate`: forget the agent, clear its question.
- A fleet with no agents left stays applied with zero agents (the daemon
  accepts an empty crew) and keeps the crew clone, which is what makes
  the next start fast.

## 11. Failure, back-pressure and metrics

- `configure` proves the App (JWT, `GET /app`, the slug); a failure exits
  1. Nothing after that is fatal.
- No GitHub fault fails an `activate`. A failed apply is reported once on
  the issue; a failed comment, edit or reaction is counted and logged;
  the actor never waits on GitHub beyond common's retry-once with the
  `Retry-After` the API asks for, up to the same inline cap as matrix.
- A 401 on an installation token discards it and refetches once; a second
  failure is `errors{kind="auth"}` and the health cell says so.
- `observe` and the webhook handler enqueue and return (G-11).
- Health: the last failure of the App token, the listener, or an apply,
  cleared by the next success of the same kind.

Metrics, prefixed by the SDK: common's `Shared` families, plus
`webhooks_total{event,outcome}` (`handled`, `ignored`, `bad_signature`,
`duplicate`, `too_large`), `sessions_open` (gauge), `applies_total{outcome}`
(`ok`, `config`, `daemon`), `github_requests_total{endpoint,status}`,
`status_edits_total{outcome}`.

## 12. Security

### 12.1 The restricted settings surface (M-13, #64)

A fleet file a plugin applies cannot choose what runs, its environment,
or its sandbox. The `FleetResolver` port only ever resolves plugin files
(the CLI resolves on the client and posts a spec), so its host
implementation resolves in a restricted mode that `balerix-config`
exposes as `ResolveOptions::restricted`. In that mode each of the file's
layers, `defaults`, `crews.<c>.defaults` and `crews.<c>.agents.<a>`, is
checked before merging:

- Refused, naming the layer: `claude.binary`, `claude.args`, `env`,
  `sandbox`. The message is `crews.repo.defaults.env: not allowed in a
  plugin-applied fleet file; the host's default applies`. The host layer
  never contributes these keys, so absence is the host default: the
  pinned binary, no extra arguments, balerix's own environment, the base
  nono profile.
- Refused inside `claude.settings`, same wording: `env` and
  `apiKeyHelper`, the two keys that redirect where credentials go (an
  `ANTHROPIC_BASE_URL`, a helper command). `hooks` was already dropped.
  This inner list is an open set that drifts with Claude Code releases
  and is reviewed when the pinned `claude` moves; the top-level list does
  not drift, since `AgentSettings` denies unknown fields.
- Allowed: `tools`, the rest of `claude.settings`, `claude.resume`,
  `runner`, `plugins`, `branch`, and the crew's `git` (`auth` can only be
  the default or `none`, which narrows).

Unrestricted resolution is unchanged, so `up` behaves as before. For
this plugin the check is free: §6 posts the daemon's 400 verbatim on the
issue, so a repository whose `.balerix.yaml` sets `sandbox` sees exactly
why nothing started.

**The operator's layer.** What a plugin's agents run as is the
operator's to set, and the place is the plugin's own entry in
`plugins.yaml`: an optional `fleetDefaults` mapping, the same shape as a
fleet file's `defaults`, which the daemon layers between the host's
`settings.json` and the plugin's file for every fleet that plugin
applies. The restricted check does not read it, since the operator wrote
it, so it is where `claude.binary`, `claude.args`, `env` and `sandbox`
for a plugin's agents live (the e2e's fake plugin runs `dev fake-claude`
from there). It travels `PluginEntry` → `ResolvedPlugin` → the registry
→ `FleetResolver::resolve(file, name, operator_layer)`; it is not part of
the plugin's restart hash, so an edit takes effect at the plugin's next
apply. Layer order, bottom up: host `claude.settings`, `fleetDefaults`,
the file's `defaults`, the crew's `defaults`, the agent.

### 12.2 The `PUT` answer (M-14, #64)

`PUT /v1/plugin-host/fleets/{name}` answers what `GET fleets/{name}`
would answer this caller: the `FleetRecord` when the plugin also
declares `fleets`, `204 No Content` otherwise. No redacted shape: a
plugin that wants to read records asks for the capability that means
that. This plugin declares `fleets` and waits through `fleets/watch`, so
it loses nothing. `docs/plugin-protocol.md`'s `PUT` row and the
`fleet-put` fixtures gain the 204 case, and `fleet-put-restricted.json`
the §12.1 rejection.

### 12.3 What the threat model says

The `manage` bullet in `docs/THREAT-MODEL.md` is rewritten and three
entries are added.

**The `manage` bullet** loses "`manage` is, in effect, equivalent to
holding the operator's credentials" and "deferred to Spec M". It now
says: a plugin with `manage` runs agents under the host's defaults
(binary, arguments, environment, sandbox) with the operator's
credentials, which is what a prompt from any permitted person already
does (G-5); the file cannot widen that (§12.1), and the `PUT` answer
carries the resolved spec only to a plugin with `fleets` (§12.2). The
table row for a plugin's fleet file gains both. Spec L §7 and §9 gain a
one-line note that Spec M §12 settled them, and #64 closes with this
spec's plan.

**Accepted risk, recorded deliberately (M-2), the GitHub G-5.** Any
collaborator with write permission on a repository the App is installed
on can start an agent that holds the operator's Claude credentials and gh
token, prompt it, answer its questions, and have it push to that
repository. What bounds it: the permission check on every inbound with a
five-minute cache; the fleet file read from the default branch only, so
a PR cannot change the sandbox, tools or settings of the agent that
reviews it; §12.1, so the file cannot change what the agent runs as
either; forks refused; the sandbox around every agent; and the
operator's choice of which repositories the App is installed on. An
operator who wants a narrower boundary installs the App on fewer
repositories.

**Untrusted input.** Issue text, comment bodies and review text are
prompt text for the agent, never commands; the only grammar the plugin
reads is the mention and the answer matching. Text the plugin renders
back (titles, labels, the assistant's message) goes through common's
one-line and split treatment. A webhook body is trusted only after the
HMAC check; `installation.id` and `repository.full_name` come from the
verified payload and never from a query or header. Names built from
repository names are sanitised to the daemon's name rule before they
reach a fleet name or a KV key.

**Secrets.** The private key and webhook secret arrive through
`secrets` (0600 files) or config literals, live in `Secret` newtypes, and
never reach KV, logs, comments or error messages. Installation tokens are
memory-only and expire within the hour. The plugin never sees Claude
credentials or the gh token (Spec L-3).

## 13. Testing

- `github.rs`: `GitHubPort` with `fake::FakePort` recording every call and
  failing on demand, as matrix's port does; methods: `app_slug`,
  `default_branch`, `read_file`, `permission`, `comment`, `edit_comment`,
  `react`, `review_comments`.
- `mention.rs`, `repo_config.rs`, `prompt.rs`, `status.rs`: pure tests and
  insta snapshots (mentions inside code are not mentions; sanitising and
  the 63-byte cut; every §6 refusal with its message; agent injection
  leaves the rest of the file byte-identical as JSON; the first prompt
  for an issue, a PR and a resume; the status comment at twenty lines).
- `webhook.rs` over HTTP: good and bad signatures, a replayed delivery,
  an oversized body, `ping`, an unknown event, each with its counter.
- `actor.rs` against `FakeHost` (with Spec L's `apply_fleet` recording)
  and `FakePort`: a mention starts a session and the applied file carries
  the agent; a non-collaborator is silent; a bot is silent; a fork PR is
  refused; the first prompt goes out on `SessionStart` and never before;
  `Stop` posts the message and a long one is split; the status comment is
  edited, not re-posted, and coalesced; a question round trip through
  common; a review renders one message and waits behind an open question;
  close removes the agent; the idle tick removes and the next mention
  resumes with the branch line; a config error is posted once and applies
  nothing; a name collision is refused; rows survive an actor restart over
  the same `FakeHost`.
- `config.rs`: defaults, unknown keys, the secret sources, `idleTimeout`
  parsing, `kind`/`number` validation with paths.
- `tests/plugin_it.rs`: the real `Plugin` over the wire with `FakeHost`
  and `FakePort`.
- `balerix-config`: restricted resolution refuses each of the four keys
  at each of the three layers with the layer path in the message, and
  `claude.settings.env` and `claude.settings.apiKeyHelper`; a file that
  sets only allowed keys resolves as before; the operator layer may set
  every refused key and sits beneath the file's `defaults`; unrestricted
  resolution is unchanged.
- `balerix-server`: `manage_it.rs` gains the restricted rejection as a
  400 with its path, and the `PUT` answer both ways (the record with
  `fleets`, 204 without); the conformance fixtures of §12.2.
- common `delivery.rs`: a prompt confirmed on an exact and on a
  whitespace-differing `UserPromptSubmit`; an unmatched submit ignored;
  FIFO order with two pending; a slash command never tracked; expiry
  waits for `Stop` while the agent is mid-turn; a `0` window never
  expires.
- `actor.rs`, continued: `eyes` then `+1` on a confirmed comment;
  `confused` and the status line on expiry; the mention's `+1` on the
  first prompt's submit; a slash command gets `eyes` only.
- matrix's actor: the same four cases with 📨 and 👍 and the thread
  notice; the existing "reacts 👍 on `Ok`" assertions move to the
  confirmation.
- `mise run verify-github`, by hand, not in CI: `scripts/verify-github.sh`
  against a real App on a scratch repository, needing `GITHUB_APP_ID`,
  `GITHUB_APP_KEY`, `GITHUB_WEBHOOK_SECRET`, `GITHUB_REPO` and a reachable
  `listen`: one issue session with a question, one PR session with a
  review, a close, and an idle stop with a short timeout.

## 14. Deliberately deferred

- Redelivery catch-up or polling for deliveries missed while down.
- Pull requests from forks.
- Commands in comments (`@bot stop`, `@bot status`).
- Per-repository `configPath`; GitHub Enterprise hosts.
- Pushes attributed to the App (installation tokens into agents, Spec L
  §9's first item). With §12.1 the remaining exposure is the operator's
  gh token inside a sandboxed agent, which G-5 accepts for a prompt; an
  installation token expires hourly and would need refresh into a
  running agent's `hosts.yml` and a per-fleet credential override on
  `PUT`, a new credential path across the boundary. M-4 stands.
- Delivery confirmation for the flow plugin's `send` step: no reader, and
  no machine gates on it.
- Routing reviews on a PR the agent opened from an issue session to that
  session.
- Posting a comment when the agent opens a PR or pushes.
- A queue when a repository's fleet has many agents; nothing limits the
  count today beyond the operator's host.

## 15. Done when

1. `balerix-plugin-github` builds, `mise run plugin github` passes,
   `cargo deny` passes, and `mise run package-plugins github` assembles it.
2. With a real App installed on a scratch repository and a reachable
   listener: a mention on an issue produces a `Ready` agent in
   `gh-<owner>-<repo>` built from that repo's `.balerix.yaml`, the status
   comment appears, the first prompt reaches Claude, and its turn appears
   as a comment.
3. A collaborator's comment reaches the agent with `+1`; a stranger's
   does nothing; a question is answered from a comment and confirmed with
   `hooray`.
4. A mention on a PR runs the agent on the PR's head branch; a submitted
   review with inline comments reaches it as one message.
5. Closing the issue removes the agent; the idle timeout removes it; a
   later mention resumes on the same branch.
6. Editing `.balerix.yaml` on the default branch changes the next
   session's settings; a broken file is reported on the issue.
7. `docs/THREAT-MODEL.md` carries §12; `ARCHITECTURE.md` and AGENTS.md
   describe the plugin and `verify-github`.
8. A comment Claude takes shows `eyes` then `+1`; matrix shows 📨 then
   👍 for the same reply; #85 closes.
9. A `.balerix.yaml` on the default branch that sets `sandbox` is refused
   on the issue with the daemon's path, and `verify-github` includes
   that case; #64 closes.

## 16. Recorded at implementation, part 1 (daemon side)

- The check is `balerix_config::restricted::check_layer`, run on each
  raw layer inside `resolve` when `ResolveOptions::restricted` is set; a
  refused key present with any value, null included, is refused.
- A present ancestor of a refused key that is not a mapping (null, a
  sequence, a scalar: `claude: null`, `claude: []`,
  `claude: { settings: [] }`) is refused too, named at the ancestor
  (`defaults.claude: not allowed …`): merging treats null as "delete
  this subtree" and lets any other non-mapping replace the lower
  layers' mapping, so it would strip the operator's and the host's
  `binary`, `args` and `settings` from beneath, and a sequence
  deserializes by position (`claude: [{}, [], false, /tmp/evil]` would
  choose the binary). A mapping ancestor is fine.
- `disableAllHooks` joined the inner list at implementation: a plugin's
  file could otherwise silence balerix's hooks. The refused
  `claude.settings` keys are `{env,apiKeyHelper,disableAllHooks}`.
- `ResolveOptions::operator_layer` is the `fleetDefaults` mapping;
  `expect_mapping("fleetDefaults", …)` names it when it is not one, and
  the plugin sync refuses a non-mapping with
  `plugins.yaml: plugins[i].fleetDefaults: expected a mapping`.
- The sync also checks the layer's shape by deserializing it as
  `AgentSettings` (nulls dropped first; they mean delete), so a typo
  (`sandox:`) or a wrong shape (`env: "X=1"`) fails the sync under
  `plugins[i].fleetDefaults` rather than an apply under the plugin's
  file. `fleetDefaults: ~` reads as `{}`.
- `FleetResolver::resolve` took a third argument, the layer;
  `FakeResolver::layers()` records it. The layer is not in
  `ResolvedPlugin::hash()`.
- `Host::apply_fleet` answers `Option<FleetRecord>`; `FakeHost::
  answer_manage_records(false)` fakes the 204. Fixtures
  `fleet-put-silent.json` and `fleet-put-restricted.json`; the count is
  twenty-eight.
- The e2e's managed journey runs fake-claude from `fleetDefaults` and
  asserts the refusal through a second, manage-only fake plugin.

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
- `Deliveries::on_event` creates an agent's state on `UserPromptSubmit`,
  `Stop` and `SessionEnd` even before anything was sent to it, so the
  mid-turn bit holds for a prompt routed during a turn that started
  earlier; `forget` on `Deactivate` frees the entry, and the matrix actor
  drops events for a deactivated agent before the tracker sees them.
- The note names its reason. Only expiry claims a duration: `**not
  confirmed by <agent> after <n>s**: Claude did not take the prompt (a
  dialog may be open, or the text may have been swallowed)`. A prompt
  skipped because Claude took a later one first posts `**not confirmed by
  <agent>**: Claude took a later prompt first, so this one was swallowed`;
  one evicted past `MAX_PENDING` posts `**not confirmed by <agent>**: too
  many prompts are waiting on this agent, so this one is no longer
  tracked`. `confirmWindow: 0` silences only the expiry note; skip and
  evict notes are evidence, not a timeout, and still post.
- `parse_duration` rejects a value whose seconds overflow `u64` with the
  same invalid-duration error instead of panicking.
- Matrix's actor tests pause tokio time (`tokio::time::pause`) after the
  fixtures' real round trips to the fake host rather than starting paused:
  a runtime paused from the start auto-advances into reqwest's timeout
  before loopback IO lands. `tokio`'s `test-util` feature is a matrix
  dev-dependency for it, and `main.rs` carries `#![recursion_limit =
  "256"]` for the actor future's size.
- Measured against claude 2.1.283 (2026-09-28, `verify-matrix` on a
  local homeserver): text typed mid-turn fires `UserPromptSubmit` when
  Claude queues it, within milliseconds and before the running turn's
  `Stop`, and each queued message is its own event with its exact text,
  so a mid-turn reply is 👍 at once and two mid-turn replies are both
  👍. The running turn's `Stop` may be elided when prompts are queued;
  one `Stop` follows the combined turn. The "clock waits for `Stop`"
  rule therefore only delays the note for a genuinely swallowed prompt,
  which is the conservative side. No concatenation match is needed.

## 18. Recorded at implementation, part 3 (the plugin)

- Dependencies are path-only on the core crates (AGENTS.md); `base64`
  was added for the `contents` API, beyond §3's list.
- `GitHubPort` gained `issue(installation, repo, number)`: a mention in
  a later comment carries no title or body, so the prompt fetches them.
- The webhook's 413 is axum's body limit and is not counted
  (`too_large` was dropped from `webhooks_total`).
- `Tick` is every five seconds; the idle check, the two-second status
  coalescing, delivery expiry and row write-back all ride on it.
- The status comment continues across an idle stop and a resume: the row
  keeps `status_comment`.
- §6's `restarted: settings changed` line is not produced: the plugin
  sees a reconciler restart as a `SessionEnd`/`SessionStart` pair and does
  not know the reason; the two lines it does post say as much.
- §11's `github_requests_total{endpoint,status}` is not produced: the
  client counts nothing, `errors_total{kind}` covers failures.
- Health: `health.fail` on an `Auth` error or a dead listener,
  `health.ok()` on the next successful apply or comment.
- #64 closes with this part (§15.9) and #85 closed with part 2.
- `Deactivate` keeps a row whose `closed` is set (the daemon deactivates
  every agent an apply drops, so the row must outlive it for the ended
  reply and the resume); an open row is removed. §10's "dropped on
  `Deactivate`" is superseded.
- The unconfirmed note names its reason, as §17 records for matrix: `not
  confirmed after <n>s` (expired), `skipped: Claude took a later one
  first`, `dropped: too many pending`.
- `end` flushes its final status line at once, ahead of the `Deactivate`
  that follows the apply. A failed refile or apply in `end` is posted
  once on the issue, fails health for a daemon refusal, and leaves the
  row open with `last_activity` touched, so the idle check retries one
  `idleTimeout` later.
- Status lines are cut at 200 characters with an ellipsis; a failed edit
  is retried on the next line, not every tick.
- The row carries `base` (the PR's base ref) so a review message renders
  against it after a restart; `installation` is 0 on a row rebuilt from
  an activation.
- `jsonwebtoken` needs its `aws_lc_rs` feature or it panics at runtime;
  the tree already carried `aws-lc-rs` through rustls. `tokio`'s
  `test-util` is a dev-dependency feature for the paused-clock tests,
  which pause after the fixtures' real round trips (as §17 records).
- `FakePort::edit_comment` records the call before answering `NotFound`
  for a deleted comment.
