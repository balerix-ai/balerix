# Balerix — Spec E: Mise pool hierarchy

**Date:** 2026-09-09
**Status:** Approved in brainstorm 2026-09-09
**Scope:** how tool installs are laid out and shared between the daemon,
fleets, crews and agents. Touches `balerix-config` (resolution),
`balerix-core` (spec types), `balerix-runtime` (layout, toolchain,
materializer, env, sandbox), the golden snapshots, and three documents.
Plugins are out of scope.

Where this document and the architecture spec
(`2026-09-05-balerix-architecture-design.md`) disagree, this document wins;
§8 lists the corrections.

---

## 1. Problem

Every agent runs with the daemon's install directory as its `MISE_DATA_DIR`,
granted read-only. Two things follow that the fleet author does not want:

1. An agent cannot install a tool for itself. `mise install` in its worktree
   fails on the read-only data dir, so the agent's only way out is to unset
   `MISE_DATA_DIR` and lose the shared installs.
2. The worktree's own `mise.toml` is hidden on purpose (`MISE_CEILING_PATHS`
   is the workspace), because the sandbox could not install what it names.

The goal: tools declared in the fleet file are installed once and shared
read-only at the level they were declared; anything an agent installs on its
own is private to that agent; and `mise install` in a worktree just works.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| E-1 | Sharing uses mise's `shared_install_dirs` (`MISE_SHARED_INSTALL_DIRS`), a colon-separated list of read-only install directories mise searches after its own. Verified 2026-09-09 on mise 2026.9.3: first match wins in list order, the private dir beats every shared dir, `mise install` skips a version found in any shared dir, and mise never writes into one. | Built for exactly this; no symlink farm, no copying, no diffing of tool tables in balerix. |
| E-2 | Pools are **daemon-owned and read-only to agents** at every level: daemon, fleet, crew. Agents write only to a private data dir inside their own `home/`. | An agent-writable crew pool would let one agent replace a binary its crew-mates execute. Two agents on the same crew may also need different versions of one tool; private dirs plus version-keyed installs mean nothing is ever clobbered. |
| E-3 | Each level's pool is filled by a `mise install` whose `MISE_DATA_DIR` is that pool and whose `MISE_SHARED_INSTALL_DIRS` are its parents. | Mise skips what a parent already holds, so the agent step lands only agent-specific tools without balerix computing which those are. |
| E-4 | Fleet and crew pools live **under the state tree next to their owner**, not under the data root. | Lifecycle is the point: `remove_crew` deletes the crew root and takes the pool with it; the fleet pool follows the fleet dir. The daemon pool stays under the data root. |
| E-5 | `MISE_CEILING_PATHS` moves from the workspace to the agent root, so the worktree's `mise.toml` is discovered and nothing above it is. `MISE_AUTO_INSTALL=false` is added to the agent environment. | Verified 2026-09-09: the ceiling directory itself is excluded from the walk, and with auto-install off `mise exec claude` starts even when the worktree names tools nobody installed. Installing is an explicit `mise install` by the agent, never a side effect of launch. |
| E-6 | The system table (`$XDG_CONFIG_HOME/balerix/mise.toml`, else the embedded `claude`/`gh` pins, P2-8) keeps its own pool and its own install step. | With the agent step writing privately, a system tool missing from the daemon pool would otherwise be installed once per agent. |
| E-7 | Plugins are unchanged: daemon pool as `MISE_DATA_DIR`, their existing ceiling. | Nothing needs it yet (YAGNI). |

## 3. Layout

```
$XDG_DATA_HOME/balerix/
  mise/                                    daemon pool: the system table (unchanged)
  mise.installed                           marker: sha256 of the system table it was installed from

$XDG_STATE_HOME/balerix/fleets/<fleet>/
  mise.toml                                generated: fleet.defaults.tools
  mise/                                    fleet pool
  mise.installed                           marker: sha256 of the mise.toml it was installed from
  crews/<crew>/
    mise.toml                              generated: crew.defaults.tools
    mise/                                  crew pool
    mise.installed                         marker
    agents/<agent>/
      mise.toml                            generated: merged table (unchanged)
      home/.local/share/mise/              private data dir, read-write to the agent
```

Every pool is a complete mise data dir (`installs/`, `downloads/`, `shims/`),
so mise keeps its own bookkeeping there. Only `installs/` is exported to
children through `MISE_SHARED_INSTALL_DIRS`.

