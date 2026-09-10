# Balerix — Spec D: The live diff

**Date:** 2026-09-08
**Status:** Approved in brainstorm 2026-09-08. Builds on
`2026-09-08-balerix-c-workspace-review-design.md` (*Spec C*) §2–§4;
`docs/plugin-protocol.md` is the wire contract it extends.
**Scope:** a fourth workspace route, `version`, answering a cheap fingerprint
of the agent's worktree; the web plugin's review page polls it through the
`events.json` it already polls and re-renders the diff when the fingerprint
changes, keeping the reviewer's draft, scroll position and open comment box
intact, with an "updated N s ago" indicator.

Where this document and Spec C disagree, this document wins.

---

## 1. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| PD-1 | **Change detection is a daemon-side fingerprint**, not the hook events and not a repeated full diff. | Events miss changes outside observed tools and fire on every `ls`; a full diff per poll is what the fingerprint exists to avoid. Three name-only git calls and a few `stat`s cost milliseconds. |
| PD-2 | **The fingerprint rides on `events.json`.** The page keeps one 2 s poll; the plugin calls `workspace_version` inside it. | One timer, one request; the column and the diff move together. |
| PD-3 | **A changed diff is applied automatically**, rate-limited to one apply per 3 s, and deferred while a comment box is open. | The reviewer asked for the diff to stream like the events. Deferral is what keeps an in-progress comment from being disrupted; the draft itself is never at risk (PC-7). |
| PD-4 | **Comments follow moved lines.** A comment whose `(path, side, line, text)` anchor fails is re-anchored by `(path, side, text)` when that matches exactly one line. | The agent inserting lines above a comment must not orphan it; a changed or vanished line still goes to the stale box, as in Spec C. |
| PD-5 | **The fingerprint covers content, not index state.** `HEAD`, the merge-base, and every changed or untracked path's size and mtime. A `git add` that changes no bytes leaves it unchanged. | The diff the page renders is worktree-against-merge-base; index-only changes do not change what it shows. |

## 2. The `version` route

### 2.1 Wire type (`balerix-api/src/workspace.rs`)

```rust
pub struct WorkspaceVersion { pub head: String, pub fingerprint: String }
```

`head` is the full sha; `fingerprint` is 64 hex chars (SHA-256). The page
compares fingerprints for equality and never interprets them.

### 2.2 Route

`GET /v1/plugin-host/agents/{fleet}/{crew}/{agent}/workspace/version`,
gated exactly like the other three (bearer, `workspace` capability, an
active pair); 404 `no workspace for agent <id>` when the worktree is absent;
the filter refusal and git failures map as for `diff` (500). No `path`
parameter.

### 2.3 Port and runtime

`WorkspaceReader` gains

```rust
fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError>;
```

`Runtime::version` (`inspect.rs`), through `inspect_git` with every Spec C
control and the `FILTER_KEYS` check first:

1. `rev-parse HEAD` → `head`; `merge-base <base_ref> HEAD` → `merge_base`.
2. The path set: `diff --name-only -z <merge_base>` ∪ `diff --name-only -z
   HEAD` ∪ `ls-files --others --exclude-standard -z`, sorted, deduplicated.
3. For each path, `symlink_metadata` under the worktree (never following a
   symlink): `size` and `mtime` as nanoseconds since the epoch, or the word
   `missing` when the path is gone (a deletion between the listing and the
   `stat` still changes the fingerprint).
4. `fingerprint = hex(sha256(head \0 merge_base \0 (path \0 size \0 mtime \0)*))`.

The hashing is a pure function `fingerprint_of(head, merge_base, entries)`
in `inspect.rs`, unit-tested on its own. `sha2` and `hex` are already
workspace dependencies; `balerix-runtime` adds them as ordinary
dependencies.

`FakeWorkspace::version` answers `head` from the stored diff and a
fingerprint hashed from the file map's paths and bytes, so a `set` with
different bytes changes it.

### 2.4 SDK, fake host, fixture

