# Balerix — Spec A / Phase 3: Control plane

**Date:** 2026-09-06
**Status:** Approved in brainstorm 2026-09-06; implements §11 phase 3 of
`2026-09-05-balerix-architecture-design.md` (the *architecture spec*) on top of
`2026-09-06-balerix-a2-runtime-design.md` (the *Phase 2 spec*).
**Scope:** the `balerix-server` crate (registry, per-fleet actors, file store,
vault, HTTP API, hook ingress, metrics), the `FleetStore` and `EventHandler`
ports in `balerix-core`, the CLI commands `serve`, `up`, `update`, `down`,
`status`, `list` and `hook-relay`, three runtime carry-overs from Phase 2, and
the end-to-end journey. Spec A is complete when this phase is done.

Where this document and the architecture spec disagree, this document wins;
§10 lists the corrections and the reasons.

---

## 1. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| P3-1 | **TLS is deferred** to the remote-control-plane milestone. The daemon speaks plain HTTP on `127.0.0.1`, authenticated by the 0600 bearer token. | An unprivileged local process cannot read loopback traffic; TLS on loopback adds cert generation, client pinning and a Claude-side CA-trust assumption for nothing this iteration protects. Amends D4. |
| P3-2 | **`SessionStart` travels through `balerix hook-relay`**, a `command` hook; the other eight events stay HTTP hooks with a literal bearer header. | Verified in claude 2.1.263: HTTP hooks are refused for `SessionStart` and `Setup` ("HTTP hooks are not supported for …"). HTTP hooks do send literal custom headers and accept loopback URLs. Amends D5 for one event only. |
| P3-3 | The relay is **balerix itself**, not `curl`. The balerix binary is granted read-only inside every nono profile. | No new host dependency; one binary, one HTTP client; the same binary is the fake Claude in e2e. |
| P3-4 | **One actor task per fleet** owns that fleet's record, secrets and status; passes run in `spawn_blocking`; status snapshots are published on a `watch` channel. | Single writer per fleet, no cross-fleet stalls, status reads never block. Matches D7 and the future operator shape. |
| P3-5 | **`FleetPhase::Down`** is the terminal phase after `down`; the record and kept directories stay until `--purge`; `up` on a `Down` fleet re-applies in place. | Gives D6's "record kept until purge" a concrete meaning and makes `down --keep` followed by `up` resume sessions. |
| P3-6 | `Outcome` carries only `response`; **`Action` arrives with Spec B**. `EventHandler` and `PassThrough` live in `balerix-core`; the `balerix-events` crate arrives with Spec B. | No producer of actions exists yet (YAGNI, as P2-9). |
| P3-7 | The CLI gains **`status` and `list`**. | Thin `GET` wrappers; `up` and `down` share their renderer; operators and the e2e need them. |
| P3-8 | The **e2e runs in the PR tier**, expected under a minute; it moves to nightly only if the tier breaks its five-minute budget. | It is the acceptance test for this phase and, with an empty system tool table, needs no downloads. |
| P3-9 | HTTP client for the CLI and relay is **`ureq`** (synchronous, no TLS feature). `rustls`, `rcgen`, `reqwest` are not added. | Small, matches a CLI and a once-per-`SessionStart` process. |

## 2. `balerix-core` additions

```rust
pub struct FleetRecord { pub spec: FleetSpec, pub generation: u64, pub desired: Desired, pub status: FleetStatus }
pub enum Desired { Up, Down { keep: Keep, purge: bool } }

/// Everything secret about a fleet. Debug prints `<redacted>`.
pub struct FleetSecrets {
    pub credentials: CredentialBundle,
    pub hook_secrets: BTreeMap<String /* AgentId */, String>,
}

pub trait FleetStore: Send + Sync {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError>;
    fn put(&self, record: &FleetRecord, secrets: &FleetSecrets) -> Result<(), StoreError>; // atomic
    fn purge(&self, name: &FleetName) -> Result<(), StoreError>;                         // record + secrets only
}

pub trait EventHandler: Send + Sync { fn handle(&self, event: &HookEvent) -> Outcome; }
pub struct Outcome { pub response: serde_json::Value }   // `{}` = allow / no-op
pub struct PassThrough;                                    // returns `{}`
```

