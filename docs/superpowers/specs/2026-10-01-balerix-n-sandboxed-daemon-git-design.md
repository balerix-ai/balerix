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
| `filesystem.read` | `SYSTEM_READ`; `crew.cache_objects()`; `paths.workspace`; the crew's `no-hooks/` directory (what `core.hooksPath` names); the canonical path of the `git` binary `ToolPaths` discovered (a single file, as the agent's profile grants `mise`); the canonical `git --exec-path` directory (§12.1). A git whose libraries sit outside `SYSTEM_READ` cannot run under it (§12.1) |
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
  directory or the `git` binary.
- `sandbox_it`: inside the git profile, the clone and the cache's
  objects read; a write to the clone, a read of the agent's `home/` and
  a read outside both are denied.
- `workspace_it`:
  - the existing cases pass through the sandboxed calls unchanged in what
    they assert (harvest, seed, branch change, dirty refusal, the error
    shape, the unreadable-HEAD cases);
  - `a_clone_pointed_at_another_repository_is_refused_and_nothing_is_harvested`
    keeps asserting today's messages with the checks in place;
  - new, for #70 and #68's third checkbox: each vector (alternates, a
    symlinked `.git`, a symlinked pack, `commondir`, a promisor remote)
    is applied *after* both checks have passed and before the harvest,
    which is the race itself and equivalent to the checks being off for
    that vector; the foreign commit is absent from the cache, and the
    planted `uploadpack` program has not run.
- The hook that runs between the checks and the harvest is reachable
  only through `balerix-runtime`'s existing `testing` module; the public
  `harvest_and_remove` passes a no-op, and no code path skips the
  checks.
- The sandboxed cases need Landlock and follow the existing rule: skip
  with a printed reason, fail under `BALERIX_REQUIRE_TOOLS=1`.
- `materialize_it`, `e2e`: unchanged in what they assert.

## 9. Documents and issues

- Spec N gains a dated bullet in §12 pointing here; §5 and §7 are read
  with this amendment.
- `ARCHITECTURE.md`: a non-obvious decision, *Daemon git in an agent's
  clone runs under a read-only nono profile*.
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

- Killing survivors (NS-1; done in §13) and the workspace reader (NS-2).
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

## 12. Amendment 2026-10-01: the profile's limits (#109)

**Status:** Approved in brainstorm 2026-10-01. Found by the final review
of the work above. Two limits of the sandboxed calls, and three tool
bumps that ride in the same PR.

### 12.1 The exec-path grant

The profile of §4 grants the git binary as one file. A git installed
outside `SYSTEM_READ` (`/usr`, `/lib`, `/lib64`, `/bin`) also needs its
`libexec/git-core`: `upload-pack` spawns `git pack-objects` through it.

- `write_git_profile` runs `<git> --exec-path` before it renders: as the
  daemon, unsandboxed, with no repository argument, from an empty
  environment but `PATH` (the outer path), as the sandboxed call
  starts: a `GIT_EXEC_PATH` the daemon inherited would otherwise make
  the grant name a directory the sandboxed git never uses. It is the
  host's git answering about itself; nothing the agent wrote is read.
  The answer is canonicalized (Landlock rules bind
  to what a path resolves to) and passed to `render_git_profile`, which
  adds it to `filesystem.read`.
- A `--exec-path` that fails, or names a directory that does not exist,
  fails the step as a `MaterializeError::Tool`. No profile is written
  without the grant (NS-5).
- On a host whose git sits under `/usr` the directory is already inside
  `SYSTEM_READ` and the profile grants nothing new in effect.

| # | Decision | Rationale |
|---|----------|-----------|
| NS-6 | Library prefixes are **not** granted, by setting or by derivation. | A git from nix or Linuxbrew needs its loader and libraries from its own prefix, and a mise shim resolves to the mise binary. Nobody has reproduced the failure on such a host; a daemon-level setting is a new configuration surface on a sandbox boundary, and deriving the directories from the binary widens the grant without the operator choosing to. The limit stays documented and fails closed: removal, `down` without `--purge` and a branch change fail there, and `--purge` is the way past. A follow-up issue tracks the setting. |

What this covers: a git under a prefix such as `/opt` or `/usr/local`
that links the system's libraries.

### 12.2 A sandbox failure is never git's "no"

Three calls through `agent_git` accept exit 1 as an answer: the promisor
probe (`config --get-regexp`), `rev-parse --verify --quiet` before the
harvest, and `symbolic-ref -q` in `head_branch`. `prepare_sandbox` proves
once that nono starts, but nono's own failure on a later call also exits
1 and read as "no keys", "branch absent" or "detached": the harvest was
skipped and the clone deleted unharvested.