`Host::workspace_version(agent) -> Result<WorkspaceVersion, SdkError>` with
the client's default timeout. `FakeHost` serves the route from
`set_workspace`'s data with the same derivation as `FakeWorkspace`.
`docs/plugin-protocol/workspace-version.json` (`plugin-to-daemon`, 200) is
the twenty-second fixture; `docs/plugin-protocol.md` §3 gains the row and
the **Workspace** paragraph a sentence; §6 counts twenty-two.

## 3. The web plugin

### 3.1 `events.json`

The body becomes `{ "phase", "events", "workspace" }`, where `workspace` is
`{ "head", "fingerprint" }` from `Host::workspace_version`, or `null` when
the call fails for any reason (no worktree, a refused filter, a transport
failure — logged at debug, never an error status, since the column must
keep flowing). The call is made on every poll; nothing is cached.

### 3.2 The page

State: `rendered` (the fingerprint of the diff on screen, or none),
`pendingDiff` (a fetched diff waiting to be applied), `lastApplied` (a
timestamp), `renderedAt` (for the indicator).

Each poll: if `workspace` is non-null and its fingerprint differs from
`rendered` and from the fingerprint of `pendingDiff`, fetch `diff.json`
and hold it as `pendingDiff` together with the fingerprint. Then, if no
comment box is open and at least 3 s have passed since `lastApplied`,
apply it. Saving or cancelling a comment box applies a waiting
`pendingDiff` at once. "Reload diff" fetches and applies unconditionally.

Applying:

- **Re-anchoring** (`anchorComments(diff, comments)`, a standalone
  function in the inline script): for each comment, exact match on
  `(path, side, line, text)` keeps it; otherwise the lines of the same
  `path` and `side` whose `text` equals the comment's are collected; exactly
  one hit rewrites `comment.line` to that line's number; zero or several
  hits mark the comment stale. Stale comments render in the "no longer in
  the diff (still sent)" box as today. The draft is saved after
  re-anchoring so a re-anchored line number persists.
- **Scroll**: the diff column's `scrollTop` is saved before the render; after
  it, the first file header that was at or above the top of the viewport is
  scrolled back to the top when it still exists, else the offset is
  restored.
- **Comment boxes**: text typed into a pending or editing box is written to
  the draft on every `input` event, so a render restores it. Deferral makes
  this a second line of defence, not the first.
- **The summary box** is assigned only when its value differs from the
  draft, so the caret does not move.
- **The indicator**: the header line reads `against origin/main at 3f9c2a1 ·
  updated 12 s ago`, the age ticking every second from `renderedAt`; the
  span flashes (a short background transition) when an apply lands. Before
  the first diff it reads `loading diff...`.

### 3.3 Metrics

`diff_refreshes_total` (counter, applies) and `version_failures_total`
(counter, `workspace_version` errors during a poll), through the SDK's
`Metrics`.

## 4. Security

No new trust boundary. The route runs the same hardened git invocations
as `diff` (Spec C §3.2, the follow-ups' filter refusal, submodule flags and
discovery ceiling); it emits no content, only a hash. `THREAT-MODEL.md`'s
workspace rows list the fourth route. The page renders the indicator and
the fingerprint through `textContent` like every other value.

## 5. Docs

- `docs/plugin-protocol.md`: the row, the paragraph sentence, the count.
- `ARCHITECTURE.md`: the Spec C paragraph gains "…and `version`, a cheap
  fingerprint the review page polls to refresh the diff live".
- `AGENTS.md`: one gotcha — the fingerprint covers content (size and
  mtime of changed and untracked paths), `HEAD` and the merge-base; an
  index-only change is invisible by design; a same-size same-mtime rewrite
  within the mtime's resolution is the theoretical miss.
- `docs/THREAT-MODEL.md`: the route list and the workspace mitigation row.
- `README.md` upgrade note: `Host::workspace_version` is new; nothing
  existing changed.

## 6. Testing