- `FleetPhase` gains `Down`. `finish_pass(status, terminating: true, all_ok: true)`
  sets `Down`; `plan` for a `Down` fleet with nothing observed is `[]`.
- The architecture spec's `FleetStore::get` is dropped: the in-memory registry
  is authoritative while the daemon runs. `delete` from the brainstorm draft
  was dropped: nothing calls it; a downed fleet keeps its record until purged.
- `HookEvent` lives in `balerix-api`, payload kept raw:
  `{ agent: String, name: String, session_id: Option<String>, received_at: Timestamp, payload: Value }`.
- `balerix-api::GitSettings` gains `identity: Option<GitIdentity { name, email }>`
  (§6.3); `balerix-config` validates both fields non-empty when present.

## 3. `balerix-server`

Depends on `balerix-core` and `balerix-api` only; never on `balerix-runtime`.
Modules, one concern each: `daemon` (registry), `actor` (per-fleet task),
`store` (`FileFleetStore`), `vault`, `api` (fleet routes), `hooks` (event
ingress), `auth` (admin token and per-agent secret extractors), `metrics`,
`lifecycle` (first-run files, endpoint and pid files). The binary wires
`Runtime`, `TmuxRunner`, `SystemClock`, `FileFleetStore` and `PassThrough`
into `Daemon`.

### 3.1 Registry

```rust
pub struct Daemon {
    fleets: RwLock<BTreeMap<FleetName, FleetHandle>>,     // inbox sender + watch receiver
    hook_secrets: RwLock<HashMap<AgentId, String>>,        // ingress authenticates against this
    store: Arc<dyn FleetStore>, materializer: Arc<dyn Materializer>, runner: Arc<dyn AgentRunner>,
    clock: Arc<dyn Clock>, handler: Arc<dyn EventHandler>, metrics: Metrics,
    hook_url: String,                                      // http://127.0.0.1:<port>
    policy: ReconcilePolicy,
}
```

Startup: `store.load_all()`, spawn one actor per record, each actor's first
tick reconciles once. Agents survive daemon restarts because tmux is its own
server and `observe()` finds them; a window that exited while the daemon was
down is noted and restarted by the normal rules.

`POST /v1/fleets`: if a handle exists and its snapshot is not `Down` → 409;
if it exists and is `Down` → `Apply` in place; else spawn an actor, insert the
handle, `Apply`.

### 3.2 Actor

One tokio task per fleet owns `FleetRecord` and `FleetSecrets`. Inbox messages:

| Message | Effect |
|---|---|
| `Apply { spec, credentials, reply }` | `generation += 1`, `desired = Up`, `set_desired`; existing hook secrets kept, a fresh one minted (32 random bytes, hex) for each new agent, departed agents' dropped; `hook_secrets` index updated; persist; reply with the record; run a pass |
| `Down { keep, purge, reply }` | `desired = Down { keep, purge }`; persist; reply; run a pass |
| `Event { agent, name, at }` | `last_event_at = at`; `SessionStart` → `agent_ready`; publish. Persisted at the next pass, not per event |
| tick | run a pass |

The timer fires at the earliest of any agent's `next_restart_at` and a 30 s
resync. A pass is `reconcile_pass` inside `spawn_blocking` with the shared
ports; while it runs the inbox queues, so a Ready arriving mid-pass is applied
when the pass ends. A failed `observe()` is logged, counted in
`balerix_reconcile_errors_total`, leaves the status unchanged, and the next tick
retries. After every pass and every phase change the actor persists the record
and publishes a snapshot (`FleetRecord`) on its `watch` channel; `GET` handlers
read the snapshot without touching the actor.

When a pass ends with phase `Down` and `purge` is set, the actor calls
`store.purge`, removes `fleets/<name>/` entirely, removes its handle from the
registry and its agents from the secret index, and exits. Without `purge` it
stays, idle apart from resync observes.

A pass with a failed step retries at the resync cadence, not at
`next_restart_at`. `Down` requires a clean terminating pass **and** an empty
agent map; `Apply` and `Down` persist before replying.

### 3.3 Store and vault

`FileFleetStore { root: $XDG_STATE_HOME/balerix/fleets, vault: Vault }`.

