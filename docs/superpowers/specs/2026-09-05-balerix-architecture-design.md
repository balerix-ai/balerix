# Balerix — Architecture Design

**Date:** 2026-09-05
**Status:** Approved 2026-09-05; supersedes `docs/bootstrap.md` (the origin brief)
**Scope:** Whole-system architecture. Implementation is split into two follow-up
specs (see §11): *Spec A — vertical slice* and *Spec B — state machine*.

Balerix is a control plane and orchestrator for fleets of coding agents (Claude
Code first), driven over an HTTPS API and from a thin CLI. The first iteration
runs control plane and data plane on one machine; the design keeps the seam
that later becomes a Kubernetes operator (control) and per-pod agent runtime
(data).

---

## 1. Decisions log

Decisions made during design, recorded so nobody re-derives them.

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | `nono` and `mise` are **external CLIs**, never linked as crates. Balerix generates their config and launches them through tmux. | Owner's call. Both have stable file/CLI contracts (nono profile JSON with a published schema; `mise.toml`). No coupling to fast-moving internals. |
| D2 | **Crew = one GitHub repo + one config layer + one tmux session.** Agent = one tmux window. | Resolves the brief's fleet-vs-crew ambiguity in favour of the more specific statement; crews get a physical meaning. |
| D3 | Crew repo is cloned **once per crew**; each agent gets a **git worktree** on its own branch `balerix/<fleet>/<crew>/<agent>`. | Shared objects, cheap `up`, separate working trees. Git isolation is crew-level (§5). |
| D4 | Credentials: TLS on the wire, **daemon-side encryption at rest** with a key generated at first `serve`. | The daemon must see plaintext to write an agent's `.claude`; the extra layer protects disk, not the wire. |
| D5 | Claude Code **HTTP hooks** post directly to the daemon; the daemon answers synchronously. | No process spawn per event; hooks can block/inject, not only react. |
| D6 | `down` flags: `--keep-repos`, `--keep-sessions`, `--keep` (both); fleet record kept until `--purge`. | The two kinds of state live in different directories anyway. |
| D7 | **Reconciler-centric daemon** (desired vs. observed state, per-fleet loop). | Matches the brief literally and is the shape of the future operator. |
| D8 | Hexagonal layout with an explicit `balerix-core` crate owning the ports. | `writing-clean-code` dependency rule; the infra is designed to be swapped. |
| D9 | Tool versions in config are **exact pins**; fuzzy versions are rejected. | `developer-environment`: an unpinned entry is a reproducibility bug. |

## 2. Principles

- **XDG everywhere.** All state under `$XDG_{CONFIG,DATA,STATE}_HOME/balerix`. Nothing in `~/.balerix`.
- **Rust.** One Cargo workspace, native `cargo` tooling, exact-pinned toolchain via mise.
- **Generate, don't embed.** Balerix is a config generator and process launcher for `git`, `gh`, `mise`, `nono`, `tmux`, `claude`. All six are resolved through balerix's own system `mise.toml`.
- **Declarative and idempotent.** Every materialization step can be re-run on every reconcile.
- **Isolation is the product.** Each agent gets its own `$HOME`, tools, sandbox, and session; the host's `~/.claude` is never visible to an agent.
- **Ports inward.** Domain and reconciler depend on nothing with I/O.
- **YAGNI.** Reserved names (`flow`, `balerix_flow_*`, Docker/Pod runners) are named so they slot in later; none are implemented now.

## 3. Component map & crate layout

```
balerix/            bin — CLI: serve | up | update | down | config resolve; wires adapters into ports
balerix-api/        wire DTOs only (serde): FleetSpec, FleetStatus, HookEvent, HookResponse, requests
balerix-core/       DOMAIN + PORTS. Newtypes, FleetSpec/Status domain model, Reconciler,
                    traits AgentRunner, FleetStore, EventHandler, Clock. No I/O.
balerix-config/     driving adapter — YAML → resolved FleetSpec (merge, validation, host defaults)
balerix-runtime/    driven adapters — TmuxRunner, Workspace (git/gh), Toolchain (mise),
                    SandboxProfile (nono), AgentHome, LaunchPlan rendering
balerix-server/     driving adapter — HTTPS API, hook ingress, /metrics, reconcile tasks; FileFleetStore
balerix-events/     driven adapter — state-machine EventHandler (Spec B). Ships PassThroughHandler now.
```

