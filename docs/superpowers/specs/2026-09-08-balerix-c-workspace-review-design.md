# Balerix — Spec C: Workspace reads and browser code review

**Date:** 2026-09-08
**Status:** Approved in brainstorm 2026-09-08. Builds on
`2026-09-06-balerix-b-plugins-design.md` (the *plugins spec*), §4, §5.1, §7,
§18; `docs/plugin-protocol.md` is the wire contract it extends.
**Scope:** a fifth plugin capability, `workspace`, giving a plugin read-only
access to an agent's worktree through the daemon (diff against the crew's
base ref, one file, one directory listing); the `WorkspaceReader` port and
its git implementation; multi-line `send_text` on tmux; and, in
`balerix-plugin-web`, a review page per agent — the diff with line comments,
a collapsible column of the agent's hook events, and a submit that sends the
review to the agent as one message.

Where this document and the plugins spec disagree, this document wins.

---

## 1. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| PC-1 | **Workspace access is daemon-mediated, never a filesystem grant.** A plugin declaring `workspace` calls three typed host routes; the daemon runs `git` and reads files in the agent's worktree and answers. The plugin's nono profile does not change. | A grant is fixed at plugin start (new fleets would need a plugin restart), a worktree's `.git` file points into the crew repo, and `home/` — credentials, hook secret — sits beside `workspace/`, so the grant would have to be per-directory and never the parent. The mediated route works for a plugin in any language, keeps one place to audit, and matches KV ("never granted; goes through the API"). |
| PC-2 | **Typed routes, not a generic git route.** `diff`, `file`, `tree`; no "run this read-only git command" with an argv allowlist. | git's surface cannot be allowlisted safely: `--output`, `-c`, textconv and external diff drivers, aliases turn a read into a write or an exec. The daemon decides exactly which invocations run in a repository an agent can write to. |
| PC-3 | **A new port, `WorkspaceReader`**, beside `Materializer` and `AgentRunner`; not new methods on `Materializer`. | Reading is a different responsibility from making files exist; `FakeMaterializer` is the reconciler's test double and should not carry workspace state every server test ignores. |
| PC-4 | **The diff is everything since the branch point**: committed work on the agent's branch plus uncommitted changes and untracked files, against the merge-base with `origin/<ref>`. Each file says whether it is uncommitted. | It is what the agent has actually done, whether or not it committed; a reviewer usually looks before the agent commits. The merge-base makes the diff independent of upstream moving on. |
| PC-5 | **The review reaches the agent as one `send_text`**, formatted by the web plugin; web adds `actions` to its needs. `TmuxRunner::send_text` pastes multi-line text through a tmux buffer with bracketed-paste markers. | The action exists; a new "review" action would put formatting in the daemon. `send-keys -l` with a literal newline is Ctrl-J to the application, which is version-dependent; a bracketed paste is what a terminal does, and Claude Code is expected to take it as one message (§8, to be verified by hand with `verify-claude`). |
| PC-6 | **Comments are anchored by file, side and line, and each carries the diff line it was made on.** No 409 on a changed tree. | The agent is working while you review; a hard conflict would fire constantly. The quoted line lets the agent (and the reviewer after a reload) locate the comment when lines moved. |
| PC-7 | **Drafts live in the browser; nothing is kept after submit.** `localStorage` under the agent id; cleared on a successful send. No `kv` for web. | No new capability, nothing to purge, the terminal and the activity column show what was sent. A history is a later spec if wanted. |
| PC-8 | **The activity column is the hook event timeline**, from web observing all nine events into a per-agent ring buffer; not an embedded terminal. | Structured, PR-timeline-like, and not a keyboard into the agent from the review page. The terminal page stays one click away. |
| PC-9 | **Event buffers are in memory only.** 500 events per agent, payloads cut at 4 KiB. | The daemon offers no catch-up for observers (protocol §4); persisting a partial stream would promise more than it holds. |

## 2. The `workspace` capability

### 2.1 Manifest

`Capability` gains `Workspace` (`"workspace"` on the wire); `needs:
[workspace]` in `balerix-plugin.yaml`. A call to a workspace route without
it is 403 `capability "workspace" not declared in balerix-plugin.yaml`, as
for every other capability. Host protocol major stays **1**: a plugin that
does not declare `workspace` sees nothing new.