- `fleets/<f>/fleet.json` — `FleetRecord` as JSON, 0600, tmp + rename.
- `fleets/<f>/secrets.enc` — 24-byte random nonce ‖ XChaCha20-Poly1305
  ciphertext of the `FleetSecrets` JSON, associated data = fleet name, 0600,
  tmp + rename. Written **before** `fleet.json` so a crash between the two
  leaves a readable pair (a record whose secrets are one write ahead).
- `server/vault.key` — 32 random bytes, 0600, created on first `serve`.
  `chacha20poly1305` + `rand`; `VaultError::Tampered` on any AEAD failure.
- `purge` removes the two files only; directories under `crews/` are the
  materializer's (`RemoveCrew` with `keep`) and the purge path's.

### 3.4 API

Plain HTTP; default bind `127.0.0.1:7643`. Every `/v1/fleets` route requires
`Authorization: Bearer <admin token>` (`server/token`, 32 random bytes hex,
0600, created on first `serve`). Errors are JSON `{ "error": "<message>" }`.
Body limit 4 MiB on fleet routes.

| Route | Behaviour |
|---|---|
| `POST /v1/fleets` | body `FleetRequest { spec, credentials }`; 409 if the fleet exists and is not `Down`; else spawn/re-apply; 200 with the record |
| `PUT /v1/fleets/{name}` | 404 if absent; 400 if `spec.name != name`; `Apply` |
| `GET /v1/fleets/{name}` | the `FleetRecord` snapshot |
| `GET /v1/fleets` | `[{ name, phase, generation, observed_generation, agents }]` |
| `DELETE /v1/fleets/{name}?keep_repos=true&keep_sessions=true&purge=true` | 404 if absent; 400 for `purge` with any keep flag; `Down` |
| `POST /v1/agents/{f}/{c}/{a}/events` | hook ingress (§3.5) |
| `GET /metrics`, `GET /healthz` | unauthenticated |

Flags travel as `key=true|false`; axum's `Query` rejects bare keys.

Retry semantics as the architecture spec: a 409 after a lost `POST` response
means the create applied; a replayed `PUT` bumps `generation` but restarts
nothing because restarts key on `SpecHash`.

### 3.5 Hook ingress

In order: bearer secret looked up in `hook_secrets` for the path's agent id
and compared in constant time (hand-rolled XOR fold, unit-tested); **401 for a
bad secret and for an unknown agent alike**, so the route does not enumerate
agents; per-agent token bucket, 20 events/s, burst 50, 429 beyond; body limit
1 MiB (413); body must be a JSON object with a string `hook_event_name`, else
400; unknown event names are accepted. The handler builds `HookEvent`, sends
`Event` to the fleet actor, calls `handler.handle(&event)` and writes
`outcome.response` back, all bounded by a 2 s timeout. Payloads are logged at
`debug` only.

The secret is verified before the rate limiter. Every non-2xx response,
including extractor rejections (413/415), is `{ "error": … }`; a malformed or
unknown-field fleet body is 400.

### 3.6 Metrics

The eight metrics of architecture spec §8 (`balerix_fleets`, `balerix_agents`,
`balerix_reconcile_duration_seconds`, `balerix_reconcile_errors_total`,
`balerix_agent_restarts_total`, `balerix_hook_events_total`,
`balerix_hook_handle_duration_seconds`, `balerix_hook_actions_total` — the last
always zero until Spec B) via the `prometheus` crate. Gauges are recomputed from
snapshots after every pass. `balerix_flow_*` stay reserved.

## 4. `balerix hook-relay`

Reads the hook JSON from stdin (≤ 1 MiB), takes `BALERIX_API_URL`,
`BALERIX_AGENT_ID` and the new `BALERIX_HOOK_SECRET` from its environment,
POSTs to `<url>/v1/agents/<id>/events` with a 5 s timeout, prints the response
body to stdout, exits 0. **On any failure it prints `{}`, writes the reason to
stderr, and still exits 0**: a daemon outage degrades the fleet, it never breaks
the agent (the recorded fail-open risk).

Runtime changes that support it:

- `ToolPaths` gains `balerix: PathBuf`; the binary's wiring fills it from
  `std::env::current_exe()`. `balerix-runtime` still reads no environment.
- `home.rs`: `SessionStart` is rendered as
  `{ "type": "command", "command": "<sh_quote(balerix)> hook-relay", "timeout": 10 }`;
  the other eight events stay HTTP hooks with the literal bearer header.
