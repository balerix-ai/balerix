# Balerix — Spec B: Plugins

**Date:** 2026-09-06
**Status:** Approved in brainstorm 2026-09-06. Supersedes the "Spec B — state
machine" entry of `2026-09-05-balerix-architecture-design.md` §11 (the
*architecture spec*); builds on `2026-09-06-balerix-a3-control-plane-design.md`
(the *Phase 3 spec*).
**Scope:** a general, stateful plugin mechanism for the daemon — the plugin
package format and manifest, the declarative daemon plugin config, the host
protocol (observers, interceptors, actions, fleet reads, KV, attach), the
reverse-proxied HTTP mount, materialization and sandboxing of plugins as
workloads of a reserved fleet, the `balerix plugin` CLI, the
`balerix-plugin-sdk` crate, and two in-tree plugins: `flow` (the state machine
the architecture spec called Spec B) and `web` (terminals in a browser).

Where this document and earlier specs disagree, this document wins; §12 lists
the corrections and the reasons.

---

## 1. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| PB-1 | **Plugins are out-of-process and daemon-launched.** A plugin is a separate program the daemon materializes, launches and talks to over loopback HTTP. | Any language; a plugin crash cannot take the daemon down; plugins that own long-lived processes (`ttyd`) fit. In-process traits would need a rebuild per plugin; WASM is heavy and awkward for process-owning plugins. |
| PB-2 | **Observers and interceptors.** A plugin declares, per hook event, whether it observes (asynchronous stream, never delays Claude) or intercepts (the daemon waits for a verdict inside a shared budget, fail-open). | `flow` must block a tool call; `web` must never slow one down. One mode would either forbid the first or risk the second. |
| PB-3 | **Custom HTTP is a reverse-proxied mount**, `/v1/plugins/<name>/*` on the daemon's listener, admin-token authenticated, WebSocket passthrough included. | One port, one token, one place to add TLS later. Plugin-owned ports would each reinvent auth and the loopback story. |
| PB-4 | **A plugin is a package**: a tarball (or directory) with `mise.toml` at the root and a `balerix-plugin.yaml` manifest naming the mise task that starts it, its hook subscriptions, capabilities and extra sandbox settings. | Plugins can be scripts, not only binaries; their tools are pinned exactly through mise like everything else; `ttyd` is a dependency of `web`, not of balerix. |
| PB-5 | **Plugins are workloads of the reserved fleet `balerix`**, crew `plugins`, driven through the existing `Materializer` and `AgentRunner` ports by the existing reconciler. | Sandboxing, tool installs, restart backoff, status, metrics and the model-based tests come for free; each plugin has a tmux window for its logs. The runner is untouched. |
| PB-6 | **The daemon config declares plugins**: `plugins.yaml` names each plugin's source, digest and daemon-level config; `serve` reconciles the installed set to it. `balerix plugin install` is sugar that edits the file. | Envoy/Vault posture: the file is the source of truth, balerix owns fetch and digest verification. mise installs tools, not directory trees. |
| PB-7 | **Fleet-level activation.** The fleet YAML's `plugins:` block (replacing the reserved `flow: {}`) carries per-agent plugin config, merged like every other map, still resolved client-side. | Per-agent behaviour (`flow` states) needs the three-level merge; the daemon must never merge settings layers itself (architecture spec §5). |
| PB-8 | **State is daemon-provided**: a namespaced KV with an opt-in vault path for secrets, plus a scratch directory for bulk data. | Plugins stay restartable processes; secrets have a vault path; recordings and the like do not go through an API. |
| PB-9 | **Attach is runner-neutral**: the daemon exposes a PTY stream for an agent over the protocol; tmux implements it today. | `web` never learns the tmux socket; Docker/Pod runners implement the same stream later. |
| PB-10 | **Transport is JSON over HTTP/1.1 on loopback, WebSockets for streams**; no gRPC. | Reuses `axum`, `ureq`, the proven `open_port` grant and the per-launch secret pattern. `tonic`/`prost`/`protoc` would be added for typed bidi streams WebSockets already give. |
| PB-11 | **Per-plugin capabilities (`needs`)** gate every host route; a call outside the manifest's list is 403. | Least privilege for a trusted-but-sandboxed process; makes a plugin's reach reviewable from its manifest. |
| PB-12 | **Interceptors fail open**, individually, inside a chain budget of 1.5 s within the existing 2 s hook timeout. | Continues the recorded fail-open posture: a plugin outage degrades a fleet, it never wedges an agent. |
| PB-13 | The **DSL "plugin for plugins" is named, not designed**. | No use case yet (YAGNI, as the architecture spec's "later" list). |

## 2. Package and manifest

A plugin package is a directory whose root holds `mise.toml`, the manifest
`balerix-plugin.yaml`, and the plugin's files. It is distributed as
`<name>-<version>.tar.gz`. Nothing in a package is executed at install time.

```yaml
apiVersion: balerix/v1
kind: Plugin
name: web                    # [a-z0-9-]+; unique per daemon
version: 0.1.0               # semver; informational and shown by `plugin list`
protocol: 1                  # host protocol major (§4); the daemon refuses others
start: serve                 # task in this package's mise.toml
hooks:
  observe: [SessionStart, SessionEnd, Stop]   # streamed, never delays Claude
  intercept: []                               # the daemon waits for a verdict
needs: [fleets, attach]      # host capabilities (§4.1); anything else is 403
routes: true                 # mounts /v1/plugins/web/* (§6)
sandbox: {}                  # nono-mirroring YAML merged over balerix's base (§5)
```

Rules, validated at install:

- `name`, `version`, `protocol`, `start` are required; `hooks`, `needs`,
  `routes`, `sandbox` default to empty, empty, `false`, `{}`.
- Every event under `hooks` is one of the nine hook events balerix wires
  (`HOOK_EVENTS`); an event may appear in both lists.
- `needs` values are exactly the capability names of §4.1.
- `mise.toml` exists, every `[tools]` entry is an exact version (developer
  environment rule; same check as fleet `tools:`), and `[tasks.<start>]`
  exists.
- The manifest is `deny_unknown_fields`; errors are
  `balerix-plugin.yaml: hooks.intercept: unknown event "Foo"`.

Installed packages live under `$XDG_DATA_HOME/balerix/plugins/<name>/<digest12>/`
(the first twelve hex digits of the tarball's sha256), files 0444 and
directories 0555, so a package is immutable and two versions coexist.

### 2.1 Daemon config

`$XDG_CONFIG_HOME/balerix/plugins.yaml` is the declarative load list. Its order
is the interceptor order (§4.3).

```yaml
plugins:
  - name: flow
    source: https://github.com/acme/balerix-plugin-flow/releases/download/v0.1.0/flow-0.1.0.tar.gz
    sha256: 3f2a…                    # required for URL and tarball sources
  - name: web
    source: ./plugins/web-0.1.0.tar.gz   # relative to this file
    sha256: 9c41…
    config: { title: "payments fleet" }  # daemon-level config, passed at hello
  - name: dev-thing
    source: ../src/balerix/target/plugins/dev-thing   # a directory: used in place, no digest
```

`source` is a `https://` URL, a tarball path, or a directory path. A directory
is used in place, never copied, and carries no digest; that is how the in-tree
plugins run in development and the e2e. `name` must equal the manifest's
`name`, and names are unique in the file.

**URL sources are declared but not yet installable.** This build's `ureq`
carries no TLS provider (Phase 3 spec P3-1 forbids adding one), so
`load_plugins_file` and `balerix plugin install` reject an `https://` source
with "URL sources need a TLS-enabled build; use `plugin package` and a tarball
path". `fetch`, `Source::Url` and the digest plumbing stay in place and are
enabled when a TLS provider arrives; until then an operator packages the plugin
and declares the tarball.

**Sync** (`serve` at start, and `balerix plugin sync` against a live daemon)
reconciles the installed set to the file: fetch and verify anything missing
(URL fetch through `ureq`, 60 s timeout, 64 MiB cap, digest checked before
unpacking, unpack rejecting absolute paths, `..`, symlinks and hard links),
validate every manifest, then hand the resulting `Vec<ResolvedPlugin>` to the
plugin fleet (§5.2). A plugin removed from the file is stopped and its record
kept until `plugin remove --purge`. `serve` refuses to start on a digest
mismatch, an unknown protocol, a manifest error or a duplicate name; `sync`
reports the same errors and changes nothing.

### 2.2 Fleet YAML

The settings block's reserved `flow: {}` becomes `plugins: {}`: a map from
plugin name to a passthrough JSON object, merged fleet → crew → agent like every
other map (`null` deletes). Resolution stays client-side. `balerix-config`
validates only that keys are valid names and values are objects.

```yaml
defaults:
  plugins:
    web: { enabled: true }
crews:
  backend:
    agents:
      alice:
        plugins:
          flow: { initial: working, states: { … } }   # see §8.1
```

The daemon rejects an `up` or `update` that names an unloaded plugin with 400
`plugins.web: plugin not loaded`. For every loaded plugin and every agent whose
resolved block mentions it, the daemon calls `activate` (§4.2); a rejection
fails the request with 400 `plugins.flow: <plugin's message>`. Agents whose
block does not mention a plugin are never activated for it, and interceptors
are only consulted for agents they are active for.

## 3. `balerix-core` and `balerix-api` additions

```rust
// balerix-api
pub struct PluginManifest { name, version, protocol: u32, start, hooks: HookSubscriptions,
                            needs: BTreeSet<Capability>, routes: bool, sandbox: Value }
pub struct HookSubscriptions { observe: BTreeSet<String>, intercept: BTreeSet<String> }
pub enum Capability { Fleets, Actions, Attach, Kv }
pub struct PluginStatus { name, version, phase: AgentPhase, listen: Option<String>,
                          routes: bool, active_agents: u32 }
pub struct HelloRequest { name, version, protocol, listen }   // listen: "127.0.0.1:<port>" or a unix path
pub enum PluginAction { SendText { text, submit: bool }, Restart, Stop }
// AgentSettings: `flow: Value` → `plugins: BTreeMap<String, Value>`

// balerix-core
pub struct ResolvedPlugin { pub id: AgentId /* balerix/plugins/<name> */, pub package: PathBuf,
                            pub manifest: PluginManifest, pub config: Value, pub digest: Option<String> }
impl ResolvedPlugin { pub fn hash(&self) -> String }   // package, manifest, config: a change restarts it
pub trait Materializer { …; fn materialize_plugin(&self, plugin: &ResolvedPlugin, secret: &str)
                              -> Result<LaunchPlan, MaterializeError>; }
pub trait AgentRunner { …; fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError>; }
pub trait PtyStream: Read + Write + Send { fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()>; }
pub struct Outcome { pub response: Value, pub actions: Vec<PluginAction> }   // P3-6 reversed
pub const RESERVED_FLEET: &str = "balerix";   // FleetName::from_str rejects it for user fleets
```

`FleetName` parsing rejects `balerix` with `name "balerix" is reserved for the
daemon's plugins`; the daemon constructs it through a private constructor. The
reconciler is unchanged: it plans over ids, hashes and phases, and
`ensure_agent` already takes a `LaunchPlan`. `PassThrough` stays as the handler
used when no plugin intercepts.

## 4. Host protocol

Version 1. JSON over HTTP/1.1, loopback, WebSockets for streams. Every body is
a JSON object; every non-2xx is `{ "error": … }`. The `balerix-plugin-sdk`
crate (§7) wraps both directions for Rust; `docs/plugin-protocol.md` is the
contract for every other language and is kept in step with the SDK by the
protocol conformance test (§10).

### 4.1 Plugin → daemon

Base `BALERIX_API_URL` (the daemon's `http://127.0.0.1:<port>`), bearer
`BALERIX_PLUGIN_TOKEN`: 32 random bytes hex, minted when the plugin is added
and rotated on remove + re-add (a restart reuses it), delivered
through the nono profile environment exactly like an agent's hook secret,
constant-time compared, 401 for unknown token or unknown plugin alike. Routes
under `/v1/plugin-host/`, each gated by the manifest's `needs`; a call outside
it is 403 `capability "actions" not declared in balerix-plugin.yaml`.

| Route | Capability | Semantics |
|---|---|---|
| `POST hello` `{ name, version, protocol, listen }` | always | must match the record; sets the plugin `Ready`, stores `listen`, replies `{ config }` with the daemon-level config. Idempotent; a second hello replaces `listen`. |
| `GET fleets` · `GET fleets/{f}` | `fleets` | the same `FleetRecord` snapshots the CLI reads, secrets excluded |
| `GET fleets/watch` (WS) | `fleets` | one text frame per published snapshot change, any fleet |
| `POST agents/{id}/actions` `{ action: send_text, text, submit }` · `{ action: restart }` · `{ action: stop }` | `actions` | executed through the runner on a spawned task; 404 for an agent the plugin is not active for |
| `GET agents/{id}/attach` (WS) | `attach` | binary frames are PTY bytes both ways; text frames are `{ "resize": { "cols", "rows" } }`; closes when the agent's window dies |
| `GET kv/{key}` · `PUT kv/{key}` · `DELETE kv/{key}` · `GET kv?prefix=` | `kv` | body is raw bytes ≤ 1 MiB; `PUT …?secret=true` stores through the vault; keys `[A-Za-z0-9._/-]{1,200}` |

KV lives under `$XDG_STATE_HOME/balerix/plugins/<name>/kv/`, one 0600 file per
key (secret ones sealed like `secrets.enc`), atomic writes. The scratch
directory `$XDG_STATE_HOME/balerix/plugins/<name>/scratch/` needs no call: its
path arrives as `BALERIX_PLUGIN_SCRATCH` and is read-write in the profile. Both
survive restarts and `plugin remove`; only `--purge` deletes them.

### 4.2 Daemon → plugin

At the `listen` address from `hello`. 5 s timeout unless stated.

| Route | Semantics |
|---|---|
| `POST /v1/activate` `{ agent, config }` | the agent's resolved config for this plugin; non-2xx fails the `up`/`update` with the body's `error` |
| `POST /v1/deactivate` `{ agent }` | on `down`, on removal from the spec, and on a config change (followed by a new `activate`) |
| `POST /v1/events` `{ events: [HookEvent] }` | observer batch, at most 64 events, oldest first; 2xx acknowledges, anything else is logged and counted |
| `POST /v1/intercept` `{ event, response_so_far, deadline_ms }` → `{ response }` | synchronous verdict (§4.3); timeout is `deadline_ms` |
| `GET /v1/health` | 2xx or the plugin is marked `Degraded` in its status message (never restarted for it) |
| `GET /v1/metrics` | Prometheus text, re-exposed by the daemon (§9) |
| `/v1/routes/*` | what `/v1/plugins/<name>/*` proxies to (§6) |

The daemon sends `activate` for every active agent again after each `hello`,
so a restarted plugin recovers its activations without persisting them.

### 4.3 Interceptor chain and observer delivery

For a hook event on agent `a`, the daemon walks the load-list order over the
plugins that intercept that event and are active for `a`. Each receives the
event and `response_so_far` (initially `{}`) and returns the new response; the
last one is written to Claude verbatim. The chain shares a budget of 1500 ms
inside the 2 s hook timeout: each call gets `deadline_ms` = what remains. A
plugin that times out, refuses the connection, or returns a non-2xx or a
non-object is skipped, `response_so_far` is unchanged, the failure is counted
with a `reason` label, and the chain continues. `Outcome.actions` collects the
`send`/`restart`/`stop` a verdict may carry (§8.1) and the daemon executes them
after writing the response, as the architecture spec §8 intended.

Observers get a per-plugin delivery task with a bounded queue of 1024 events.
Events for one agent keep their order; batches are cut at 64 events or 100 ms.
On overflow the oldest event is dropped and `balerix_plugin_events_dropped_total`
is incremented. A plugin that is not `Ready` receives nothing and its queue is
cleared at the next `hello`; catching up on missed events is not offered.

## 5. Plugins as workloads

### 5.1 Materialization and sandbox

A plugin is materialized like an agent under
`$XDG_STATE_HOME/balerix/plugins/<name>/`:

```
plugins/<name>/
  home/            HOME, XDG_CONFIG_HOME…, TMPDIR=home/tmp (0700)
  nono/            nono's own $HOME (its state root), as for agents
  kv/  scratch/    §4.1
  (no mise.toml copy — the package's own is used in place, see §15)
  nono-profile.json  0600; carries BALERIX_PLUGIN_TOKEN
  launch.sh        cd <package>; exec nono run --profile … -- mise run <start>
  logs/
```

`mise install` runs with the shared `MISE_DATA_DIR` so tools are shared with
agents when versions match; `MISE_CEILING_PATHS` is the package's parent
directory plus the plugin's home, so mise's upward config walk sees neither
above the package nor above the plugin's own `$HOME` (see §15). The base
profile grants: read on `/usr`, `/lib`, `/lib64`, `/bin`, `/etc`, the mise
data dir, the package directory (which holds its own `mise.toml`), and the
balerix and mise binaries; read-write on `home/` and `scratch/`, and **no**
grant on `kv/` (KV goes through the API); `network.open_port: [<daemon port>]`;
`environment.set_vars` = `HOME`, `XDG_*`, `TMPDIR`, `PATH`,
`BALERIX_API_URL`, `BALERIX_PLUGIN_NAME`, `BALERIX_PLUGIN_TOKEN`,
`BALERIX_PLUGIN_SCRATCH`, with `deny_vars: ["*"]`. The manifest's `sandbox`
merges over the base with the agent rules: balerix's required grants win,
conflicting user values are rejected at sync with
`balerix-plugin.yaml: sandbox.filesystem.write: "/etc" conflicts with a required grant`.
The profile is checked with `nono profile validate` before launch.

### 5.2 The plugin fleet

The daemon owns one extra actor for the fleet `balerix`, crew `plugins`, whose
"spec" is the synced `Vec<ResolvedPlugin>`. It reuses the actor and reconciler
of the Phase 3 spec verbatim: `ResolvedPlugin::hash` drives restarts on package,
manifest or config change; `observe` gives process liveness; backoff and the
resync cadence apply; the tmux session is `balerix-plugins`, one window per
plugin. Differences from a user fleet: it is never `down`ed by the API, it is
not stored in `fleets/` (the config file is its record; `Ready`/`listen` are
in-memory), its secrets index holds plugin tokens instead of hook secrets, and
`hello` is its readiness event as `SessionStart` is an agent's. `balerix status
balerix` renders it; `balerix plugin list` is that view with `listen` and
`routes` columns.

`PluginHost` (`balerix-server/src/plugins/`) owns: sync (§2.1), the plugin
actor handle, the token index, the `listen` map, the per-plugin observer
queues, the interceptor chain (which is the daemon's `EventHandler`), the
activation map (agent → plugins), the KV store, and the proxy (§6). Fleet
actors call `PluginHost::activate_all(fleet)` from the `Apply` path before
their pass and `deactivate_all` on `Down`; a rejected activation fails the
request before anything is materialized.

## 6. HTTP mount

`ANY /v1/plugins/{name}/{*rest}` on the daemon's router, authenticated by the
admin bearer token like every other API route. The daemon forwards to
`<listen>/v1/routes/{rest}` with the query string, method, body (1 MiB limit)
and headers minus hop-by-hop ones and `Authorization`, adds
`X-Balerix-Forwarded-Prefix: /v1/plugins/<name>` so plugins can build links,
and streams the response back. A request with `Upgrade: websocket` is upgraded
on both sides and the two byte streams are copied until either closes. 404
`plugin "x" has no routes` when `routes: false` or the plugin is unknown, 503
`plugin "x" is not ready` before `hello`. The proxy is `hyper` + `hyper-util`
client-side, which `axum` already depends on; no new HTTP client crate.

## 7. `balerix-plugin-sdk`

A library crate with two halves: `Host` (typed client for §4.1, including
`attach` returning a `PtyStream` over `tungstenite`) and `Plugin` (an `axum`
router for §4.2 that a plugin fills by implementing a trait with default
no-op methods: `activate`, `deactivate`, `observe`, `intercept`, `metrics`,
`routes`). `Plugin::serve()` binds `127.0.0.1:0`, sends `hello`, and runs. The
SDK reads only the `BALERIX_*` variables of §5.1, in one `Env::from_env`
constructor, so plugins stay testable with an injected environment. It also
provides `balerix_plugin_sdk::testing::FakeHost`, an in-process host used by
plugin unit tests and by the server's own tests through the same wire format.

## 8. In-tree plugins

Both are workspace crates on the SDK, packaged by `mise run package-plugins`
into `target/plugins/<name>/` (a directory source, §2.1) and used in place by
the e2e. They are the only plugins this spec designs.

### 8.1 `balerix-plugin-flow`

The state machine of the original brief. Intercepts every hook event; needs
`actions`, `kv`. Per-agent config:

```yaml
plugins:
  flow:
    initial: working
    states:
      working:
        on:
          - event: Stop
            goto: review
            send: { text: "Run the tests and fix any failures.", submit: true }
          - event: PreToolUse
            match: { /tool_input/command: "rm -rf.*" }   # full-match, §17.2
            respond: { decision: block, reason: "no recursive deletes" }
      review:
        on:
          - event: Stop
            goto: done
      done: {}
```

Semantics: a rule matches when the event name equals `event` and every `match`
entry — a JSON pointer into the payload and a regex anchored at both ends —
matches a string value at that pointer (non-strings are stringified with
`to_string`; a missing pointer never matches). The first matching rule of the
current state wins; no match means `{}` and no transition. `respond` is
returned as the verdict merged over `response_so_far` (top-level keys
replaced); `send` becomes a `send_text` action; `goto` moves the state, must
name a declared state, and `initial` must too. Regexes compile at `activate`
with a 10 KiB size limit; any error rejects the activation with the config
path (`states.working.on[1].match./tool_input/command: …`). The current state
per agent is stored in KV under `state/<agent>` so a plugin restart resumes;
`activate` with a changed config resets to `initial`. Metrics:
`balerix_plugin_flow_state{fleet,crew,agent,state}` (1 for the current state)
and `balerix_plugin_flow_transitions_total{fleet,crew,agent,from,to}`.

### 8.2 `balerix-plugin-web`

*Superseded by §18.5 (2026-09-07): no `ttyd`; the plugin serves a vendored
xterm.js and bridges the daemon's attach stream itself. Kept as written for
history.*

Observes `SessionStart` and `SessionEnd`; needs `fleets`, `attach`; `routes:
true`; its `mise.toml` pins `ttyd`. `GET /v1/routes/` is an index page of the
agents whose per-agent config has `enabled: true`, with their phase from the
fleets snapshot. Opening an agent starts, on demand, `ttyd --port 0 --writable
--base-path <prefix>/agents/<id> -- balerix-plugin-web attach <id>`; the
`attach` subcommand bridges the daemon's attach WebSocket to its own stdio and
forwards `SIGWINCH` as resizes. The plugin reverse-proxies `agents/<id>/*` to
that `ttyd` (WebSocket included), stops it after ten minutes without a client,
and the daemon proxies the whole plugin under `/v1/plugins/web/`. The plugin
never learns about tmux.

## 9. Metrics

Daemon-side additions to the Phase 3 registry:

| Metric | Type | Labels |
|---|---|---|
| `balerix_plugin_events_total` | counter | `plugin, event, mode` (`observe`/`intercept`) |
| `balerix_plugin_intercept_duration_seconds` | histogram | `plugin, event` |
| `balerix_plugin_intercept_failures_total` | counter | `plugin, reason` (`timeout`/`connect`/`status`/`body`) |
| `balerix_plugin_events_dropped_total` | counter | `plugin` |
| `balerix_plugin_actions_total` | counter | `plugin, action` |
| `balerix_plugin_proxy_requests_total` | counter | `plugin, status` |

Plugin phases are already `balerix_agents{fleet="balerix",crew="plugins"}`, and
`balerix_hook_actions_total` gains its producer. On every `/metrics` request the
daemon fetches each `Ready` plugin's `GET /v1/metrics` with a 500 ms timeout in
parallel and appends the bodies whose every family name starts with
`balerix_plugin_<name>_`; a body with any other family, or a timeout, is
dropped whole and counted in `balerix_plugin_metrics_scrape_failures_total{plugin}`
(a seventh daemon-side counter). The
`balerix_flow_*` names the architecture spec reserved are therefore
`balerix_plugin_flow_*` (§12).

## 10. CLI

- `balerix plugin install <path|url> [--sha256 <hex>]` — computes or checks the
  digest, appends an entry to `plugins.yaml` (fails if the name exists), runs
  `sync`. `--sha256` is required for URLs.
- `balerix plugin sync` — `POST /v1/plugins/sync` against the live daemon;
  prints what was installed, stopped and left alone.
- `balerix plugin list` — name, version, phase, listen, routes, active agents.
- `balerix plugin remove <name> [--purge]` — removes the entry, syncs; `--purge`
  also deletes `plugins/<name>/` (kv, scratch, home) and the installed packages.
- `balerix plugin package <dir> [--out <file>]` — validates the manifest and
  `mise.toml`, writes the tarball, prints `sha256`.

`serve` fails fast with the §2.1 errors. `up`/`update` print
`plugins.<name>: <message>` for activation rejections in the existing
config-path style.

## 11. Security, errors, docs, testing

**Threat model** (`docs/THREAT-MODEL.md`) gains a row: *plugin ↔ daemon*,
trust "operator-installed, sandboxed"; controls: nono profile from the base
plus the manifest's `sandbox`, `needs` enforced per route, per-plugin token
carried only in the 0600 profile and the `Authorization` header, admin token
required on the proxy, 1 MiB bodies, KV secrets through the vault, no grant on
`kv/`, `server/token` or the state root. Accepted risks recorded: a plugin with
`actions` can drive any agent it is active for; a plugin with `fleets` sees
every fleet's spec (secrets excluded); interceptor fail-open means a dead
`flow` plugin stops blocking. Package unpacking is treated as untrusted input
(§2.1 rules) and fuzzed alongside the YAML parser.

**Errors.** `PluginError` (`thiserror`) in `balerix-server/src/plugins/` with
config-path messages (`plugins.yaml: plugins[1].sha256: mismatch (expected …, got …)`,
`balerix-plugin.yaml: needs: unknown capability "root"`); `ApiError` mappings for
400/403/404/503 above. The SDK's errors are `thiserror` too; only the plugin
binaries use `anyhow`.

**Docs.** `docs/plugin-protocol.md` (the wire contract, §4 and §6, versioned
with `protocol`). `ARCHITECTURE.md`: the plugin piece, the reserved fleet,
PB-5, PB-6, PB-12. `AGENTS.md`: `package-plugins` task; gotchas as found
(reserved name, directory sources, metrics prefix rule). `README.md`: a
`plugins.yaml` example and status "Spec B (plugins) in progress". Architecture
spec: dated addendum pointing at §12 here. The Phase 3 spec's "Next: Spec B
brainstorm" line is left as history.

**Testing.**

| Layer | What | Where |
|---|---|---|
| unit | manifest parse/validate; `plugins.yaml` load and sync planning; digest check and unpack rejections; `FleetName` reserved; `needs` gating; chain merge and budget; observer batching, ordering and drop; KV round trip and secret path; proxy header filtering; flow rule matching, regex limit, state persistence; attach frame encoding; SDK `Env::from_env` | in-module |
| property (`proptest`) | the chain's final response equals the fold over the non-failing plugins regardless of which fail; every KV `put` then `get` round-trips arbitrary bytes and keys; unpack never writes outside the target for arbitrary tar entries | in-module |
| protocol conformance | the SDK `Plugin` router and the server's client agree with `docs/plugin-protocol.md` fixtures (recorded request/response JSON under `docs/plugin-protocol/`); the SDK `Host` against the real router | `balerix-plugin-sdk/tests`, `balerix-server/tests` |
| server integration | a fake plugin built on the SDK runs in-process against `PluginHost` with `FakeMaterializer`/`FakeRunner`/`FakeClock`: hello readiness and re-activation, activation rejection failing `up`, intercept timeout fail-open, WebSocket echo through the proxy, sync adding and removing | `balerix-server/tests/plugins_it.rs` |
| runtime integration (`test-it`) | materialize and launch a real package under nono: it reaches `hello`, cannot read outside its grants, its mise tools resolve, a manifest `sandbox` conflict is rejected | `balerix-runtime/tests` |
| e2e | `plugins.yaml` with directory sources for `flow` and `web`; `up` with both enabled; `fake-claude` gains a stdin reader that appends what tmux sends to `$HOME/fake-claude.stdin`, so the test asserts flow's `send_text` arrived and a `PreToolUse` block verdict came back through the HTTP hook; `GET /v1/plugins/web/` lists the agent; the attach WebSocket streams bytes | `balerix/tests/e2e.rs` |
| fuzz | package unpacking; manifest YAML | nightly |

### 11.1 Verify at implementation time

| Assumption | Fallback |
|---|---|
| a nono profile can let the plugin bind a loopback listener (`network.open_port` or a listen-side equivalent) | **Verified 2026-09-06** (nono 0.75.0, e2e plugin_hello_journey): dev fake-plugin binds 127.0.0.1:0 inside the profile and reports it in hello; the Unix-socket fallback is not needed. |
| `ttyd --base-path` works behind two WebSocket proxies | **Not used** (2026-09-07): the web plugin serves a vendored xterm.js and bridges the attach itself (§18.5). |
| tmux `attach-session` on the private socket gives a clean PTY for `attach` (via `portable-pty` or `openpty`) | **Verified 2026-09-07 (the day Task 4's test first passed)** (tmux 3.7c, portable-pty 0.9.0, `tmux_it::attach_streams_the_pane…`): a grouped session attached in a PTY streams the pane, takes input and resizes, and dies with the stream. |
| balerix's `hyper`/`hyper-util` WebSocket upgrade passthrough works for `axum`'s upgrade path | **Verified 2026-09-07 (the day Task 3's test first passed)** (hyper-util 0.1.20, `browser_it::the_mount_proxies…`): a WebSocket echo through the mount round-trips. |

## 12. Corrections to earlier specs

| Earlier spec said | This spec says | Why |
|---|---|---|
| Architecture §11: Spec B is `balerix-events` and the `flow` block | Spec B is the plugin mechanism; `flow` is the first in-tree plugin | generality: metrics, web terminals and future DSLs share one extension point |
| Architecture §3: `balerix-events` crate | no such crate; `balerix-plugin-sdk`, `balerix-plugin-flow`, `balerix-plugin-web` | the state machine is out of process |
| Architecture §5, `balerix-api`: settings block field `flow: {}` | `plugins: { <name>: {…} }` | one block for every plugin |
| Architecture §8: `enum Action`, `Outcome { response, actions }`; Phase 3 P3-6 dropped `actions` | `PluginAction`, `Outcome { response, actions }` restored | a producer exists |
| Architecture §8: `balerix_flow_state`, `balerix_flow_transitions_total` | `balerix_plugin_flow_state`, `balerix_plugin_flow_transitions_total` | the metrics prefix rule keeps plugin families attributable |
| Phase 3 §2: `Materializer` and `AgentRunner` complete | plus `materialize_plugin` and `attach` | plugins as workloads; runner-neutral attach |
| Phase 3 §3.4: fleet names are any `[a-z0-9-]+` | `balerix` is reserved | the plugin fleet |

## 13. Build order

Three mergeable phases, each fully tested before the next:

1. **Plugin workloads** — package format and manifest, `plugins.yaml` and sync,
   `plugin install|list|remove|package|sync`, `ResolvedPlugin`,
   `materialize_plugin`, the plugin fleet and `PluginHost` skeleton, `hello`
   readiness, the SDK's `Env`/`hello` half. Ends with a trivial SDK plugin
   reaching `Ready` in the e2e.
2. **Event protocol** — split in two on 2026-09-07 (§16):
   - **2a, the protocol** — activate/deactivate, observers, the interceptor
     chain replacing `PassThrough`, `Outcome.actions`, actions, `fleets`, KV,
     metrics re-export, the full SDK, `docs/plugin-protocol.md` and the
     conformance test. Ends with `dev fake-plugin` blocking a `PreToolUse`
     and sending text in the e2e.
   - **2b, the flow plugin** — `balerix-plugin-flow`, `mise run
     package-plugins`, the SDK's `Metrics` and `testing::Harness`. Ends
     with the flow e2e assertions. (`fleets/watch` moved to phase 3 on
     2026-09-07, §17.1.)
3. **Proxy and attach** — the `/v1/plugins/*` mount with WebSocket passthrough,
   `AgentRunner::attach` on tmux, `fleets/watch`, `balerix-plugin-web`. Ends
   with the web e2e assertions. (Refined in §18 on 2026-09-07: browser
   sessions, the daemon bearer on plugin calls, xterm.js instead of `ttyd`.)

## 14. Done when

- `mise run check` passes here and in CI with `BALERIX_REQUIRE_TOOLS=1`, e2e
  included, within the five-minute budget.
- By hand: `plugins.yaml` with the two directory sources, `serve -d`, `up
  examples/payments.yaml` with `flow` and `web` enabled, a browser on
  `/v1/plugins/web/` shows a live Claude terminal, and a blocked `rm -rf` shows
  up in Claude as the flow plugin's reason.
- Every row of §11.1 has a recorded verdict.
- `cargo mutants -p balerix-core` still reports no surviving mutants in
  `reconcile`.

## 15. Refinements from the phase 1 plan (2026-09-06)

- Plugins are driven through a synthetic `Fleet` (`balerix_core::plugin_fleet`); the reconciler is unchanged and `PluginMaterializer` maps the ids back.
- The plugin token is the actor's per-agent hook secret, delivered through the `HookTarget` the executor already passes; `hello` authenticates with `Daemon::verify_secret`.
- The package's `mise.toml` is used in place, never copied: the sandboxed `mise run` finds it as the local config from the package cwd (no `MISE_GLOBAL_CONFIG_FILE` inside the sandbox; `MISE_CEILING_PATHS` = package parent + plugin home), while the daemon-side `mise trust`/`mise install` name it through `MISE_GLOBAL_CONFIG_FILE`.
- The tmux session is `balerix/plugins`.
- The reserved name is enforced at the API and in `balerix_config::resolve`, not in `FleetName`.
- `Materializer` gains `materialize_plugin` and `purge_plugin`.
- `PluginStatus` has no `active_agents` until Phase 2.
- `plugin install` syncs only when a daemon is running; `plugin remove --purge` requires one.
- Unpacked package directories are 0755 (files 0444/0555) so `--purge` is a plain `remove_dir_all`.
- `plugins.yaml` is written atomically by `plugin install|remove` (temp file + rename).
- The plugin token is minted on the `Apply` that first declares the plugin and rotates on remove + re-add; the actor reuses an existing secret, so a restart keeps the token (§4.1).
- `PluginError::Fetch` carries `url`, not `source`.
- URL sources are rejected at load and at `plugin install` until a TLS-enabled build (§2.1); a pinned URL whose digest is already unpacked is answered from `install_root` without fetching.

## 16. Refinements from the phase 2a brainstorm (2026-09-07)

Decisions phase 1's code forced on the event protocol. Phase 2a is the
protocol alone; the flow plugin is phase 2b.

### 16.1 Async end to end, on `reqwest`

Every daemon → plugin call (§4.2) and the SDK's `Host` (§4.1) use `reqwest`,
pinned exact, `default-features = false` with `json` only: no TLS stack, so
P3-1 holds. This reverses the phase 1 plan's "no `reqwest`" constraint: the
interceptor chain runs on every hook event under a deadline, and a blocking
client in `spawn_blocking` would have cost a thread per event and per observer
batch. `ureq` stays in the CLI and the relay (P3-9) and leaves the SDK.
`EventHandler::handle` becomes async, returning a boxed `std::future::Future`
so `Arc<dyn EventHandler>` still works and `balerix-core` stays free of tokio.
`PassThrough` remains for tests and as the zero-plugin behaviour.

### 16.2 Activation runs in `Daemon::apply`, before the actor

`up`/`update` resolve each agent's `plugins:` map against the plugin
registry before `Msg::Apply` is sent:

- A name no `plugins.yaml` entry declares is 400
  `crews.<c>.agents.<a>.plugins.<p>: no plugin "<p>" is installed`; nothing
  reaches the actor.
- For every `(agent, plugin)` pair whose config is new or changed: a `Ready`
  plugin is sent `activate` now; a non-2xx is 400 in the same path style
  quoting the plugin's `error`, and the whole apply is rejected before any
  pair is marked active. A plugin that is not `Ready` records the pair as
  **pending** and the apply proceeds.
- After the actor replies, `deactivate` goes to the pairs the new spec
  dropped. A changed pair is never deactivated: its new config arrives by
  `activate` alone, before the actor, and the plugin replaces the one it
  holds (§17.9). `down` deactivates every pair of the fleet and removes
  the rows.
- Each `hello` clears the plugin's observer queue and re-sends `activate` for
  every pair, pending or active; a rejection then marks the pair
  **rejected** with the message. Nothing is persisted for this: the
  registry is rebuilt from the fleet records at daemon start.

Two concurrent `up`s of one fleet can both pass activation; the actor's
last-writer-wins is unchanged from Phase 3. The actor and reconciler are
untouched by activation (PB-5).

### 16.3 Activation state is visible, and `up` waits on it

`AgentStatus` gains `plugins: BTreeMap<String, PluginActivation>` with
`state: pending | active | rejected` and a `message`; `PluginStatus` gains
`active_agents`. `status` prints both. Activation state is a read-time
overlay, not an actor message: the `PluginRegistry` owns the `(agent,
plugin) → PluginActivation` table, and `Daemon::get`/`snapshots`/`apply`
copy it into `AgentStatus.plugins` on the way out. The actor never sees
activations and its persisted record never carries them, so the
single-writer rule holds for both records — the actor writes the fleet
record, the registry writes activations.

`balerix up`/`update` wait for fleet `Ready` **and** every activation of the
fleet `active`. A `rejected` row fails the command at once with
`crews.<c>.agents.<a>.plugins.<p>: <message>`; the `--timeout` failure lists
the rows still pending next to the agents still not ready. `--no-wait` is
unchanged. The fleet keeps running in every case, as today: a plugin that
never activates is the interceptor chain's fail-open, not a reason to stop
the agent (§11 accepted risks).

### 16.4 `stop` and `restart` are per-agent desired state in the planner

`FleetRecord` gains `stopped: BTreeSet<AgentId>`, owned by the daemon, never
part of the user's spec. The planner treats a stopped agent as: `Stop` if
observed, phase `Stopped`, never restarted, restart counter untouched. The
`stop` action adds the id and runs a pass; `restart` is add, pass, remove,
pass. `Apply` clears the set for every agent the new spec declares, so an
operator's `up` always wins. This is the one `reconcile` change of the phase;
`cargo mutants` and the model-based test cover it, and a future
`balerix stop <agent>` reuses it.

Execution is one path, `Daemon::execute_action`, for chain verdicts and for
`POST agents/{id}/actions` alike: `send_text` to the runner in
`spawn_blocking`; `stop`/`restart` as the new actor message
`SetStopped { agent, stopped }`. Both `balerix_hook_actions_total` and
`balerix_plugin_actions_total{plugin, action}` count them. The route answers
404 for an agent the plugin is not active for.

### 16.5 Smaller points

- **Health**: a daemon task polls each `Ready` plugin's `GET /v1/health`
  every 10 s; a failure sets the plugin's status message to
  `degraded: <reason>`, a success clears it; never a restart (§4.2).
- **KV**: keys are validated against `[A-Za-z0-9._/-]{1,200}` and may not
  contain a `..` segment; secret entries are sealed with the plugin name and
  key as associated data; writes are atomic through `fsutil`.
- **`fleets/watch`** (WS) moves to phase 2b: nothing in 2a consumes it, and
  2b/3 bring the WebSocket machinery anyway.
- **`dev fake-plugin`** is rewritten on the full SDK: it intercepts
  `PreToolUse` with `{ decision: block }` when `tool_input.command` starts
  with `rm -rf`, answers `Stop` with a `send_text` action, and observes
  everything into `$BALERIX_PLUGIN_SCRATCH/events.jsonl`. `fake-claude` gains
  the stdin reader of §11 so the e2e asserts both.
- **Server modules**: `plugins/client.rs` (`PluginClient`),
  `plugins/registry.rs` (order, `listen`, subscriptions, `needs`, the
  activation table; replaces `PluginHost.listen`), `plugins/chain.rs`
  (`PluginEventHandler`), `plugins/kv.rs`. The vault is shared with KV
  through an `Arc`.
- **SDK**: `Env` unchanged; `Host` async with one method per §4.1 route;
  `Plugin` is a trait with default no-op `activate`, `deactivate`, `observe`,
  `intercept`, `health`, `metrics`; `serve(plugin)` binds `127.0.0.1:0`,
  sends `hello`, serves the §4.2 router; `testing::FakeHost` is an
  in-process axum host that records calls.
- **Protocol doc**: `docs/plugin-protocol.md` states §4.1 and §4.2 as the
  versioned contract; fixtures under `docs/plugin-protocol/` are replayed
  through the SDK router and the server's client by the conformance test.

### 16.6 Refinements from the phase 2a plan (2026-09-07)

Where the phase 2a implementation plan refined this section:

- **Activation state is a read-time overlay, not an actor message.** §16.3
  above said the daemon publishes activation changes "through the fleet's
  actor"; the registry owns the `(agent, plugin) → PluginActivation` table
  and `Daemon::get`/`snapshots`/`apply` copy it into `AgentStatus.plugins` on
  the way out. The actor never sees activations, its persisted record never
  carries them, and the single-writer rule holds for both: the actor writes
  the record, the registry writes activations. `up` polls `GET
  /v1/fleets/{name}`, so it sees the overlay.
- **A plugin is identified by its token alone.** No route under
  `/v1/plugin-host/` carries the plugin's name (§4.1 specifies only the
  bearer). `Daemon::plugin_for_token` walks the `balerix` fleet's entries of
  the secret index with constant-time compares; plugins are few. `hello`
  still checks that the body's `name` matches.
- **`stopped` is keyed by the agent id's display form**, like
  `FleetStatus.agents`, so a stored `fleet.json` from phase 1 loads with an
  empty set (`#[serde(default)]`).
- **Health is polled by a daemon task every 10 s**, and a failure only sets
  the plugin's status message (`degraded: <reason>`), shown by `plugin
  list`; the registry keeps that message, the actor's record is untouched.
- **`fleets/watch` is not built** (§16.5); the `fleets` capability gates
  `GET fleets` and `GET fleets/{f}` only.
- **`balerix_plugin_proxy_requests_total` is not registered**: it has no
  producer until phase 3.
- **`FleetRecord`, `Desired` and `Keep` moved to `balerix-api`** (re-exported
  by `balerix-core`) so the SDK's `fleets()` is typed.
- **The `balerix` binary depends on `reqwest` only through the SDK**: no
  direct dependency in `crates/balerix/Cargo.toml`.
- **An unchanged pair whose row is not `Active` is re-activated by the next
  apply** (R24): `Daemon::apply` diffs the new spec against the fleet's
  `Active` rows only, so a `pending` or `rejected` pair is offered to the
  plugin again by the next `up`/`update` even when its config did not
  change — a ready plugin gets the `activate` (a rejection fails the apply
  like any other), a plugin that is not ready leaves the row pending.

## 17. Refinements from the phase 2b brainstorm (2026-09-07)

Decisions the flow plugin forced on §8.1, §7 and §13. Phase 2b is
`balerix-plugin-flow`, `mise run package-plugins`, and the SDK additions the
plugin needs; nothing in `balerix-core`, `balerix-server`, `balerix-config`
or `balerix-runtime` changes beyond adapting the server's in-process plugin
tests to the SDK's new `metrics` signature.

### 17.1 `fleets/watch` moves to phase 3

Nothing in phase 2b consumes it: `flow` needs `actions` and `kv` only.
Building it now would add axum's `ws` feature, a WebSocket client in the SDK
and a daemon-level fan-in of every fleet actor's snapshot channel, all
testable only in-process until `web` exists. Phase 3 brings the WebSocket
machinery for the proxy and attach anyway, and `web` is the first consumer,
so it gets a real end-to-end test there. §13 is corrected in place.

### 17.2 The flow config language

The §8.1 block, made exact. A rule has `event` (required, one of
`HOOK_EVENTS`) and any of:

- `match`: a map from JSON pointer into the event payload to a regex. The
  regex is **full-match**: it is compiled as `^(?:pattern)$` with a 10 KiB
  size limit. §8.1 already said "anchored at both ends"; its example
  `"^rm -rf"` contradicted that and is now `"rm -rf.*"`. A pattern like `rm`
  silently matching every command containing it is the worse surprise.
- `respond`: an object whose top-level keys replace those of the chain's
  response so far. Nested values are not merged.
- `send`: `{ text, submit }`, `submit` defaulting to `true`; becomes a
  `send_text` action.
- `action`: `stop` or `restart`; becomes the corresponding action. These
  complete the `send`/`restart`/`stop` mapping §4.3 already names.
- `goto`: a declared state.

`states.<name>.on` defaults to empty, so `done: {}` is legal. The config is
`deny_unknown_fields` and is validated **only at `activate`**, in the plugin;
`balerix-config` keeps treating plugin blocks as opaque objects (§2.2). Every
error is one line with the config path, which the daemon prefixes into
`crews.<c>.agents.<a>.plugins.flow: …`:

```
initial: no state "foo" declared
states.working.on[0].event: unknown event "Foo"
states.working.on[1].goto: no state "foo" declared
states.working.on[1].match.tool_input: not a JSON pointer (must start with "/")
states.working.on[1].match./tool_input/command: <regex error>
states.working.on[1].foo: unknown field
```

The last shape comes from `serde_path_to_error`, a new exact workspace
dependency that attaches the path to serde's message; a hand-written walker
over the raw JSON would duplicate the schema. `regex` (already in the lock
transitively) becomes a direct exact dependency.

### 17.3 The machine

The plugin holds, per active agent, the compiled config and the current
state name, in one mutex never held across an await. `intercept` for an
agent it has no entry for passes the response through unchanged.

A rule fires when the event name equals `event` and every `match` entry
matches: a string payload value is matched as is, any other present value is
matched against its JSON text, a missing pointer never matches. The first
firing rule of the current state wins; none firing means pass-through and
no transition. The effects of a firing rule, in order: `respond` is merged;
`send` and `action` are returned as verdict actions for the daemon to
execute after writing the response (§4.3); `goto` moves the state, counts a
transition (self-transitions included) and writes the new state to KV before
the verdict returns. A KV write failure is logged to stderr and the
in-memory state stands: a hook never fails on it. `deadline_ms` is ignored;
one loopback KV put is milliseconds.

**Persistence and resets.** KV key `state/<agent>` holds
`{ "state": "<name>", "config": "<sha256 of the agent's config>" }`. On
`activate` the plugin compiles the config, reads the key, and resumes the
stored state when the hash matches and the state is still declared;
otherwise it starts at `initial` and writes the key. A KV error at
`activate` rejects the activation with `kv: <error>`: that fails the
operator's `up` loudly on a daemon-side fault, which is where loud is right.
`deactivate` removes the entry and deletes the key. Hence:

- a plugin restart or a daemon restart **resumes** the state (neither sends
  `deactivate`, only a fresh `activate` after `hello`, §16.2);
- `update` with a changed flow config **resets** to `initial` through the
  hash check: the new config arrives by `activate` in place (§17.9) and
  its hash does not match the stored one. `down` and dropping the plugin
  from the agent **reset** through `deactivate`, which deletes the key. A
  *rejected* `update` changes nothing: the plugin never hears about it
  beyond the `activate` it refused.
- Two events for one agent transitioning concurrently could race their KV
  writes. Claude fires a session's hooks sequentially; noted, not guarded.

### 17.4 Metrics live in the SDK

The daemon drops a whole scrape when one family lacks the
`balerix_plugin_<name>_` prefix (§9), and a hand-formatted body is exactly
how a plugin author trips that rule. So the SDK owns the registry:

- `balerix_plugin_sdk::Metrics` wraps a `prometheus::Registry` and the
  plugin name from `Env`. `counter`, `counter_vec`, `gauge`, `gauge_vec`
  take the short family name (`transitions_total`) and register it as
  `balerix_plugin_<name>_transitions_total`. The vector types are
  re-exported so plugin crates never name `prometheus` themselves.
- `Plugin::metrics` changes from returning text to
  `fn metrics(&self) -> Option<&Metrics>`, default `None`; the router
  encodes it with `TextEncoder`. An empty registry renders an empty body.

Flow registers `state{fleet,crew,agent,state}` (a gauge set to 1 for the
current state and removed for the previous one on each transition and on
`deactivate`) and `transitions_total{fleet,crew,agent,from,to}` (a counter,
empty at each plugin start like any process counter). The three labels are
the agent id split at its two slashes.

### 17.5 `testing::Harness`

`FakeHost` covers the daemon side only. The SDK gains the plugin side:

- `Harness::start(&fake_host, plugin)` serves the plugin through the real
  §4.2 router on a loopback port and sends `hello`, so every call crosses
  the wire exactly as the daemon's would.
- `activate(agent, config) -> Result<(), String>` maps a 400 to
  `Err(message)`; `deactivate`, `observe`, `health` likewise;
  `intercept(agent, event)` with `response_so_far` defaulting to `{}` and a
  variant taking it; `event(agent, name, payload) -> HookEvent` fills the
  timestamp and session id.
- `metrics() -> String` scrapes the router; `metric(family, labels) ->
  Option<f64>` parses one sample from the text format.
- `restart(plugin)` drops the served instance and starts a new one against
  the same `FakeHost`, whose KV survives: the "restart resumes" case in one
  call.
- `FakeHost` gains `kv_json(key)` and `actions_for(agent)`.

Flow's tests and the SDK's own router tests move onto it. No daemon change,
no new dependency.

### 17.6 Crate, package and distribution

`plugins/flow` is a library (`config.rs`: types, validation,
compilation; `machine.rs`: the pure step function, where the property tests
live; `plugin.rs`: the `Plugin` impl owning the agent map, the `Host` for
KV and the `Metrics`) and a thin binary (`main.rs`: `Env::from_process`,
`Host::new`, `serve`; failures print `flow: …` to stderr and exit non-zero).
It ships `package/mise.toml` (empty `[tools]`, a `serve` task running
`./bin/balerix-plugin-flow`) and `package/balerix-plugin.yaml` (name `flow`,
version `0.1.0`, protocol 1, all nine events under `hooks.intercept`,
`needs: [actions, kv]`, `routes: false`). The binary reports
`CARGO_PKG_VERSION` in `hello`. Dependency rule: plugin crates depend on the
SDK and `balerix-api` only, never on `core`, `server`, `runtime` or
`config`.

`mise run package-plugins` (`scripts/package-plugins.sh`) builds the crate
and recreates `target/plugins/flow/` with `bin/balerix-plugin-flow` and the
two package files; `test` and `e2e` depend on it, so `check` and CI assemble
the package before nextest and the e2e uses the directory verbatim as a
directory source. It loops over the plugin crates, so `web` joins it in
phase 3.

**Distribution.** That is the *development* layout, host-only by
construction. The intended release shape puts **no binary in the tarball**:
the package's `mise.toml` pins the plugin binary as a mise tool (a `ubi` or
`github` backend entry against a release), the `serve` task runs it by name,
and the daemon's `mise install` on the package fetches the asset for the
host platform exactly as it installs `node` for an agent — one
platform-neutral tarball, one digest, and the sandbox already grants read on
the mise data dir (PB-4). Cross-compilation is the plugin repository's
release pipeline (a target matrix; Linux x86_64 and aarch64 while nono is
Landlock; static musl builds sidestep glibc mismatches). Balerix has no
release workflow yet; `package-plugins` gains a release mode when one exists.

### 17.7 Testing and docs

Layers, following §11: unit tests in the flow crate asserting every
validation error's exact text and the step function's rules; `proptest` in
`machine.rs` (the fired rule equals a naive reference matcher's; a validated
config never panics on any event; every `goto` lands in a declared state);
plugin integration through `Harness` in `plugins/flow/tests/`
(rejection with path, merged verdict, actions, transitions and metrics,
restart resumes, deactivate and changed config reset, unknown agent passes
through); the conformance fixtures replayed unchanged, `metrics.json` now
rendered from a `Metrics`; and a new e2e `flow_journey` beside the two
plugin journeys, loading `target/plugins/flow` (located from the balerix
binary's target directory; skips with a reason when `package-plugins` has
not run, fails under `BALERIX_REQUIRE_TOOLS`), asserting alice's
`PreToolUse` reply carries the block and the flow reason while bob's is
`{}`, alice's stdin received the `send` text, the daemon's `/metrics` shows
`balerix_plugin_flow_state` at 1 for `review` and one `working`→`review`
transition, `plugins/flow/kv/state/e2e/c/alice` exists, `update` with a bad
regex fails with the full config-path line and leaves the fleet running, and
`down` removes the key. `mise run mutants` and `verify-claude` are unchanged.

Docs: `docs/plugin-protocol.md` gains a packaging section (both distribution
shapes) and a paragraph on `metrics` via the SDK; `ARCHITECTURE.md` the flow
plugin and the plugin-crate layer; `AGENTS.md` the `package-plugins` task and
the gotchas as found (the KV reset rule and full-match anchoring first);
`README.md` status "phase 2b done, phase 3 next" and a flow block;
`examples/payments.yaml` a commented flow block for alice. The threat model
is unchanged: flow adds no trust boundary.

### 17.8 Refinements from the phase 2b plan (2026-09-07)

Where the phase 2b implementation plan refined this section:

- **`Harness::start` takes `&Env`, not the `FakeHost`**: the `Env` a
  `FakeHost::env(name, scratch)` returns is what `Host::new` needs, and
  the flow tests hold the same `Env` to build the plugin's own `Host`.
- **`Harness::intercept` takes the event only**; the agent is in the
  `HookEvent` that `event(agent, name, payload)` builds. `intercept_with`
  adds `response_so_far` and `deadline_ms`.
- **`Metrics` constructors** are `int_counter`, `int_counter_vec`,
  `int_gauge`, `int_gauge_vec` and `render()`; the vector types are
  re-exported from `balerix_plugin_sdk::metrics`; registration errors are
  `SdkError::Metrics`.
- **`metrics.json` was re-recorded**: the encoder writes a `# HELP` line
  the hand-written fixture lacked; `families_ok` accepted it already.
- **Config errors are `ConfigError { path, message }`** (thiserror,
  displayed `<path>: <message>`); `activate` stringifies it. Regex errors
  keep only their last line without `error: `; unknown fields read
  `unknown field `foo``.
- **§11.1 row, verified 2026-09-07:** a binary inside a directory-source
  package is executable under the package's read grant — yes, no sandbox
  block needed (the manifest carries no `sandbox:` block; the plugin ran
  to `hello` and `plugin list` showed `ready` with only the base package
  read grant).
- **`test` and `e2e` depend on `package-plugins`**, so `mise run check`
  and CI assemble `target/plugins/flow/` before nextest.

### 17.9 A rejected `update` leaves plugin state alone (2026-09-07)

Phase 2b's e2e showed a gap between §16.2 and §17.3: the daemon sent
`deactivate` to every changed pair before offering the new config, flow's
`deactivate` deletes `state/<agent>`, and when the new config was rejected
the restored old config found no state and started at `initial`. An
operator's typo in a flow rule reset every agent whose block it touched.

The order is now: a changed pair gets its new config by `activate` alone,
and **an `activate` for an agent the plugin already holds replaces that
agent's config in place**. `deactivate` is sent only for `down` and for
pairs the new spec drops. A rejection therefore touches nothing the
plugin holds; on acceptance, flow resets through the hash check as
before. Every `hello` re-send already arrived as an in-place `activate`,
so plugins on the SDK were written for this; the protocol doc and the
`Plugin::activate` contract now say it, and a plugin keeping per-agent
resources releases the old ones itself.

One edge remains: when an `update` changes several pairs and a later one
is rejected, the pairs already accepted are put back by re-activating
their old config, and flow resets those through the hash check. The
blast radius is the pairs whose config actually changed in the rejected
spec, not every changed pair as before; a `validate` route that would
close it entirely is not worth a protocol addition yet.

## 18. Refinements from the phase 3 brainstorm (2026-09-07)

Decisions phases 1–2b's code forced on §4.1, §6, §8.2 and §11, plus two
gaps the earlier text left open: a browser cannot present the admin bearer
§6 requires, and nothing authenticated the daemon to a plugin. Phase 3 is
the proxy with browser sessions, `AgentRunner::attach` on tmux, the
`attach` and `fleets/watch` streams, the daemon bearer on every plugin
call, the SDK's stream clients and `routes`, and `balerix-plugin-web`.

### 18.1 Transport: generic upgrade passthrough

The proxy is a plain reverse proxy on a `hyper-util` legacy client (already
in the lock through `axum` and `reqwest`; promoted to a direct exact
dependency together with `http-body-util`). It forwards any method and
streams response bodies; when the plugin answers 101 the daemon takes
`hyper::upgrade::on` on both sides and runs `copy_bidirectional` on a
spawned task until either end closes. The proxy never parses a WebSocket
frame, so subprotocols, extensions and any other upgrade pass untouched.
The daemon terminates WebSockets only where it owns the stream — `attach`
and `fleets/watch` — through `axum`'s `ws` feature. The SDK's stream
clients and the e2e use `tokio-tungstenite`, a new exact workspace
dependency with no TLS feature (P3-1 holds). The frame-copying proxy of
§11.1 remains the fallback if the legacy client's upgrade handoff fails at
implementation time.

### 18.2 The proxy and browser sessions

**Proxy.** `ANY /v1/plugins/{name}/{*rest}` sits outside the admin
router's bearer middleware and authenticates itself: the admin bearer, or a
session cookie (below). It resolves the plugin in the registry: 404
`plugin "x" has no routes` when the manifest says `routes: false` or the
name is unknown, 503 `plugin "x" is not ready` without a `listen`. It
forwards to `http://<listen>/v1/routes/<rest>?<query>` (an empty `rest` or
a trailing slash maps to `/v1/routes/`) with the method, the body (1 MiB
cap, 413 beyond), and the headers minus hop-by-hop ones, `Authorization`
and `Cookie`; it adds `X-Balerix-Forwarded-Prefix: /v1/plugins/<name>` and
`Authorization: Bearer <that plugin's token>` (§18.3).
`balerix_plugin_proxy_requests_total{plugin,status}` is registered now,
labelled with the response status.

`GET /v1/plugins/<name>` without the slash — a hand-typed or truncated
URL — is a 308 to `/v1/plugins/<name>/`, unauthenticated, revealing
nothing; DELETE there remains the admin purge route.

**Sessions.** A browser cannot send the admin bearer on a navigation, so
the daemon gains two routes. `POST /v1/sessions` (admin bearer) answers
`{ login_url }` carrying a one-time code: 32 random bytes hex, valid for
60 s, single use. `GET /v1/login/{code}?to=<path>` exchanges the code for
a session — another 32 random bytes hex, in memory only, expiring after
12 h — sets `balerix_session=<id>; HttpOnly; SameSite=Strict;
Path=/v1/plugins` and answers 303 to `to`, which must be a path under
`/v1/plugins/` (400 otherwise; a bare `/v1/plugins/<name>` gains its
trailing slash). A bad, used or expired code is a plain-text
404 that names which (a spent or lapsed code is remembered for 10 min, so
the human who pasted a stale URL learns whether something fetched it first
or the 60 s ran out); every attempt is logged with its `Host`,
`Sec-Fetch-Site` and `User-Agent`, never the code. Sessions die with the
daemon; nothing is persisted; there is no logout route. The admin token
never enters the browser.

A cookie-authenticated proxy request must also be same-origin: when
`Sec-Fetch-Site` is present it must be `same-origin` or `none`; otherwise
`Origin` must be absent, or equal the daemon's own origin — exactly the
`http://127.0.0.1:<port>` of the printed login URL (`localhost` is a
different origin to a browser) — or have as its authority the request's
own `Host` (a browser cannot forge `Origin` and fills `Host` from the URL
the page connected to, so the two agree only for a page that host served;
the scheme is ignored because a TLS-terminating reverse proxy rewrites
it). The last form is what a WebSocket handshake through a reverse proxy
carries: `Sec-Fetch-Site` absent, `Origin` the proxy's hostname. Anything
else is 403. The cookie is accepted on the proxy alone, never on another
route.

**CLI.** `balerix plugin open <name>` calls `POST /v1/sessions` with `to`
set to `/v1/plugins/<name>/` and prints the login URL. It does not launch
a browser.

### 18.3 The daemon authenticates to the plugin

Until now nothing authenticated the daemon → plugin direction: any local
process could call a plugin's `/v1/activate` or `/v1/intercept`, and with
`routes` it could have opened a terminal on the plugin's port without the
admin token. Every daemon → plugin request — `activate`, `deactivate`,
`events`, `intercept`, `health`, `metrics` and the proxied `/v1/routes/*`
— now carries `Authorization: Bearer <that plugin's BALERIX_PLUGIN_TOKEN>`.
The SDK router checks it with a constant-time compare on every route,
`/v1/routes/*` included, and answers 401 `{ "error": "bad daemon token" }`
otherwise; the token comes from the `Env` the plugin already holds. The
protocol doc states that a plugin must check it and every fixture under
`docs/plugin-protocol/` gains the header. Still protocol 1: the secret and
its delivery are unchanged, only its use widens.

### 18.4 `attach` and `fleets/watch`

**The port.** `AgentRunner` gains
`fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError>`.
`PtyStream` is sync like every other port and is split for the async
bridge:

```rust
pub trait PtyStream: Send {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>>;
    fn writer(&self) -> io::Result<Box<dyn Write + Send>>;
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
}
```

Dropping the stream ends the session. `FakeRunner::attach` returns an
in-memory echo (bytes written come back on the reader, resizes are
recorded) and `FakeRunner::close_attach(agent)` ends the reader so tests
can play "the window died".

**tmux.** A direct `attach-session` on the crew session would share the
current window with every other client, an operator's terminal included,
so each attach gets a throwaway grouped session: `new-session -d -t =f/c
-s balerix-attach-<8 hex>`, then `set-option destroy-unattached on` and
`status off` on it, `select-window` to the agent's window, and
`attach-session -t =<that session>` spawned through `portable-pty` (new
exact dependency of `balerix-runtime`; the workspace forbids the `unsafe`
a hand-rolled `pre_exec` would need) at 80×24 until the first resize.
`observe` already ignores session names without the `<fleet>/` prefix.
`stop_crew` changes: it lists sessions as `#{session_name}\t#{session_group}`
and kills every session in the crew's group, because `kill-session` on the
crew alone leaves the windows alive in a grouped session. Dropping the
stream kills the tmux client process and waits for it; if the daemon dies
the PTY closes, the client detaches and `destroy-unattached` cleans up.
`window-size latest` (tmux's default since 3.1) makes the newest resize
win when two viewers watch one agent.

**Daemon route.** `GET /v1/plugin-host/agents/{id}/attach` upgrades
through `axum`'s `ws`; capability `attach`; 404 unless the plugin is
active for that agent, as for actions. The handler calls `runner.attach`
in `spawn_blocking`, then runs one blocking reader task feeding binary
frames and one writer task taking binary frames to the writer and text
frames `{ "resize": { "cols", "rows" } }` to `resize`. Any other text
frame closes with 1003; EOF on the reader closes with 1000; an error with
1011. Attach failures before the upgrade are 500 with the runner's error.

**`fleets/watch`.** `GET /v1/plugin-host/fleets/watch`, capability
`fleets`. Every frame is the complete `GET fleets` body — the user fleets
with their activation overlay — so a consumer replaces its state and never
diffs or handles removals. The first frame is sent on connect. The daemon
gains one `watch::Sender<u64>` tick: a forwarder task per fleet actor
bumps it on each published snapshot, and every daemon method that writes a
registry row bumps it too. The handler waits on the tick, recomputes, and
sends only if the list differs from what it last sent. The daemon pings
every 30 s so dead clients are reaped.

**SDK.** `Host::attach(agent) -> Attach` with `read().await ->
Option<Vec<u8>>`, `write(bytes)`, `resize(cols, rows)` and `close()`.
`Host::watch_fleets() -> FleetWatch` with `next().await ->
Option<Vec<FleetRecord>>`, reconnecting with a 1–10 s backoff and yielding
the full list after each reconnect. `Plugin` gains
`fn routes(&self) -> Option<axum::Router>`, default `None`, which `router`
nests under `/v1/routes` behind the bearer check. `FakeHost` serves both
streams: attach as an echo, watch from a `set_fleets` the test calls.

### 18.5 `balerix-plugin-web`, without `ttyd`

§8.2 is superseded. The plugin serves one page with a vendored xterm.js and
bridges the daemon's attach stream itself: one WebSocket hop per browser
tab, no subprocess per viewer, no `SIGWINCH` bridge, no idle timer, an
empty `[tools]` like flow, and the unverified "`ttyd` behind two proxies"
row of §11.1 is closed as *not used*.

**Crate and package.** `plugins/web`, a library plus thin
binary like flow: `config.rs` (the per-agent block), `state.rs` (the
watch-fed cache), `routes.rs` (the router and the bridge), `plugin.rs`
(the `Plugin` impl). `package/mise.toml` has an empty `[tools]` and a
`serve` task; the manifest is name `web`, protocol 1, no hook
subscriptions, `needs: [fleets, attach]`, `routes: true`.
`scripts/package-plugins.sh` loops over `flow web`.

**Per-agent config.** `{ enabled: bool }`, default `true`,
`deny_unknown_fields`, validated at `activate` in flow's path style. An
agent is listed when it is active for web and enabled; `deactivate` drops
it. The cache holds the enabled set and the latest `Vec<FleetRecord>`
from `watch_fleets`, started after `hello`. The plugin never calls `GET
fleets`, so an index that is right at all proves the watch path (§17.1).

**Routes** under `/v1/routes/`, every link built from
`X-Balerix-Forwarded-Prefix`:

- `GET /` — the index: one row per enabled agent with phase and message,
  refreshed every 2 s from `agents.json`. Names are validated identifiers;
  everything else is HTML-escaped.
- `GET /agents.json` — `[{ id, phase, message }]` from the cache.
- `GET /agents/{fleet}/{crew}/{agent}` — the terminal page: xterm.js with
  the fit addon, opens the bridge, sends `{ resize }` on fit and on window
  resize, prints a "disconnected" line when the socket closes. 404 for an
  agent that is not enabled.
- `GET /agents/{fleet}/{crew}/{agent}/ws` — the bridge: `Host::attach`,
  frames copied both ways (binary as bytes, text resizes as resizes); one
  daemon attach per tab, closed when either side closes.
- `GET /assets/xterm.js`, `xterm.css`, `addon-fit.js` — `include_bytes!`,
  with cache headers.

**Vendored assets.** `@xterm/xterm` 6.0.0 and `@xterm/addon-fit` 0.11.0,
the minified builds only, under `plugins/web/assets/`, with
`VENDOR.md` recording the npm tarball URL and sha256 of each file and
`scripts/vendor-xterm.sh` re-fetching and verifying them. About 400 KB in
the repo; `cargo audit` and `deny.toml` do not cover it, so the recorded
digest is the supply-chain control.

**Metrics** through the SDK: `balerix_plugin_web_terminals_open` (gauge)
and `balerix_plugin_web_terminals_total` (counter).

### 18.6 Security, docs, testing, scope

**Threat model.** A new boundary, *browser ↔ daemon*: cookie session over
loopback, `HttpOnly` + `SameSite=Strict`, same-origin check on every
cookie request, single-use 60 s login code so the admin token never enters
the browser, sessions in memory only. The *plugin ↔ daemon* row gains "the
daemon authenticates with the plugin's own token on every call". New
accepted risks: a plugin with `attach` has a keyboard into every agent it
is active for, and anyone holding a browser session can type into every
agent web lists. New mitigation rows: proxy header filtering, the grouped
tmux session.

**Docs.** `docs/plugin-protocol.md`: the daemon bearer, the `attach` and
`fleets/watch` frame formats, `routes` and the forwarded prefix, fixtures
`attach-resize.json`, `fleets-watch.json`, `routes.json`.
`ARCHITECTURE.md`: a phase 3 paragraph and four non-obvious decisions
(passthrough proxy, grouped attach sessions, full-list watch frames, the
daemon bearer on plugin calls). `AGENTS.md`: gotchas as found, `stop_crew`
and session groups first. `README.md`: "Spec B complete", with an upgrade
note that plugins in other languages must now check the bearer.
`examples/payments.yaml` enables web in `defaults`. `THREAT-MODEL.md` as
above.

**Testing**, by layer:

| Layer | What |
|---|---|
| unit | proxy header filtering and URL building; session code expiry and single use; the same-origin table; bridge framing; tick coalescing; tmux session-group parsing; web config errors; index escaping |
| property | a header set the proxy forwards never contains a hop-by-hop, `Authorization` or `Cookie` header; a watch consumer that replaces state on every frame ends equal to `GET fleets` for any sequence of changes |
| server integration (fakes) | cookie login round trip and 403 cross-origin; 401 without the bearer on the SDK router; a plain route and a WebSocket echo proxied to a fake plugin with `routes`; attach echo and resize through the route; watch frames on apply, activation change and purge; 404 attach for an inactive agent |
| runtime (`test-it`) | a real tmux attach through the PTY sees output; typed bytes reach the pane; the grouped session disappears on drop; `stop_crew` leaves no session in the group |
| plugin (`Harness`) | index from watch frames; bridge echo; `enabled: false` hidden |
| e2e `web_journey` | `plugins.yaml` with the flow and web directory sources; `up` with web enabled; `plugin open web`, then a `ureq` GET of the login URL that yields the cookie; the index lists alice; a `tokio-tungstenite` client through the proxy receives bytes containing `fake-claude` from the pane, sends `hello\n`, and `fake-claude.stdin` shows it; `down`, and `agents.json` drops alice with no `GET fleets` ever hitting the daemon |
| by hand | `verify-claude` prints the login URL for a terminal check in a browser — the §14 "live Claude terminal" verdict |

**§11.1 verdicts** are recorded at implementation for the `hyper-util`
upgrade handoff and the `portable-pty` attach; the `ttyd` row is closed
as "not used, xterm.js chosen".

**Out of scope, stated.** No `balerix attach` CLI, no persistent sessions,
no logout route, no browser launch, no TLS, no live-push index. A plugin's
`/v1/routes/*` is the only browser surface.

### 18.7 Refinements from the phase 3 plan (2026-09-07)

Where the phase 3 implementation plan refined this section:

- **`PluginAddr { listen, token }`** replaces the bare listen address in
  every daemon → plugin call; the registry learns the token at `hello`
  (`set_listen(name, listen, token)`), `ready_addr` hands both out, and
  `Debug` redacts the token (§18.3).
- **The root of a mount forwards to `/v1/routes`, no trailing slash**:
  axum's `nest` answers the nested `/` at `/v1/routes` and 404s
  `/v1/routes/`; `/v1/plugins/<name>` without a slash stays the purge
  route for DELETE, and a GET there is a 308 to `/v1/plugins/<name>/`
  (§18.2).
- **`destroy-unattached` is set after the client is attached**, in one
  command sequence inside the PTY (`new-session -t =<crew> -s
  balerix-attach-<hex> ; select-window -t =<agent> ; set-option
  destroy-unattached on ; set-option status off`): tmux 3.7c destroys a
  detached session the moment the option lands (§18.4).
- **`FleetWatch::next` returns `Vec<FleetRecord>` and never ends**;
  drop the watch to stop (§18.4). **`Host` is `Clone`**; **`Plugin::routes`
  defaults to `None`**.
- **The login `to` path is `[A-Za-z0-9/._~-]` under `/v1/plugins/`**, so
  the login URL carries it unencoded (§18.2).
- **The proxy's status counter labels an unparseable plugin name
  `unknown`** so guesses cannot grow the series.
- **Eighteen fixtures**: `activate-bad-token` (Task 1), `routes`,
  `attach-resize` and `fleets-watch` (Task 6); the last two carry
  `transport: "websocket"` and one `frame`, and are asserted by the SDK's
  stream test rather than replayed over HTTP.
- **`StubScript.expect_token`** makes the server's stub plugin refuse the
  wrong bearer, so `protocol_it` proves the daemon sends it.
- **The web plugin's index polls every 2 s** and renders polled rows with
  `textContent`; the terminal page sends a resize on open, on fit and on
  window resize (§18.5).
- **`balerix plugin open` prints only.** No browser is launched.
- **The §18.6 watch property test is not written**: with every frame the
  whole list, "a consumer that replaces its state ends equal to `GET
  fleets`" holds by construction; `streams_it` asserts the frames the
  daemon sends and `state.rs` the cache that replaces on each.
- **The login `to` path rejects any `.` or `..` segment** (a browser
  normalizes them out of the `Location` path per RFC 3986 and could walk
  the redirect back out of the mount).
- **The proxy forwards `<rest>` from the raw request path, encoding
  preserved** (`%20`, `%2F`, `%3F` cross verbatim), and answers 400 to any
  `.`/`..` segment including `%2e` spellings; axum's decoded path capture
  is used for routing only.
- **`balerix_plugin_proxy_requests_total{plugin}` is labelled with the
  registry-resolved name only for an authenticated request to an
  installed `routes: true` plugin**; every other request (unauthenticated,
  unknown, unparseable, `routes: false`) is counted under
  `plugin="unknown"`, so an unauthenticated caller cannot mint labels.
- **`apply`'s rejection path bumps the watch tick after `restore_pairs`
  writes the rolled-back rows**, so a rejected activation is a watch
  frame.
- **The attach bridge performs the PTY write on `spawn_blocking`** (a PTY
  master write blocks when the tmux client stops draining) and drops the
  stream on a blocking thread; the resize ioctl stays inline.
- **`FleetWatch` backs off on every reconnect** — a lost socket as well as
  a refused handshake — and resets the backoff only after a list frame is
  received.
- **The terminal page never sends a resize with a zero dimension**
  (xterm.js's fit addon reports 0 when its container is hidden, and the
  daemon closes 1003 on it); the bridge filters non-resize text frames
  itself.
- **`balerix_server::sessions` and the SDK each carry their own
  `constant_time_eq`/`bearer`**: the SDK depends on `balerix-api` only, so
  the copy is forced by the dependency direction.
- **Every mount shares one browser origin, as an accepted risk**: the
  session cookie is scoped `Path=/v1/plugins/`, so a page under one
  plugin's mount can reach every other plugin's routes same-origin with
  the operator's session — bypassing `needs` for anything a browser can
  reach — and that is recorded in `docs/THREAT-MODEL.md` rather than
  mitigated (one loopback origin; no default CSP, since the web plugin's
  pages use inline script).
- **`http-body-util` was not added after all**: §18.1 lists it beside
  `hyper`/`hyper-util`, but the proxy never names it (axum's `Body`
  carries the request and response bodies through), so it is not a
  workspace dependency.
- **`attach` closes 1000 on a PTY read error, not 1011** (§18.4 assigns
  1011 to the runner failing): a Linux PTY master answers `EIO`, never
  EOF, once the last client of the slave is gone, so the error *is* the
  ordinary end of a session and 1011 would be the common case.
- **The attached stream travels to the bridge in a `StreamGuard`**
  (`attach.rs`): the runner attaches before `ws.on_upgrade`, so an upgrade
  that never completes leaves axum dropping the closure — and a
  `TmuxAttach` drop kills its client, waits for it and runs `tmux
  kill-session` — on a tokio worker. The guard offloads that drop to
  `spawn_blocking`; the bridge takes the stream out and keeps offloading
  its own.
- **`sync_plugins` bumps the watch tick**: `replace_plugins` drops the
  activation rows of every removed plugin with no actor snapshot behind
  it, so `plugin remove`/`plugin sync` would otherwise leave
  `fleets/watch` consumers serving rows that are gone.

### 18.8 Refinements after the phase 3 reviews (2026-09-08)

Contract changes the phase 3 review follow-ups (issues #25–#31) made;
everything else in them is internal.

- **A zero-sized resize is ignored, not refused** (§18.4 said any text
  frame that is not a well-formed resize closes 1003, and "well-formed"
  meant both dimensions at least 1). xterm.js's fit addon reports zeroes
  for a hidden container; the in-tree page guards against sending them,
  but a third-party client that does not would lose its terminal over a
  cosmetic frame. `ResizeFrame::parse` returns `TextFrame::{Resize,
  ZeroSized, Malformed}`; the daemon, the SDK's `FakeHost` and the web
  plugin treat `ZeroSized` as a no-op and `Malformed` as before.
- **`watch` is a reserved fleet name**, beside `balerix`: `GET
  /v1/plugin-host/fleets/watch` is the watch route, so a fleet of that
  name could never be fetched by a plugin. `balerix_core::
  reserved_fleet_reason` carries both with their reasons; `is_reserved_fleet`
  keeps meaning the plugin fleet alone (the daemon serves its record and
  filters its agents by it). A stored fleet already named `watch` is
  logged and ignored at the next daemon start, as one named `balerix` is.
- **The 1011 close on a PTY write failure says so** ("the terminal's
  writer failed"); it used to reuse the 1000 reason.
- **The web plugin's assets live under a digest segment**,
  `/assets/<sha256 of the three files, 12 hex>/xterm.js` (§18.5 had
  `/assets/xterm.js`). They are served `immutable` for a year, and an
  unversioned URL would have handed any browser that visited before a
  stale bundle after a vendor bump; a new bundle is now a new URL. The
  pages link the digest path; the old path is a 404.
- **The web bridge forwards the daemon's close to the browser** through
  the SDK's new `AttachRead::close_reason()`: the page's `[disconnected:
  <reason>]` line shows 1000/1003/1011 and the daemon's text instead of a
  fixed "the terminal closed" (reserved codes 1005/1006/1015 cross as
  1011).
