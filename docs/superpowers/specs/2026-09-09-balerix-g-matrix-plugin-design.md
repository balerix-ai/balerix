# Balerix — Spec G: the Matrix plugin

**Date:** 2026-09-09
**Status:** Approved in brainstorm 2026-09-09
**Scope:** a new plugin crate, `balerix-plugin-matrix`, that posts important
agent events to a Matrix room per crew and a thread per agent session, and
turns a reply in one of those threads into a `send_text` to that agent. Also
two small changes to shared crates: `Plugin::configure` in
`balerix-plugin-sdk`, and a `secrets` map on `PluginEntry` resolved by
`balerix-server`.

---

## 1. Problem

An operator running a fleet has no way to see what their agents are doing
without attaching to tmux or opening the web plugin, and no way to answer an
agent that is waiting on them without being at that machine. The two moments
that matter most are cheap to detect and expensive to miss: `Notification`,
which is Claude saying it needs a permission or has gone idle, and `Stop`,
which is Claude saying the turn is finished and it is the operator's move.

Matrix is a good fit because the operator already has a client on their phone,
rooms and threads map onto crews and sessions without inventing a UI, and a
self-hosted homeserver keeps the traffic on infrastructure they control.

### 1.1 What OpenClaw got right and wrong

OpenClaw's `@openclaw/matrix` channel is the most-used Matrix integration for
an agent runner, and it is worth learning from in both directions.

Worth copying: credentials as either a token or a user id and password, with
the resulting access token cached so the password is used once; an
acknowledgement reaction instead of a chatty confirmation message; a chunk
limit on outbound text; explicit bot-loop protection. Its allowlists and
policies are the right shape and are deliberately not taken here, because G-5
chose room membership as the boundary; if that is ever revisited, OpenClaw's
`groupPolicy` and `dm.policy` split is the model to follow.