| Layer | What | Where |
|---|---|---|
| unit | `fingerprint_of`: deterministic; changes with `head`, the merge-base, any path, size, mtime, or a `missing` marker; the fake's fingerprint changes with a file's bytes; `anchorComments` (through `node`, as `parsePatch` was checked): exact keep, unique re-anchor, ambiguous and vanished → stale | in-module; `routes.rs` tests extract the script |
| runtime (`inspect_it`) | stable across two calls; changes on an edit, a new untracked file, a commit, a deletion; unchanged on a byte-identical `git add`; refused by the filter check; `Missing` before the worktree | `balerix-runtime/tests` |
| server (`workspace_it`) | the fourth route behind the gate; 403 and the two 404s | `balerix-server/tests` |
| conformance | the fixture through `Host` against `FakeHost`; 22 | `balerix-plugin-sdk/tests/conformance.rs` |
| plugin (`plugin_it`) | `events.json` carries `workspace` for an agent with a workspace, `null` without; the fingerprint changes after `set_workspace` with different bytes; the metrics | `balerix-plugin-web/tests` |
| page | `review_html` carries the indicator and the poll's `workspace` handling | `routes.rs` tests |
| e2e (`web_journey`) | `events.json` shows a fingerprint after `NOTES.md` is written; a second write changes it and `diff.json` shows the new content | `balerix/tests/e2e.rs` |
| by hand | `verify-claude`: edit a file while the page is open; the diff updates and the indicator flashes; with a comment box open the update waits until Save | `scripts/verify-claude.sh` |

## 7. Verify at implementation time

| Assumption | Fallback | Verdict |
|---|---|---|
| `symlink_metadata().modified()` has sub-second resolution on the host filesystem, so an edit within the same second still changes the fingerprint | hash the first 4 KiB of each changed file as well | Verified 2026-09-08 (inspect_it: an edit within the same second changed the fingerprint) |
| Scrolling the previously-top file header back into view after a render feels stable in the browser | restore the raw `scrollTop` only | Verified 2026-09-09 (by-hand `verify-claude` against the real `claude`: the diff refreshed live and the header snap-back read as stable; no fallback) |

## 8. Build order

1. `WorkspaceVersion`, the port method, `fingerprint_of`, `FakeWorkspace::version`.
2. `Runtime::version` with `inspect_it`.
3. The daemon route with `workspace_it`.
4. SDK, `FakeHost`, the fixture, conformance, the protocol doc.
5. `events.json`'s `workspace` field, the metrics, `plugin_it`.
6. The page: `anchorComments`, deferral, scroll, the indicator, the poll.
7. The e2e, `verify-claude`, the docs.

## 9. Done when

- `events.json` for an enabled agent with a worktree carries a fingerprint
  that changes when the agent edits, adds, deletes or commits, and not when
  nothing changed.
- With the review page open, an edit in the worktree appears within a few
  seconds with the indicator flashing; a comment box being typed into is
  untouched until it is saved; the scroll position holds; a comment on a
  line that moved stays on it.
- `mise run check`, `mise run test-it` and `mise run e2e` are green; the
  by-hand check is recorded in §7.

## 10. Out of scope, stated

No server push (WebSocket or SSE) for the diff; no per-file incremental
rendering; no history of versions; no change to the submission.

## 11. Refinements from the plan (2026-09-08)

- **`diff_refreshes_total` counts `diff.json` responses**, the one server-side event per refresh; the browser's applies are not observable.
- **The fakes' fingerprint** is `sha256(head \0 (path \0 bytes \0)*)` over the file map, in core and in the SDK alike, so `workspace-version.json` holds a derivable value.
- **The first diff comes from the first poll**, which carries the fingerprint; `loadDiff()` is the manual reload and the path that shows a daemon refusal in the banner.
- **`anchorComments` is checked with `node` by hand** (not a `mise.toml` tool); the Rust test asserts the function's presence.
- **`save()` strips `editing` and `typing`**, so a reload never reopens a comment box (a Spec C deferred minor).
- **The mtime is hashed as `{seconds}.{nanoseconds:09}`** (`(size, mtime, mtime_nsec)` from `symlink_metadata`), information-equivalent to §2.3's nanoseconds since the epoch.