- `sandbox.rs`: `filesystem.read` gains the balerix binary path;
  `environment.set_vars` gains `BALERIX_HOOK_SECRET` (the reserved `BALERIX_`
  prefix already keeps user `env` out).
- `HookTarget.url` is `http://127.0.0.1:<port>`; `dev materialize --hooks-url`
  defaults to `http://127.0.0.1:7643`.
- `env.rs` sets `MISE_CEILING_PATHS` to the agent workspace so mise's config
  walk stops there instead of applying the repository's own `mise.toml`
  (§8.1); `sandbox.rs` grants a single-file `read` on the agent's own
  rendered `mise.toml` (`MISE_GLOBAL_CONFIG_FILE`), without which `mise exec`
  cannot read its own config inside the sandbox.

## 5. CLI

Client commands resolve the endpoint from `--api-url`, then `BALERIX_API_URL`,
then `server/endpoint`; the token from `server/token`. An unreachable daemon
fails with `daemon not running; run \`balerix serve -d\``. A daemon that
accepts the connection but does not answer in time fails with `daemon did not
answer within Ns`, distinct from the not-running case. Exit codes: 0 ok, 1
failure or timeout, 2 usage.

`serve` gains the hidden `--tmux-socket` and `--detached-child` flags (used to
give the integration tests a private tmux socket and to drive the `-d`
re-exec); `up`/`update` client-validate with `Fleet::try_from` before any
request.

| Command | Behaviour |
|---|---|
| `serve [--bind ADDR] [-d\|--detach]` | first run creates `server/token` and `server/vault.key`; every start binds, then writes `server/endpoint` (`http://127.0.0.1:<port>`, so tests can bind port 0) and `server/balerix.pid`; `tracing` to stderr, or to `server/server.log` when detached. `-d` re-executes the same argv without the flag in its own process group (`CommandExt::process_group(0)`) with stdio redirected, waits ≤ 10 s for the endpoint file, prints it. SIGINT/SIGTERM stop cleanly; agents keep running. `$XDG_CONFIG_HOME/balerix/config.toml` holds `[server] bind = "127.0.0.1:7643"`, `log = "info"`; flags win over the file |
| `up <file> [--name] [--no-host-defaults] [--timeout 5m] [--no-wait]` | resolve exactly as `config resolve`; host credential bundle attached unless `--no-host-defaults`; `POST`; poll `GET` every 1 s until `observed_generation == generation && phase == Ready`; one progress line per agent phase change; on timeout print every non-Ready agent's `message`, exit 1. `--no-wait` prints the returned record and exits 0 right after the `POST` |
| `update <file> …` | as `up`, with `PUT` |
| `down <fleet> [--keep-repos] [--keep-sessions] [--keep] [--purge] [--timeout]` | `--keep` means both keep flags; `--purge` with any keep flag is a usage error; `DELETE`; poll until `Down`, or until 404 when purging |
| `status <fleet> [--json]` | one `GET`, rendered as the table `up`/`down` use: fleet line, then `agent  phase  restarts  message` |
| `list [--json]` | `GET /v1/fleets` as `name  phase  gen/observed  agents` |
| `hook-relay` | §4; listed under an "internal" help heading |
| `dev fake-claude` | hidden; §7 |

**Implementation refinements** (Task 13): `list`'s table renders `GEN` and
`OBSERVED` as two separate columns rather than a combined `gen/observed`
column. `hook-relay` is a `#[command(hide = true)]` subcommand (so it never
appears in `--help` at all) rather than a command listed under a visible
"internal" heading.

## 6. Runtime carry-overs

### 6.1 Cheap steady state
`Runtime::ensure_crew` clones when `repo/` is absent and otherwise does
nothing; `Workspace` fetches only immediately before `worktree add -b` (a
branch that does not exist yet). `TmuxRunner::ensure_crew` is already
`has-session` only. A `Ready`, unchanged fleet therefore costs one
`list-sessions`, and one `has-session` plus one `list-windows` per crew, per
30 s resync.

### 6.2 Skip repeated installs
When the rendered `mise.toml` and `nono-profile.json` are byte-identical to
the files on disk and `agents/<a>/.installed` exists, `materialize` skips
`mise trust`, `mise install` and `nono profile validate`. The marker is written
after both succeed and removed whenever either rendered file changes.

