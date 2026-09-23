# Balerix — Spec L: plugin-managed fleets

**Date:** 2026-09-22
**Status:** Approved in brainstorm 2026-09-22
**Scope:** the daemon-side changes the GitHub plugin (Spec M) needs and
any future plugin that runs agents on its own initiative: a `manage`
capability with two host routes that apply and tear down a fleet from an
unresolved fleet file; two ports so the daemon resolves and finds
credentials without importing `balerix-config`; an owner on fleet records
that keeps the CLI and plugins out of each other's fleets; and an optional
per-agent `branch` so an agent can work on an existing remote branch.

Depends on nothing new; Spec K is independent. Spec M depends on both.

---

## 1. Problem

Only the CLI creates fleets. `balerix up` reads the fleet file, folds the
host's `claude.settings` into it, resolves every agent, reads the Claude
credentials and the gh token from the operator's home, and POSTs
`{ spec, credentials }` to the admin-only `/v1/fleets`. A plugin has none
of those: no admin token, no host home inside its sandbox, no
`balerix-config` (plugins depend on the SDK and `balerix-api` only), and
no route that would take a spec if it had one.

A GitHub issue that should become a running agent needs a plugin to say
"run this fleet file, with these agents in it" and have the daemon do
what `up` does. Because the plugin adds and removes agents as issues open
and close, the file changes often and every change is a full apply.

A second gap: an agent's branch is always `balerix/<fleet>/<crew>/<agent>`
started from the crew's `ref`. A session attached to a pull request must
work on the PR's head branch, or its pushes never reach the PR.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| L-1 | A new capability, **`manage`**, gates two routes: apply a fleet from an unresolved file, and down it. | The threat model gates every host power by a manifest `needs` entry the operator reads. Running agents with the operator's credentials is the largest power a plugin can hold; it must be a word in the manifest. |
| L-2 | The route takes the **unresolved fleet file**; the daemon resolves it. | The resolver is one crate. Client-side resolution existed so that merge semantics could not drift between the CLI and the daemon; using the same crate on the daemon keeps that property, and a sandboxed plugin cannot read the host's `claude.settings` anyway. |
| L-3 | The daemon reads the **operator's** Claude credentials and gh token from the host home at apply time, exactly as `up` does. | A plugin must never hold Claude credentials. The daemon already runs as the operator. Agents push as the operator; only the plugin's own messages carry the App's identity. |
| L-4 | Resolution and credential reading are **ports** (`FleetResolver`, `CredentialSource`) wired by the binary. | `balerix-server` depends on `core` and `api` only; `balerix-config` stays out of it, as `balerix-runtime` does. |
| L-5 | A fleet applied through the route carries an **owner**; the admin routes refuse to touch it and the route refuses a fleet it does not own. | Two writers of one fleet name would race; the CLI's `up` on a plugin's fleet would silently drop the plugin's agents. `--force` on `down` exists for cleanup. |
| L-6 | **`plugin remove` downs the plugin's fleets.** | A removed plugin cannot down them itself; leaving agents running with nobody to talk to them is the wrong default. |
| L-7 | `AgentSettings.branch` names an existing remote branch as the agent's worktree branch and start point; the crew `ref` stays the diff base. | A PR agent must push to the PR's head. The change touches one field in `api`, the resolved agent's `branch()` in `core`, and the argument `ensure_worktree` already takes. |

## 3. The `manage` capability

`balerix_api::Capability` gains `Manage`, serialised `manage`. The
`plugin_api` router gates the two routes on it like `workspace` gates its
four (403 `capability "manage" not declared in balerix-plugin.yaml`).

### 3.1 `PUT /v1/plugin-host/fleets/{name}`

Request: `{ "file": <fleet file as a JSON object> }`. The object is the
YAML fleet file's structure (`apiVersion`, `kind`, `name`?, `defaults`,
`crews`), parsed by the plugin; the daemon deserialises it into
`balerix_config::FleetFile` through the resolver port, which is where a
YAML file and this object meet. `name`, when present, must equal the path
name.

