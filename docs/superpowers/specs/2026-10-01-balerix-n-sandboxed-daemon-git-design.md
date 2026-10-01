# Balerix — Spec N amendment: daemon git in an agent's clone runs sandboxed

**Date:** 2026-10-01
**Status:** Approved in brainstorm 2026-10-01
**Scope:** every git call the daemon makes in an existing agent clone at
removal and at a branch change (the harvest's `upload-pack` and the
`agent_git` probes) runs under a read-only, network-blocked nono profile
the daemon renders for that purpose. Closes #68 and #70. The workspace
reader (`inspect.rs`) and the killing of processes that outlive `stop`
are out of scope and become issues of their own.

Amends Spec N (`2026-09-25-balerix-n-private-clones-design.md`) §5, §7
and §12. No change to state on disk beyond one new file per agent; no
breaking change.

---

## 1. Problem

Spec N made the harvest a fetch run in the crew cache, served by
`git upload-pack --strict <workspace>/.git`. That server process, and the
probes around it (`config`, `symbolic-ref`, `rev-parse`, and `status` on
a branch change), run as the daemon in a repository the agent can write.
The daemon reads every repository its uid can; the agent reads only its
own clone and the crew cache. A clone pointed at another repository makes
the daemon a confused deputy: the harvest copies foreign objects into the
cache, which every sibling reads.

Two things stand between that and today's code:

- `check_clone` and `check_clone_config` (Spec N §12, #67): a denylist of
  the ways a clone can reach outside `workspace/.git/objects` (a
  symlinked `.git` or object directory, `alternates`, `commondir`, a
  promisor remote). Each git feature that names a path has to be added
  to it (#68).
- The session is stopped before both call sites, so nothing races the
  check. But `stop_agent` is `tmux kill-window`; a process the agent
  detached with `setsid` survives it and can rewrite the clone between
  the check and the fetch (#70).

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| NS-1 | #70 is closed by making the daemon **immune** to a rewritten clone, not by killing survivors. | The confused deputy and a process outliving its fleet are different risks with different owners (`workspace.rs`, the tmux runner). Killing the sandbox's whole process tree depends on what nono exposes and on what works in containers; it gets its own issue. |
| NS-2 | The scope is **every git call in an existing clone at removal and branch change**: the harvest's `upload-pack` and all of `agent_git`. The workspace reader stays as it is. | These are the calls the #70 race reaches, and with them #68's title holds for the stopped-agent paths. The reader runs against a live agent on every web poll, shows foreign objects to the operator rather than writing them to the cache, and pays the per-call cost on a hot path: a different threat and cost, a follow-up issue. |
| NS-3 | The calls run under a **derived, read-only profile**, not the agent's own `nono-profile.json`. | The agent's profile carries write access to the clone, open network, the user's `sandbox` block and the agent's `env` (a user-set `GIT_*` would reach the daemon's git), and may be absent or mid-rewrite when the daemon needs it. The derived one is a subset of what the agent can read, is independent of anything the user or agent configured, and with the network blocked closes the promisor lazy-fetch route on any git version. |
| NS-4 | `check_clone` and `check_clone_config` **stay**, unchanged. | They are what gives the operator a refusal that names the path and the `--purge` remedy. The sandbox is the boundary; a stale verdict from the checks no longer matters. |
| NS-5 | **No fallback** to unsandboxed git when nono cannot run. | A fallback is the hole. A host where nono cannot run could not have launched the agent either. |

## 3. Feasibility (probed 2026-10-01, nono 0.79.0, throwaway)

With a hand-written profile of the shape in §4:

- an honest harvest through `fetch --upload-pack='… nono run --profile P
  -- git upload-pack --strict'` brings the branch into the cache; one
  sandboxed call takes about 55 ms;
- `alternates` pointed at a foreign repository with a hand-written ref:
  refused (`Permission denied` on the foreign objects, `not our ref`),
  the foreign commit absent from the cache; the same vector with a
  daemon-privileged `upload-pack` and no `check_clone` puts it there;
- `.git` replaced by a symlink to a foreign repository: refused;
- with the clone read-only: `status --porcelain` (reports an untracked
  file), `symbolic-ref`, the promisor `config` probe, `rev-parse` and the
  harvest all work; a write into the clone and a DNS lookup from inside
  are denied.

## 4. The git profile

`sandbox::render_git_profile` renders, per agent:

| Key | Value |
|-----|-------|
| `meta` | name `balerix-git-<fleet>-<crew>-<agent>` |
| `filesystem.read` | `SYSTEM_READ`; `crew.cache_objects()`; `paths.workspace`; the crew's `no-hooks/` directory (what `core.hooksPath` names) |
| `filesystem.allow` | absent |
| `workdir` | `access: none` |
| `network` | `block: true`; no `open_port` |
| `environment` | `deny_vars: ["*"]`; `set_vars`: `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1`, `GIT_TERMINAL_PROMPT=0`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CEILING_DIRECTORIES=<canonical agent root>` |

It takes no user `sandbox` block and no agent `env`. It is written to
`agents/<a>/nono-git-profile.json` (a new `AgentPaths::git_profile`),
mode 0600 through `write_profile_at`. The agent root is outside both of
the agent's `allow` paths, so the agent can neither read nor change it.

It is written just before use, at the two call sites in §5, and
validated with `validate_profile_at` only when `write_profile_at` reports
the bytes changed. Nothing depends on the order of the materialize steps
or on a file left by an earlier pass.

"No more than the agent can read" includes nono's default groups, the
same set the agent's own session gets. Objects under a path the agent
can already read could reach the cache through a rewritten clone; the
agent could have copied them into its clone anyway, so nothing is
gained.

## 5. The calls

One helper builds the sandbox prefix for an agent:

```
env -i HOME=<agent>/nono PATH=<outer path>
  <nono> -s --log-file <agent>/logs/nono-git.log run --profile <git profile> --
  <git> …
```

`HOME` is the agent's `nono/` directory, as for `launch.sh` and
`profile validate` (nono's state root must not be the agent's `home/`).

- **`agent_git`** runs the prefix followed by today's arguments:
  `-c core.fsmonitor=false`, `-c core.hooksPath=<no-hooks>`, `-C`,
  `--git-dir`, `--work-tree`, then the subcommand. The hardening
  variables `harden_agent_git` sets on the command's environment are in
  the profile's `set_vars` for these calls, since nono drops every other
  variable; the five repo-locating `GIT_*` variables cannot arrive at
  all. `harden_agent_git` itself stays for the workspace reader.
- **`harvest`** keeps its fetch in the cache, run as the daemon with the
  flags it has today (`--quiet --no-auto-gc`, the `+` refspec). Only the
  `--upload-pack` string changes: the prefix, each word shell-quoted
  (`quote::sh_quote`), followed by `'<git>' upload-pack --strict`.

Order at each call site (`harvest_and_remove`, and the branch-change
path of `ensure_clone` after the marker's reuse check):

1. `check_clone` (filesystem only, as today);
2. write the git profile, validating it if it changed;
3. `check_clone_config`, sandboxed;
4. the remaining probes (`symbolic-ref`, `status`, `rev-parse`) and the
   harvest, sandboxed.

The reuse path (marker equals `branch`) still makes no git call and
writes no profile. `create_clone` (a clone the agent has never written),
every git call in the cache, and `inspect.rs` are untouched.

## 6. Failure handling

- When nono cannot run (no Landlock, a profile nono rejects) the call
  fails. At removal that fails the removal, as a failed harvest does
  today; at a branch change the agent is `Failed` and retried at the
  resync cadence. `--purge` remains the way past: it deletes without a
  harvest or a check.
- A failure is still a `MaterializeError::Tool` naming the id, `git`, the
  git subcommand and git's first stderr line, not `nono run`. A sandbox
  denial reads as git's own `Permission denied` on the path.
- A surviving process that rewrites the clone during the harvest can
  make it fail or carry a broken branch. That costs the agent only its
  own work and is accepted.

## 7. Security

`docs/THREAT-MODEL.md`:

- The row **"The daemon runs read-only `git` in a repository an agent can
  write to"** gains, for the clone step: those calls and the harvest's
  `upload-pack` run under a daemon-rendered nono profile that reads the
  clone and the crew cache's objects, writes nothing and has no network,
  so a clone pointed elsewhere serves nothing the agent could not read,
  and a promisor remote fetches nothing on any git version.
  `check_clone` and `check_clone_config` are the second layer and the
  source of the operator's message. Evidence: `workspace_it` (the
  foreign-repository vectors with the checks off; the rewrite after the
  check).
- The same row keeps today's wording for the workspace reader, and
  records as residue that the reader still runs with the daemon's
  privileges behind `refuse_filters` and `harden_agent_git`.
- Residue recorded: a process the agent detached outlives `stop`; it
  stays inside the agent's sandbox and can no longer steer the daemon's
  git, but it runs until it exits.

## 8. Testing

- `sandbox.rs` unit: the git profile has no `allow`, has `network.block`,
  no `open_port`, and exactly the five `set_vars`; every `read` path is
  in `balerix_grants(…).read` or `.allow`, or is the `no-hooks`
  directory.
- `workspace_it`:
  - the existing cases pass through the sandboxed calls unchanged in what
    they assert (harvest, seed, branch change, dirty refusal, the error
    shape, the unreadable-HEAD cases);
  - `a_clone_pointed_at_another_repository_is_refused_and_nothing_is_harvested`
    runs each vector twice: with the checks, asserting today's message;
    with the checks off, asserting the foreign commit is absent from the
    cache (#68's third checkbox);
  - the promisor case with the checks off: nothing foreign reaches the
    cache, and the planted `uploadpack` program, if it runs at all,
    reaches no network and writes no file;
  - new, for #70: the clone is rewritten after `check_clone` has passed
    and before the fetch; nothing foreign is harvested;
  - new: a daemon git call cannot write the clone (a config that would
    make `status` write leaves the tree byte-identical).
- "Checks off" is a test-only constructor in `balerix-runtime`'s existing
  `testing` module; production code cannot disable the checks.
- The sandboxed cases need Landlock and follow the existing rule: skip
  with a printed reason, fail under `BALERIX_REQUIRE_TOOLS=1`.
- `materialize_it`, `e2e`: unchanged in what they assert.

## 9. Documents and issues

- Spec N gains a dated bullet in §12 pointing here; §5 and §7 are read
  with this amendment.
- `ARCHITECTURE.md`: a non-obvious decision, *Daemon git in an agent's
  clone runs under a read-only nono profile*; the agent-root layout
  lists `nono-git-profile.json`.
- `AGENTS.md`: one gotcha: for the clone step the hardening variables
  live in the git profile's `set_vars`, not the command's environment;
  adding one to `harden_agent_git` alone does not reach those calls.
- PR title `fix(runtime): run daemon git in an agent's clone under a
  read-only nono profile (#68, #70)`; it closes both.
- New issues, opened with the PR:
  - `stop` ends the sandbox's whole process tree and waits for it to be
    empty (#70's first and third checkboxes);
  - the workspace reader runs under the git profile.

## 10. Deliberately deferred

- Killing survivors (NS-1) and the workspace reader (NS-2).
- Removing `check_clone` or `check_clone_config`. They cost one
  filesystem walk and one sandboxed call and give the operator the
  message.
- A shared, per-crew git profile. Per agent keeps `workspace/` the only
  clone a call can read.

## 11. Done when

1. Every git call in an existing clone at removal and branch change runs
   under the git profile; no code path runs those calls without it.
2. The foreign-repository vectors and the #70 race case pass with the
   checks off.
3. `mise run check`, `mise run test-it` and `mise run e2e` pass.
4. `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md` and Spec N §12
   read as §7 and §9 say; #68 and #70 are closed and the two follow-up
   issues exist.