### 6.3 In-sandbox git
`home/.gitconfig` is written every pass (0600):

```ini
[user]
    name = <identity.name  | default: fleet/crew/agent>
    email = <identity.email | default: <agent>@<crew>.<fleet>.balerix.invalid>
[credential]                          ; only when git.auth = gh and git.push = true
    helper =
    helper = !gh auth git-credential  ; gh resolves through the agent's mise shims
```

Host git configuration is not read; `git.identity` is the only source.

## 7. `balerix dev fake-claude`

Hidden subcommand used as `claude.binary` in e2e (`binary: <balerix>`,
`args: [dev, fake-claude]`). It reads `$CLAUDE_CONFIG_DIR/settings.json`; runs
every `SessionStart` `command` hook through `sh -c` with a `SessionStart`
payload on stdin (as Claude does); POSTs a `Notification` payload to every
HTTP hook URL with its headers; writes its argv to `$HOME/fake-claude.argv` and
a marker file under `$CLAUDE_CONFIG_DIR/projects/e2e/` so the `--continue` path
is exercised on restart; then sleeps until killed. Because it runs inside nono
it is also a live isolation check.

## 8. Testing

| Layer | What | Where |
|---|---|---|
| unit | vault seal/open, tamper rejection; store round trip on a tempdir; token bucket; constant-time compare; event body validation; `Terminating → Down`; actor message handling against `FakeMaterializer`/`FakeRunner`/`FakeClock` (`#[tokio::test]`); endpoint resolution order; table rendering; `.gitconfig` rendering; relay stdin/env handling against a local listener | in-module |
| golden (`insta`) | `settings.json` with the command hook, `.gitconfig`, `nono-profile.json` with the new grant and `set_var` (existing `generated_golden` updated); `list`/`status` output | `balerix-runtime/tests`, `balerix/tests` |
| property (`proptest`) | `open(seal(x)) == x` for arbitrary bytes and names; any flipped byte fails; the bucket never exceeds its burst | in-module |
| API integration | in-process axum router over the fakes: create, update, get, list, delete; 409/404/400/401/413/429; a `SessionStart` event turning an agent `Ready`; `/metrics` shows the counters | `balerix-server/tests` |
| e2e | `crates/balerix/tests/e2e.rs`: temp XDG roots under `target/tmp`; local bare repo over `file://` with one commit and a `mise.toml` naming an uninstalled tool; empty `[tools]` system table via `$XDG_CONFIG_HOME/balerix/mise.toml` (no downloads, no `claude` install in CI); `claude.binary` = balerix `dev fake-claude`; `git.auth: none`. Journey: `serve -d --bind 127.0.0.1:0` → `up` (1 crew, 2 agents, bob `resume: true`) → both `Ready` → `update` bob's model → bob's `applied_hash` changes and `restarts` stays 0, alice keeps her pid → `down --keep` → phase `Down`, `repo/` and `agents/` present → `up` → bob's argv contains `--continue` → `down --purge` → `status` 404, directory gone. Also: `Notification` counted in `/metrics`; no hook secret or token in `server.log` or any `launch.sh`. Skips without tools locally; `BALERIX_REQUIRE_TOOLS=1` in CI | PR tier (P3-8) |
| mutation | `cargo mutants -p balerix-core`, now including the `Down` branch | nightly |

**CI.** `check` installs `rust cargo:cargo-nextest cargo-insta tmux nono gh` as
today; the e2e needs nothing more. New mise tasks: `serve` (foreground daemon
under a temp XDG root for manual poking) and `e2e` (`cargo nextest run -p
balerix --test e2e` with `BALERIX_REQUIRE_TOOLS=1`).

### 8.1 Verify at implementation time