The daemon:

1. Refuses a reserved name (`balerix`, `watch`) with 400, and a name that
   is not a `FleetName` with 400.
2. Refuses with 409 `fleet <name> is managed by plugin <other>` when the
   record's owner is another plugin, and 409 `fleet <name> is not managed
   by a plugin` when a record exists with no owner.
3. Resolves: `FleetResolver::resolve(file, name)` folds the host's
   `claude.settings` in and answers a `FleetSpec` or a `ConfigError`; the
   error is 400 with its message verbatim, config path first
   (`crews.repo.agents.issue-12.plugins.github: …`).
4. Reads credentials: `CredentialSource::load()`; a failure is 500 with
   the message (a missing `~/.claude` on the daemon's host is an operator
   problem, not the plugin's).
5. Calls `Daemon::apply(name, spec, credentials, replace = true, owner =
   Some(plugin))`. The first apply of a name creates the record with the
   owner; every later one must match. The apply's existing rules hold:
   activations run before the actor, a rejection is 400 with nothing
   landed, the per-fleet lock serialises concurrent applies.

Response: 200 with the `FleetRecord`, the same shape `GET fleets/{name}`
answers. The plugin then waits for readiness through `fleets/watch`, as
the CLI polls; the route does not block on it.

### 3.2 `DELETE /v1/plugin-host/fleets/{name}?keep_repos=&keep_sessions=&purge=`

The admin `DELETE`'s flags and rules (`DownQuery`, purge excludes keep).
Refuses with 409 unless the record's owner is this plugin. Answers the
record after `Daemon::down`.

### 3.3 SDK and protocol

`Host` gains:

```rust
pub async fn apply_fleet(&self, name: &str, file: &Value) -> Result<FleetRecord, SdkError>;
pub async fn down_fleet(&self, name: &str, query: &DownQuery) -> Result<FleetRecord, SdkError>;
```

`testing::FakeHost` records both calls and answers a configurable record
or error, and exposes the last applied file for assertions.
`docs/plugin-protocol.md` §3 gains the two rows and the capability;
fixtures `fleet-put.json`, `fleet-put-rejected.json` (a resolver error)
and `fleet-delete.json` are replayed by the conformance tests.

## 4. The two ports (`balerix-core::ports`)

```rust
pub trait FleetResolver: Send + Sync {
    /// An unresolved fleet file, as the YAML's JSON, resolved against the
    /// host's defaults for `name`. Errors are the resolver's own
    /// messages, config path first.
    fn resolve(&self, file: &Value, name: &FleetName) -> Result<FleetSpec, String>;
}

