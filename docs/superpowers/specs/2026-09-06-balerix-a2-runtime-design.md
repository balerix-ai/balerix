# Balerix — Spec A / Phase 2: Runtime

**Date:** 2026-09-06
**Status:** Approved in brainstorm 2026-09-06; implements §11 phase 2 of
`2026-09-05-balerix-architecture-design.md` (the *architecture spec*)
**Scope:** `balerix-runtime` (driven adapters over git, gh, mise, nono, tmux),
the ports and reconciler in `balerix-core`, status wire types in
`balerix-api`, and one hidden dev command. No daemon, no API, no `up`/`down`.

Where this document and the architecture spec disagree, this document wins;
§8 lists the corrections and the reasons.

---

## 1. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| P2-1 | Phase 2 ships one user-visible thing: a hidden `balerix dev materialize` command that renders an agent's generated files without launching anything. | Gives humans and golden tests the same inspectable artefact; costs one small command; does not pre-empt Phase 3's `up`. |
| P2-2 | The reconciler is a **pure planner plus a dumb executor**. `plan()` is a total function from (desired, status, observed, now) to an ordered `Plan`; `execute()` walks it through the ports. | Every decision is testable without fakes; the model-based suite compares plans structurally; `cargo mutants` has something sharp to bite. |
| P2-3 | Ports are **synchronous** traits. No `tokio` in `balerix-core`. | Every port implementation is a subprocess call. Phase 3 wraps a reconcile pass in `spawn_blocking`; the port signatures do not change. |
| P2-4 | **Materialization is its own port** (`Materializer`), separate from `AgentRunner`. | Files and processes fail differently and are faked differently; `down --keep-repos` maps onto `remove_crew(keep_repo)` directly. The architecture spec folded this into "the pipeline". |
| P2-5 | The agent environment is set through the **nono profile's `environment` block**, not inherited through `env -i`. nono runs with its own balerix-owned `$HOME`. | Verified 2026-09-05: `nono run` refuses a read-write grant on any directory containing its own state root, and derives that root from its own `$HOME`. The architecture spec's §12 fallback is the only working shape. |
| P2-6 | **Worktree branches are reused, never reset.** If `balerix/<fleet>/<crew>/<agent>` exists, check it out; otherwise create it from `origin/<ref>`. | The architecture spec's `worktree add -B … origin/<ref>` would discard unpushed agent commits on every re-`up` after `down --keep-repos`. |
| P2-7 | Generated nono profiles **keep nono's built-in system groups** and add balerix's explicit grants; user `sandbox` merges with nono's own `extends` semantics; conflicts are rejected, not overridden. | The built-in groups exist so ordinary tooling works (`/tmp`, `/dev/null`, …); re-deriving them is not balerix's job. Rejection keeps the isolation contract legible. |
| P2-8 | The system tool table defaults to an **embedded** `claude` pin taken from this repo's `mise.toml` at build time; `$XDG_CONFIG_HOME/balerix/mise.toml` overrides it when present. | A fresh install must be able to launch an agent without the user writing a file first; the pin stays single-sourced. |
| P2-9 | `FleetStore` and `EventHandler` are **not** defined in this phase. | Nothing in Phase 2 consumes them (YAGNI). They arrive with the server. |

## 2. `balerix-core` additions

### 2.1 Identity and observation types

```rust
pub struct CrewRef { pub fleet: FleetName, pub crew: CrewName }       // Display: fleet/crew
// AgentId { fleet, crew, agent } already exists.                      // Display: fleet/crew/agent

/// One agent, fully resolved, as the runtime sees it. Built from `Fleet`.
pub struct ResolvedAgent {
    pub id: AgentId,
    pub repo: RepoRef,
    pub git_ref: String,
    pub git: GitSettings,
    pub settings: AgentSettings,
}

/// SHA-256 over the canonical JSON of (repo, git_ref, git, settings).
pub struct SpecHash(String);

pub enum ProcessState { Running { pid: u32 }, Exited { code: Option<i32> } }
pub struct ObservedState {
    pub crews: BTreeMap<CrewName, BTreeMap<AgentName, ProcessState>>,
}

pub struct LaunchPlan {
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,   // the OUTER environment (PATH, nono's HOME)
    pub argv: Vec<String>,
    pub script: PathBuf,                 // rendered launch.sh; the runner executes this
}

pub struct HookTarget { pub url: String, pub secret: String }   // Debug prints <redacted> for secret
pub struct Timestamp(/* seconds since epoch, u64 */);
```