| Assumption | Fallback | Verdict |
|---|---|---|
| nono accepts a single file path (the balerix binary) in `filesystem.read` | grant the binary's parent directory | **Holds.** Probed 2026-09-06 with nono 0.75.0: the profile's single-file `read` grant validates, is enforced, and the granted binary executes inside the sandbox. |
| Claude runs `SessionStart` `command` hooks with the hook JSON on stdin and the profile environment (`BALERIX_HOOK_SECRET` visible) | pass the secret as a 0600 file path in the command | **Holds.** `mise run verify-claude` 2026-09-06 with claude 2.1.263 inside the generated profile: the relay posted `SessionStart` (`balerix_hook_events_total{event="SessionStart"} 1`) and `balerix status` showed the agent `ready` with 0 restarts. The e2e proves the same path with `dev fake-claude`. |
| the real Claude fires HTTP hooks to a loopback `http://` URL with the literal `Authorization` header (checked once by hand with claude 2.1.263; the strings `allowedHttpHookUrls` and "HTTP hook not sent: … proxy, TLS trust or resolver settings differ" exist in the binary and their triggers are unknown) | move the remaining events to the relay | **Holds.** Observed 2026-09-06 during `mise run verify-claude` (claude 2.1.263 inside the generated nono profile): claude's debug log shows `Hooks: HTTP hook POST to http://127.0.0.1:<port>/v1/agents/verify/c/a/events` followed by `HTTP hook response status 200` — a 200 means the daemon matched the literal bearer secret from `settings.json`. |
| the real Claude starts inside the generated profile at all | — | **FAILED, then fixed.** claude 2.1.263 refused to start: "Temp directory /tmp/claude-1000 is not readable … Set CLAUDE_CODE_TMPDIR" — the profile grants nothing under `/tmp`. Fix: `home/tmp` (0700) with `TMPDIR` and `CLAUDE_CODE_TMPDIR` in `set_vars`; both names reserved from user `env`. The e2e never saw it because `dev fake-claude` needs no temp dir. |
| the real Claude can reach the Anthropic API from inside the profile | — | **FAILED, then fixed.** With the Phase 2 base `network.connect_port: [<daemon_port>]`, Landlock treats the list as an outbound allowlist: `curl https://api.anthropic.com` got "Could not resolve host" and claude's log showed `getaddrinfo ETIMEOUT`. Probed 2026-09-06 with nono 0.75.0: without `connect_port` egress is allowed and the daemon stays reachable; `open_port: [<daemon_port>]` behaves the same and additionally survives a user `block: true`. Fix: the base profile uses `open_port`, matching the architecture spec's "loopback to the daemon always allowed; other network unrestricted by default". |
| a repository's own `mise.toml` is ignored as untrusted inside the sandbox (e2e repo carries one) | pin `MISE_TRUSTED_CONFIG_PATHS` / `MISE_CONFIG_FILE` | **FAILS.** mise 2026.9.1 honours `[tools]` from an untrusted config — observed trying to install `node@0.0.1` from the e2e repo's `mise.toml`. Applied fallback (differs from the one above): the nono profile now sets `MISE_CEILING_PATHS` to the agent workspace and grants read on the agent's own rendered `mise.toml` (`crates/balerix-runtime/src/env.rs`, `sandbox.rs`, commit `3059886`). Consequence: agents see no project-level mise config at all; balerix's `tools:` table is the only toolchain source inside the sandbox. |
| which `.claude.json` copy Claude reads under `CLAUDE_CONFIG_DIR`, and whether the seed suppresses onboarding (by hand) | keep both copies; extend the seed | **Settled, seed extended.** Same run: only `$CLAUDE_CONFIG_DIR/.claude.json` (`home/.claude/.claude.json`) was rewritten by Claude; `home/.claude.json` kept its materialization mtime, so the Phase 2 double write is gone. No login or onboarding prompt appeared (`hasCompletedOnboarding` works), but the per-folder **trust dialog** did, and `up` cannot reach `ready` until someone accepts it. Claude recorded the acceptance under `projects.<crew repo dir>.hasTrustDialogAccepted` — keyed on the worktree's common git dir, not the workspace it asked about. The seed now pre-accepts both the workspace and the crew `repo/` (`render_claude_json`, `crates/balerix-runtime/src/home.rs`); confirmed on a second run: a fresh `up` reached `ready` unattended in ~5 s and Claude opened on its prompt with no dialog. |
| `HOME` relocation under a live `claude`: nothing lands in nono's `$HOME` (by hand) | add grants | **Holds.** Same run: after a full session `agents/a/nono` held only `.config/nono/{profiles,profile-drafts}` and `.local/state/nono`; every Claude file (`.claude.json`, sessions, projects, history, plugins) landed under `home/.claude`. |