### 2.2 Routes

Under `/v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/`, bearer
`BALERIX_PLUGIN_TOKEN`. Every route requires the pair to be `Active` — 404
`plugin is not active for agent <id>` otherwise, exactly like `attach` and
`actions` — and answers 404 `no workspace for agent <id>` when the agent's
worktree directory does not exist (not yet materialized, or purged).

| Route | Answer | Status |
|---|---|---|
| `GET …/workspace/diff` | `WorkspaceDiff` (JSON) | 200 |
| `GET …/workspace/file?path=<rel>` | raw bytes, `application/octet-stream` | 200; 404 `no such path`; 413 `file larger than 1 MiB`; 400 `not a regular file` |
| `GET …/workspace/tree?path=<rel>` | `WorkspaceTree` (JSON) | 200; 404 `no such path`; 400 `not a directory` |
| any, bad path | `{ error }` | 400 `workspace: invalid path: <reason>` |

The two 404 texts differ on purpose: `no workspace for agent` means the
worktree is absent, `no such path` means the worktree exists and the path
does not; the SDK maps the second to `None` and surfaces the first as an
error.

**`WorkspaceDiff`** is

```json
{
  "base_ref": "origin/main",
  "merge_base": "3f9c2a1…",
  "head": "b7e0d44…",
  "files": [
    { "path": "src/lib.rs", "old_path": null, "status": "modified",
      "uncommitted": true, "binary": false, "truncated": false,
      "patch": "diff --git a/src/lib.rs b/src/lib.rs\n…" }
  ],
  "truncated": false
}
```

- `base_ref` is the crew's `ref` as `origin/<ref>`; `merge_base` and `head`
  are full shas. Everything is measured worktree-against-merge-base.
- `status` is one of `added`, `modified`, `deleted`, `renamed`, `copied`,
  `typechange`; `old_path` is set for `renamed` and `copied` only.
- `uncommitted` is true when the worktree differs from `HEAD` for that path
  (an untracked file is `added` and uncommitted).
- `patch` is the unified diff for that one file with three lines of context,
  the `diff --git` header included; empty when `binary` is true. A patch over
  **256 KiB** is cut at a line boundary and `truncated` set on the file. The
  list stops at **500** files, sorted by path, and sets the top-level
  `truncated`.

**`WorkspaceTree`** is `{ "path": "src", "entries": [{ "name": "lib.rs",
"kind": "file", "size": 1234 }] }`; `kind` is `file`, `dir`, `symlink` or
`other`; `size` is present for files only; entries are sorted by name;
`.git` is never listed; the listing never recurses. An empty `path` is the
worktree root.