Worth avoiding: threads were a retrofit. Matrix threads still share the parent
room's session there, per-thread session isolation is an open feature request
(openclaw/openclaw#31644, #29729) that proposes bolting a `:thread:<id>` suffix
onto an existing session key, and replies inside a room fail to resolve their
thread id at all (#32744). This spec makes the agent session the thread's
identity from the start, so routing has one key and no fallback path.

Also worth avoiding: OpenClaw documents no since-token or backfill behaviour and
leans on the SDK's defaults. That is how a restart replays room history into a
live agent. §9.3 makes the first-sync rule explicit.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| G-1 | The room is a **notification and nudge** surface: observe only, plus `send_text`. No interception, no approve/deny, no lifecycle commands. | The intercept chain runs inside Claude's 2 s hook timeout and shares a 1500 ms budget. Putting a human's phone in that path would either block the agent or answer for them. Approvals are a separate design if they are ever wanted. |
| G-2 | One room per `fleet/crew`, one thread per **agent session**, keyed by the `session_id` the hook events already carry. | Crews already share a repository and a purpose, so they are the natural room. A session is what a `send_text` lands in, so it is the honest unit for a thread. |
| G-3 | The event set is a **curated default with a per-agent override**. Default: `SessionStart`, `Notification`, `Stop`, `SessionEnd`, plus agent phase changes. | `PreToolUse` and `PostToolUse` fire constantly; a default that includes them makes the room useless and the homeserver unhappy. An operator who wants tool visibility can ask for it per agent. |
| G-4 | Rooms are **auto-created, with a config pin as an override**. | A new crew should start posting without a manual step. An operator with an existing room should not be forced to abandon it. |
| G-5 | Access control is **room membership**. Anyone in the room can nudge any agent whose thread is in it. | The operator's decision, taken with the risk stated. §11 records it as an accepted risk and names what bounds it. |
| G-6 | Credentials are `userId` plus `password`; the plugin logs in once and caches the access and refresh tokens in **sealed KV**, and never reads the password again while a cached session works. | Sealed KV is encrypted at rest with the daemon's vault key. A cached refresh token that rotates is a better thing to hold than a static access token. |
| G-7 | The password may come from the config literal **or** from a host file the **daemon** reads, named in a new `secrets` map on the `plugins.yaml` entry. | A file inside the plugin's scratch directory cannot be written before the plugin is first materialized, so a file-backed secret must be resolved daemon-side. Resolving it in the daemon also generalizes to every future plugin. |
| G-8 | Login pins a **device id and display name**, and requests a refresh token. | One revocable device in the operator's client instead of a new one per restart. Revoking it invalidates this session and nothing else. |
| G-9 | The Matrix client is **`matrix-sdk` with E2EE**, not a hand-rolled client. | The operator's decision. It buys encrypted rooms, device verification and key backup, and the Rust SDK's crypto is pure Rust, so there is no native-bindings problem of the kind the JS SDK has. The cost is a large dependency tree; §12.3 says how that is contained. |
| G-10 | Internals are **one actor task** owning all state, fed by a bounded channel. | Ordering is a correctness requirement: a thread root must exist before any event posts into it, and an inbound reply must resolve a root that already exists. An actor gives that without locks and mirrors the daemon's own convention. |
| G-11 | `observe` **enqueues and returns**. A full channel drops the oldest event and counts it. | `observe` is a daemon-to-plugin HTTP call. Blocking it on a slow homeserver would stall hook delivery to Claude. The drop-oldest policy is the one the daemon already uses for its observer queues. |
| G-12 | A reply in a thread whose session has ended is **ignored**, with a reaction saying so. | The thread is the session. Delivering into a different session than the one the operator is reading is a surprise, and keeping every historical root routable would grow the map without bound. |
| G-13 | `matrix.rs` exposes a **port**; the actor is generic over it. | The repo's ports-and-adapters convention, and the only way to test the ordering rules without a homeserver. The actor takes it as a type parameter rather than a trait object, because the port's methods return `impl Future` and so are not dyn-compatible. |
| G-14 | No Matrix fault ever fails an `activate`. Credentials are proved once at `configure`. | A homeserver blip should not fail an operator's `up`. A wrong homeserver or password should fail loudly and immediately, which `configure` does. |

## 3. Crate and modules

```
plugins/matrix/
  Cargo.toml
  src/
    main.rs      env, tracing, build, serve; failures print `matrix: …`, exit 1
    lib.rs
    config.rs    DaemonConfig and AgentConfig, parsed with serde_path_to_error
    matrix.rs    the MatrixPort trait and its matrix-sdk implementation
    render.rs    HookEvent | PhaseChange -> markdown; pure, no I/O
    routing.rs   room and thread maps, and their KV persistence
    actor.rs     the owned task: state plus the command loop
    plugin.rs    the SDK Plugin impl; validates agent config and enqueues
  package/
    balerix-plugin.yaml
    mise.toml
  tests/
    plugin_it.rs
```

Dependencies: `balerix-plugin-sdk` and `balerix-api` only, as with flow and
web. `mise run package-plugins` assembles it into `target/plugins/matrix/`
alongside the other two.

Manifest:

```yaml
apiVersion: balerix/v1
kind: Plugin
name: matrix
version: 0.1.0
protocol: 1
start: serve
# every event: the set is per-agent config (G-3), so filtering is the
# plugin's job, not the subscription's
hooks:
  observe: [SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, SubagentStop, PreCompact]
  intercept: []
# fleets: the phase-change feed. actions: send_text. kv: the maps and the
# sealed session.
needs: [fleets, actions, kv]
routes: false
```

No `sandbox` block. The base profile already grants read on `/etc` for root
certificates, read-write on `home/` and `scratch/`, and leaves outbound network
at nono's default, which is allowed. `TMPDIR` is `home/tmp`, which SQLite needs
and already has.

The matrix-sdk store, crypto keys included, lives under the plugin's scratch
directory, which persists across restarts and is removed only by
`plugin remove --purge`. Documented consequence: a purge discards the device
keys, so the bot rejoins as a new device and previously encrypted history stops
being readable by it.

## 4. Configuration

### 4.1 Daemon level

The `plugins.yaml` entry's `config` object, delivered in the `hello` reply:

```yaml
plugins:
  - name: matrix
    source: ./target/plugins/matrix
    secrets:
      password: ../secrets/matrix-password    # optional; see §5
    config:
      homeserver: https://matrix.example.org
      userId: "@balerix:example.org"
      password: "…"                # optional if `secrets.password` is set
      deviceId: balerix            # default "balerix"
      deviceName: balerix daemon   # default "balerix"
      invite:                      # invited to every auto-created room
        - "@rahul:example.org"
      rooms:                       # optional pins, keyed fleet/crew
        payments/backend: "!abcdef:example.org"
```

`homeserver` and `userId` are required. Exactly one source of the password must
be present, either the `password` key or a `secrets.password` entry; both is an
error, and neither is an error unless a cached session already exists in KV.
Unknown keys are rejected with their path. Auto-created rooms are always
private and always encrypted, so there is no knob for either.

### 4.2 Per agent

The fleet YAML's `plugins.matrix` block, merged fleet to crew to agent like
every other map:

```yaml
defaults:
  plugins:
    matrix: {}                     # enabled with the curated defaults
crews:
  backend:
    agents:
      alice:
        plugins:
          matrix:
            events: [SessionStart, Notification, Stop, SessionEnd, PreToolUse]
            phases: true
```

- `enabled`, default `true`.
- `events`, a list of hook event names, default
  `[SessionStart, Notification, Stop, SessionEnd]`. `SessionStart` and
  `SessionEnd` are lifecycle: they open and close the thread and are always
  posted. Naming them is accepted and has no effect, and leaving them out does
  not suppress them. Every other name in the list is a filter.
- `phases`, default `true`: whether agent phase changes are posted.

Unknown keys and names outside `HOOK_EVENTS` are rejected with their config
path, which fails the operator's `up` with
`crews.backend.agents.alice.plugins.matrix.events[4]: unknown event "Frobnicate"`.

Parsing follows `balerix-plugin-web/src/config.rs`: a non-object config is
rejected before deserializing, and `serde_path_to_error` supplies the path.

## 5. Credentials

### 5.1 The `secrets` map (a `balerix-api` and `balerix-server` change)

`PluginEntry` gains one field:

```rust
/// Config keys whose values are read from a host file at load, so a
/// secret need not be written into plugins.yaml. Paths are relative to
/// this file, like `source`.
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub secrets: BTreeMap<String, PathBuf>,
```

At load and at `plugin sync`, for each entry the daemon reads the file, trims
one trailing newline, and inserts the contents as a string at that top-level
key of `config` before the resolved plugin is handed to the plugin fleet. Rules:

- A key already present in `config` is an error:
  `plugins.yaml: matrix: secrets.password collides with config.password`.
- A missing or unreadable file is an error, so `serve` refuses to start and
  `plugin sync` changes nothing, matching how a digest mismatch behaves.
- A file readable by group or others is an error:
  `plugins.yaml: matrix: secrets.password: /path is mode 0644, expected no
  group or other access`. Every other secret in this system is 0600 and this
  one is held to the same rule; 0400 and 0600 both pass.
- The **resolved** config exists only in memory, for the `hello` reply and for
  `ResolvedPlugin::hash`. Every rendering of plugin config — `config resolve`,
  `plugin list`, error messages — uses the unresolved entry, which holds paths
  and never values.

Because the resolved config feeds `ResolvedPlugin::hash`, rotating the file
restarts the plugin on the next sync, which is the behaviour an operator
rotating a credential wants.

### 5.2 The login flow, at `configure`

Sealed KV holds one record under `auth`:

```rust
#[derive(Serialize, Deserialize)]
struct Session {
    homeserver: String,
    user_id: String,
    device_id: String,
    access_token: Secret,
    refresh_token: Option<Secret>,
}
```

1. Read `auth`. If present and its `homeserver` and `user_id` match the config,
   restore the session and do not read the password at all.
2. Otherwise read the password, log in with `refresh_token` requested and the
   configured `deviceId` and display name, seal the result into `auth`, and log
   at warn that the password source can now be removed.
3. Prove the session with `whoami`. A failure here exits 1, so the daemon
   reports the plugin as not ready and restarts it with backoff.
4. A 401 later triggers a refresh. If the refresh also fails and a password
   source is configured, log in again and log at warn that it happened. If none
   is configured, fail with a message naming the likely cause, a revoked
   device, which is what the operator can act on.

`Secret` is a newtype over `String` with a hand-written `Debug` printing
`<redacted>`, per the repo's convention. It never reaches argv, environment,
logs, error messages or any API response.

## 6. The actor

```rust
enum Command {
    Configure(DaemonConfig),
    Activate { agent: AgentId, config: AgentConfig },
    Deactivate { agent: AgentId },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Inbound(InboundMessage),
}
```

`plugin.rs` does nothing but validate and enqueue. `activate` parses the agent
block synchronously so a bad block still fails the operator's `up`, then sends
`Activate` and returns `Ok`. `observe` sends `Events` and returns; a full
channel drops the oldest and increments `events_dropped_total` (G-11). The
fleet watch task translates `FleetRecord` deltas into `Phases`. The Matrix
inbound stream feeds `Inbound`.

The actor buffers every command until it has seen `Configure`, because the
daemon may call `activate` before `configure` completes (§12.1). Its state:

```rust
struct State<M: MatrixPort> {
    port: M,
    host: Host,
    agents: HashMap<AgentId, AgentConfig>,
    rooms: HashMap<CrewKey, RoomId>,             // fleet/crew -> room
    threads: HashMap<AgentId, Thread>,           // the live session's thread
    routes: HashMap<(RoomId, EventId), AgentId>, // reverse of `threads`
}

struct Thread { session_id: String, root: EventId, room: RoomId, closed: bool }
```

`routes` is derived, held in memory, and rebuilt at startup by listing the KV
`thread/` prefix, so `threads` is the one source of truth. A closed thread is
dropped from `routes` when the agent's next session opens (G-12).

## 7. Rooms and threads

**Room resolution**, on the first activation of any agent in a crew, in order:
the KV record `room/<fleet>/<crew>`; then the config's `rooms` pin, which is
stored to KV on first use; then create. Creation makes a private encrypted room
named `balerix <fleet>/<crew>`, invites every id in `invite`, and stores the id.
Because this runs in the one actor, two agents in the same crew activating at
once cannot race into two rooms.

**Thread lifecycle**, keyed by agent and session id:

- A `SessionStart` whose `session_id` differs from the stored one posts a new
  root message into the crew room carrying the agent name, the session id and
  the start source, then stores
  `thread/<fleet>/<crew>/<agent> = { session_id, root, room, closed: false }`.
- A `SessionStart` with the same id, which is what a compaction produces, posts
  into the existing thread and does not start a new one.
- Every other event for that session posts as an `m.thread` relation to that
  root.
- `SessionEnd` posts a closing line and sets `closed`.
- An event for an agent with no thread — the plugin started mid-session, so no
  `SessionStart` was seen — opens a thread rooted on a synthetic
  "session already running" message, so nothing is dropped for want of a root.

## 8. Outbound: events to messages

`render.rs` holds pure functions returning markdown, which the sdk's `markdown`
feature converts into a plain `body` and an HTML `formatted_body`. Nothing in
`render.rs` performs I/O, so every case is snapshot-testable.

| Event | Message |
|---|---|
| `SessionStart` | the thread root: agent, short session id, `source` |
| `Notification` | the payload's `message`, verbatim — the permission or idle line |
| `Stop` | a one-line turn-finished marker |
| `SubagentStop` | a one-line subagent-finished marker |
| `SessionEnd` | the payload's `reason`, and the thread closes |
| `UserPromptSubmit` | the prompt text |
| `PreToolUse` | tool name and a short input summary |
| `PostToolUse` | tool name and success or failure |
| `PreCompact` | a one-line compaction marker |
| phase change | previous phase, new phase, and the daemon's message |

Any rendered body longer than 4000 characters is truncated at a line boundary
with a marker, which is OpenClaw's default limit and a comfortable margin under
the 64 KiB event size limit homeservers enforce.

## 9. Inbound: replies to `send_text`

### 9.1 The filter

A message is routed only when all of these hold. Each rejection increments
`inbound_total` with its outcome label, which is what makes this debuggable.

- It is not from our own user id (`own_message`).
- It carries an `m.thread` relation (`not_a_thread`). A message posted at room
  level is ignored and gets a reaction saying so; this is the case OpenClaw has
  open as #32744.
- Its thread root resolves through `routes` to an agent (`unknown_thread`).
- That thread is not closed (`stale_thread`), per G-12, and gets a reaction
  saying the session has ended.

### 9.2 The send

A routed message becomes `PluginAction::SendText { text, submit: true }` through
`Host::action`. On success the plugin adds an acknowledgement reaction to the
Matrix message. On failure it posts the daemon's error into the thread and adds
a failure reaction, so a nudge never disappears silently.

### 9.3 History is never replayed

The sdk store persists the sync token, so a restart resumes and picks up a
nudge sent while the daemon was down. When the store has no token, on a first
run or after a purge, the plugin performs one sync with the timeline limited to
zero events, keeps the resulting batch token, and starts from there. Without
this an operator who purges the plugin replays a room's entire history into
live agents.

## 10. Failure, back-pressure and metrics

- `configure` failures exit 1 (§5.2 step 3). Everything after that is retried,
  never fatal.
- No Matrix fault fails an `activate` (G-14). A room that cannot be created is
  retried with backoff, counted, and reported through `Plugin::health`, which
  is what surfaces it in `balerix plugin list`.
- `M_LIMIT_EXCEEDED` responses honour the server's `retry_after_ms`.
- The command channel is bounded; overflow drops the oldest (G-11).

Metrics, through the SDK registry, so every family is already
`balerix_plugin_matrix_`-prefixed:

| Family | Type | Labels |
|---|---|---|
| `messages_sent_total` | counter | `kind` (`root`, `event`, `phase`, `notice`) |
| `events_dropped_total` | counter | — |
| `inbound_total` | counter | `outcome` (`routed`, `own_message`, `not_a_thread`, `unknown_thread`, `stale_thread`, `send_failed`) |
| `rooms` | gauge | — |
| `threads_open` | gauge | — |
| `errors_total` | counter | `kind` (`send`, `create_room`, `sync`, `auth`) |

## 11. Security

**Accepted risk, recorded deliberately (G-5).** Access control is Matrix room
membership. Anyone in a crew's room can post a thread reply that becomes a
prompt to a sandboxed agent holding repository write access and, unless the
crew sets `git.push: false`, push rights. A mis-set join rule, a careless
invite, or a homeserver administrator therefore reaches every agent in that
crew. What bounds it: rooms the plugin creates are private, invite-only and
encrypted; the invite list is daemon config, not per agent; and the agent is
still inside its nono profile, so the blast radius is that crew's repository
and whatever the agent's own settings permit. An operator who wants a narrower
boundary should put one crew per room and invite accordingly. This is added to
`docs/THREAT-MODEL.md` as a named accepted risk.

**Secrets.** The password is 0600 on disk when file-backed, redacted in every
`Debug`, and read at most once per cached session. The access and refresh
tokens live only in sealed KV. The device id is pinned so the operator has
exactly one entry to revoke.

**Untrusted input.** Message bodies from Matrix are prompt text for an agent
and are never interpreted as commands by the plugin. There is no command
grammar to confuse (G-1). Room names, display names and event ids are treated
as data and never interpolated into anything but a message body.

## 12. Changes to shared crates

### 12.1 `balerix-plugin-sdk`: `Plugin::configure`

`serve` currently discards the `HelloResponse.config`, so no plugin can read
daemon-level config. One trait method, with a no-op default, keeps every
existing plugin unchanged:

```rust
/// The daemon-level config from the `hello` reply (plugins spec §2.1).
/// Called by `serve` once, after `hello` succeeds and before the plugin is
/// expected to do any work. `Err(message)` aborts the server and returns
/// the error, so the process exits 1 and the daemon reports the plugin as
/// not ready. The daemon may deliver an `activate` before this returns, so
/// a plugin that needs the config must buffer until it arrives.
fn configure(&self, config: Value) -> impl Future<Output = Result<(), String>> + Send {
    let _ = config;
    async { Ok(()) }
}
```

`serve_on` calls it between `hello` and awaiting the server, aborting and
awaiting the spawned server on `Err` exactly as it already does for a `hello`
failure. `testing::Harness` gains a way to drive it, and the conformance test
covers the default and the failing case.

### 12.2 `balerix-api` and `balerix-server`: the `secrets` map

As specified in §5.1. `PluginEntry` gains the field; the loader resolves it;
rendering paths use the unresolved entry.

### 12.3 Containing the dependency

`matrix-sdk 0.18.0` is pinned exactly in `[workspace.dependencies]` with a
comment saying why, per AGENTS.md. Default features plus `markdown` and
`bundled-sqlite`: the defaults carry `e2e-encryption`, `sqlite` and
`automatic-room-key-forwarding`, `markdown` gives the body and
`formatted_body` pair that §8 needs, and `bundled-sqlite` removes the system
SQLite requirement at the cost of a C compile, which CI already has.

It is a large tree. Two containment rules: only `balerix-plugin-matrix` depends
on it, so nothing else in the workspace inherits it; and `mise x -- cargo deny
check` runs early in implementation, with any `deny.toml` license or advisory
additions made as their own reviewed commit rather than discovered at the end.

## 13. Testing

- `render.rs`: insta snapshots over every row of §8's table, including the
  truncation boundary.
- `config.rs`: defaults, unknown keys and unknown event names, each asserting
  the config path in the message, matching flow and web.
- `routing.rs`: map behaviour and KV round-trips against
  `balerix_plugin_sdk::testing::FakeHost`.
- `actor.rs` against a fake `MatrixPort`, covering the rules that are easy to
  get wrong: a thread root exists before any child posts into it; two agents in
  one crew activating concurrently produce one room; a same-id `SessionStart`
  reuses the thread; a stale thread is ignored; our own messages are ignored;
  a room-level message is ignored; overflow drops the oldest.
- `credentials`: a cached session is reused without reading the password; a
  homeserver change discards it; a 401 refreshes; a failed refresh with no
  password source fails with the revoked-device message.
- `tests/plugin_it.rs` alongside the existing plugin integration tests, driving
  the real `Plugin` impl over the wire format with `FakeHost` and the fake port.
- `balerix-server`: `secrets` resolution, the collision error, the missing-file
  error, the mode check, and that no rendering path emits a resolved value.
- No homeserver in CI. A documented manual check against a real homeserver, in
  the spirit of `scripts/verify-claude.sh`, covering login, room creation,
  encryption, a thread and a round-trip nudge.

## 14. Deliberately deferred

- Approving or denying Claude permission prompts from Matrix (G-1).
- Lifecycle commands: stop, restart, status queries.
- Posting what Claude actually said. The transcript lives in the agent's
  sandboxed home and the plugin cannot read it; a `workspace`-style host route
  for transcripts would be its own design.
- Media, file attachments and diff rendering into the room.
- Spaces: grouping a fleet's crew rooms under a Matrix space.
- Per-thread streaming of tool progress, OpenClaw's `streaming` modes.
- A CLI that seeds a plugin's sealed KV directly, which would remove the last
  plaintext secret at rest. The better end state, and its own piece of work.

## 15. Done when

1. `balerix-plugin-matrix` builds, is packaged by `mise run package-plugins`,
   and starts under the daemon.
2. `Plugin::configure` exists in the SDK with its default, its conformance test
   and its `Harness` support, and flow and web are unchanged.
3. `PluginEntry.secrets` resolves at load, errors on collision, missing file
   and loose permissions, and never renders a value.
4. With a configured homeserver, an `up` on a two-agent crew creates one
   encrypted room, and each agent's `SessionStart` opens its own thread.
5. A `Notification` from an agent appears in that agent's thread.
6. A reply in a live thread reaches the agent as a `send_text` with submit, and
   is acknowledged with a reaction.
7. A reply in a closed thread and a message at room level are both ignored,
   each with its reaction, and each counted under its `inbound_total` label.
8. Restarting the daemon replays nothing; purging and restarting replays
   nothing.
9. `mise run check` passes, `cargo deny` passes, and the accepted risk is in
   `docs/THREAT-MODEL.md`.