**Dependency direction**

| Crate | Depends on |
|---|---|
| `balerix-api` | — (leaf: serde DTOs only) |
| `balerix-core` | `api` (implements `From`/`TryFrom` between DTOs and domain types) |
| `balerix-config` | `core`, `api` |
| `balerix-runtime` | `core` |
| `balerix-events` | `core` |
| `balerix-server` | `core`, `api` |
| `balerix` (bin) | everything — it is the only crate that sees both ports and adapters, and it does the wiring (`TmuxRunner`, `FileFleetStore`, `PassThroughHandler` → `server`) |

No adapter crate depends on another adapter crate, and `server` never imports `runtime`. The future Kubernetes split cuts between `server` (operator) and `runtime` (in-pod agent); `core` is shared by both.

**One-line responsibilities**

- `balerix` — parse args, load YAML, discover host defaults, call the API, poll status, print. No orchestration logic.
- `balerix-config` — turn a three-level YAML file into a `FleetSpec` in which every agent is fully resolved. Pure apart from reading the file and `~/.claude`.
- `balerix-core` — decide what should happen (reconcile desired vs. observed) and express it through ports.
- `balerix-runtime` — make one resolved agent exist or not exist, and report whether it does, by driving subprocesses.
- `balerix-server` — persist fleets, host the reconcile tasks, serve the API, accept hook events, export metrics. Receives its `AgentRunner` and `EventHandler` from the bin.
- `balerix-events` — `fn handle(&HookEvent) -> Outcome`. The regex/state-machine implementation is Spec B.

### Ports (owned by `balerix-core`)

```rust
pub trait AgentRunner {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<()>;                        // session exists
    fn ensure_agent(&self, agent: &AgentRef, plan: &LaunchPlan) -> Result<()>;  // window runs plan
    fn stop_agent(&self, agent: &AgentRef) -> Result<()>;
    fn stop_crew(&self, crew: &CrewRef) -> Result<()>;
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState>;              // sessions/windows, alive|dead
    fn send_text(&self, agent: &AgentRef, text: &str, submit: bool) -> Result<()>;
}

pub trait FleetStore {
    fn load_all(&self) -> Result<Vec<FleetRecord>>;
    fn get(&self, name: &FleetName) -> Result<Option<FleetRecord>>;
    fn put(&self, record: &FleetRecord) -> Result<()>;       // atomic
    fn delete(&self, name: &FleetName) -> Result<()>;
}

pub trait EventHandler {
    fn handle(&self, event: &HookEvent) -> Outcome;          // fast and pure; slow work is an Action
}

pub trait Clock { fn now(&self) -> Timestamp; }               // lets backoff and resync be tested deterministically

pub struct LaunchPlan { pub cwd: PathBuf, pub env: BTreeMap<String, String>, pub argv: Vec<String> }
```

`LaunchPlan` is runner-agnostic; a Docker or Pod runner consumes the same struct.

## 4. XDG layout & agent isolation

```
$XDG_CONFIG_HOME/balerix/
  config.toml               daemon settings: bind addr/port, log level (all optional)
  mise.toml                 balerix's SYSTEM tools: tmux, git, gh, nono, claude — inherited by every agent

$XDG_DATA_HOME/balerix/
  mise/                     shared MISE_DATA_DIR: tool installs, written by the daemon, read-only to agents

$XDG_STATE_HOME/balerix/
  server/                   tls.crt, tls.key, token, vault.key (0600), balerix.pid, server.log
  fleets/<fleet>/
    fleet.json              FleetRecord: resolved spec + status + generation (atomic writes)
    credentials.enc         encrypted ~/.claude credential bundle
    crews/<crew>/
      repo/                 shared clone; worktrees hang off it
      agents/<agent>/
        home/               isolated $HOME (.claude/, .config/, .local/, .cache/)
        workspace/          git worktree = Claude's cwd; branch balerix/<fleet>/<crew>/<agent>
        mise.toml           generated (system tools ⊕ agent tools)
        nono-profile.json   generated
        launch.sh           generated; runnable by hand for debugging
        logs/
```