**Paths** (`file` and `tree`) are relative, `/`-separated, at most 4096
bytes, with no empty, `.` or `..` segment, no `\`, no NUL, and no segment
named `.git` anywhere. Validation happens before any I/O. `file` refuses
anything whose `symlink_metadata` is not a regular file (a symlink is not
followed); `tree` refuses anything that is not a directory; after the
join, the canonical path must still start with the canonical worktree
(defence in depth against a directory symlink higher up).

### 2.3 Wire types, SDK, fixtures

`balerix-api/src/workspace.rs`: `WorkspaceDiff`, `FileDiff`, `FileStatus`,
`WorkspaceTree`, `TreeEntry`, `EntryKind`; re-exported from `lib.rs`.
`balerix-core` re-exports them like `FleetRecord`.

`balerix_plugin_sdk::Host` gains `workspace_diff(agent)`,
`workspace_file(agent, path)` (`Option<Vec<u8>>`, `None` on 404 for the
path, like `kv_get`), `workspace_tree(agent, path)`. `workspace_diff` uses
a **30 s** request timeout instead of the client's 10 s: a first diff of a
large repository can take longer. `testing::FakeHost` gains
`set_workspace(agent, WorkspaceDiff, files: BTreeMap<String, Vec<u8>>)` and
serves the three routes from it, deriving `tree` from the file map.

Three fixtures under `docs/plugin-protocol/`: `workspace-diff.json`,
`workspace-file.json` (`raw`), `workspace-tree.json`, `plugin-to-daemon`,
replayed by `balerix-plugin-sdk/tests/conformance.rs` against `FakeHost`.
The 403, both 404s and the 413 are asserted against the real daemon by the
server test (§9), as for `actions`.

## 3. The port and the runtime

### 3.1 `WorkspaceReader` (`balerix-core/src/ports.rs`)

```rust
pub trait WorkspaceReader: Send + Sync {
    /// The agent's worktree against the merge-base with `base_ref`
    /// (`origin/<ref>`); `Err(WorkspaceError::Missing)` when there is no
    /// worktree directory.
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError>;
    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError>;
    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError>;
}
```

`WorkspaceError` (`thiserror`): `Missing(id)`, `NoSuchPath`,
`InvalidPath(reason)`, `NotAFile`, `NotADirectory`, `TooLarge { limit }`, `Tool { … }` (the git
failure shape `MaterializeError::Tool` uses), `Io { path, message }`. Path
validation is a pure function in core (`workspace::check_path`), shared by
the runtime and the fake, so the rules are tested once.

`Ports` gains `workspace: Arc<dyn WorkspaceReader>`; `serve` wires
`Runtime`; `balerix_core::fakes::FakeWorkspace` holds, per agent, a
`WorkspaceDiff` and a `BTreeMap<String, Vec<u8>>` of files, answers
`list_dir` from the map, and records calls like the other fakes. The daemon
takes the crew's `ref` from the fleet record's spec and calls the port in
`spawn_blocking`.

### 3.2 `balerix-runtime/src/inspect.rs`

Every git call runs through `Cmd` with `-C <workspace>`, logging to the crew's
`logs/git.log` as `Workspace::git` does, with the same five `GIT_*` variables
scrubbed, and additionally:

| Setting | Why |
|---|---|
| `GIT_OPTIONAL_LOCKS=0` | a read never takes the index lock from under the agent |
| `-c core.fsmonitor=false` | repo config can name a program to run on status/diff |
| `-c core.hooksPath=<empty dir under the crew root>` | no hook of the repository can run |
| `--no-ext-diff --no-textconv` on every `diff` | `.gitattributes` plus repo config can name external programs |
| `--no-color`, `-c diff.noprefix=false`, `-c core.quotePath=true` | stable output for the parser |
| no `fetch`, no `GH_CONFIG_DIR`, no credential helper | reads only; `origin/<ref>` is whatever the last worktree creation fetched |

The diff:

1. `rev-parse HEAD` → `head`; `merge-base origin/<ref> HEAD` → `merge_base`
   (a missing `origin/<ref>` is a `Tool` error, surfaced as 500 with git's
   first stderr line).
2. `diff --name-status -z --find-renames <merge_base>` → the tracked file
   list with statuses (worktree against the merge-base).
3. `diff --name-only -z HEAD` → the set of paths that differ from `HEAD`
   (`uncommitted`); `ls-files --others --exclude-standard -z` → untracked
   files, appended as `added` + `uncommitted`.
4. Per file, in path order, until 500: `diff -U3 <merge_base> -- <path>` for
   tracked files; `diff --no-index -U3 -- /dev/null <path>` for untracked
   ones, where exit status 1 means "differs" and is success. A `Binary files
   … differ` body sets `binary` and empties `patch`. Cut at 256 KiB on a
   line boundary.

`read_file`: `check_path`, join, `symlink_metadata` must be a regular file,
canonical-prefix check, size over 1 MiB → `TooLarge`, then read.
`list_dir`: `check_path`, join, must be a directory, canonical-prefix check,
`read_dir`, skip `.git`, sort by name.

### 3.3 `send_text` with newlines (`balerix-runtime/src/tmux.rs`)

Text without a newline keeps `send-keys -l -- <text>`. Text with one runs
`set-buffer -b balerix-send-<hex> -- <text>` then `paste-buffer -p -d -b
balerix-send-<hex> -t <window>`: `-p` adds the bracketed-paste markers when
the application enabled them (Claude Code does; `fake-claude` does not and
gets plain text), `-d` deletes the buffer. `submit` still sends `Enter`
afterwards. `FakeRunner::send_text` is unchanged.

## 4. `balerix-plugin-web`

### 4.1 Manifest and cache

```yaml
hooks:
  observe: [SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, SubagentStop, PreCompact]