**The hand-run.** The remaining "not yet verified" rows need one real
`claude` session: `mise run verify-claude` (`scripts/verify-claude.sh`) stands
up a scratch daemon with the real `claude` and the host credentials, pauses
for the operator to attach (`tmux -L balerix-verify-<pid> attach -t verify/c`)
and type one prompt, then prints a report with `balerix status` (relay →
`Ready`), the `/metrics` hook counts (HTTP hooks), both `.claude.json`
mtimes, and a listing of nono's own `$HOME` (relocation). Write each result
into the table above. If the HTTP-hook row ever fails, open a follow-up item
"move remaining events to the relay" rather than changing code in this task.

## 9. Errors, security, docs

**Errors.** `StoreError`, `VaultError`, `ApiError` are `thiserror` enums in
`balerix-server`; `ApiError` maps to status codes and the JSON error body. The
binary maps everything through `anyhow` and prints `error: <message>`.

**Threat model updates** (`docs/THREAT-MODEL.md`):
- CLI ↔ daemon: loopback bind + 0600 bearer token; TLS deferred (P3-1) —
  recorded as an accepted risk: "no TLS on loopback; the remote control plane
  brings it".
- Vault, hook-ingress and local-process rows lose their *(planned)* markers and
  point at `balerix-server/src/{vault,hooks,auth}.rs`.
- The per-agent hook secret now also lives in the 0600 `nono-profile.json`
  and the relay process's environment; still authenticates only that agent.
- The balerix binary is readable inside the sandbox; the admin token
  (`server/token`) and the state root are not granted, so an agent cannot drive
  the fleet API. `hook-relay` lets it post events as itself, which it could
  already do over HTTP.
- `server.log` never carries payloads at `info`.

**Docs.** `ARCHITECTURE.md`: Phase 3 flow (client → API → actor → pass;
hooks → ingress → actor) and decisions P3-1, P3-2, P3-4, P3-5. `AGENTS.md`:
`serve` and `e2e` tasks; gotcha that the e2e overrides the system tool table
and why. `README.md`: quickstart `serve -d` → `up` → `status` → `down`; status
line "Spec A complete; Spec B (flows) next". Architecture spec: dated addendum
after the Phase 2 one pointing at §10 here.

## 10. Corrections to the architecture spec

| Architecture spec said | This spec says | Why |
|---|---|---|
| D4: TLS on the wire, cert generated at first `serve` | plain HTTP on loopback; first `serve` creates token and vault key only (P3-1) | loopback is unsniffable by unprivileged processes; no remote yet |
| D5/§6: `SessionStart` HTTP hook signals Ready | `SessionStart` via `balerix hook-relay` command hook (P3-2, P3-3) | Claude refuses HTTP hooks for `SessionStart`/`Setup` |
| §3: `FleetStore::get` | dropped | registry authoritative while running |
| §3: `balerix-events` ships `PassThroughHandler` now | `EventHandler` + `PassThrough` in `balerix-core`; crate arrives with Spec B (P3-6) | one struct; no crate needed yet |
| §8: `Outcome { response, actions }`, `enum Action` | `Outcome { response }` only (P3-6) | no producer |
| §7: fleet phases end at `Terminating`; "record kept until `--purge`" | `Down` terminal phase; `up` on `Down` re-applies in place; 409 only when not `Down` (P3-5) | D6 needed a concrete resting state |
| §7: CLI is `serve \| up \| update \| down` | plus `status`, `list`, `hook-relay` (P3-7, P3-2) | operators and the e2e need reads |
| §10: e2e in the nightly tier | PR tier (P3-8) | acceptance test; cheap |
| §5: `git: { push, auth }` | plus optional `identity { name, email }` with a derived default (§6.3) | commits need an identity |

## 11. Done when

- `mise run check` passes on a fresh clone here and in CI with
  `BALERIX_REQUIRE_TOOLS=1`, e2e included, inside the five-minute budget.
- The README quickstart (`serve -d`, `up examples/payments.yaml` against a
  local repo, `status`, `down --keep`) works by hand on this machine.
- Every row of §8.1 has a recorded verdict.
- `cargo mutants -p balerix-core` reports no surviving mutants in `reconcile`.
- Next: Spec B brainstorm (`balerix-events`, the `flow` block, `Action`).
