# Architecture

Full design: `docs/superpowers/specs/2026-09-05-balerix-architecture-design.md`.
This page is the short map — decisions a newcomer would otherwise re-derive.

## Start here
`crates/balerix/src/main.rs` is the only entry point. Every subcommand is a thin
wrapper: parse args → call a library crate → print. `config resolve` and `dev
materialize` never talk to the daemon; `serve` runs it; `up`, `update`,
`down`, `status`, `list` are HTTP clients of it; `hook-relay` is what an
agent's `SessionStart` hook runs.

Mental model: **balerix is a config generator and process launcher.** It turns a
fleet YAML file into one fully-resolved settings block per agent, then (later
phases) materializes each agent as a tmux window running
`nono run → mise exec → claude` in its own `$HOME`, and reconciles what exists
against what was declared.

## The pieces
- `balerix-api` — serde wire types. A leaf; no logic. Anything that holds a secret
  hand-implements `Debug` and prints `<redacted>`.
- `balerix-core` — the domain: validated names (`FleetName`, …), `RepoRef`, the
  `Fleet` that `FleetSpec` converts into with `TryFrom`, the pure reconciler
  (`reconcile::plan`/`execute`/`apply`), and the ports adapters implement —
  `Materializer`, `AgentRunner`, `Clock`, `WorkspaceReader` today; `FleetStore`
  and `EventHandler` arrive with the server. Never does I/O, so it tests with
  fakes (`balerix_core::fakes`).
- `balerix-config` — YAML → resolved `FleetSpec`. Parses the three-level file,
  deep-merges settings layers as JSON values, types and validates each agent.
- `balerix-server` — the daemon: one actor task per fleet over the reconciler,
  `FileFleetStore` with an encrypted `secrets.enc`, axum routes, hook ingress,
  `/metrics`, `plugins/`: `plugins.yaml` sync, package install, `PluginHost`,
  `PluginRegistry` (activations, interceptor order), `PluginEventHandler` (the
  chain), `PluginKv`, `sessions.rs` (login codes and cookies), `proxy.rs` (the
  mount), `attach.rs` and `watch.rs` (the two streams). Depends on `core` +
  `api` only; the binary hands it the runtime.
- `balerix` — the binary, and the only crate allowed to see both ports and
  adapters; it does the wiring.
- `balerix-runtime` — driven adapters over git, gh, mise, nono, tmux. One
  module per materialization step; every path from StateLayout, every binary
  from ToolPaths; never reads the process environment. `inspect.rs` reads a
  worktree for the plugin host's workspace routes.
- `balerix-plugin-sdk` — the plugin side of the host protocol; depends on
  `api` only. `Host` (async, one method per route, including
  `Host::attach`/`watch_fleets`), the `Plugin` trait (`Plugin::routes`) and
  `serve`, `testing::FakeHost`, `Metrics` (the prefixing registry) and
  `testing::Harness`.