needs: [fleets, attach, actions, workspace]
routes: true
```

`state.rs` gains, per agent, an event ring buffer: `VecDeque<Entry>` of at
most 500, `Entry { seq: u64, at: Timestamp, name: String, summary: String,
payload: Value, payload_truncated: bool }`; `seq` is per agent and never
reused. `Plugin::observe` appends each event of a batch to its agent's
buffer, deriving `summary` (§4.3) and cutting the serialized payload at 4
KiB (kept as `{ "truncated": true, "head": "<first 4 KiB>" }`). Events for
an agent that is not enabled are dropped. `deactivate` drops the buffer with
the enabled flag. A sent review appends a synthetic entry named
`review_sent` with `summary` `review sent (n comments)` and the rendered
message as payload.

### 4.2 Routes

Beside the existing five, each 404 `no such agent` for an agent that is not
enabled:

| Route | Answer |
|---|---|
| `GET /agents/{id}/review` | the review page |
| `GET /agents/{id}/diff.json` | the daemon's `WorkspaceDiff`, unchanged; the daemon's error status and message pass through (a 404 `no workspace` becomes a 404 with that text) |
| `GET /agents/{id}/file?path=` | the bytes as `text/plain; charset=utf-8`; the daemon's error passes through |
| `GET /agents/{id}/events.json?after=<seq>` | `{ "phase": "<agent phase>", "events": [Entry…] }`, the entries with `seq > after`, oldest first; `after` defaults to 0 |
| `POST /agents/{id}/review` | body §4.4; `{}` on success |

The index gets a second link per row, `review`, after the agent name (both
server-rendered and in the refresh script).

### 4.3 The page

Two columns. Left: the diff. Right: the activity column with a header
holding the agent id, its phase and a collapse button; collapsed, the column
is a 2 rem strip with the button and an unread count, and the choice is kept
in `localStorage` with the draft.

**The diff.** A server-rendered shell (title, both columns, the footer) with
one inline script and no new vendored asset: the script fetches
`diff.json`, parses each `patch` (a unified-diff parser of about a hundred
lines: `@@ -a,b +c,d @@` headers, `+`/`-`/` ` lines, `\ No newline at end of
file`), and renders per file a header (status, `old_path → path` for
renames, an `uncommitted` badge, a `truncated` note, a `view file` link to
`file?path=`) and its hunks with old and new line numbers. A binary file
shows its header only. Clicking a line's gutter opens a textarea under it;
a saved comment shows inline with edit and delete. Every value goes through
`textContent`, as the index does.

**The footer**: a summary textarea, the comment count, `Reload diff`,
`Send review`. Reload re-fetches, keeps comments whose `(path, side, line,
text)` still match a line of the new diff, and lists the others under "no
longer in the diff" with their bodies, still counted and still sent.

**The draft** — comments, summary, collapse state — is written to
`localStorage["balerix-review/<id>"]` on every change and removed on a
successful send. `localStorage` failures are caught and ignored.

**The activity column** polls `events.json?after=<last seq>` every two
seconds, appends, and keeps itself scrolled to the bottom unless the reader
scrolled up. An entry is the time, the name and the summary; clicking it
expands the payload as pretty-printed JSON. The `review_sent` entry is
rendered as a divider. When the buffer is empty the column says "no events
yet; the daemon delivers no history".

`summary` per event name, derived by the plugin from the payload:

| Name | Summary |
|---|---|
| `PreToolUse`, `PostToolUse` | `<tool_name>: <tool_input.command>` for `Bash`; `<tool_name> <tool_input.file_path>` for `Edit`, `Write`, `Read`, `MultiEdit`; `<tool_name>` otherwise; cut at 200 chars |
| `Notification` | `message`, cut at 200 chars |
| `UserPromptSubmit` | the first line of `prompt`, cut at 200 chars |
| `Stop`, `SubagentStop` | `turn ended` / `subagent ended` |
| `SessionStart`, `SessionEnd`, `PreCompact` | the bare name |
| anything else | the bare name |

### 4.4 Submission

Body: `{ "head": "<sha>", "summary": "<text>", "comments": [{ "path", "side":
"old"|"new", "line": <n>, "text": "<the diff line, sign included>", "body":
"<text>" }] }`. Limits: 200 comments, 64 KiB over all `body` and `summary`,
`path` passing `check_path`, `text` at most 4 KiB; 400 with the reason
beyond any. A body with no comments and an empty summary is 400 `nothing to
send`.

The message, one string:

```
Review against origin/main at 3f9c2a1 (2 comments)