**Agent environment** is built from scratch (`env -i`), never inherited:

| Variable | Value |
|---|---|
| `HOME` | `agents/<agent>/home` |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` | inside `home/` |
| `CLAUDE_CONFIG_DIR` | `home/.claude` |
| `GH_CONFIG_DIR` | `home/.config/gh` |
| `MISE_CONFIG_DIR`, `MISE_STATE_DIR`, `MISE_CACHE_DIR` | inside `home/` |
| `MISE_GLOBAL_CONFIG_FILE` | `agents/<agent>/mise.toml` |
| `MISE_DATA_DIR` | `$XDG_DATA_HOME/balerix/mise` (read-only) |
| `BALERIX_FLEET`, `BALERIX_CREW`, `BALERIX_AGENT`, `BALERIX_AGENT_ID`, `BALERIX_API_URL` | identity |
| `PATH` | minimal system dirs + mise shims |
| user `env` block | appended last; cannot override the rows above |

**mise.** The daemon runs `mise install` for the agent's generated `mise.toml` *before* launch, outside the sandbox, into the shared data dir. Inside the sandbox `mise exec` only resolves; it can never install.

**nono profile (generated).** Read-only: `/usr`, `/lib`, `/lib64`, `/bin`, `/etc`, the shared mise dir. Read-write: `home/`, `workspace/`, and the crew's `repo/.git` (see below). Loopback network to the daemon port is always allowed (hooks); other network is unrestricted by default and tightened per agent through the `sandbox` block. The user's `sandbox` YAML mirrors the nono profile schema and is emitted as JSON; balerix appends its required grants and rejects (rather than overrides) user values that conflict with them. Profiles are checked with `nono profile validate` before launch.

**Isolation boundary — stated plainly.** Git worktrees write objects and refs into the shared `crews/<crew>/repo/.git`, so that directory is read-write for every agent in the crew. Git isolation is therefore **crew-level**; `$HOME`, Claude sessions, tools and network policy are **agent-level**. Agents in a crew share a repo by definition (D2), so this is the intended boundary.

**tmux.** A dedicated server, `tmux -L balerix`, so `down` can never touch a user's own sessions. Sessions are named `<fleet>/<crew>`, windows `<agent>`. Attach with `tmux -L balerix attach -t <fleet>/<crew>`.

## 5. Configuration model & merge semantics

One YAML file per fleet. The **settings block** — `claude`, `sandbox`, `tools`, `env`, `runner`, `flow` — appears at three levels and merges downward: `fleet.defaults` → `crew.defaults` → `crew.agents.<name>`.

```yaml
apiVersion: balerix/v1
kind: Fleet
name: payments                 # CLI arg overrides; all names match [a-z0-9-]+
defaults:                      # settings block, fleet level
  claude:
    settings: { model: sonnet, permissions: { allow: ["Bash(git *)"] } }  # merged into settings.json
    args: ["--verbose"]
    resume: true               # with --keep-sessions, next `up` starts claude with --continue
    binary: claude             # overridable for tests / alternative builds
  sandbox:                     # mirrors the nono profile schema
    network: { mode: allow }
  tools: { node: "22.11.0" }   # exact versions only (D9)
  env: { RUST_LOG: info }
  runner: { type: tmux }       # only tmux exists today
  flow: {}                     # reserved for Spec B
crews:
  backend:
    repo: acme/payments-api    # gh shorthand or full URL; required
    ref: main                  # base ref for per-agent branches
    git: { push: true, auth: gh }
    defaults:                  # settings block, crew level
      tools: { python: "3.12.8" }
    agents:
      alice: {}                # settings block, agent level
      bob: { claude: { settings: { model: opus } } }
