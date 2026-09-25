# Balerix — Spec N: a private clone per agent

**Date:** 2026-09-25
**Status:** Approved in brainstorm 2026-09-25
**Scope:** git isolation between the agents of one crew. Every agent gets
its own clone of the repository instead of a worktree hanging off a
shared crew clone; the crew clone becomes a daemon-owned, read-only
object cache the private clones borrow objects from; the daemon harvests
an agent's branch into that cache before it deletes the clone, so
unpushed work survives `down --keep-repos` and agent removal as it does
today. A breaking change for state on disk: fleets created by 0.1.x must
be purged and brought up again.

Depends on Spec L (the per-agent `branch`, the branch marker). Spec M is
unaffected in interface; one sentence of its session model changes.

---

## 1. Problem

Spec A2 §6 gave each crew one `git clone --no-checkout` under
`crews/<c>/repo/` and each agent a worktree of it under
`agents/<a>/workspace/`. A worktree's `.git` is a file pointing into the
crew clone, so every agent's nono profile grants write access to
`crews/<c>/repo/.git`. `docs/THREAT-MODEL.md` records the consequence as
an accepted risk: "Git isolation between agents of the same crew — they
share `.git` by design; agents in a crew trust each other."

That trust is broader than it reads. From inside its sandbox one agent
can delete or reset a sibling's branch, run `git gc` under a sibling
mid-operation, set `core.hooksPath` or a clean filter in the shared
`.git/config` that every sibling's git then honours, or corrupt the
object store for the whole crew. A compromised agent, the threat model's
own actor, reaches every other agent's work through the one path they
share. The same sharing is what makes two agents on one `branch` fail at
`git worktree add` (#63): one clone cannot check a branch out twice.

The shared clone was chosen for cost: one object store and one fetch per
crew. That property is kept.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| N-1 | Each agent's `workspace/` is a **full clone** of origin, made with `--reference <crew repo>`. | Refs, index, config, hooks and HEAD become the agent's own. Objects already in the cache are neither transferred nor duplicated, so disk and network stay where the shared clone put them. `origin` stays the real remote, so pushes go where they go today. |
| N-2 | The crew `repo/` becomes an **object cache the daemon alone writes**: `gc.auto=0` at creation, `--no-auto-gc` on every fetch, never pruned. | A borrower is only safe while the reference never loses an object. Auto-gc after `fetch` is the one write git would do on its own; forbidding it makes the cache append-only. The path and the `--no-checkout` form are unchanged. |
| N-3 | Agents get the cache **read-only** (`crew.repo/.git/objects`), and no other path in common. | That is the isolation. A read-only alternate cannot be corrupted, and an append-only store is safe to read while the daemon fetches into it. |
| N-4 | Before deleting a clone the daemon **harvests** the agent's assigned branch into the cache, and a new clone for a branch the cache holds **seeds** from it. | Today an agent's unpushed commits survive `down --keep-repos` and removal in the crew clone, and Spec M-10 resumes a session from them. A private clone dies with its directory; the cache is where its branch lives on. |
| N-5 | The harvest is a **fetch run in the cache**, never a push run in the clone. | A push from the agent's clone honours the clone's `.git/config`; an `url.<x>.insteadOf` there would send the daemon's push at another crew's cache. A fetch reads the agent's repository through `upload-pack`, which takes no repo-local hook or program config, and writes only into the daemon-owned cache. |
| N-6 | **No migration.** A workspace whose `.git` is a file is refused with a message naming the remedy. The change is marked breaking. | The project is 0.x with one release out. Converting worktrees in place would be more code than the feature, for state that a `down --purge` recreates. |
| N-7 | Two agents on one branch **materialize**; they meet at push. #63 is closed as obsoleted, without a resolve-time check. | With private clones the collision at `worktree add` no longer exists. Two agents on one upstream branch is an ordinary git situation, not a configuration error. |

## 3. Layout

```
crews/<c>/
  repo/                       object cache: `clone --no-checkout`, gc.auto=0,
    .git/objects/             daemon writes, agents read
    .git/refs/remotes/origin/ what the daemon last fetched
    .git/refs/heads/          harvested agent branches (N-4)
  agents/<a>/
    .branch                   marker: the branch balerix last created the clone on
    workspace/                private clone; origin = the real remote
      .git/objects/info/alternates → crews/<c>/repo/.git/objects
```

`Layout` does not change: `CrewPaths.repo` and `AgentPaths.workspace`
keep their paths, and `branch_marker` keeps its meaning. The shape of
`workspace/.git` (a directory, not a file) is what distinguishes the new
layout from the old.

## 4. Materialize

`Workspace::ensure_worktree` becomes `ensure_clone`, with the same
arguments. Decisions, in order:

1. **`workspace/.git` is a directory** (an existing clone). The marker
   logic is Spec L §12's, unchanged: marker equals `branch` → done,
   whatever HEAD is; HEAD equals `branch` → record the marker; else
   `status --porcelain` dirty → fail with today's message (both branches
   and the path); clean → harvest the old branch (§5), `rm -rf` the
   clone, continue at step 3. A detached HEAD is reused as is; a missing
   marker is judged by HEAD once. Every git call in this step carries the
   hardened flags the workspace reader uses (`-c core.fsmonitor=false`,
   `-c core.hooksPath=<empty dir>`, `GIT_OPTIONAL_LOCKS=0`, the five
   repo-locating `GIT_*` variables scrubbed, `GIT_CEILING_DIRECTORIES` at
   the agent root), because the clone is agent-writable. This closes the
   first bullet of #62.