Resolution order inside the sandbox: private dir, crew pool, fleet pool,
daemon pool. A crew that pins a different version of a fleet tool gets that
version in the crew pool; the agent's merged table names the crew's version
and resolves it there; the fleet's version stays in the fleet pool for crews
that still use it. Mise keys every install by `<tool>/<version>`, so nothing
is overwritten.

`StateLayout` additions:

```rust
pub struct FleetPaths { pub root: PathBuf, pub mise_toml: PathBuf }   // new; fleet_dir() stays
impl FleetPaths { pub fn mise_pool(&self) -> PathBuf; pub fn installed_marker(&self) -> PathBuf }
impl CrewPaths  { pub fn mise_toml(&self) -> PathBuf; pub fn mise_pool(&self) -> PathBuf; pub fn installed_marker(&self) -> PathBuf }
impl AgentPaths { pub fn mise_data_dir(&self) -> PathBuf }             // home/.local/share/mise
impl StateLayout { pub fn fleet(&self, f: &FleetName) -> FleetPaths }
```

`StateLayout::mise_data_dir()` keeps meaning the daemon pool.

## 4. Configuration resolution

`resolve()` merges host, fleet defaults, crew defaults and the agent layer
into one `AgentSettings` and today keeps nothing else. It additionally
extracts:

```rust
pub struct FleetSpec { pub name: String, pub tools: BTreeMap<String, String>, pub crews: … }
pub struct CrewSpec  { …, pub tools: BTreeMap<String, String>, pub agents: … }
```

`FleetSpec::tools` is the `tools` table of `file.defaults`; `CrewSpec::tools`
is the `tools` table of `crew.defaults`. Each is validated as agent tools are
(exact versions only, same error path prefix: `defaults.tools.<k>` and
`crews.<c>.defaults.tools.<k>`). A `null` entry in a crew layer that removes
a fleet tool simply means the crew table lacks it. The host layer contributes
no tools. The merged agent table is unchanged, so `ResolvedAgent`, restart
hashing and everything downstream are untouched.

`ensure_crew` on the `Materializer` port gains the two tables it needs:

```rust
fn ensure_crew(&self, crew: &CrewRef, repo: &RepoRef, git_ref: &str, git: &GitSettings,
               creds: &CredentialBundle, tools: &CrewTools) -> Result<(), MaterializeError>;

pub struct CrewTools<'a> { pub fleet: &'a BTreeMap<String, String>, pub crew: &'a BTreeMap<String, String> }
```

The reconciler passes them from the desired `FleetSpec`; `plan()` is not
touched.

## 5. Install pipeline

`Toolchain` grows one generic operation, `install_level`: write the
generated `mise.toml` for a level with `render_mise_toml` (its header
comment generalised to take a label instead of an `AgentId`; system table
empty, `with_gh` false: the fleet and crew files carry only what their
level declared), `mise trust` it, then `mise install` with `cwd=/`,
`MISE_DATA_DIR` = that level's pool and `MISE_SHARED_INSTALL_DIRS` = its
parents' `installs/` dirs. Guarded by a marker holding the sha256 of the
file's content: a level is reinstalled only when its table changes.

The four levels, in the order they run:

| Level | Runs in | Table | Data dir | Parents |
|---|---|---|---|---|
| system | `ensure_crew`, first | system table (P2-8) | daemon pool | none |
| fleet | `ensure_crew` | `FleetSpec::tools` | fleet pool | daemon |
| crew | `ensure_crew` | `CrewSpec::tools` | crew pool | fleet, daemon |
| agent | `install_and_validate` (existing) | merged table (existing file) | private dir | crew, fleet, daemon |

The system marker lives at `$XDG_DATA_HOME/balerix/mise.installed`. The
agent step keeps its existing marker and semantics; the only change is where
its data dir points and the shared list it carries. Because mise skips what a
parent holds, the agent step lands only agent-specific tools, including a
version the agent layer pinned over its crew's.

The reconciler executes steps one at a time, so two crews never fill the
same fleet pool concurrently and two agents never fill the same crew pool.

**Removal.** `remove_crew` deletes the crew root when neither keep flag is
set, which takes the crew pool and marker with it. With `keep.sessions` or
`keep.repos` set the root survives and so does the pool; the crew may come
back. The fleet pool sits in the fleet dir and follows its lifecycle
(`--purge`). Nothing prunes the daemon pool, as today.

## 6. Agent environment and sandbox

`agent_env` rows that change (spec §4 table):