```

**Merge rules**

| Value kind | Rule |
|---|---|
| map | recursive deep merge |
| scalar | more specific level wins |
| list | **replace** (never concatenate) |
| explicit `null` at a more specific level | deletes the inherited key |

`claude.settings` and `sandbox` are passthrough maps merged like any other map. Balerix owns `hooks` in `settings.json` and its required grants in the sandbox profile; user values for those keys are **rejected with an error**, never silently overwritten.

**Validation** (client-side, before any request): names, exact tool versions, required `repo`, unknown keys in balerix-owned blocks, forbidden passthrough keys. Errors carry a path: `crews.backend.agents.bob.tools.node: expected exact version (try: mise latest node@22)`.

**Resolution** produces a `FleetSpec` in which every agent is self-describing — all six blocks concrete, no inheritance left. That resolved form is the wire format and what the daemon stores; the server never sees the three-level file, so merge semantics cannot drift between client and server.

**Host defaults** (client-side, on by default, `--no-host-defaults` to disable):

- `~/.claude/settings.json` → base layer *beneath* `fleet.defaults.claude.settings`.
- `~/.claude/.credentials.json` + account fields of `~/.claude.json` → credential bundle, sent alongside the spec.
- `~/.config/gh/hosts.yml` → `gh` token when `git.auth: gh`.

**`balerix config resolve <file>`** prints the resolved `FleetSpec` without contacting the server. It is the debugging tool for three-level merges.

## 6. Runtime: materialization pipeline & runner

Each step is idempotent and re-run on every reconcile.

1. **Workspace** — `git clone` the crew repo (through `gh auth`) into `crews/<crew>/repo` if absent; `git worktree add -B balerix/<fleet>/<crew>/<agent> agents/<agent>/workspace origin/<ref>` if absent.
2. **AgentHome** — create `home/` and XDG subdirs; write `.claude/settings.json` (merged settings + balerix's `hooks` block), `.claude/.credentials.json` (decrypted from the vault, 0600), a seeded `.claude.json`, `.config/gh/hosts.yml` (0600).
3. **Toolchain** — write `agents/<agent>/mise.toml`; `mise trust`; `mise install` with the shared `MISE_DATA_DIR`.
4. **SandboxProfile** — user `sandbox` ⊕ required grants → `nono-profile.json`; `nono profile validate`.
5. **LaunchPlan** — rendered to `launch.sh`:
   `exec env -i <vars> nono run --profile nono-profile.json -- mise exec -- claude <args>`
   All values shell-quoted; secrets never appear in `launch.sh`, argv, or env (§10).

**TmuxRunner** implements `AgentRunner`: `new-session -d -s <fleet>/<crew>`, `new-window -n <agent> -c <workspace> -- launch.sh`, `set remain-on-exit on` so a crashed Claude leaves its output on screen, `respawn-window` for restarts, `send-keys -l <text>` then `Enter` for `send_text`. `observe()` parses `list-sessions`/`list-windows -F` (`#{pane_dead}`, `#{pane_pid}`).

**Liveness has two levels.** `observe()` gives *process* state. *Ready* — Claude up and accepting input — is signalled by the `SessionStart` hook reaching the daemon. `up` waits for `Ready`, not merely for windows to exist.

## 7. Control plane

### API

HTTPS (`axum` + `rustls`), bearer token, default bind `127.0.0.1:7643`.

| Method / path | Purpose |
|---|---|
| `POST /v1/fleets` | `up` — body `{spec, credentials}`; **409 if the fleet exists** |
| `PUT /v1/fleets/{name}` | `update` — same body; 404 if absent; bumps `generation` |
| `GET /v1/fleets/{name}` | spec + status; what `up`/`update` poll |
| `DELETE /v1/fleets/{name}?keep_repos&keep_sessions&purge` | `down` |
| `GET /v1/fleets` | list |
| `POST /v1/agents/{fleet}/{crew}/{agent}/events` | hook ingress; authenticated by a **per-agent secret** (not the admin token) |
| `GET /metrics`, `GET /healthz` | Prometheus + liveness; unauthenticated on loopback |

`POST /v1/fleets` is safe to retry: a 409 after a lost response means the create already applied. A replayed `PUT` bumps `generation` but restarts nothing, because agent restarts are keyed on the resolved-spec hash, not on the generation.

### Status model

```
FleetStatus { generation, observed_generation, phase, agents: { AgentId → { phase, message, last_event_at } } }
fleet phase ∈ Pending | Reconciling | Ready | Degraded | Terminating
agent phase ∈ Pending | Materializing | Starting | Ready | Dead | Stopped
```

`up`/`update` return when `observed_generation == generation && phase == Ready`, or fail at `--timeout` (default 5m), printing every non-Ready agent's `message`.