2. **`workspace/.git` is a file** (a 0.1.x worktree). Fail:
   `<workspace>: created by balerix 0.1 as a worktree; run \`balerix down
   <fleet> --purge\` and \`up\` again (push unpushed work first)`. The
   agent is `Failed` with it and retried at the resync cadence like any
   materialize failure.
3. **No clone.** `git -C <cache> fetch --quiet --no-auto-gc origin`
   (the one moment `origin/<ref>` must be current, as today, and what
   makes the clone cheap), then `git clone --quiet --no-checkout
   --reference <cache> <url> <workspace>`. Then the branch:
   - the cache has `refs/heads/<branch>` (a harvest, §5): `git -C
     <workspace> fetch --quiet <cache> refs/heads/<branch>:refs/heads/<branch>`
     then `checkout --quiet <branch>`;
   - else `checkout --quiet -b <branch> origin/<start_ref>`; a missing
     `origin/<start_ref>` fails with git's message, as today.

   Then record the marker.

The pure part of step 1 (marker, HEAD, dirty → reuse / record / fail /
re-create) is a function of three values and gets a unit test without
git; the git calls around it are the integration test's.

`ensure_repo` (the crew step) still clones the cache when absent, now
followed by `git -C <cache> config gc.auto 0`. A cache that exists is
left alone, so a steady-state pass still costs no git call.

The clone runs with the fleet's gh config dir, as clone and fetch do
today, so a private repository is reachable with the operator's token.

## 5. Harvest and seed

`remove_worktree` becomes `harvest_and_remove`. When `workspace/.git` is a
directory and `refs/heads/<branch>` exists in the clone:

```
git -C <cache> fetch --quiet --no-auto-gc <workspace> +refs/heads/<branch>:refs/heads/<branch>
```

The `+` is intended: the clone was seeded from the cache's copy, so the
clone's is always the newer state (an agent that rebased makes it a
non-fast-forward). A branch the agent deleted locally is skipped. A failed
harvest fails the removal, as a failed `worktree remove` does today, so
the operator sees it rather than losing work; `--purge` is the way past a
clone too broken to read. Only the assigned branch (`ResolvedAgent::branch()`)
is harvested; other local branches the agent created are lost with the
clone, which the doc comment and the Upgrading note in `CHANGELOG.md`
(§9) say.

Callers: `remove_agent`; the per-agent loop in `remove_crew` when
`keep.sessions` is false; and the branch-change re-create in §4 step 1
(harvesting the branch the marker recorded, not the new one). `--keep-repos`
keeps the cache with its harvested branches; `--purge` deletes it.

Two agents that share a branch and are both removed harvest into the same
ref; the last one wins. The spec accepts that: nothing in balerix reads
the cache's `refs/heads` except the seed in §4, and either copy is a
legitimate state of that branch.

## 6. Sandbox

`sandbox::grants`: `allow` becomes `[home, workspace]`; `read` gains
`crew.repo.join(".git").join("objects")`, the alternates target. A Landlock
read rule on a directory covers the tree beneath it. Nothing else the
agent's git needs lives in the cache: alternates name an `objects/`
directory, not a repository.

## 7. Security

`docs/THREAT-MODEL.md`:

- The accepted risk **"Git isolation between agents of the same crew"**
  is removed. A mitigation row replaces it: *One agent reaching a
  sibling's repository* — refs, index, config, hooks and HEAD are per
  agent (a private clone); agents share only a read-only object cache
  (`crews/<c>/repo/.git/objects`, granted `read`, never `allow`); the
  daemon writes the cache and reads an agent's clone only by fetching from
  it (§5), so nothing an agent puts in its own `.git/config` runs under
  the daemon or reaches another crew. Evidence: `sandbox_it` (a write into
  the cache from inside the profile is denied, an object read succeeds),
  `workspace_it` (harvest by fetch).