src/lib.rs line 42 (new):
> +    let x = foo();
This unwrap can panic on an empty list; return the error instead.

src/lib.rs line 80 (old):
> -    // TODO
Good riddance, but the docs still mention this.

Overall:
Looks close. Please address the comments above and run the tests.
```

The `Overall:` block is omitted when the summary is empty; `at <sha>` is the
first seven characters of `head`; comments are in file then line order. The
header names no agent: the agent is the recipient, and the fleet and crew
are Balerix's addressing, not something it can see (changed 2026-09-09). The
plugin sends it as `send_text { submit: true }`, appends the `review_sent`
entry, and answers `{}`. A failed action is 502 with the daemon's message
and the page keeps the draft.

### 4.5 Metrics

`reviews_total{outcome="sent"|"failed"}` (counter),
`review_comments_total` (counter), `events_buffered_total` (counter, every
event appended). Through the SDK's `Metrics`, so prefixed
`balerix_plugin_web_`.

## 5. Security

**Threat model** (`docs/THREAT-MODEL.md`):

- *Plugin ↔ daemon* gains the workspace routes in its list.
- New accepted risk: **a plugin with `workspace` reads every worktree it is
  active for** — the operator's choice in `needs`, as for `attach`; never
  `home/`, never the crew repo, never another agent's worktree.
- New accepted risk: **the daemon runs read-only `git` in a repository an
  agent can write to.** `worktree add` already runs there and applies the
  repository's smudge filters; `diff` can still run clean filters declared
  through `.gitattributes` plus `.git/config`. The controls in §3.2 close
  fsmonitor, hooks, external diff and textconv; the residue is the existing
  "agents in a crew share `.git`" trust (superseded by §12).
- New mitigation rows: workspace path rules and the canonical-prefix check
  (`balerix-core/src/workspace.rs`, `balerix-runtime/src/inspect.rs`); the
  git invocation controls (`inspect.rs`); per-file and per-response caps;
  the review page renders every value with `textContent`
  (`balerix-plugin-web/src/routes.rs`).
- *Browser ↔ daemon*: the review submission is a cookie-authenticated POST
  through the proxy; the existing same-origin check covers it (a `fetch`
  from the page sends `Sec-Fetch-Site: same-origin`).

**Errors.** `WorkspaceError` in core; `ApiError` mappings 400/404/413/500
in `plugin_api.rs`; the web plugin's 400s carry the field path
(`comments[3].path: workspace: invalid path: ..`).

## 6. Docs

- `docs/plugin-protocol.md`: §3 gains the three rows and a **Workspace**
  paragraph (path rules, caps, the 404 pair); §6 counts twenty-one fixtures.
- `docs/THREAT-MODEL.md`: §5 above.
- `ARCHITECTURE.md`: `WorkspaceReader` in the pieces list and the ports
  convention; a Spec C paragraph in "How it flows"; two non-obvious
  decisions — *git is run by the daemon, never granted* (PC-1) and *the
  review is a paste* (PC-5).
- `AGENTS.md`: gotchas as found; at least the bracketed paste and
  `GIT_OPTIONAL_LOCKS`.
- `README.md`: status "Spec C (workspace reads and review) complete" and an
  upgrade note: web's manifest now needs `actions` and `workspace`; a
  plugin in another language gains three optional routes.
- `examples/payments.yaml`: unchanged (web is already enabled).

## 7. Testing

| Layer | What | Where |
|---|---|---|
| unit | `check_path` (`..`, absolute, `.git`, empty segment, `\`, NUL, length); `--name-status -z` and `ls-files -z` parsing into `FileDiff`; binary detection; per-file and list truncation; review message rendering; event summary per name; ring buffer sequence and eviction; payload cut; `review_html` escaping and prefix links; the manifest accepting `workspace` | in-module |
| property (`proptest`) | for any string, `check_path` accepts only paths whose join stays under the root and contains no `.git` segment; for any event sequence and `after`, `events.json` returns exactly the entries with `seq > after`, in order | in-module |
| runtime (`test-it`, `inspect_it.rs`) | a real clone and worktree with one committed change, one uncommitted edit, one untracked file, one binary, one rename: `diff` reports each with the right status and `uncommitted`, patches equal `git diff`'s; `read_file` refuses `.git` and a symlink out of the tree and answers `TooLarge` over 1 MiB; `list_dir` hides `.git`; a `.git/config` with `core.fsmonitor` pointing at a script that touches a marker leaves no marker after `diff`; `tmux_it`: a two-line `send_text` arrives intact in the pane | `balerix-runtime/tests` |
| server integration (fakes, `workspace_it.rs`) | 403 without `workspace`; 404 for an inactive pair; 404 with no worktree; the three routes against `FakeWorkspace`; 413 over the file cap; 400 on a bad path | `balerix-server/tests` |
| protocol conformance | the three fixtures through `Host` against `FakeHost` | `balerix-plugin-sdk/tests/conformance.rs` |
| plugin (`Harness`) | the review page renders and links through the prefix; `diff.json` and `file` pass the fake's data through; `events.json` follows `observe` batches and `after`; `POST review` yields exactly one `send_text` with the rendered message in `FakeHost.actions()` and a `review_sent` entry; 400 over the comment cap; 502 when the fake refuses the action | `balerix-plugin-web/tests/plugin_it.rs` |
| e2e (`web_journey`) | after `up`, the test writes a file into alice's worktree; `diff.json` through the mount lists it as `added`, uncommitted; a review with one comment is posted through the mount; `fake-claude.stdin` shows the quoted line and the body; `events.json` shows the `review_sent` entry and the `UserPromptSubmit` fake-claude posts; `server.log` names no path outside the worktree | `balerix/tests/e2e.rs` |
| by hand | `verify-claude`: the pasted review arrives as one message in the real `claude`; the column shows its tool calls | `scripts/verify-claude.sh` |

## 8. Verify at implementation time

| Assumption | Fallback | Verdict |
|---|---|---|
| Claude Code takes a tmux bracketed paste of a multi-line text as one message and `Enter` submits it | send the review as a single line with comments separated by ` · `, or write it to `home/balerix/reviews/<ts>.md` and send a one-line pointer (a daemon write path — a spec change) | Pending — by-hand run of `mise run verify-claude` (not run at implementation; tmux_it proves the buffer paste against `cat`) |
| `git diff --no-index -- /dev/null <path>` inside a worktree exits 1 with a usable `added` patch | `diff -U3 --no-index` against an empty temp file under the crew's `logs/` | Verified 2026-09-08 (inspect_it: the_diff_reports_every_change_kind_and_reads_stay_inside_the_worktree) |
| `-c core.fsmonitor=false` on the command line overrides a repo-level `core.fsmonitor=<script>` (git 2.47) | `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/`GIT_CONFIG_VALUE_0` in the environment | Verified 2026-09-08 (inspect_it: the_diff_reports_every_change_kind_and_reads_stay_inside_the_worktree) |