### Reconciler (`balerix-core`)

One task per fleet, woken by a channel (spec change, delete, agent event) plus a 30 s resync.

Loop body: load desired → `runner.observe()` → for each crew `ensure_crew` → for each agent run the pipeline then `ensure_agent` → stop and clean anything observed but no longer desired. On `update`, an agent whose resolved-spec hash changed is restarted; unchanged agents are untouched. Dead agents restart with bounded exponential backoff; after N failures the agent is `Dead` and the fleet `Degraded` (never stuck `Reconciling`).

### Store

`FileFleetStore` writes `fleets/<name>/fleet.json` atomically (tmp + rename). The in-memory map is authoritative while running; on `serve` start every record is loaded and reconciled once. Because tmux is its own server, **agents survive daemon restarts**.

### Vault

`server/vault.key`: 32 random bytes, 0600, created at first `serve`. `credentials.enc` per fleet, XChaCha20-Poly1305. Plaintext exists only transiently while writing an agent's `.credentials.json`.

### Daemon lifecycle

`serve` runs in the foreground. `serve -d | --detach` re-execs itself detached with stdio → `server/server.log` and writes `balerix.pid`. First run generates cert, key and token. Client commands read token and cert from XDG state; if the daemon is unreachable they fail with "run `balerix serve -d`" rather than auto-starting it.

## 8. Hook ingress & metrics

### Wire types (`balerix-api`)

```rust
struct HookEvent { agent: AgentRef, name: String /* hook_event_name */, session_id: String,
                   received_at: Timestamp, payload: serde_json::Value /* raw */ }
type HookResponse = serde_json::Value;          // returned verbatim to Claude; `{}` = allow / no-op
enum Action { SendText { text: String, submit: bool }, Restart, Stop }
struct Outcome { response: HookResponse, actions: Vec<Action> }
```

The payload stays raw JSON on purpose: Claude's hook schema keeps growing, and Spec B will match on JSON-pointer paths (`/tool_input/command`) with regexes, so balerix never needs a typed model of every event.

### Request path

authenticate per-agent secret → enforce body limit → build `HookEvent` → update agent status (`SessionStart` ⇒ `Ready`; always bump `last_event_at`) → `handler.handle()` → **write the HTTP response first** → execute `actions` through the runner on a spawned task.

Events for one agent are processed under a per-agent lock (ordered); different agents are concurrent. Claude blocks on the response, so the handler must be fast and pure — slow work is an `Action`.

First iteration ships `PassThroughHandler` (returns `{}`, no actions).

### Prometheus (`/metrics`)

| Metric | Type | Labels |
|---|---|---|
| `balerix_fleets` | gauge | `phase` |
| `balerix_agents` | gauge | `fleet, crew, phase` |
| `balerix_reconcile_duration_seconds` | histogram | `fleet` |
| `balerix_reconcile_errors_total` | counter | `fleet` |
| `balerix_agent_restarts_total` | counter | `fleet, crew, agent` |
| `balerix_hook_events_total` | counter | `fleet, crew, agent, event` |
| `balerix_hook_handle_duration_seconds` | histogram | `event` |
| `balerix_hook_actions_total` | counter | `fleet, crew, agent, action` |
| `balerix_flow_state` | gauge | `fleet, crew, agent, state` — reserved for Spec B |
| `balerix_flow_transitions_total` | counter | `fleet, crew, agent, from, to` — reserved for Spec B |

Per-agent labels are acceptable at fleet scale (tens of agents).

## 9. Error handling

- Config errors are caught client-side before any request, with a path to the offending key.
- A failing pipeline step marks the agent `Pending`/`Dead` with the tool's stderr in `message` and the fleet `Degraded`; the reconciler retries with backoff; `up` prints every non-Ready agent's message on timeout. Nothing half-applied is hidden.
- Every external-tool invocation logs stdout/stderr to the daemon log tagged with the agent id.
- Daemon crash loses nothing: tmux keeps agents alive, `fleet.json` is atomic, restart reconciles.
- Library crates return `thiserror` errors; only the binary uses `anyhow`.

## 10. Engineering practices

Derived from the devkit skills (`developer-environment`, `writing-clean-code`, `testing-practices`, `security-practices`, `navigable-codebases`).