- The row **"The daemon runs read-only `git` in a repository an agent can
  write to"** keeps its content; the sentence about the common dir of a
  linked worktree goes, since the `--local` probe now reads the clone's
  own config. The residue it records grows by one clause: `clone
  --reference` and `checkout` at creation apply the repository's smudge
  filters and hooks exactly as `worktree add` did.
- Spec L §7's bullet on `branch` reaching a git argv changes its example
  from `worktree add -b <branch> <path> origin/<branch>` to `checkout -b
  <branch> origin/<branch>`; the leading-`-` refusal does the same work.

## 8. Testing

- `balerix-runtime` unit: the sandbox grants (read on `objects`, no
  `allow` on the cache); the pure clone decision (marker, HEAD, dirty).
- `balerix-runtime` `workspace_it`, rewritten around the clone:
  - a fresh clone has `objects/info/alternates` naming the cache's
    `objects`, and a commit pushed from it reaches the bare origin;
  - a matching marker reuses the clone; an agent that checked out a
    branch of its own is left alone on an unchanged setting;
  - a changed `branch` on a clean clone harvests the old branch into the
    cache and re-creates on the new; on a dirty clone it fails and keeps
    the tree;
  - an unpushed commit survives `remove_agent` followed by `materialize`
    (seeded from the cache), the seed being the exact commit;
  - two agents on one branch both materialize;
  - a `workspace/.git` file is refused with the purge message;
  - the cache has `gc.auto=0` after `ensure_repo`; a missing remote
    branch still fails; the errors still name id, tool and first stderr
    line; `check_branch_name` still agrees with `git check-ref-format`.
- `sandbox_it`: inside the profile, a write under the cache is denied and
  an object under it reads.
- `inspect_it`, `materialize_it`, `e2e`: unchanged in what they assert;
  fixtures that built a worktree by hand build a clone.

## 9. Breaking change

- PR title `feat(runtime)!: a private clone per agent (Spec N)`, so
  git-cliff writes a `[breaking]` entry and the 0.x rule in `cliff.toml`
  bumps the core unit to 0.2.0. Squash merges use the title alone, so the
  `!` is the whole signal to the changelog.
- A hand-written **Upgrading** note in `CHANGELOG.md` above the generated
  0.2.0 entries: fleets created by 0.1.x are refused with the §4 message;
  push unpushed work, then `balerix down <fleet> --purge` and `up`.
  `docs/RELEASING.md` gains one line saying where such notes go.
- `ARCHITECTURE.md`: the layout in "How it flows" (clone, not worktree),
  the non-obvious decisions "Worktree branches are reused, never reset"
  (reworded for the clone and the cache) and "Git is run by the daemon,
  never granted to a plugin" (no crew repo to mention), and a new one:
  *Harvest is a fetch from the cache, not a push from the clone.*
- `AGENTS.md` gotchas: the sandbox now grants the cache read-only; the
  workspace-reader gotcha's "deleting the worktree's `.git` file" and
  "the crew's `.git/config`" become the clone's `.git` directory and the
  clone's own config; the `branch` gotcha says clone, not worktree.
- Spec M-10: "the branch survives in the crew repo" → "in the crew cache".
- Issues: #63 closed as obsoleted; #62's first bullet (status hardening)
  closed by the PR, and its third (ignored files) by the doc comment on
  `harvest_and_remove` saying the clone, ignored files included, is
  deleted once the branch is harvested.

## 10. Deliberately deferred

- Harvesting every local branch, not only the assigned one. Nothing today
  reads more than the assigned branch back.
- Partial clone (`--filter=blob:none`) or sparse checkout for very large
  repositories. Orthogonal: both work inside a private clone, and the
  cache would serve as their promisor.
- Cache compaction. The cache only grows (force-pushed history is never
  pruned); `--purge` is the reset. A daemon-side `repack` without `-d`
  is safe and could come later.

## 11. Done when

1. An agent's nono profile has no `allow` entry under `crews/<c>/repo`,
   and `sandbox_it` proves a write there is denied.
2. `workspace_it` passes every case in §8, including the unpushed commit
   that survives removal.
3. `mise run test-it`, `mise run e2e` and `mise run verify-claude` pass
   on the clone layout, the last by hand with the claude in `mise.toml`.
4. A 0.1.x state directory is refused with the purge message and comes up
   after `down --purge`.
5. `CHANGELOG.md` carries the Upgrading note and the `[breaking]` entry;
   `ARCHITECTURE.md`, `AGENTS.md`, `docs/THREAT-MODEL.md` and Spec M-10
   read as §7 and §9 say.