## 9. Build order

Each step is mergeable on its own:

1. `balerix-api` workspace types; `Capability::Workspace`; `check_path`,
   `WorkspaceError`, the `WorkspaceReader` port and `FakeWorkspace` in core;
   `Ports.workspace`.
2. `balerix-runtime/src/inspect.rs` with `inspect_it.rs`.
3. Daemon routes in `plugin_api.rs`; `workspace_it.rs`.
4. SDK `Host` methods, `FakeHost` workspace, the three fixtures,
   conformance and the protocol doc rows.
5. `TmuxRunner::send_text` bracketed paste; `tmux_it` case.
6. web: manifest `observe` and `needs`; the event buffer and `events.json`.
7. web: the review page, `diff.json`, `file`.
8. web: `POST review`, the message, the `review_sent` entry, metrics.
9. e2e, `verify-claude`, `THREAT-MODEL.md`, `ARCHITECTURE.md`,
   `AGENTS.md`, `README.md`.

## 10. Done when

- A plugin declaring `workspace` reads an agent's diff, a file and a
  listing; one that does not is 403; an inactive pair is 404; no route
  reaches `home/`, the crew repo or another agent.
- `mise run test-it` proves the five change kinds, the path refusals and
  the fsmonitor control against real git.
- `balerix plugin open web`, `review` beside an agent: the diff renders
  with committed and uncommitted changes marked, a comment on a line and a
  summary are sent, the agent's terminal shows one pasted message, and the
  activity column shows the `review sent` divider followed by the agent's
  tool calls; collapsing the column survives a reload.