`SpecHash` is stable under JSON key reordering (canonical form = `serde_json`
with `BTreeMap`s, which every wire type already uses). `ResolvedAgent::from_fleet(&Fleet) -> Vec<ResolvedAgent>` is the only way the runtime reaches the fleet.

Addendum (Task 18): `Timestamp` and `SpecHash` are defined in
`balerix-api::status` and re-exported at the `balerix-api` crate root;
`balerix-core` uses them from `balerix_api` (`balerix-runtime` depends on
`balerix-api` too but does not yet reference either type directly — that
arrives with Phase 3's `Clock` adapter).

### 2.2 Ports

```rust
pub trait Materializer: Send + Sync {
    fn ensure_crew(&self, crew: &CrewRef, repo: &RepoRef, git_ref: &str,
                   git: &GitSettings, creds: &CredentialBundle) -> Result<(), MaterializeError>;
    fn materialize(&self, agent: &ResolvedAgent, creds: &CredentialBundle,
                   hooks: &HookTarget) -> Result<LaunchPlan, MaterializeError>;
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError>;
    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError>; // Task 18: keep_repo: bool became Keep { repos, sessions }
}

pub trait AgentRunner: Send + Sync {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError>;
    fn ensure_agent(&self, agent: &AgentId, plan: &LaunchPlan) -> Result<(), RunnerError>; // create or respawn
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError>;
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError>;
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError>;
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError>;
}

pub trait Clock { fn now(&self) -> Timestamp; }
```

Both error types are `thiserror` enums whose `Display` is
`<fleet/crew[/agent]>: <tool> <subcommand>: <first line of stderr>`, with the
full stderr and argv available as fields. That string is what lands in
`AgentStatus.message`.

Every port method is idempotent: calling it when the world already matches is
a no-op success.

### 2.3 Status wire types (`balerix-api`)

```rust
pub struct FleetStatus {
    pub generation: u64,
    pub observed_generation: u64,
    pub phase: FleetPhase,                       // derived, see §3.4; stored for the wire
    pub agents: BTreeMap<String /* AgentId */, AgentStatus>,
}
pub enum FleetPhase { Pending, Reconciling, Ready, Degraded, Terminating }

pub struct AgentStatus {
    pub phase: AgentPhase,
    pub message: String,
    pub last_event_at: Option<Timestamp>,
    pub applied_hash: Option<SpecHash>,          // hash the running window was started from
    pub restarts: u32,
    pub next_restart_at: Option<Timestamp>,
}
pub enum AgentPhase { Pending, Materializing, Starting, Ready, Dead, Stopped }
```

## 3. Reconciler (`balerix-core::reconcile`)

### 3.1 Functions

```rust
pub struct ReconcilePolicy { pub max_restarts: u32, pub backoff_base: Duration, pub backoff_cap: Duration }
// defaults: 5, 2 s, 5 min

pub fn plan(desired: Option<&Fleet>, status: &FleetStatus, observed: &ObservedState,
            policy: &ReconcilePolicy, now: Timestamp) -> Plan;

pub fn apply(status: &mut FleetStatus, step: &Step, outcome: &StepOutcome,
             policy: &ReconcilePolicy, now: Timestamp);

pub fn execute(plan: &Plan, status: &mut FleetStatus, m: &dyn Materializer,
               r: &dyn AgentRunner, creds: &CredentialBundle, hooks: &dyn Fn(&AgentId) -> HookTarget,
               policy: &ReconcilePolicy, clock: &dyn Clock) -> ExecuteReport;

// external signals, also pure
pub fn agent_ready(status: &mut FleetStatus, agent: &AgentId, now: Timestamp);
pub fn set_desired(status: &mut FleetStatus, generation: u64);
```

`desired: None` means the fleet is being removed (`down`); `plan` then emits
only stops and removals and `Terminating` is the fleet phase.

### 3.2 Plan and steps

`Plan` is `Vec<Step>` in this fixed order: `Stop*`, `RemoveAgent*`,
`RemoveCrew*`, `EnsureCrew*`, then per agent `Materialize`, `Start`; `NoteExit*`
last. Within each group, alphabetical by id. Tests compare plans with `==`.

| Step | Emitted when |
|---|---|
| `Stop(agent)` | agent observed running and (not desired, or `applied_hash != desired_hash`) |
| `RemoveAgent(agent)` | agent recorded or observed, not desired |
| `RemoveCrew(crew, keep)` | crew recorded or observed, not desired — with the caller's `keep`, whether or not the fleet is still desired |
| `EnsureCrew(crew)` | crew desired |
| `Materialize(agent)` + `Start(agent, hash)` | agent desired and one of: no window observed; `applied_hash != desired_hash`; `Exited` with `restarts < max_restarts` and `next_restart_at <= now` |
| `NoteExit(agent, code)` | agent observed `Exited`, not yet noted (`next_restart_at.is_none()`) |

Addendum (Task 18): the row that was `MarkDead(agent)` in this draft became
`NoteExit(agent, code)` — emitted when the window is observed `Exited` (a
`ProcessState`), the agent is not `Dead`, and `next_restart_at` is `None`;
`apply` records the exit (`restarts += 1`, `next_restart_at = now + backoff`)
and moves the agent to `Dead` when `restarts > max_restarts`; otherwise the
agent's phase is unchanged until the restart is due.

Every step carries the ids it needs and nothing else; `Start` carries the
`SpecHash` so `apply` can record `applied_hash` without recomputing it.

A fleet that is `Ready` and unchanged yields only `EnsureCrew` steps (one per
desired crew), nothing else. This is the central invariant; making
`ensure_crew` a cheap no-op on such a pass (fetch only before creating a
branch; skip `mise install` when the table is unchanged and nothing is
missing) is the first Phase 3 runtime item.

### 3.3 Executor

`execute` walks the plan in order. For each step it calls the port, then
`apply`. On a failed step for an agent it records the error in that agent's
`message`, marks the agent's remaining steps in this plan as skipped, and
continues with other agents. A failed crew step skips that crew's agents. There
is no retry inside a pass; the next pass replans. `ExecuteReport` says whether
every step succeeded, which decides `observed_generation` (§3.4).

`Materialize` calls `Materializer::materialize` and stashes the `LaunchPlan` for
the following `Start`. `Start` calls `AgentRunner::ensure_agent` — which
creates the window or respawns it — and records `applied_hash`.

### 3.4 Transitions

```
Pending ──Materialize ok──▶ Materializing ──Start ok──▶ Starting ──agent_ready──▶ Ready
   ▲                                                       │                        │
   │           observed Exited, restarts < max, next_restart_at ≤ now               │
   └───────────────────────────────────────────────────────┴────────────────────────┘
Starting | Ready ──observed Exited, restarts ≥ max──▶ Dead    (left alone until desired_hash changes)
any ──not desired──▶ Stopped ──RemoveAgent ok──▶ (entry deleted)
```

- On observing `Exited` for a `Starting`/`Ready` agent: `restarts += 1`,
  `next_restart_at = now + min(backoff_base × 2^(restarts-1), backoff_cap)`,
  phase stays until the restart is due. `plan` emits nothing for an agent whose
  restart is not yet due.
- `agent_ready` sets `Ready`, `restarts = 0`, `next_restart_at = None`,
  `last_event_at = now`. A Ready signal for an unknown agent is ignored.
- A `desired_hash` change resets `restarts` to 0 and lifts `Dead`.
- Any failed step sets the agent's `message` to the error and leaves its phase
  where it was.

**Fleet phase**, derived at the end of every pass and by `agent_ready`:
`Terminating` if desired is `None`; else `Degraded` if any agent is `Dead` or
any step failed this pass; else `Reconciling` if any agent is
`Pending | Materializing | Starting`; else `Ready` if every agent is `Ready`;
`Pending` for a fleet with no agents. `observed_generation := generation` only
at the end of a pass in which every step succeeded.

### 3.5 Fakes (in `balerix-core`, behind `cfg(test)` and a `test-support` feature)

`FakeMaterializer` and `FakeRunner` record every call in order, hold a
settable `ObservedState`, and can be told to fail the next call of a named
method for a named id. `FakeClock` is a `Cell<Timestamp>`.

Implemented always-compiled (`balerix_core::fakes`), not behind a feature:
dependency-free and Phase 3's tests need them.

## 4. `balerix-runtime`

One module per step; each is a struct over `&StateLayout` and `&ToolPaths`.
Nothing in this crate reads the process environment or the current directory.

```rust
pub struct StateLayout { pub state_root: PathBuf /* $XDG_STATE_HOME/balerix */,
                         pub data_root: PathBuf  /* $XDG_DATA_HOME/balerix  */,
                         pub config_root: PathBuf/* $XDG_CONFIG_HOME/balerix */ }
pub struct ToolPaths { pub git, pub gh, pub mise, pub nono, pub tmux: PathBuf }   // absolute
pub struct Runtime { layout: StateLayout, tools: ToolPaths, tmux_socket: String, daemon_port: u16 }
impl Materializer for Runtime { … }   // orchestrates §4.2 steps 1–5
pub struct TmuxRunner { tools, tmux_socket }   impl AgentRunner
```

`StateLayout::from_host(&HostPaths)` and `ToolPaths::discover(path: &str)` live
in the binary's wiring, not here; `discover` fails naming the missing tool.

### 4.1 Layout

```
$XDG_DATA_HOME/balerix/mise/                      shared MISE_DATA_DIR (daemon writes, agents read)
$XDG_CONFIG_HOME/balerix/mise.toml                optional system tool table (P2-8)
$XDG_STATE_HOME/balerix/fleets/<fleet>/
  gh/hosts.yml                                    fleet-level gh config used by the daemon's git (0600)
  crews/<crew>/
    repo/                                         `git clone --no-checkout`; worktrees hang off it
    agents/<agent>/
      home/                                       agent $HOME: .claude/ .config/ .local/ .cache/
      workspace/                                  worktree, branch balerix/<fleet>/<crew>/<agent>
      nono/                                       nono's own $HOME (state root, drafts) — P2-5
      mise.toml  nono-profile.json  launch.sh
      logs/                                       nono.log tmux.log <tool>.<step>.log
```

### 4.2 Steps, in the order `materialize` runs them

**1. Workspace** (`ensure_crew` does the clone; `materialize` does the worktree)
- `repo/` absent → `git clone --no-checkout <url> repo`; present → `git -C repo fetch origin` (all remote-tracking refs, which includes `origin/<ref>`).
- `workspace/` registered in `git worktree list --porcelain` → nothing. Else
  `git worktree prune`, then: branch exists → `git worktree add workspace <branch>`;
  else `git worktree add -b <branch> workspace origin/<ref>` (P2-6).
- `git.auth: gh` → env `GH_CONFIG_DIR=fleets/<fleet>/gh` and
  `-c credential.helper= -c credential.helper=!<gh> auth git-credential` on
  every git call. `hosts.yml` there is written from `creds.gh_token` (0600)
  before the first git call. `auth: none` → plain git in the daemon's own
  environment.
- `RepoRef` gains a `file://` form (`RepoRef::Local(PathBuf)`); `clone_url()`
  returns it unchanged.

**2. AgentHome**
- Create `home/` and `.claude .config/gh .local/share .local/state .cache`.
- `.claude/settings.json` = `settings.claude.settings` with the balerix `hooks`
  block inserted: for every event in a constant list (`SessionStart`,
  `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`,
  `Stop`, `SubagentStop`, `PreCompact`) one `{ type: "http", url:
  "<hooks.url>/v1/agents/<f>/<c>/<a>/events", headers: { "Authorization":
  "Bearer <secret>" } }` entry. Whether Claude sends the header is a Phase 3
  verification (architecture spec §12 row 1).
- `.claude/.credentials.json` ← `creds.claude_credentials` (0600, absent if `None`).
- `.claude.json` ← `creds.claude_account` fields merged over a seed that marks
  onboarding complete. The exact seed keys are verified at implementation.
- `.config/gh/hosts.yml` ← `creds.gh_token` (0600) when `git.auth: gh`.
- All files written via tmp + rename and rewritten every pass.
- Modes: `settings.json` (it carries the hook bearer secret),
  `.credentials.json`, `.claude.json`, `hosts.yml` and `nono-profile.json`
  are `0600`; the agent directory, `home/` (with `home/.claude/`) and `logs/`
  are `0700`; `launch.sh` is `0755`.

**3. Toolchain**
- `agents/<agent>/mise.toml` `[tools]` = system table (P2-8) ⊕ `settings.tools`
  ⊕ `gh = <balerix's pinned gh>` when `git.auth: gh`. User keys override
  system keys; the result must still be all-exact (already validated upstream).
- `mise trust <file>` then `mise install` with env
  `MISE_GLOBAL_CONFIG_FILE=<file> MISE_DATA_DIR=<shared> MISE_CONFIG_DIR=home/.config/mise
  MISE_STATE_DIR=home/.local/state/mise MISE_CACHE_DIR=home/.cache/mise`, run
  unsandboxed by the daemon. Skipped when the file is byte-identical to the
  previous pass and every tool is already installed (`mise ls --missing` empty)
  (deferred to Phase 3; Phase 2 runs `mise install` every pass, an offline
  no-op when nothing is missing).

**4. SandboxProfile** → `nono-profile.json`
```json
{ "meta": { "name": "balerix-<fleet>-<crew>-<agent>" },
  "filesystem": { "read":  ["/usr", "/lib", "/lib64", "/bin", "/etc", "<shared mise>"],
                  "allow": ["<home>", "<workspace>", "<repo>/.git"] },
  "workdir": { "access": "none" },
  "network": { "open_port": [<daemon_port>] },   // was connect_port; see the Phase 3 spec §8.1 (Landlock allowlist)
  "environment": { "deny_vars": ["*"],
                   "set_vars": { "HOME": "<home>", "XDG_CONFIG_HOME": …, "XDG_DATA_HOME": …,
                                 "XDG_STATE_HOME": …, "XDG_CACHE_HOME": …, "CLAUDE_CONFIG_DIR": …,
                                 "GH_CONFIG_DIR": …, "MISE_GLOBAL_CONFIG_FILE": …, "MISE_DATA_DIR": …,
                                 "MISE_CONFIG_DIR": …, "MISE_STATE_DIR": …, "MISE_CACHE_DIR": …,
                                 "BALERIX_FLEET": …, "BALERIX_CREW": …, "BALERIX_AGENT": …,
                                 "BALERIX_AGENT_ID": …, "BALERIX_API_URL": …,
                                 /* then settings.env, which cannot name any key above (validated in Phase 1) */ } } }
```
- User `settings.sandbox` merges in with nono's `extends` rules: arrays append
  and dedupe, scalars user-wins, maps recurse. Rejected with a path-prefixed
  `MaterializeError::SandboxConflict`: any `environment` key at all
  (`sandbox.environment: balerix-owned; use env instead`); any `filesystem.*`
  entry that equals or is a prefix/suffix of a balerix path at a different
  access level (`sandbox.filesystem.read[2]: overlaps balerix read-write path
  <home>`); `meta`. nono's built-in groups are neither included nor excluded
  explicitly (P2-7).
- `nono profile validate <file>` must succeed; its output is the error otherwise.

**5. LaunchPlan** → `launch.sh` (`0755`)
```sh
#!/bin/sh
# generated by balerix — safe to run by hand
exec env -i PATH='/usr/local/bin:/usr/bin:/bin:<dir of mise>' HOME='<agents/x/nono>' \
  '<nono>' -s --log-file '<logs>/nono.log' run --profile '<nono-profile.json>' -- \
  '<mise>' exec -- '<binary>' <args…> [--continue]
```
- `LaunchPlan { cwd: workspace, env: {PATH, HOME}, argv: [nono, …], script: launch.sh }`.
- `--continue` is appended when `settings.claude.resume` and
  `home/.claude/projects/` exists and is non-empty.
- Every value passes through a single `sh_quote` function (property-tested,
  §6). No credential is ever an input to this step.

**`remove_agent`**: `git worktree remove --force workspace` (if registered),
then delete `agents/<agent>/`. **`remove_crew(keep_repo)`**: remove every
agent, then delete `crews/<crew>/` unless `keep_repo`, in which case only
`agents/` goes.

### 4.3 TmuxRunner

All commands are `tmux -L <socket> …` via argv arrays.

| Port method | tmux |
|---|---|
| `ensure_crew` | `has-session -t =<f>/<c>` else `new-session -d -s <f>/<c> -n balerix -- sh -c 'while :; do sleep 3600; done'` — an anchor window so the session outlives its agents |
| `ensure_agent` | window absent → `new-window -d -t =<f>/<c> -n <a> -c <cwd> -- <script>`, `set-option -t =<f>/<c>:<a> remain-on-exit on`, `pipe-pane -o -t … 'cat >> <logs>/tmux.log'`; present → `respawn-window -k -t =<f>/<c>:<a> -c <cwd> <script>` |
| `stop_agent` | `kill-window -t =<f>/<c>:<a>` (no-op if absent) |
| `stop_crew` | `kill-session -t =<f>/<c>` (no-op if absent) |
| `observe` | `list-sessions -F '#{session_name}'` filtered to `<f>/`; per session `list-windows -F '#{window_name}\t#{pane_dead}\t#{pane_pid}\t#{pane_dead_status}'`, `balerix` window excluded |
| `send_text` | `send-keys -t … -l <text>` then, if `submit`, `send-keys -t … Enter` |

Session names contain `/`; verified working on tmux 3.7c. The `=` prefix
forces exact-match targeting. `balerix` is a reserved agent name (the anchor
window); `Fleet::try_from` rejects it.

Addendum (Task 18, refined in Task 16): `ensure_agent` on a fresh window
actually creates a placeholder window, sets `remain-on-exit`, attaches
`pipe-pane`, and only then `respawn-window`s into the real script — attaching
`pipe-pane` after `new-window` loses the script's first output — and
`send_text`/`ensure_agent` pass `--` before the text/command in every
`send-keys` call.

### 4.4 Verified at implementation time

Carried from architecture spec §12; each is an early plan task with a probe
and its fallback.

| Assumption | Fallback |
|---|---|
| `CLAUDE_CONFIG_DIR` and `GH_CONFIG_DIR` relocate all state (nothing lands in nono's `$HOME`) | add explicit grants / `set_vars` for what escapes. Verdict (Task 18): partial — balerix's own files land under `home/` (settings.json, .credentials.json, hosts.yml verified by home.rs tests and generated_golden); nono's `$HOME` (`agents/<a>/nono`, per `sandbox_it::generated_profile_validates_and_enforces_isolation`) held only nono's own bookkeeping — `.config/nono/{profiles,profile-drafts}` and `.local/state/nono/{sessions,audit}` — no Claude or gh state, since that test never runs `claude`/`gh` inside the sandbox; relocation under a live `claude` run is verified in Phase 3's e2e. |
| `mise exec` under `MISE_GLOBAL_CONFIG_FILE` + read-only `MISE_DATA_DIR` resolves without writing there | pin with `MISE_CONFIG_FILE`; grant the specific subdirs mise insists on. Verdict (Task 11): holds — `toolchain_it::installs_nothing_when_seeded_and_exec_resolves_read_only` seeds `MISE_DATA_DIR` with the host's `gh@2.100.0` install, chmods the whole data dir `a-w`, then runs `mise exec -- gh --version` with `MISE_GLOBAL_CONFIG_FILE`/`MISE_DATA_DIR`/`MISE_CONFIG_DIR`/`MISE_STATE_DIR`/`MISE_CACHE_DIR` all pointed outside it; it resolves and prints the pinned version with no write attempted against the read-only tree. |
| `gh auth git-credential` works from a `hosts.yml` holding only `oauth_token` and `git_protocol: https` | resolve `user:` with `gh api user` during `ensure_crew`, or require it in the credential bundle. Verdict (Task 14): holds — `oauth_token` + `git_protocol` suffice: with only those two keys in `hosts.yml`, `GH_CONFIG_DIR=… gh auth git-credential get` (given `protocol=https`/`host=github.com` on stdin) printed `username=…` and `password=…`. |
| the `.claude.json` seed keys that suppress first-run prompts | discover by diffing a fresh Claude run; keep the seed in one constant. Verdict (Task 18): seed is the single constant in `crates/balerix-runtime/src/home.rs` (`hasCompletedOnboarding: true`, overlaid by `claude_account`); unverified against a fresh Claude run until Phase 3's e2e, and written to both `home/.claude.json` and `home/.claude/.claude.json` because Claude Code reads `$CLAUDE_CONFIG_DIR/.claude.json` when the variable is set; Phase 3's e2e removes the unused copy. Phase 3 verdict (by hand, claude 2.1.263): only `$CLAUDE_CONFIG_DIR/.claude.json` is read, the `$HOME` copy is gone, and the seed also pre-accepts the trust dialog for the workspace and the crew `repo/` (Phase 3 spec §8.1). |

Already verified (2026-09-05, this machine, nono 0.75.0, tmux 3.7c):
`environment.deny_vars/set_vars` relocate `HOME` and pass `PATH`; Landlock
enforcement denies writes outside grants; `/tmp` is reachable through the
built-in groups; tmux session names with `/`, `remain-on-exit`, and the
`list-windows` format fields behave as used above.

## 5. `balerix dev materialize`

```
balerix dev materialize <fleet.yaml> <crew>/<agent>
    [--out DIR] [--no-host-defaults] [--hooks-url URL] [--install] [--with-credentials]
```

- Resolves the file exactly as `config resolve`, converts to `Fleet`, selects
  the agent, runs steps 2–5 of §4.2 into `--out` (default: a fresh temp dir,
  printed). Step 1 is skipped; `workspace/` is created empty so every path in
  the profile and `launch.sh` is real.
- `--install` additionally runs `mise install` and `nono profile validate`.
  Without it the command is offline and fast.
- Credentials are written as `"<redacted>"` placeholders in
  `.credentials.json` and `hosts.yml` unless `--with-credentials`, and the
  command says which it did. `--hooks-url` defaults to `https://127.0.0.1:7643`
  with a throwaway secret.
- Output: the directory, then one line per generated file.
- `dev` is a hidden clap subcommand group; the binary gains `runtime.rs` doing
  `StateLayout::from_host` + `ToolPaths::discover`.

## 6. Testing

| Layer | What | Where |
|---|---|---|
| unit | every `plan` branch and `apply` transition; backoff arithmetic; fleet-phase derivation; sandbox merge and each conflict rule; `sh_quote`; `list-windows` parsing; `hosts.yml`/`settings.json` rendering | in-module |
| golden (`insta`) | the four generated files for `payments.yaml` agents `alice` and `bob` with the layout root replaced by `<root>`; `Plan` rendered as text for: fresh up, one agent's hash changed, one agent exited past `max_restarts`, `down` | `balerix-runtime/tests`, `balerix-core/tests` |
| property (`proptest`) | `SpecHash` stable under key reordering and equal for equal `ResolvedAgent`; sandbox merge idempotent (`merge(a, merge(a, b)) == merge(a, b)`); `sh -c "printf %s $(sh_quote s)"` round-trips arbitrary strings; an unchanged `Ready` fleet always plans `[]` | in-module |
| model-based (`proptest-state-machine`) | reference model over `Up(fleet) \| Update(agent, settings) \| Down \| AgentExits(agent) \| AgentReady(agent) \| Tick(secs)`; model tracks the desired set, each agent's expected phase and restart count; checked after every `execute` against `FakeMaterializer` + `FakeRunner` + `FakeClock`, including runs where a named step is told to fail | `balerix-core/tests/reconcile_model.rs` |
| integration | Workspace against a local bare repo over `file://` (clone, worktree create, worktree reuse preserving a commit, remove); Toolchain against `mise` with a tool already present in a pre-seeded shared dir (no network); SandboxProfile against `nono profile validate` plus one `nono run` proving a write outside `home/` is denied; TmuxRunner: ensure crew, ensure agent, observe running, kill the process and observe `Exited`, respawn, stop crew — in a temp XDG root with socket `balerix-test-<pid>` | `balerix-runtime/tests/*_it.rs` |
| mutation | `cargo mutants -p balerix-core` | nightly |

Integration tests skip with a printed reason when a tool or Landlock is
missing locally; in CI the environment variable `BALERIX_REQUIRE_TOOLS=1`
turns a skip into a failure. No `fake-claude` and no e2e here; those are
Phase 3.

**CI.** The `check` job installs `tmux`, `nono` and `gh` through mise alongside
the Rust tools. A `mutants` job joins the nightly schedule. If tool installs
push the PR tier past the five-minute budget, integration tests move to their
own blocking job.

## 7. Errors, security, docs

**Errors.** `MaterializeError` and `RunnerError` (`thiserror`) carry the id,
the argv, and captured stderr; `Display` per §2.2. Every subprocess's stdout
and stderr are also appended to `logs/<tool>.<step>.log` under the agent. The
binary maps errors through `anyhow`.

**Threat model updates** (`docs/THREAT-MODEL.md`):
- *(planned)* → concrete: environment set through the profile, so user `env`
  cannot reach `PATH`, `HOME`, or `NONO_*`; sandbox conflicts rejected; no
  secret in `launch.sh`, argv, or the outer env; gh token only in two
  `hosts.yml` files (fleet-level for the daemon, agent-level inside `home/`),
  both 0600.
- New accepted risk: the per-agent hook secret sits in the agent's own
  `settings.json`, readable by the sandboxed agent; it authenticates only that
  agent.
- New boundary note: `repo/.git` is read-write for every agent in the crew
  (already recorded as crew-level git isolation).

**Docs.** `ARCHITECTURE.md`: Phase 2 flow, the two ports, the nono-environment
decision. `AGENTS.md`: "integration tests skip locally without tools; CI never
skips"; the `include_str!` coupling between the repo `mise.toml` and the
embedded default tool table. `README.md`: status line and the `dev materialize`
quickstart line. Architecture spec: a dated addendum after §12 pointing here
for §4 environment, §6 worktree and pipeline changes, and §3 ports.

## 8. Corrections to the architecture spec

| Architecture spec said | This spec says | Why |
|---|---|---|
| §4/§6: `exec env -i <vars> nono run …` builds the agent env | outer env is only `PATH` + nono's `HOME`; agent env via profile `environment` (P2-5) | nono refuses grants overlapping its own state root under `$HOME` |
| §4: `HOME` for nono = agent home | `agents/<agent>/nono/` | same |
| §6 step 1: `worktree add -B … origin/<ref>` | reuse existing branch, create only if absent (P2-6) | `-B` discards unpushed work after `down --keep-repos` |
| §3: `AgentRunner` runs "the pipeline" | `Materializer` port (P2-4) | separate failure modes and fakes |
| §3: `FleetStore`, `EventHandler` in core now | Phase 3 (P2-9) | no consumer yet |
| §4: nono profile lists only explicit paths | built-in groups kept (P2-7) | ordinary tooling needs them |
| §4: `mise.toml` "balerix's system tools inherited by every agent" | agents inherit only `claude` (and `gh` when needed); `git`/`tmux`/`nono` are daemon tools | agents never run them |

## 9. Deliberately deferred

- In-sandbox `git push` needs a credential helper: `home/.gitconfig` with
  `[credential "https://github.com"] helper = !gh auth git-credential` (or
  `gh auth setup-git` at materialize time). Phase 3. — Resolved in Phase 3
  (§6.3 of its spec): `home/.gitconfig` carries the gh credential helper for
  crews that may push.

## 10. Done when

- `mise run check` passes with the new crate, including integration tests, on
  a fresh clone here and in CI.
- `balerix dev materialize examples/payments.yaml backend/bob --no-host-defaults`
  writes the four files and the golden snapshots match them.
- The model-based suite runs ≥ 256 cases with no counterexample; `cargo
  mutants -p balerix-core` reports no surviving mutants in `reconcile`.
- Every row of §4.4 has a resolved verdict recorded in this document.
- Next: Phase 3 plan (`balerix-server`, vault, API, CLI `serve`/`up`/`update`/`down`, e2e).