### Developer environment

- Repo `mise.toml` pins **exact** versions of: `rust`, `cargo-nextest`, `cargo-insta`, `cargo-audit`, `cargo-deny`, `cargo-mutants`, `gitleaks`, `tmux`, `gh`, `nono`, `claude`. `git` is not in the mise registry and is a system prerequisite (the devcontainer image provides it). `Cargo.lock` is committed. Native `cargo`; no Bazel trigger applies.
- Intended crate set, so additions are visible decisions: `tokio`, `axum`, `rustls`/`axum-server`, `serde`, `serde_json`, `serde_norway`, `thiserror`, `anyhow`, `clap`, `chacha20poly1305`, `prometheus`, `tracing`, `reqwest` (client), `insta`, `proptest`, `proptest-state-machine`.
- Renovate keeps dependencies current on a cadence.

### Clean code (Rust)

- Newtypes for every domain identifier (`FleetName`, `CrewName`, `AgentName`, `AgentId`, `Generation`).
- `Result` + `?`; no panics in library crates.
- Modules mirror domain boundaries (`fleet`, `crew`, `agent`, `reconcile`); one unit per file; files small enough to hold in a context window.
- `rustfmt` + `cargo clippy -- -D warnings` are the style source of truth.
- Hexagonal layout per §3; rule of three before abstracting; delete dead code.

### Testing

Cheapest layer that catches the bug class, climbing only when needed.

| Layer | Target | Oracle |
|---|---|---|
| static | `cargo fmt --check`, `clippy -D warnings` | — |
| unit | reconciler decisions, merge rules, validators | specified |
| golden (`insta`) | resolved spec; generated `settings.json`, `mise.toml`, `nono-profile.json`, `launch.sh` | recorded (narrow snapshots, reviewed, deterministic) |
| property (`proptest`) | merge never emits `null`, is idempotent (`merge(a,a) = strip_nulls(a)`), and re-applying an overlay is a no-op; `FleetSpec` serde round-trips. (Merge is deliberately a left fold and *not* associative — `null` deletes relative to the layers beneath it.) | invariant |
| model-based (`proptest-state-machine`) | reconciler vs. a reference model over random `up / update / down / agent-dies` sequences, through `FakeRunner` + `InMemoryStore` | derived |
| integration | runtime adapters against real `tmux`, `git`, `mise`, `nono` in a temp XDG root and a local bare repo | specified |
| fuzz (`cargo-fuzz`) | YAML config parser; hook-event JSON (untrusted input from agents) | crash |
| e2e | one journey: `serve -d` → `up` (1 crew, 2 agents, `claude.binary` = `fake-claude` that fires `SessionStart` then sleeps) → both `Ready` → `update` a model → one restarts → `down --keep` | specified |
| mutation (`cargo-mutants`) | audit assertion strength in `balerix-core` | — |

Tiers: **pre-commit** = fmt + clippy + gitleaks + unit; **PR (blocking)** = unit + golden + property + integration, wall-clock budget ≤ 5 min; **nightly** = e2e, fuzz, mutation, `cargo audit`. Flaky tests are quarantined immediately, then fixed or deleted. No Docker/testcontainers in tests.

### Security

A committed threat model lives at `docs/THREAT-MODEL.md` (Tier 1, plus a STRIDE pass on hook ingress and the vault) and is linked from `AGENTS.md`. Boundaries and controls:

| Boundary | What crosses | Control |
|---|---|---|
| CLI ↔ daemon | fleet spec, credentials | TLS (self-signed, cert pinned by the client), bearer token 0600 |
| agent ↔ daemon (hook ingress) | **untrusted** event JSON — Claude runs arbitrary code in the sandbox | per-agent secret, body size limit, request timeout, per-agent rate limit, validation at the edge |
| daemon ↔ disk | credentials, tokens | vault (XChaCha20-Poly1305), 0600 files, atomic writes |
| agent ↔ host | filesystem, network | nono sandbox is *the* least-privilege control; cloned repos are untrusted, so balerix-owned `settings.json` keys cannot be overridden by a repo's `.claude/` |
| daemon ↔ tools | argv | argv arrays, never shell strings; `launch.sh` shell-quoted; **secrets never in argv or env** — gh token in `hosts.yml` 0600, Claude credentials only in `.credentials.json` |
| supply chain | crates, tools | exact pins; `cargo-audit` + `cargo-deny` in CI; `gitleaks` pre-commit |