- The e2e `web_journey` covers the diff, the submission and the events
  through the mount; `mise run check` is green.

## 11. Out of scope, stated

No write access of any kind, no `git log` or commits tab, no `?base=`
override, no event persistence or history, no hunk expansion inline, no
threading of the agent's reply back to a comment, no resolve state, no
terminal in the review page, no per-plugin filesystem grants.

## 12. Refinements from the plan (2026-09-08)

- **`check_path` lives in `balerix-api::workspace`**, not `balerix-core`
  (§3.1 said core): the web plugin validates comment paths with the same
  rule and may depend on `balerix-api` only; the function is pure, like
  `ResizeFrame::parse` already there.
- **The submission body carries `base_ref`** beside `head` (§4.4), so the
  message header names the base without a second diff call.
- **`Cmd::run_with_exit_codes`**: `git diff --no-index` exits 1 when the
  files differ; the runtime treats that as the answer.
- **A renamed file's per-file diff names both paths** (`-- <old> <new>`);
  with the new path alone git reports an addition.
- **`old_path` is always serialized**, `null` when absent, as §2.2's
  example shows.
- **`FakeHost::fail_actions`** stands in for a runner failure so the web
  plugin's 502 path is tested through the harness.
- **`Ports.workspace` landed with the daemon routes** (build order item 3,
  not 1), since the binary wires it to the runtime implementation.
- **§8 verdicts**: the `diff --no-index` and `core.fsmonitor=false`
  assumptions are recorded at implementation, against real git
  (`inspect_it`); the bracketed paste against the real `claude`
  (`verify-claude`) is left pending — a by-hand run, not run at
  implementation.
- **`load-buffer -` from stdin instead of `set-buffer`** (§3.3): the tmux
  client packs a command's argv into one message and refuses it over
  16 KiB ("command too long"; 15000 bytes accepted, 17000 refused on the
  pinned tmux 3.7c), so a review of the 64 KiB §4.4 allows could never be
  an argument. `send_text` fills the buffer with `load-buffer -b
  balerix-send-<n> -` and hands the text to the client on its stdin; the
  `paste-buffer -p -d` and the delete-on-failure cleanup are unchanged.
- **`inspect.rs` logs its git calls argv-only** (§3.2): the crew's
  `git.log` keeps `$ argv`, stderr and the exit status, not the stdout.
  `Workspace::git`'s full-output logging is unchanged, but one `diff.json`
  fetch runs up to 500 `diff -U3` calls of 256 KiB each, and logging those
  would grow the file by the whole diff on every page load
  (`Cmd::log_argv_only`).
- **`read_file` checks the open handle, not the path** (§3.2): after
  `check_path`, `symlink_metadata` and the canonical-prefix check it opens
  the file once and requires the handle to be a regular file with the same
  `(dev, ino)`, then takes the size from that handle and reads from it —
  the agent owns the worktree and could otherwise swap the final component
  for a symlink between the check and the read.
- **The review caps bound the rendered message, not only the fields**
  (§4.4): `head` and `base_ref` are at most 256 bytes each, no rendered
  field (`summary`, `path`, `text`, `body`, `head`, `base_ref`) may carry
  a NUL, and the route refuses a rendered message over
  `MAX_MESSAGE_BYTES` (256 KiB) with `message: longer than 262144 bytes`.
  The per-field caps do not bound the message on their own: `path` (4096)
  and `text` (4096) multiply by the 200 comments.
- **A diff fails closed on a declared filter** (§5 called it the planned
  follow-up): `WorkspaceError::Filter`, 500 `repository config sets <key>;
  workspace diff refused`. Also refused on `extensions.worktreeconfig`:
  balerix never enables `worktreeConfig`, so one set means `config.worktree`
  may hold a filter the `--local` config read cannot see.
- **A single line over 4 KiB is pasted through the buffer too**:
  `send-keys -l` carries the text as argv, which tmux caps at ~16 KiB.