pub trait CredentialSource: Send + Sync {
    /// The operator's Claude credentials and gh token, read now.
    fn load(&self) -> Result<CredentialBundle, String>;
}
```

The binary implements both over `balerix_config`: `resolve` is
`serde_json::from_value::<FleetFile>` + `host::load(&HostPaths::discover())`
+ `resolve(&file, &ResolveOptions { name_override: Some(name), host_claude_settings })`
+ `Fleet::try_from` for names and repos, the same sequence
`commands::fleet::load_request` runs; `load` is `host::load(..).credentials`.
`Daemon::start` takes both in `Ports`. `balerix_core::fakes` gains
`FakeResolver` (a fixed spec or a fixed error) and `FakeCredentials`.

`balerix_config::FleetFile` gains `Serialize` so a file round-trips as
JSON in tests; nothing else changes there.

## 5. Owned fleets

`balerix_api::FleetRecord` gains `owner: Option<String>` (a plugin name),
persisted in `fleet.json`, defaulted on read so older records load.
`FleetSummary` gains it too; `balerix list` shows `MANAGED BY` and
`balerix status` prints `managed by <plugin>` on its first line.

Rules, in `Daemon`:

- Admin `POST`/`PUT /v1/fleets/{name}` on an owned fleet: 409 `fleet
  <name> is managed by plugin <p>`. The CLI's `up`/`update` surface it
  as-is.
- Admin `DELETE /v1/fleets/{name}` on an owned fleet: 409 unless
  `?force=true` (`DownQuery` gains the flag, `key=true|false` like the
  others); `balerix down --force` sets it. The record keeps its
  owner in `Down`, so a later apply by the plugin resumes it.
- `PUT`/`DELETE plugin-host/fleets/{name}` on a fleet with a different or
  absent owner: 409 (§3.1 step 2).
- `plugin remove <p>`: after the plugin is out of the record, `down` every
  fleet owned by `p` (`--purge` purges them too); the CLI prints one line
  per fleet. A fleet that fails to go down is reported and the removal
  continues.
- `Daemon::apply` with `owner: None` on a fleet that has an owner is the
  admin case above; the daemon method takes the owner explicitly so the
  two routes share it.

The registry rebuild at start and `fleets/watch` frames carry the owner
unchanged; `FleetRecord`'s wire shape gains the field for plugins with
`fleets`, which is harmless data.

## 6. Per-agent `branch`

`balerix_api::AgentSettings` gains:

```rust
/// An existing remote branch this agent works on. When set, the worktree
/// branch is this name, created from `origin/<branch>`; the crew `ref`
/// remains the base the workspace diff is taken against. Absent: the
/// per-agent branch `balerix/<fleet>/<crew>/<agent>` from `origin/<ref>`.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub branch: Option<String>,
```

- Merges like every settings key (a `null` deletes). In practice an
  agent-level key; a `defaults.branch` is accepted and merely unusual.
- Validated in `balerix-config::validate_agent` with the rules of `git
  check-ref-format --branch`, implemented in `balerix-api` as
  `check_branch_name`: non-empty, at most 255 bytes, no component starting
  with `.` or ending with `.lock`, no `..`, no ASCII control or space, none
  of `~ ^ : ? * [ \`, no `@{`, no leading or trailing `/`, no `//`, not
  ending in `.`, not starting with `-`, and not starting with `refs/`.
  The error is `crews.<c>.agents.<a>.branch: <reason>`.
- `ResolvedAgent::branch()` answers `settings.branch` when set; the
  materializer passes it to `ensure_worktree` as both `branch` and
  `git_ref`, so a missing local branch is created from `origin/<branch>`
  and an existing one is reused. `ResolvedAgent::hash` already covers
  settings, so changing it re-materializes the agent.
- `WorkspaceReader`'s diff base is unchanged: `origin/<crew ref>`, which
  for a PR agent is the PR's base, the diff a reviewer wants.
- A `branch` that does not exist on the remote fails the materialize step
  with git's message; the agent is `Failed` with it, retried at the
  resync cadence like a clone failure.

## 7. Security

`docs/THREAT-MODEL.md`, under accepted risks:

- **A plugin with `manage` runs agents with the operator's credentials.**
  It can apply any fleet file, so any repository the operator's gh token
  reaches and any settings the resolver accepts, and down what it
  created. The operator's choice in `needs`, like `attach` and
  `workspace`; bounded by the owner rule (it cannot touch a fleet it did
  not create, and the CLI cannot silently take over its fleets), by the
  same validation `up` applies, and by every agent still running inside
  its nono profile. The plugin never sees the credentials: the route
  takes a file, the daemon reads the bundle.
- The fleet file crosses the plugin → daemon boundary as **untrusted
  input** with the same treatment as the CLI's: full validation through
  the resolver, `deny_unknown_fields`, name rules, exact tool versions.
  `branch` is validated as a ref name before it reaches a git argv
  (`worktree add -b <branch> <path> origin/<branch>`); the leading-`-`
  refusal is what keeps a name from being read as a flag.

## 8. Testing

- `balerix-api`: `Capability::Manage` serde; `FleetRecord.owner` default
  on an old record; `AgentSettings.branch` merge and serde;
  `check_branch_name` against a table of good and bad names.
- `balerix-config`: `branch` validation with the config path in the
  message; `FleetFile` JSON round trip.
- `balerix-core`: `ResolvedAgent::branch()` with and without the field;
  `FakeResolver`, `FakeCredentials`.
- `balerix-runtime` (`workspace_it`): a worktree on an existing remote
  branch is created from it and reused across a second `ensure_worktree`.
- `balerix-server` (`api_it`, `events_it`): the two routes with the fake
  ports: success, resolver error as 400 with the path, missing capability
  403, a foreign owner 409, an unowned name 409, admin `PUT` on an owned
  fleet 409, admin `DELETE` with and without `force`, `plugin remove`
  downing an owned fleet, the owner surviving a daemon restart.
- `balerix` (`cli_fleet`): `list` and `status` render the owner; `down
  --force`.
- SDK conformance: the three fixtures through `Host` and `FakeHost`.
- e2e (`cli_serve`): `dev fake-plugin` gains a `manage` mode that applies a
  one-agent fleet on `hello`; the journey asserts the agent turns `Ready`
  and that `balerix up` on that name is refused.

## 9. Deliberately deferred

- A per-fleet credential choice (App installation token for pushes).
  Needs token refresh into a running agent's `hosts.yml`.
- Partial updates (add or remove one agent) instead of a full apply. The
  reconciler already diffs, so a full apply costs a resolve and a pass.
- Transferring ownership between a plugin and the CLI.

## 10. Done when

1. `mise run check`, `mise run test-it` and the e2e pass with the new
   routes, ports and fakes.
2. `docs/plugin-protocol.md` §3 documents `manage`, the two routes and
   their fixtures; the SDK's `Host` and `FakeHost` implement them.
3. A fleet applied by `dev fake-plugin` shows `managed by fake` in
   `balerix status`, refuses `balerix up`, and goes down on `plugin remove`.
4. An agent with `branch: feature/x` runs on that branch (verified by
   `workspace_it` and by hand with `mise run verify-claude` on a branch).
5. `docs/THREAT-MODEL.md` and `ARCHITECTURE.md` describe the capability,
   the owner rule and the ports.

## 12. Recorded at implementation (2026-09-23)

- `Daemon::apply`/`down` stay the admin wrappers; the shared forms are
  `apply_as(name, spec, credentials, ApplyMode, &Caller)` and
  `down_as(name, keep, purge, &Caller)`, with `ApplyMode::Upsert` for
  the plugin's `PUT` (§3.1 step 5 said `replace = true`; an absent name
  must create). `Caller::Admin { force }` / `Caller::Plugin(name)`.
- The owner error is `DaemonError::Managed(String)` → 409; a record
  is created with `FleetRecord::with_owner`.
- `plugin remove` downs owned fleets inside `Daemon::sync_plugins`, for
  every plugin the sync stopped; the fleets and failures ride back in
  `SyncReport.downed` / `SyncReport.down_failed` and the CLI prints one
  line per fleet from them. `--purge` re-downs each with `purge` +
  `force` from the CLI. A plugin already undeclared at daemon start
  downs nothing.
- `balerix_config::from_value` reads the file from JSON with
  `serde_path_to_error`, so a shape error names its key (`file` for the
  root); the daemon checks `file.name` against the path before resolving.
- The plugin `DELETE` accepts `force` and ignores it.
- `dev fake-plugin`'s manage mode is driven by `config.manage` in
  `plugins.yaml`, not a flag: the package's `start` task takes no args.
- `FakeHost` records refused calls too (`applied_fleets`, `downed_fleets`)
  and fails both routes with `fail_manage(Some((status, message)))`.
- `check_branch_name` also refuses the bare `@`, as git does.