- `balerix-plugin-common` — the published library the chat plugins are
  built on (Spec K): `render` (events to markdown, the fence-aware
  `split`), `question` and `pending` (the `AskUserQuestion` dialog, its
  matching and key plan, the open question mirrored to KV), `answer` (the
  answer flow as a pure decision the plugin's actor executes), `config`
  helpers, `queue`, `metrics::Shared`, `phases` and `review`. A standalone
  project under `plugins/common/`, published to crates.io as its own
  release unit.
- `balerix-plugin-flow` — the first in-tree plugin, on the SDK: a per-agent
  state machine over hook events (`config.rs` parses and compiles the
  `plugins.flow` block, `machine.rs` is the pure step, `plugin.rs` owns the
  agents, the KV-mirrored state and the metrics). Plugin crates depend on
  the SDK and `api` only, and are not workspace members: each is a
  standalone project under `plugins/<name>/` with its own lockfile and
  dependency table (Spec H).
- `balerix-plugin-web` — the second in-tree plugin: a per-agent `enabled`
  flag (`config.rs`), a cache fed by `fleets/watch` (`state.rs`), the
  index, terminal page and the bridge to the daemon's attach on a
  vendored xterm.js (`routes.rs`, `assets/`), the review page with the
  diff, line comments and the agent's hook events (`review.rs`, `state.rs`).
- `balerix-plugin-matrix` — the third in-tree plugin: a Matrix room per
  crew and a thread per agent session, with thread replies coming back as
  `send_text` (Spec G), and an `AskUserQuestion` dialog shown in the thread
  and answered from it with paced `send_keys` (Spec J). Everything about
  Claude's dialog is in common's `question.rs`; the matrix actor executes
  common's `answer` decisions. Bumping `claude` means running
  `mise run verify-questions`. Its `matrix-sdk` tree is larger
  than the rest of the repository put together, which is why plugins
  stopped being workspace members.

## How it flows
**Config (Phase 1):** `read` (file.rs) → `resolve` (resolve.rs): for each agent fold
`host claude.settings → defaults → crew.defaults → agent` with `merge_layers`
(merge.rs), deserialize into `AgentSettings`, `validate_agent` (validate.rs),
then `Fleet::try_from` for names and repos.

**Runtime (Phase 2):** `reconcile::plan` (core) turns desired `Fleet` + last
`FleetStatus` + `ObservedState` into an ordered step list; `execute` walks it
through two ports. `Materializer` (`balerix-runtime::Runtime`) makes files:
clone/worktree → `home/` (settings.json with balerix's hooks, credentials,
hosts.yml) → `mise.toml` + `mise install` → `nono-profile.json` + validate →
`launch.sh`. `AgentRunner` (`TmuxRunner`) makes processes: session per crew,
window per agent, `remain-on-exit`, `respawn-window`. `balerix dev
materialize` runs the file half alone.

**Control plane (Phase 3):** `up` resolves the file exactly like `config resolve`,
validates, and `POST`s `{spec, credentials}` to `http://127.0.0.1:7643`. The
`Daemon` registry spawns (or re-applies) the fleet's actor; the actor bumps the
generation, mints a hook secret per new agent, runs `reconcile_pass` in
`spawn_blocking`, persists `fleet.json` + `secrets.enc`, and publishes a snapshot
that `GET` reads. Agents post hook events to `/v1/agents/{f}/{c}/{a}/events` with
their secret; `SessionStart` arrives via `balerix hook-relay` (a command hook) and
turns the agent `Ready`, which is what `up` waits for. `down` sets `desired: Down`;
a clean terminating pass settles in `Down`, `--purge` deletes the directory.

**Plugins (Spec B, phase 1):** `serve` syncs `$XDG_CONFIG_HOME/balerix/plugins.yaml`
— installs tarballs under `$XDG_DATA_HOME/balerix/plugins/<name>/<digest12>/`,
validates manifests — and renders the list as a synthetic fleet
`balerix`/`plugins`/`<name>` that an ordinary fleet actor reconciles.
`PluginMaterializer` maps each synthetic agent back to its plugin and the
runtime materializes it like an agent under `plugins/<name>/` (`home/`,
profile, `launch.sh` running `nono run → mise run <start>` from the package
root). The actor's per-agent hook secret is the plugin's token;
`POST /v1/plugin-host/hello` verifies it and is the plugin's `SessionStart`.

**Event protocol (Spec B, phase 2a):** `up` resolves the spec and the daemon,
before its actor sees it, activates every `(agent, plugin)` pair on a `Ready`
plugin (`POST /v1/activate` at the address the plugin gave in `hello`); a
rejection is a 400 `crews.<c>.agents.<a>.plugins.<p>: <message>` and nothing
lands, a plugin that is not ready leaves the pair `pending` until its next
`hello`. Activation rows are the `PluginRegistry`'s and are overlaid on the
record at read time, so `up` waits on them and `status` shows them. Every
hook event runs `PluginEventHandler`: the interceptors that subscribe to it
and are active for the agent, in `plugins.yaml` order, each given what
remains of a 1500 ms budget, failures skipped and counted; the last response
goes to Claude; the verdict's actions run afterwards (`send_text` through the
runner, `stop`/`restart` through the actor's `SetStopped`). Observers get
batches from a per-plugin queue. Plugins call back through
`/v1/plugin-host/{fleets,agents/*/actions,kv}` with their token, gated by the
manifest's `needs`. `docs/plugin-protocol.md` is the contract.

**Flow (Spec B, phase 2b):** an agent whose settings carry `plugins.flow`
is activated on the flow plugin with that block; the plugin compiles it
(full-match regexes, config-path errors the daemon prefixes into
`crews.<c>.agents.<a>.plugins.flow: …`) and resumes or resets the agent's
state from KV `state/<agent>`. Every hook event of that agent runs one
step: the first rule of the current state whose event and `match` entries
hold sets the verdict's keys, carries `send`/`action` as verdict actions,
and moves the state — written back to KV before the verdict returns.
`mise run package-plugins` assembles `target/plugins/flow/`, the directory
source the e2e loads.

**Proxy, attach, watch, web (Spec B, phase 3):** a manifest with `routes:
true` mounts the plugin under `/v1/plugins/<name>/`: the daemon forwards
to `<listen>/v1/routes[/<rest>]` with the plugin's own token and
`X-Balerix-Forwarded-Prefix`, strips credentials and hop-by-hop headers,
and passes a 101 through as raw bytes. A browser gets in through `balerix
plugin open <name>`: a 60 s single-use code becomes a 12 h in-memory
session cookie, accepted on the mount from the daemon's origin only.
Every daemon → plugin call now carries the plugin's own token and the SDK
router checks it. `GET /v1/plugin-host/agents/{fleet}/{crew}/{agent}/attach`
bridges a WebSocket to `AgentRunner::attach` — on tmux a throwaway session grouped
with the crew's, in a `portable-pty` PTY — and `fleets/watch` sends the
whole fleets list on every actor snapshot or activation change. `web`
lists the agents whose `plugins.web` block enables it, with phases from a
`fleets/watch`-fed cache, and bridges each browser tab to one attach.

**Workspace reads and review (Spec C):** a plugin declaring `workspace`
reads an agent's worktree through the daemon — `GET
/v1/plugin-host/agents/{id}/workspace/{diff,file,tree}`, gated like
`attach` by the capability and an active pair — and the daemon answers
through the `WorkspaceReader` port, which the runtime implements over
`git` in the worktree with the repository's config escape hatches
closed. The diff is the worktree against the merge-base with
`origin/<crew ref>`, committed, uncommitted and untracked alike. A
fourth route, `version`, answers a cheap fingerprint of the worktree
(Spec D) that the review page polls through `events.json` to refresh
the diff live. `web` observes every hook event into a per-agent buffer
and serves a review page: the diff with line comments, a collapsible
activity column, and a submit that renders one message and sends it
through `send_text`, which tmux delivers as a bracketed paste when the
text has newlines.

## Non-obvious decisions
- **Merge is a left fold, not associative.** `null` means "delete relative to the
  layers below me"; that only has meaning in order. Tested by property
  (idempotent, overlay-dominant, never emits null).
- **Resolution happens client-side.** The daemon only ever sees resolved specs, so
  merge semantics cannot drift between client and server.
- **Exact tool versions only.** `tools: { node: "22" }` is rejected; an unpinned
  entry is a reproducibility bug (developer-environment skill).
- **`claude.settings.hooks` is balerix-owned.** Hook wiring is how the daemon
  hears from agents; users shape behaviour through the settings block's
  `plugins:` map instead (it was called `flow:` before Spec B, and still
  loads under that name).
- **Ports live in `balerix-core`, adapters depend on it, never on each other.**
  The future Kubernetes split cuts between `balerix-server` and
  `balerix-runtime`; `core` is shared.
- **The agent environment lives in the nono profile, not in `env -i`.** nono
  refuses a read-write grant on any directory holding its own state root, and
  it derives that root from its own `$HOME`. So nono runs with `HOME=agents/<a>/nono`
  and sets the agent's `HOME`, `XDG_*`, `BALERIX_*`, `TMPDIR`/`CLAUDE_CODE_TMPDIR`
  (the 0700 `home/tmp`; nothing under `/tmp` is granted and claude refuses an
  unreachable temp dir) through `environment.set_vars` with `deny_vars: ["*"]`.
  `PATH` is the one variable that crosses from outside — so the pane's `TERM`
  does not, and `set_vars` carries `TERM=tmux-256color` and
  `COLORTERM=truecolor` (user `env` may override both), without which claude
  renders no colour.
- **The daemon port is an `open_port`, not a `connect_port`.** On Landlock a
  `connect_port` list is an outbound allowlist: the agent could reach the daemon
  and nothing else, not even DNS or the Anthropic API. `open_port` grants
  localhost TCP on that port only, leaves other egress at nono's default
  (allowed) for the fleet's `sandbox.network` to tighten, and still holds under
  a user `block: true`, so hooks keep flowing when egress is cut off.
- **Worktree branches are reused, never reset.** `-B … origin/<ref>` would drop
  unpushed agent commits on every re-`up` after `down --keep-repos`.
- **The planner is pure; the executor is dumb.** Every decision is in
  `reconcile::plan` (a total function) so the model-based test compares plans
  structurally and `cargo mutants` has something to bite.
- **Plain HTTP on loopback; TLS deferred.** An unprivileged local process cannot
  read loopback traffic, and TLS would have added cert generation, client pinning
  and a Claude CA-trust knob for nothing this iteration protects (Phase 3 spec P3-1).
- **`SessionStart` is a command hook, everything else HTTP.** Claude 2.1.263 refuses
  HTTP hooks for `SessionStart`/`Setup`; the relay is balerix itself, so the nono
  profile grants the binary read-only and passes `BALERIX_HOOK_SECRET` (P3-2, P3-3).
- **One actor per fleet.** Single writer, passes in `spawn_blocking`, snapshots on a
  `watch`; a Ready that lands mid-pass is applied when the pass ends (P3-4).
- **`Down` is a resting state.** The record and kept directories stay; `up` on a
  `Down` fleet re-applies in place; only `--purge` deletes (P3-5).
- **A failing pass retries at the resync cadence**, not at `next_restart_at`, so a
  flapping clone never spins the daemon.
- **Tool installs are a pool hierarchy.** Each level of the fleet file is
  installed into its own read-only mise data dir — the system table into the
  daemon pool, `defaults.tools` into the fleet pool, a crew's
  `defaults.tools` into the crew pool — and every agent runs with its own
  writable data dir in `home/`, reaching the three pools through
  `MISE_SHARED_INSTALL_DIRS` (crew, then fleet, then daemon). An agent can
  therefore `mise install` what its worktree declares without touching a
  binary any other agent executes.
- **The daemon pool has one owner.** The system table is installed by a
  `SystemPool` actor at daemon start and re-checked each resync tick, not by
  whichever crew's pass reached it first. Its readiness is a live condition:
  a fleet pass is skipped, not failed, while the pool is unready, and a pool
  deleted underneath a running daemon is noticed at the next tick. A failed
  install is never fatal — the daemon stays up, publishes the reason and
  retries.
- **The sandbox pins mise's config walk** (`MISE_CEILING_PATHS` = the agent
  root) so the walk sees the worktree's own `mise.toml` and nothing above
  it. `MISE_AUTO_INSTALL=false` keeps installing an explicit act rather than
  a side effect of launch.
- **Plugins ride the reconciler as a synthetic fleet.** `plugin_fleet()`
  renders plugins as agents whose only setting is the plugin hash;
  `plan`/`execute`/`apply` were not touched, and `cargo mutants` still covers
  them for plugins too (PB-5).
- **The plugin token is the hook secret.** One minting path, one index, one
  constant-time check; `hello` is authenticated exactly like a hook event.
- **`balerix` is a reserved fleet name.** Rejected client-side (`config
  resolve`) and by `POST`/`DELETE /v1/fleets`; readable through `GET
  /v1/fleets/balerix` and `balerix status balerix`; never a fleet-list row.
- **`plugins.yaml` is the record.** The plugin actor persists nothing
  (`NullStore`); state survives removal under `plugins/<name>/` until
  `--purge`.
- **The package's `mise.toml` is used in place.** Never copied: the sandboxed
  `mise run` finds it as the local config from the package cwd (no
  `MISE_GLOBAL_CONFIG_FILE` inside the sandbox; `MISE_CEILING_PATHS` =
  package parent + plugin home), while the daemon-side `mise trust`/`mise
  install` name it through `MISE_GLOBAL_CONFIG_FILE`.
- **Activation runs before the actor, and its state is a read-time overlay.**
  `Daemon::apply` activates first so a rejection can be a 400 with nothing
  landed; the registry owns the rows and `Daemon::get`/`snapshots` copy them
  into `AgentStatus.plugins`. Two writers, two records: the actor's is
  persisted, the registry's is rebuilt from the fleet records at start
  (§16.2, §16.3).
- **`stop` and `restart` are per-agent desired state.** `FleetRecord.stopped`
  is honoured by the planner: stopped if observed, never restarted, counter
  untouched. A `restart` is stop then resume, two passes; an `Apply` clears
  the set for every declared agent (§16.4).
- **The chain fails open.** A dead or slow interceptor is skipped and
  counted; a dead `flow` plugin stops blocking, which the threat model
  accepts.
- **The daemon speaks `reqwest` to plugins, the CLI still speaks `ureq`.**
  The chain runs on every hook event under a deadline; a blocking client
  would cost a thread per event (§16.1). No TLS feature on either.
- **Flow's regexes are full-match.** `match: { /tool_input/command: "rm" }`
  matches only the command `rm`; write `rm.*`. A pattern silently matching
  every command containing it is the worse surprise (§17.2).
- **Flow's state survives restarts and resets on `deactivate`.** Plugin and
  daemon restarts re-`activate` without a `deactivate`, so the KV state is
  resumed; `down`, a config change and dropping the block go through
  `deactivate`, which deletes it (§17.3). A changed config arrives as an
  `activate` in place, with no `deactivate` before it, and resets through
  the hash check; a rejected `update` therefore changes nothing (§17.9).
- **Plugin metrics are registered through the SDK.** `Metrics` prefixes
  every family, so an SDK plugin cannot trip the daemon's prefix rule
  (§17.4).
- **Plugin metrics are re-exported only under `balerix_plugin_<name>_`.** A
  body with any other family is dropped whole, so a plugin cannot spoof the
  daemon's own series.
- **The proxy is a byte-level passthrough.** hyper's legacy client
  forwards any method and streams responses; a 101 is upgraded on both
  sides and `copy_bidirectional` does the rest, so the daemon parses no
  WebSocket frame it does not itself terminate and any subprotocol works
  (§18.1). The root of a mount forwards to `/v1/routes` without a slash:
  axum answers a nested router's `/` there.
- **The admin token never enters the browser.** A single-use login code,
  an in-memory session, an `HttpOnly; SameSite=Strict` cookie scoped to
  `/v1/plugins/`, and a same-origin check on every cookie request (§18.2).
- **The daemon presents the plugin's own token.** A plugin's listener is
  a loopback port any local process can reach; the token it already holds
  is what tells the daemon apart (§18.3). Still protocol 1.
- **An attach is a grouped tmux session, set `destroy-unattached` after
  the client is on it.** Attaching the crew session would flip the
  operator's current window; a grouped session has its own. tmux 3.7c
  destroys a detached session the instant the option lands, so the
  create, select, set sequence runs in the PTY with the client attached.
  `kill-session` on the crew alone leaves windows alive in the group, so
  `stop_crew` kills the group (§18.4).
- **`fleets/watch` frames are the whole list.** A consumer replaces its
  state and never diffs or handles removals; the daemon sends only when
  the list differs from the last frame (§18.4).
- **The web plugin never calls `GET fleets`.** Its index comes from the
  watch-fed cache alone, so a correct index is the watch's test (§18.5).
- **Git is run by the daemon, never granted to a plugin.** A read-only
  nono grant on `workspace/` would be fixed at plugin start, would need
  the crew repo too (a worktree's `.git` is a file pointing there) and
  would sit beside `home/`; the typed routes keep one audited set of git
  invocations, in a repository an agent can write to (Spec C PC-1, PC-2).
- **A review is a paste.** The review reaches the agent as one
  `send_text`; `TmuxRunner` fills a named buffer with `load-buffer -`
  from the tmux client's stdin (an argv over 16 KiB is refused, and a
  review may be 64 KiB) and sends it with `paste-buffer -p`, which Claude
  Code is expected to take as one message — verify with `mise run
  verify-claude` (Spec C §8, pending). A literal newline through
  `send-keys -l` is Ctrl-J to the application (Spec C PC-5).