Logs never contain credentials; hook payloads are logged at `debug` only. **Accepted risk, recorded:** if the daemon is down, Claude's HTTP hooks fail open for most events.

### Navigability

- `README.md` quickstart points at task *names*; `AGENTS.md` is canonical, `CLAUDE.md` is one line: "see AGENTS.md".
- `ARCHITECTURE.md` is the "decisions, not layout" map, seeded from §3–§4 of this spec.
- Workflows are `mise` tasks: `mise run test | lint | fmt | e2e | serve`.
- Clone-to-running is verified by CI running it.
- `docs/bootstrap.md` is the origin brief this spec supersedes.

## 11. Build order

**Spec A — vertical slice.** One plan in three mergeable phases, each fully tested before the next:

1. *Foundation:* workspace scaffold, repo `mise.toml`, `balerix-api`, `balerix-core` types, `balerix-config` (schema, merge, resolve, validate, host defaults), `balerix config resolve`.
2. *Runtime:* `balerix-runtime` adapters and integration tests; `balerix-core` reconciler with `FakeRunner` and the model-based suite.
3. *Control plane:* `balerix-server` (store, vault, API, reconcile tasks, pass-through hook ingress, metrics) and the CLI (`serve`, `up`, `update`, `down`). Ends with the e2e journey in §10 passing.

**Spec B — state machine.** `balerix-events`, the `flow` block (regex matching on hook-event JSON-pointer paths, states, actions), and the `balerix_flow_*` metrics. Brainstormed separately once Spec A is done.

**Later, not specified now:** Docker and Pod runners; control/data-plane split; `balerix attach`; list-append merge operator; a typed DSL for flows.

## 12. Verify at implementation time

Facts assumed here that Spec A must confirm early, with the fallback if they fail:

| Assumption | Fallback |
|---|---|
| Claude Code's HTTP hook client accepts a self-signed HTTPS cert, or supports custom headers for the per-agent secret | daemon exposes a plain-HTTP loopback hook listener separate from the HTTPS API; secret moves into the URL path |
| `CLAUDE_CONFIG_DIR` and `GH_CONFIG_DIR` fully relocate their tools' state | additionally bind the paths inside the nono profile |
| `nono run` preserves the environment passed by `env -i` | set the environment through the profile's `environment` block instead |
| `MISE_GLOBAL_CONFIG_FILE` + `MISE_DATA_DIR` give a read-only resolve inside the sandbox | grant read-only access to the shared install dir explicitly and pin with `MISE_CONFIG_FILE` |

## Addendum 2026-09-06 (Phase 2)
`docs/superpowers/specs/2026-09-06-balerix-a2-runtime-design.md` §8 lists the
corrections Phase 2 made to §3 (ports), §4 (agent environment via the nono
profile; nono's own `$HOME`), and §6 (worktree branch reuse, pipeline split
into `Materializer`). Where they differ, the Phase 2 spec wins.

## Addendum 2026-09-06 (Phase 3)
`docs/superpowers/specs/2026-09-06-balerix-a3-control-plane-design.md` §10 lists
the corrections Phase 3 made: D4 (TLS deferred), D5 (`SessionStart` via
`balerix hook-relay`), §3 (`FleetStore` without `get`; `EventHandler` in
`core`), §7 (`FleetPhase::Down`, re-`up` in place, `status`/`list`), §8
(`Outcome` without actions until Spec B), §10 (e2e in the PR tier). Where they
differ, the Phase 3 spec wins. Spec A is complete.

## Addendum 2026-09-06 (Spec B)
`docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` §12 lists the
corrections Spec B made: §11 (Spec B is the plugin mechanism; `flow` is a
plugin), §3 (no `balerix-events`; `balerix-plugin-sdk` and in-tree plugin
crates), §5 (`plugins:` replaces `flow:`), §8 (`Outcome.actions` restored;
`balerix_plugin_flow_*` names), and the reserved fleet `balerix`. Where they
differ, the plugins spec wins.