| Variable | Before | After |
|---|---|---|
| `MISE_DATA_DIR` | daemon pool | `home/.local/share/mise` |
| `MISE_SHARED_INSTALL_DIRS` | unset | `<crew pool>/installs:<fleet pool>/installs:<daemon pool>/installs` |
| `MISE_CEILING_PATHS` | `workspace/` | `agents/<agent>/` |
| `MISE_AUTO_INSTALL` | unset | `false` |

The generated global file is still applied through
`MISE_GLOBAL_CONFIG_FILE`. Mise honours an untrusted config's `[tools]` for
`mise install` (verified 2026-09-09, non-interactive), so no trust plumbing
is needed for the worktree file; an agent that wants the repo's `[tasks]` or
`[env]` runs `mise trust` itself, which writes to its own `MISE_STATE_DIR`.

`balerix_grants` adds two read grants: the fleet pool and the crew pool. The
daemon pool grant stays. The private dir needs nothing (under `home/`). The
generated fleet and crew `mise.toml` files are not granted; the agent never
reads them.

**Threat model** (`docs/THREAT-MODEL.md`, new accepted-risk line): an agent
can execute binaries from its worktree's `mise.toml` that it downloaded
itself. This is no new capability; the agent could already download and run
anything the sandbox allows. It cannot affect any other agent: every pool is
read-only to it, its private dir is inside its own home, and a fleet's agents
cannot see another fleet's pool. The "shared mise dir" wording in the
sandbox rows becomes "the daemon, fleet and crew pools".

## 7. Errors and testing

**Errors.** A failed pool install is a `MaterializeError::Tool` from
`ensure_crew`; the reconciler already treats that as a crew failure, skipping
the crew's agents for the pass and putting the error in the crew's status
message. A failed fleet pool install fails the first crew that tries it and
is retried by the next, because the marker is written only on success. The
agent step keeps its current error path.

**Unit.**
- `resolve.rs`: fleet and crew tables extracted; a crew `null` removes a
  fleet tool from the crew table; version validation at each level with the
  right error path.
- `layout.rs`: the new accessors.
- `env.rs`: the four rows; the row count.
- `sandbox.rs`: the two new read grants.
- `toolchain.rs`: `install_level` writes the file, honours the marker, and
  passes the right env (through `Cmd` logging in the existing style).
- Golden snapshots for the agent profile update.

**Integration** (`balerix-runtime`, real mise, gated by
`BALERIX_REQUIRE_TOOLS` like the rest):
- One tool declared at each of the four levels, using small tools already on
  the host. Assert each pool holds exactly its level's tools, the private dir
  holds only the agent's tool, a second run installs nothing, and `mise
  which` under the agent env resolves each tool to the expected pool.
- The same tool pinned at two versions at fleet and crew level: both
  survive, and the agent resolves the crew's.

**End to end.** The existing journey passes unchanged, which shows launch
still works with the ceiling moved and auto-install off.

**Mutants.** `plan()` is untouched; the nightly tier is unaffected.

**Docs.** ARCHITECTURE.md: the layout diagram, the two ceiling notes, and a
line on the pool hierarchy. THREAT-MODEL.md as in §6. AGENTS.md: the
`verify-claude` line still describes its data root as "the shared
`MISE_DATA_DIR`"; reword to "the daemon pool".

## 8. Corrections to the architecture spec

- §4 layout: `mise/` under the data root is now only the system table's
  pool; fleet and crew pools live under the state tree (E-4).
- §4 environment table: `MISE_DATA_DIR` is the agent's private dir;
  `MISE_SHARED_INSTALL_DIRS`, `MISE_CEILING_PATHS` and `MISE_AUTO_INSTALL`
  as in §6.
- §4 "mise" paragraph: "Inside the sandbox `mise exec` only resolves; it can
  never install" becomes "at launch `mise exec` only resolves; the agent may
  install into its private dir afterwards".
- Phase 3 note "a repo's own mise config is therefore invisible to agents"
  is reversed (E-5).

## 9. Deliberately deferred

- Pruning the daemon pool.
- A private data dir for plugins (E-7).
- Any agent-writable shared pool (E-2).

## 10. Done when

- `mise run check` and `mise run test-it` pass, including the two new
  integration tests.
- A fleet with a fleet tool, a crew tool and an agent tool materializes with
  each in its own pool, and `mise install` in the agent's worktree installs
  a repo-declared tool into `home/.local/share/mise` without touching any
  pool.
- The three documents and the golden snapshots reflect §3, §6 and §8.
