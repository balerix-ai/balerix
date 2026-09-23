# Balerix plugin host protocol

The wire contract between the daemon and a plugin (plugins spec §4, §11):
what a plugin in any language must send and answer. The
`balerix-plugin-sdk` crate implements both halves for Rust; this document
and the fixtures under `docs/plugin-protocol/` are the contract for every
other language, and are replayed through the SDK and the daemon's client by
the conformance tests so the three cannot drift apart (§6).

## 1. Scope and versioning

Host protocol major **1**. A plugin's `hello` (§3) sends `protocol`; the
daemon refuses any value it does not speak. Both directions are JSON over
plain HTTP/1.1 on loopback — no TLS, no proxies. Every request and response
body is a JSON object, except the routes stated to carry a raw byte body
(`GET`/`PUT /v1/plugin-host/kv/{key}`, `GET /v1/health`, `GET /v1/metrics`).
Every non-2xx response is `{ "error": "<message>" }`
(`hello-bad-token.json`, `fleet-missing.json`, `activate-rejected.json`,
`fleet-put-rejected.json`).
Body size caps differ by direction and route: plugin → daemon bodies are
capped at 1 MiB, except `hello`, capped at 64 KiB
(`balerix-server/src/api.rs`'s `plugins`/`plugin_host` router layers);
daemon → plugin request bodies are capped at 1 MiB by the SDK's `router`
(`balerix-plugin-sdk/src/plugin.rs`); a plugin's response body over 1 MiB is
rejected by the daemon before it is parsed and counted as a `body` failure
— for `intercept`, the interceptor chain's fail-open (§4) —
(`balerix-server/src/plugins/client.rs`).

## 2. Environment

The daemon delivers four variables to a plugin's process through the nono
profile environment (plugins spec §5.1):

| Variable | Meaning |
|---|---|
| `BALERIX_API_URL` | The daemon's base URL, `http://127.0.0.1:<port>`, no trailing slash. Every plugin → daemon route in §3 is relative to it. |
| `BALERIX_PLUGIN_NAME` | This plugin's name, as declared in `balerix-plugin.yaml` and `plugins.yaml`. |
| `BALERIX_PLUGIN_TOKEN` | The bearer for every plugin → daemon call: `Authorization: Bearer <token>`. A wrong or missing token is 401 `{ "error": "unknown plugin or bad token" }` on every route under `/v1/plugin-host/`, `hello` included (`hello-bad-token.json`). It is also the bearer the daemon presents on every daemon → plugin call (§4); a plugin must check it and answer 401 `{ "error": "bad daemon token" }` to anything else (`activate-bad-token.json`), since its listener is a loopback port any local process can reach. |
| `BALERIX_PLUGIN_SCRATCH` | A read-write scratch directory; no API call needed. |

## 3. Plugin → daemon

Base `BALERIX_API_URL`, path prefix `/v1/plugin-host/`, bearer
`BALERIX_PLUGIN_TOKEN` on every call. Each route beyond `hello` is gated by
a capability the manifest's `needs` must declare (`fleets`, `actions`,
`attach`, `kv`, `workspace`, `manage`); a call outside what is declared is rejected
before the route runs, 403 `{ "error": "capability \"<cap>\" not declared
in balerix-plugin.yaml" }`. No fixture carries this status: `FakeHost` (the
SDK's test double, §6) does not gate capabilities, so it cannot be
exercised through the SDK conformance test; it is asserted against the real
daemon by `crates/balerix-server/tests/events_it.rs` (§6).

| Route | Capability | Request | Response | Status | Fixture |
|---|---|---|---|---|---|
| `POST hello` | always | `{ name, version, protocol, listen }` | `{ config }` | 200 | `hello.json` |
| `POST hello`, bad/missing token | always | same | `{ error }` | 401 | `hello-bad-token.json` |
| `GET fleets` | `fleets` | — | `[FleetRecord]` | 200 | `fleets.json` |
| `GET fleets/{name}` | `fleets` | — | `FleetRecord` | 200 | (shape as in `fleets.json`'s `response[0]`) |
| `GET fleets/{name}`, unknown name | `fleets` | — | `{ error }` | 404 | `fleet-missing.json` |
| `GET fleets/watch` (WebSocket) | `fleets` | — | one text frame per change, each the complete `GET fleets` body | 101 | `fleets-watch.json` (Task 6) |
| `PUT fleets/{name}` | `manage` | `{ file }` — the fleet file's structure as JSON (`apiVersion`, `kind`, `name`?, `defaults`, `crews`) | `FleetRecord`, `owner` set to this plugin | 200 | `fleet-put.json` |
| `PUT fleets/{name}`, file does not resolve | `manage` | same | `{ error }`, config path first | 400 | `fleet-put-rejected.json` |
| `PUT`/`DELETE fleets/{name}`, owned by another plugin or by the CLI | `manage` | — | `{ "error": "fleet <name> is managed by plugin <p>" }` or `{ "error": "fleet <name> is not managed by a plugin" }` | 409 | (asserted by `crates/balerix-server/tests/manage_it.rs`, §6) |
| `DELETE fleets/{name}?keep_repos=&keep_sessions=&purge=&force=` | `manage` | — | `FleetRecord` | 200 | `fleet-delete.json` |
| `GET agents/{fleet}/{crew}/{agent}/attach` (WebSocket) | `attach` | — | binary frames are terminal bytes both ways; the one text frame is `{ "resize": { "cols", "rows" } }` | 101 | `attach-resize.json` (Task 6) |
| `GET agents/…/attach`, agent not active for this plugin | `attach` | — | `{ "error": "plugin is not active for agent <id>" }` | 404 | (as for actions) |
| `GET agents/{fleet}/{crew}/{agent}/workspace/diff` | `workspace` | — | `WorkspaceDiff` | 200 | `workspace-diff.json` |
| `GET agents/…/workspace/file?path=<rel>` | `workspace` | — | raw bytes | 200 | `workspace-file.json` |
| `GET agents/…/workspace/tree?path=<rel>` | `workspace` | — | `{ path, entries }` | 200 | `workspace-tree.json` |
| `GET agents/…/workspace/version` | `workspace` | — | `{ head, fingerprint }` | 200 | `workspace-version.json` |
| `GET agents/…/workspace/*`, agent not active for this plugin | `workspace` | — | `{ "error": "plugin is not active for agent <id>" }` | 404 | (as for actions) |
| `GET agents/…/workspace/*`, no worktree yet | `workspace` | — | `{ "error": "no workspace for agent <id>" }` | 404 | (asserted by `workspace_it.rs`, §6) |
| `POST agents/{fleet}/{crew}/{agent}/actions` | `actions` | one of the three action shapes below | `{}` | 200 | `action.json` |
| `POST agents/…/actions`, agent not active for this plugin | `actions` | — | `{ "error": "plugin is not active for agent <id>" }` | 404 | (same status as `fleet-missing.json`; asserted by `events_it.rs`, §6) |
| `GET kv?prefix=` | `kv` | — | `{ keys }` | 200 | `kv-list.json` |
| `GET kv/{key}` | `kv` | — | raw bytes | 200 | `kv-get.json` |
| `GET kv/{key}`, unknown key | `kv` | — | `{ "error": "no such key" }` | 404 | (same status as `fleet-missing.json`) |
| `PUT kv/{key}?secret=<bool>` | `kv` | raw bytes | `{}` | 200 | `kv-put.json` |
| `DELETE kv/{key}` | `kv` | — | `{}` | 200 | (same success shape as `PUT`) |

A `FleetRecord` is `{ spec: { name, crews }, owner?, generation, desired: { state },
stopped, status: { generation, observed_generation, phase, agents } }`
(`fleets.json`); secrets are excluded. `fleets/{name}` answers the same
shape for one fleet. `owner` is present when a plugin manages the fleet
(Spec L §5) and is the plugin's name.

**Actions** (`POST agents/{fleet}/{crew}/{agent}/actions`) are one of:

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

**KV**: keys match `[A-Za-z0-9._/-]{1,200}`, with no empty segment and no
bare `.` or `..` segment. `PUT` and
`GET` bodies are raw bytes, content-type `application/octet-stream`;
`?secret=true` on `PUT` stores the value through the daemon's vault.
`?prefix=` on the list route filters returned `keys` by prefix
(`kv-list.json`).

**Streams** (plugins spec §18.4). `fleets/watch` sends the current list
as its first frame and the whole list again after every change (fleet
records and activation rows alike), pinging every 30 s; a consumer
replaces its state on each frame and reconnects when the socket drops.
`attach` opens a terminal on the agent's window: binary frames carry
bytes both ways, a text frame must be a resize (`{ "resize": { "cols":
120, "rows": 40 } }`) or the daemon closes with 1003; a resize with a
zero dimension (what a hidden terminal's fit reports) is ignored, not
refused. The daemon closes with 1000 when the window ends and 1011 on a
runner failure. Both take the plugin's bearer on the handshake and answer
the usual 401/403 before upgrading. Because `fleets/watch` sits beside
`fleets/{name}`, `watch` is not a fleet name: `up` refuses it.

**Workspace** (Spec C §2.2). `diff` is the agent's worktree against the
merge-base with `origin/<crew ref>`: `{ base_ref, merge_base, head, files,
truncated }`, each file `{ path, old_path, status, uncommitted, binary,
patch, truncated }` with `status` one of `added`, `modified`, `deleted`,
`renamed`, `copied`, `typechange`; untracked files are `added` and
`uncommitted`; a patch is unified with three lines of context, empty for a
binary, cut at 256 KiB (`truncated`), and the list stops at 500 files
(top-level `truncated`). `file` answers the bytes of one regular file, 404
`no such path`, 400 `not a regular file` (a symlink is never followed),
413 `file larger than 1 MiB`. `tree` lists one directory (`{ name, kind:
file|dir|symlink|other, size }`, sorted, `.git` never listed, never
recursive); the empty path is the root. `path` is relative, `/`-separated,
at most 4096 bytes, with no empty, `.` or `..` segment, no `\`, no NUL and
no `.git` segment, else 400 `workspace: invalid path: <reason>`. Two 404
texts: `no workspace for agent <id>` (no worktree) and `no such path`.
`version` (Spec D) answers `{ head, fingerprint }`: `head` is the
worktree's `HEAD`, `fingerprint` 64 hex chars over `HEAD`, the merge-base
and the size and mtime of every changed or untracked path — equal values
mean `diff` would answer the same; compare, never parse.

**Managed fleets** (Spec L). `PUT fleets/{name}` takes the unresolved
fleet file — exactly what `balerix up -f` reads, as JSON — and does what
`up` does on the daemon: folds the host's `claude.settings` in, resolves
every agent, reads the operator's Claude credentials and gh token from
the daemon's host home, and applies. The plugin never sees credentials.
A `name` in the file must equal the path's (400 otherwise); `balerix`
and `watch` are refused. The first `PUT` of a name creates the record
with this plugin as its `owner`; every later `PUT` is a full replace and
must come from the same plugin; the CLI's `up`/`update`/`down` refuse an
owned fleet (409; `balerix down --force` is the operator's override).
The call returns once the spec is applied, not when the fleet is ready:
watch `fleets/watch`. `DELETE` takes the admin `DELETE`'s flags; `force`
is ignored here. `plugin remove` downs every fleet the plugin owned. An
agent's `branch` setting names an existing remote branch to work on
(`crews.<c>.agents.<a>.branch`, validated as `git check-ref-format
--branch` would); the diff base stays the crew's `ref`.

## 4. Daemon → plugin

At the `listen` address the plugin's `hello` gave (§3), plain HTTP, 5 s
timeout unless stated otherwise.

Every request carries `Authorization: Bearer <BALERIX_PLUGIN_TOKEN>` — the
plugin's own token (§2). The fixtures' `headers` object is what the daemon
sends; `activate-bad-token.json` records the refusal a plugin must answer.

| Route | Request | Response | Status | Fixture |
|---|---|---|---|---|
| `POST /v1/activate` | `{ agent, config }` | `{}` | 200 | `activate.json` |
| `POST /v1/activate`, rejected | `{ agent, config }` | `{ error }` | 400 | `activate-rejected.json` |
| `POST /v1/activate`, wrong or missing bearer | same | `{ error }` | 401 | `activate-bad-token.json` |
| `POST /v1/deactivate` | `{ agent }` | `{}` | 200 | (same success shape as `activate.json`) |
| `POST /v1/events` | `{ events: [HookEvent] }` | `{}` | 200 | `events.json` |
| `POST /v1/intercept` | `{ event, response_so_far, deadline_ms }` | `{ response, actions }` | 200 | `intercept.json` |
| `GET /v1/health` | — | raw bytes | 200 | `health.json` |
| `GET /v1/metrics` | — | raw bytes (Prometheus text) | 200 | `metrics.json` |

**`activate`**: `config` is the agent's resolved settings for this plugin.
A non-2xx rejects the agent's activation; the operator sees it as
`crews.<c>.agents.<a>.plugins.<name>: <message>`, `<message>` being the
body's `error` (`activate-rejected.json`). An `activate` for an agent the
plugin already holds replaces that agent's config in place — a changed
`update` and every re-send after `hello` arrive this way, with no
`deactivate` before them — so a rejected config leaves whatever the plugin
held for the agent untouched.

**`deactivate`**: sent on `down` and when the agent's spec drops the
plugin; never for a config change.

**`events`**: an observer batch, at most 64 events, oldest first; a 2xx
acknowledges, anything else is logged and counted, and there is no
catch-up for what was missed while the plugin was not `Ready`. A
`HookEvent` is `{ agent, name, session_id?, received_at, payload }`
(`events.json`; `session_id` is omitted when absent, present in
`intercept.json`'s event). `balerix_plugin_events_dropped_total{plugin}`
counts every event that never reached the plugin: those dropped on queue
overflow, and — a whole batch at a time — those whose batch was ready to
send while the plugin was not, and those whose batch the plugin did not
acknowledge.

**`intercept`**: `response_so_far` is the chain's response before this
plugin (`{}` for the first); `deadline_ms` is what remains of the chain's
1500 ms shared budget. The reply's `response` must be a JSON object
(`intercept.json`); `actions` is the list of §3's action shapes to run
after the response is written, and is omitted when empty. A plugin that
times out, refuses the connection, answers a non-2xx, or answers a
non-object `response` is skipped for this event: `response_so_far` passes
through unchanged and the chain continues (fail-open).

**`health`**: a 2xx keeps the plugin's status clear; anything else marks it
degraded. Never causes a restart.

**`metrics`**: Prometheus text. Every family name — `# TYPE`/`# HELP` lines
and samples alike — must start with `balerix_plugin_<name>_`
(`metrics.json`'s `balerix_plugin_flow_state`); the daemon drops a body
that breaks this rule instead of re-exposing it. The Rust SDK's
`balerix_plugin_sdk::Metrics` registers every family under that prefix and
`Plugin::metrics` returns it for the router to render, so an SDK plugin
cannot break the rule; a plugin in another language formats the text
itself and must apply the prefix.

### 4.1 Routes

A manifest with `routes: true` mounts the plugin's own HTTP surface at
`/v1/plugins/<name>/…` on the daemon's listener, authenticated by the
admin bearer or a browser session cookie (plugins spec §18.2). The daemon
forwards `/v1/plugins/<name>/` to `GET|POST|… http://<listen>/v1/routes`
and `/v1/plugins/<name>/<rest>?<query>` to `/v1/routes/<rest>?<query>`
— `<rest>` crosses percent-encoded exactly as the client wrote it, and a
`.` or `..` segment (its `%2e` spellings included) is refused with 400
rather than forwarded — with the method, the body (1 MiB cap, 413 beyond), and the request
headers minus `Authorization`, `Cookie`, `Host`, the hop-by-hop set and
whatever `Connection:` names (on an upgrade request `Upgrade` is kept and
`Connection` is sent as exactly `Upgrade`). Two headers
are added: `Authorization: Bearer <BALERIX_PLUGIN_TOKEN>` (§2) and
`X-Balerix-Forwarded-Prefix: /v1/plugins/<name>`, the mount to build links
from. The response streams back with its hop-by-hop headers removed; a
101 is upgraded on both sides and the two byte streams copied until either
closes, so a WebSocket route works unchanged behind the mount. 404
`plugin "x" has no routes` without `routes: true`, 503 `plugin "x" is not
ready` before `hello`. The Rust SDK nests `Plugin::routes` under
`/v1/routes` behind the same bearer check as every other route
(`routes.json`, Task 6).

## 5. Activation lifecycle

An agent's `(agent, plugin)` pair is one of three states, visible in
`balerix status` and the CLI's wait: **pending** (recorded, `activate` not
yet sent because the plugin was not `Ready`), **active** (the plugin
answered 2xx), or **rejected** (the plugin's non-2xx, with its message).

Every `hello` re-sends `activate` for every row the daemon holds for that
plugin — pending, active and **rejected** alike, since re-offering a
rejected pair is how it recovers — so a restarted plugin gets its
activations back without the daemon persisting anything about them. The
answer to each is the pair's new state: a pair rejected before can become
active, and one active before can be rejected. An `up` or `update` also
re-offers every pair whose row is not `active`, even when its config did
not change. A pair is deactivated when its fleet goes `down` or when an
`up` or `update` drops the plugin from the agent's spec; a changed config
arrives as a new `activate` in place, and a rejected one changes nothing
for that pair. A `rejected`
pair does not stop the agent — the fleet keeps running, and a plugin that
never activates simply never runs for that agent.

## 6. Conformance

`docs/plugin-protocol/*.json` holds twenty-six fixtures, one JSON object
each: `{ route, direction, request, status, response }` for
`daemon-to-plugin` and most `plugin-to-daemon` routes; `raw` (base64)
replaces `request`/`response` for the kv byte bodies, `health.json` and
`metrics.json`; `hello-bad-token.json` additionally carries a top-level
`"token"` to send instead of the real one. Daemon-to-plugin fixtures also
carry `headers`, the request headers the daemon sends.

Fixtures with `"transport": "websocket"` (`attach-resize.json`,
`fleets-watch.json`) describe one frame, not a request/response pair, and
are asserted by the SDK's stream test against `FakeHost`; `routes.json`
is replayed through the SDK router alone — the daemon's proxy forwards
requests unparsed.

Two tests replay the request/response fixtures (every fixture but the
three just named):

- `crates/balerix-plugin-sdk/tests/conformance.rs` — every
  `daemon-to-plugin` fixture through the SDK's `router` (a real HTTP round
  trip to a `Reference` plugin), and every `plugin-to-daemon` fixture
  through the SDK's `Host` against `balerix_plugin_sdk::testing::FakeHost`.
- `crates/balerix-server/tests/protocol_it.rs` — every `daemon-to-plugin`
  fixture through the daemon's own `PluginClient` against
  `balerix_server::testing::stub_plugin`: the `activate`,
  `activate-rejected`, `events` and `intercept` `request` bodies are
  checked against the bytes the stub recorded (and the parsed verdict
  against the fixture's `response`), and `health`/`metrics`, which carry a
  `raw` body and no request, against what the client makes of it.

Two statuses no fixture carries because `balerix_plugin_sdk::testing::FakeHost`
does not gate capabilities or activation the way the real daemon does — the
403 capability gate and the 404 `plugin is not active for agent …` on
`agents/…/actions` (§3) — are asserted against the real daemon by
`crates/balerix-server/tests/events_it.rs`, and
`crates/balerix-server/tests/workspace_it.rs` the workspace routes' 403,
404s, 413 and 400; `crates/balerix-server/tests/manage_it.rs` the manage
routes' 403 and 409s.

## 7. Packaging and distribution

A package is a directory (or a tarball of one) with `mise.toml` and
`balerix-plugin.yaml` at its root (plugins spec §2). The manifest's
`start` names a task in that `mise.toml`; the daemon runs `mise trust` and
`mise install` on it, then `mise run <start>` inside the sandbox with the
package directory as the working directory. Two shapes:

- **Development**: the binary sits inside the package (`bin/…`) and the
  task runs it by relative path. `mise run package-plugins` assembles the
  in-tree plugins this way under `target/plugins/<name>/`; a
  `plugins.yaml` entry names such a directory as its `source` and it is
  used in place. Host-only by construction.
- **Release**: the package carries **no binary**. Its `mise.toml` pins
  the plugin binary as a mise tool (for example a `ubi:` or `github:`
  backend entry against a release asset, exact version) and the task runs
  it by name; the daemon's `mise install` fetches the asset for the host
  platform, exactly as it installs `node` for an agent, and the sandbox
  already grants read on the mise data dir. One platform-neutral tarball,
  one `sha256`. Cross-compiling the per-platform assets is the plugin
  repository's release pipeline (Linux x86_64 and aarch64 while the
  sandbox is Landlock; static builds avoid libc mismatches).

A `plugins.yaml` entry may also carry `secrets`: a map from a config key
to a file path, resolved against the directory holding `plugins.yaml`
exactly as `source` is (for example `secrets: { password:
/run/balerix-secrets/matrix-bot-password }`). The daemon reads each file
when it loads the entry, trims one trailing newline, and splices the
contents into the entry's `config` under that key before the plugin's
first `hello` — it hands the plugin the resolved value, not the path.
That is deliberate rather than the plugin reading the file itself: a
plugin's own `scratch/` does not exist until the daemon materializes the
plugin, which is exactly the moment it first needs the credential, so the
daemon is the one positioned to read it. Each file must not be readable
by group or other; a looser mode is rejected at load, alongside a missing
file or a `secrets` key that collides with one already in `config`. The
resolved value lives only in memory from there — it is never written back
to `plugins.yaml` or logged. Rotating a file named in `secrets` changes
the resolved plugin's hash, so the daemon restarts the plugin on its next
sync to pick up the new value.