- `CmdOutput` carries `stderr`. In `agent_git`, an accepted non-zero exit
  whose stderr is not empty is a `MaterializeError::Tool` with that
  stderr. Git is silent on exit 1 for all three probes; nono prints
  `nono: …`.
- The rule fails closed both ways: a git warning beside a real "no" also
  fails the step, as any git error already does (#74).
- Exit 0 is untouched; stderr there is not inspected.
- When the canary in `prepare_sandbox` fails, the error's stderr is
  prefixed with `the sandbox did not start; see
  <agent>/logs/nono-git.log: `. A prefix, not a line of its own: the
  error's display is the first stderr line.

Re-running the canary after each "no" was rejected: it doubles the
sandboxed calls (about 55 ms each, §3) and leaves a window between the
answer and the check.

### 12.3 Tests

- `sandbox.rs` unit: the rendered profile's `read` holds the exec-path
  given; §8's "every `read` path" assertion admits it.
- The exec-path fixture is a wrapper script under `target/tmp` used as
  `ToolPaths::git`: `exec <host git> --exec-path=<dir under target/tmp>
  "$@"`, so `--exec-path` answers that directory. A harvest through it
  would not tell a granted exec-path from a denied one, because git
  falls back to the host `git` on `PATH` for its helpers; so the grant
  is asserted directly:
  - `sandbox_it`: `write_git_profile` with the wrapper writes a profile
    whose `read` holds the canonical directory, and a file in it reads
    inside the sandbox (the existing case proves a path outside the
    grants does not);
  - `sandbox_it`: a wrapper whose `--exec-path` exits non-zero, and one
    naming a missing directory, fail `write_git_profile` and leave no
    profile.
- `workspace_it`, removal: a nono shim that passes `validate` and the
  canary, then exits 1 printing `nono: …` on later calls;
  `harvest_and_remove` fails, the clone stays, the cache has no branch.
- `workspace_it`, branch change: the same shim under `ensure_clone` with
  a changed `branch`; it fails and the clone stays.
- The existing "branch absent", "no promisor keys" and detached-HEAD
  cases prove that git's own exit 1 is silent and still a "no".
- The canary's hint: asserted in
  `a_nono_that_cannot_run_fails_the_removal_and_keeps_the_clone`.

### 12.4 Tool bumps in the same PR

`mise.toml`: rust 1.98.1 → 1.99.0, claude 2.1.286 → 2.1.287, trivy
0.74.0 → 0.75.0.

- rust: `mise run check` and `mise run plugins` pass, new lints fixed.
- claude: the embedded tool table changes what agents get, so
  `mise run verify-claude` and `mise run verify-questions` are run and
  their results stated in the PR.
- trivy: CI only.

### 12.5 Documents and issues

- `render_git_profile`'s doc comment loses "need not sit under `/usr`"
  and states §12.1 and NS-6.
- `AGENTS.md` (the clone-step gotcha) and the README's limit: the
  exec-path is granted, library prefixes are not; an accepted exit 1
  with stderr is a failure.
- `docs/THREAT-MODEL.md`: the clone-step row names the exec-path grant
  as part of what the profile reads.
- PR title `fix(runtime): grant git's exec-path in the git profile; a
  sandbox failure is never git's "no" (#109)`; it closes #109, the bumps
  named in the body.
- New issue, opened with the PR: a daemon-level setting for read-only
  library prefixes in the git profile (NS-6).

### 12.6 Done when

1. The git profile grants the canonical exec-path, and no profile is
   written without it.
2. No accepted exit 1 with stderr reads as "no"; the two shim tests and
   the exec-path tests pass.
3. `mise run check`, `mise run plugins`, `mise run test-it` and
   `mise run e2e` pass on rust 1.99.0; `verify-claude` and
   `verify-questions` pass on claude 2.1.287.
4. The documents read as §12.5 says; #109 is closed and the follow-up
   issue exists.

## 13. Amendment 2026-10-01: `stop` ends the whole process tree (#107)

**Status:** Approved in brainstorm 2026-10-01. Takes up NS-1's deferred
half: a process an agent detached with `setsid` or a double fork
outlives `tmux kill-window`, keeps running in the agent's sandbox with
network after `down`, and can write into `workspace/` while the daemon
deletes it.

**Outcome:** when `stop_agent`, `stop_crew` or a restart returns `Ok`,
no process the agent started is alive.

### 13.1 Feasibility (probed 2026-10-01, nono 0.79.0, throwaway)

On the development host: an unprivileged container, pid 1 not systemd,
the cgroup v2 root owned by root.

| Candidate | `setsid` sleeper | double-forked sleeper | Runs here |
|---|---|---|---|
| `tmux kill-window` (today) | survives | survives | yes |
| `nono stop --force <session>` | survives | survives | yes |
| nono's cgroup (`--max-processes`) | not reached | not reached | no: `no delegated cgroup v2 subtree for this session` |
| A subreaper wrapper in front of nono | killed | killed | yes |

- nono is not a subreaper: the orphans re-parent to pid 1.
- The wrapper emptied the tree about 10 ms after the hangup, and was
  gone 35–65 ms after `kill-window`, in 4 of 4 runs; three of them had a
  chain in which each generation detaches the next and exits, every
  1–10 ms.
- From inside the sandbox, `kill -0 <wrapper>` fails with `Operation not
  permitted`.

### 13.2 Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| NS-7 | The tree is tracked by a **child subreaper** in front of nono, not by a cgroup or by nono's session. | It is the only candidate that killed the survivors and the only one that needs no delegation from the host. A cgroup is the stronger mechanism where it exists; nono refuses to start with one in an unprivileged container. |
| NS-8 | The wrapper is a hidden `balerix` subcommand, not a second binary. | `ToolPaths` already carries the `balerix` path; the wrapper always matches the daemon that wrote `launch.sh`; nothing new to ship or discover. |
| NS-9 | On stop: hangup to the main child, a fixed 2 s grace, then SIGKILL until empty. Not configurable. | claude and nono get the signal they get today and time to flush (nono's audit ledger). Nobody has asked to tune the value. |
| NS-10 | When the main child exits on its own, the rest of the tree is killed at once. | One rule: no agent process outlives its main process. Leaving the wrapper alive to hold survivors would make `observe` report a dead agent as `Running`. |
| NS-11 | `stop` learns the tree is empty from the wrapper's exit: the pane's pid and its start time, polled. | The wrapper exits only when it has no children. Everything stays inside `TmuxRunner`; the `AgentRunner` port does not change. A lock file in the agent's directory was rejected: the runner knows no agent path at `stop`. |
| NS-12 | A wrapper still alive after 5 s fails the call. | The reconciler reports and retries it; removal never reaches the workspace delete with the agent's processes alive. |

### 13.3 The wrapper: `balerix agent-supervise -- <argv…>`

A hidden subcommand beside `hook-relay`. The logic is a new
`balerix-runtime` module, `supervise.rs`; the subcommand installs the
signal handlers (tokio, before the child exists) and hands the module a
channel that carries one message per signal.
`rustix` (already in the lockfile) becomes a direct dependency of
`balerix-runtime` for `set_child_subreaper`, `kill_process` and
`waitpid`. No `unsafe`.

1. Mark the process a child subreaper. A failure is fatal: exit
   non-zero with the error on stderr, nothing spawned.
2. Spawn `<argv>` with stdin, stdout, stderr and the environment
   inherited unchanged.
3. Wait for the first of: the main child exits; SIGHUP, SIGTERM or
   SIGINT arrives.
4. On a signal: send SIGHUP to the main child, then wait up to 2 s
   (`STOP_GRACE`) for the wrapper to have no children. A further signal
   during the grace changes nothing.
5. On the main child's own exit: no grace.
6. The kill loop, in both cases: list every descendant from `/proc`
   (parent pids, starting at the wrapper's own pid), SIGKILL each, reap
   with `waitpid`, repeat until `waitpid` reports no children. It does
   not give up; the caller's bound (§13.5) reports a stuck one.
7. Exit status: the main child's (128+n if a signal killed it), so
   `pane_dead_status` keeps its meaning; 143 if the wrapper was stopped
   before the main child exited.

A descendant is any process whose chain of parents reaches the wrapper.
Because the wrapper is a subreaper, an orphan anywhere below it
re-parents to it, never to pid 1, whatever session or process group the
orphan joined.

### 13.4 `launch.sh`

`render_launch` puts `<balerix> agent-supervise --` in front of the nono
argv; `LaunchPlan::argv` carries the same prefix. Environment, cwd and
"safe to run by hand" are unchanged: Ctrl-C by hand tears the tree down
through step 4. The wrapper runs outside the sandbox; the nono profile
does not change.

### 13.5 The runner waits

All in `TmuxRunner`. The port and `FakeRunner` do not change.

- A helper reads a process's identity: pid, and the start time from
  field 22 of `/proc/<pid>/stat`. "Gone" means the stat file is absent
  or its start time differs (the pid was reused).
- `stop_agent`: read the pane's pid and identity, `kill-window`, then
  poll every 10 ms until it is gone. A window that does not exist, or a
  pane already dead, is `Ok` as today.
- `stop_crew`: collect the identities of every pane in the group's
  sessions before the kills, then wait for all of them against one
  deadline. The anchor's idle shell is one of them.
- `ensure_agent` on a window in `Running`: `respawn-window -k` into the
  idle placeholder, wait for the old pane's identity to be gone, then
  `pipe-pane` and `respawn-window` into the script as the other arms do.
  Today it respawns over the live process, and the new agent would start
  while the old tree is dying.
- The bound is 5 s (`STOP_WAIT`): the grace plus a margin. Past it the
  call returns a new `RunnerError::StillRunning` for that agent or crew,
  whose message is `agent processes still running after stop (pid <n>)`.
  The bound is a field of `TmuxRunner` so a test can shorten it. Nothing is
  rolled back; the next reconcile pass calls stop again, which finds no
  window and waits on nothing. So the error is reported once and the
  retry can succeed while survivors remain. That is acceptable only
  because the wrapper's loop does not give up; §13.6 names the case
  where there is no wrapper.

### 13.6 Limits

Stated in `docs/THREAT-MODEL.md`:

- A wrapper that is itself SIGKILLed (the OOM killer, an operator)
  leaves its orphans to pid 1, as before this amendment. The sandboxed
  agent cannot do it: signalling the wrapper from inside the sandbox is
  refused (§13.1), and a test pins that.
- The kill loop races a process that forks faster than one `/proc` scan.
  It won every probe run; it is not a proof. A cgroup would be one, and
  is not available in an unprivileged container.
- An agent launched before the upgrade has no wrapper until its next
  launch. `stop` waits for nono's pid and otherwise behaves as before:
  survivors are not found. No restart is forced.
- Linux only, like the sandbox.

### 13.7 Tests

- `supervise.rs` unit: the `/proc/<pid>/stat` parser (a command name
  holding `) `), the descendant walk, and a process's identity (a zombie
  and a reaped pid are both gone).
- `crates/balerix/tests/cli_supervise.rs`, the real binary against real
  processes, no nono. A subreaper must be its own process: set in a test
  process it would adopt, and kill, other tests' children. Each test
  finds its processes by a marker unique to the test:
  - after SIGHUP, a `setsid` sleeper and a double-forked one are dead
    when the wrapper returns;
  - a main child that exits on SIGHUP ends the wrapper well inside the
    grace;
  - the main child's own exit kills the rest, and its status is the
    wrapper's;
  - a chain in which each generation detaches the next and exits is
    emptied.
- `launch.rs` unit: the script and `argv` start with the wrapper.
  - a main child that ignores the hangup is killed after the grace, and
    a second signal during the grace changes nothing;
  - a command that does not exist fails with a message, and the child's
    stdin and stdout pass through.
- `cli_supervise.rs`, with tmux and the wrapper in front of a script:
  #107's case, an agent that `setsid`s a sleeper; when `stop_agent`
  returns, no process of it is alive. The same through `stop_crew`, and
  through `ensure_agent` on a running window.
- `tmux_it.rs`, no wrapper: `stop_agent`, `stop_crew` and a restart
  return only when a pane process that takes half a second to die is
  gone; a pane process that ignores the hangup fails the call after the
  bound with the message of §13.5; a window whose pane already exited
  stops at once.
- `sandbox_it.rs`, real nono, the agent's generated profile: a sandboxed
  process that signals a process outside the sandbox (the test process,
  same user) is refused.
- By hand: `mise run verify-claude`, because the wrapper now sits
  between tmux and nono's terminal handling. Its result is stated in the
  PR.

### 13.8 Documents and issues

- `docs/THREAT-MODEL.md`: a clause for the process tree, with §13.6's
  limits.
- `ARCHITECTURE.md`: the wrapper in the launch path, one line.
- `materializer.rs`: the comments that say nono writes "shortly after
  `tmux kill-window` returns" are corrected if the wait makes them
  untrue; `retry_rmdir` stays (a purge of a plugin's directory, and
  agents without a wrapper).
- §10's "Killing survivors (NS-1)" is done by this section.
- PR title `fix(runtime): stop ends the sandbox's whole process tree and
  waits for it to be empty (#107)`; it closes #107.

### 13.9 Done when

1. Every `launch.sh` starts the agent under the wrapper, and
   `stop_agent`, `stop_crew` and a restart return only when the pane's
   process is gone, or fail after `STOP_WAIT`.
2. The tests of §13.7 pass; `mise run check`, `mise run test-it` and
   `mise run e2e` pass; `verify-claude` passes.
3. The documents read as §13.8 says; #107 is closed.
