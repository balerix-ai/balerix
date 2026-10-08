# Unix socket isolation for sandboxed processes — design

Date: 2026-10-08. Status: approved; implemented.

## 1. Problem

In local mode, balerix runs agents, plugins and its own git under nono 0.79
profiles (`crates/balerix-runtime/src/sandbox.rs::{render_profile,
render_git_profile}`, `plugin.rs::render_plugin`). nono leaves pathname Unix
sockets unmediated by default (`linux.af_unix_mediation` is `off` and
balerix never set it), so sandboxed processes could connect to Unix sockets
outside their grants, including the daemon's tmux server, and have it run
commands outside the sandbox.

Facts established by research (2026-10-08):

- Landlock does not mediate `connect()` on pathname sockets; an un-granted
  `0700` directory does not stop it. tmux `server-access` cannot refuse a
  same-uid client (the owner).
- ssh-agent's socket under `/tmp` is exposed the same way. The daemon API
  is TCP only.
- `af_unix_mediation: "pathname"` filters correctly but works through a
  seccomp supervisor that reads `/proc/<pid>/mem`. With
  `kernel.yama.ptrace_scope = 2` (the development host) that read fails and nono
  denies every intercepted call, TCP and DNS included. With `ptrace_scope`
  0 or 1 nono is the sandboxed process's parent, so it is expected to
  work (unverified).
- Unprivileged user namespaces are blocked on the development host, so a private
  `/tmp` via `unshare` is not available.
- Pod mode: the agent pod has its own tmux server on a shared `emptyDir`
  (`agent/src/run.rs`); the agent's sandbox can reach it. The blast radius
  is that pod. Plugin pods run without nono or tmux.

## 2. Requirements

- R1. A sandboxed agent, plugin or daemon git cannot connect to a Unix
  socket outside its own grants (the tmux server, ssh-agent, others).
- R2. Where the host allows it, an agent can create and use Unix sockets
  inside its own writable directories (e.g. a test suite's local
  postgres). On hosts where it cannot be allowed safely, R1 wins by
  default.
- R3. TCP (including the daemon's loopback port) and DNS keep working.
- R4. The operator can override the choice in `config.toml`, including
  knowingly keeping today's behaviour.

Out of scope: running sandboxed processes as a separate uid (a later
option for restricted hosts); hiding the socket path (`/proc/net/unix`
lists it; Landlock does not restrict `chmod`).

## 3. Design

### 3.1 Policy

New key in the daemon's own `config.toml` (`[sandbox]`, beside `git_read`;
`crates/balerix/src/commands/serve.rs::SandboxTable`):

```toml
[sandbox]
unix_sockets = "auto"   # auto | mediate | deny | open
```

`serve` resolves it once at start-up to `SocketPolicy::{Mediate, Deny, Open}`:

| Setting   | Probe        | Result                                                   |
|-----------|--------------|----------------------------------------------------------|
| `auto`    | passes       | `Mediate`                                                |
| `auto`    | fails / times out | `Deny`                                              |
| `mediate` | passes       | `Mediate`                                                |
| `mediate` | fails        | start-up error naming the cause (e.g. `ptrace_scope`)   |
| `deny`    | not run      | `Deny`                                                   |
| `open`    | not run      | `Open`, with one start-up warning that sandboxed processes can reach Unix sockets outside the sandbox |

One start-up log line states the resolved policy and why. An unknown value
is a config error (`deny_unknown_fields` style, naming the key).

### 3.2 Start-up probe

`serve` (and `balerix-agent` in pod mode) runs `nono run` with a minimal
profile carrying `linux.af_unix_mediation: "pathname"`, a fresh temporary
directory granted with `unix_socket_subtree_bind`, and `open_port` for a
loopback TCP listener the caller opens. Inside it runs a new hidden
subcommand `balerix sandbox-probe`, which checks, in order:

1. TCP connect to the caller's loopback listener succeeds;
2. bind + connect of a Unix socket inside the granted directory succeeds;
3. connect to a Unix socket the caller listens on outside the grant is
   refused.

All three must hold for a pass. Step 3 proves mediation filters rather than
denying nothing. Deadline 10 s; a timeout, a missing nono or any other error
is a failure, reported with its cause. The temporary directory and both
listeners are removed afterwards.

### 3.3 Profile rendering

`render_profile`, `render_git_profile` and the plugin profile take the
policy:

- **Mediate:** add `linux.af_unix_mediation: "pathname"` at the top level
  and in `platform_overrides.linux.linux` (pinned like `signal_mode`,
  since nono applies a parent's per-OS override after the top level).
  Add `filesystem.unix_socket_subtree_bind` for the sandbox's own writable
  directories: an agent's workspace and `home/`; a plugin's `home/` and
  `scratch/`. The git profile gets no socket grant.
- **Deny:** the sandboxed command gains a wrapper inside nono:
  `nono run … -- <balerix> sandbox-exec -- <cmd…>`. `sandbox-exec` sets
  `PR_SET_NO_NEW_PRIVS`, installs a seccomp filter that fails
  `socket(AF_UNIX, …)` with `EAFNOSUPPORT`, checks the architecture (a
  foreign-arch call fails, so 32-bit or x32 `socket` cannot bypass it), and
  `exec`s `<cmd…>`. `socketpair` stays allowed. The filter is inherited
  across `exec` and cannot be removed, and nono's own supervisor (outside
  the wrapper) is unaffected. The balerix binary must be in each profile's
  read set (check agents', plugins' and the git profile's).
- **Open:** unchanged.

The launch scripts (`launch.rs`, `plugin.rs::render_plugin_launch`) and
the git runners (`workspace.rs`) carry the wrapper under Deny.

### 3.4 Pinning

Note: the warning cadence changed in implementation; see "Deviations during execution".

`check_conflicts` refuses a user, fleet or plugin-manifest `sandbox` block
that sets `linux.af_unix_mediation` or
`platform_overrides.linux.linux.af_unix_mediation`, naming the path (as
#118 does for `signal_mode`). The plugin manifest allowlist (#2) already
refuses `linux` and `unix_socket*`. A fleet block's own `unix_socket*`
grants (e.g. an operator granting `docker.sock`) work under Mediate; under
Deny they have no effect and `serve` logs one warning per fleet that sets
them.

### 3.5 Pod mode

Note: the bundle field was not shipped; see "Deviations during execution".

The agent bundle carries the operator's setting (new optional field,
default `auto`). `balerix-agent run` resolves it with the same probe inside
the agent container and renders the agent's profile accordingly. Plugin
pods are unchanged.

### 3.6 macOS

Unverified: whether Seatbelt already filters Unix socket connects by path.
CI has no macOS runner. On macOS the policy resolves to `Open` behaviour for
now (no `sandbox-exec`, no Linux key), and a follow-up records the
check for someone with a Mac.

## 4. Edge cases

- Agents launched before the upgrade keep their old profile until their
  next launch; `serve` re-renders on its first pass (as #118). The upgrade
  note says to restart agents.
- `sandbox-exec` failing to install the filter (no seccomp) fails closed:
  non-zero exit with a clear message; the agent does not start. `open` is
  the documented way out.
- Under Deny, an agent's tools that need Unix sockets (local postgres,
  docker, `git fsmonitor`) fail with "address family not supported".
  AGENTS.md and the README say so and point at the setting.
- The policy is resolved once per `serve` start; changing the host's
  `ptrace_scope` needs a restart.

## 5. Testing

- Unit: policy resolution table (§3.1) with a fake probe; profile golden
  files per policy; refusal of `af_unix_mediation` in user, fleet and
  manifest blocks at both levels; seccomp filter construction.
- `sandbox_it`, runs here: under Deny, a sandboxed process cannot connect
  to a listener socket outside the sandbox and cannot create a Unix socket;
  `socketpair` and TCP work; a test tmux server on a private `-L` name
  cannot be connected to from inside.
- `sandbox_it`, Mediate (needs `ptrace_scope` ≤ 1): a socket in the
  agent's own directory works, an outside one is refused, TCP and DNS
  work. Skips locally with a message when the probe fails; CI sets
  `BALERIX_REQUIRE_MEDIATION=1` so it cannot skip silently. First plan
  task: confirm Ubuntu runners' `ptrace_scope` and that the probe passes
  there.
- Probe: one integration test per outcome (pass in CI only).
- By hand after merge: `verify-claude` on the development host under Deny, and a real
  agent session, to confirm claude and its tools work without Unix sockets.

## 6. Rollout

- Commits `fix(runtime): …` (core patch). CHANGELOG `### Upgrading`: the
  new setting, Deny on restricted hosts and what it breaks, restart agents.
- Follow-ups: macOS check (§3.6); separate-uid
  option for restricted hosts.

## Deviations during execution

- Under `deny`, `sandbox-exec` also fails `io_uring_setup`, `io_uring_enter` and `io_uring_register` with `ENOSYS` (io_uring can create sockets without `socket()`); the two errnos need two stacked seccomp filters.
- The probe profile has no `environment.set_vars` (nono reserves `PATH`); the probe passes only if its JSON line is exactly `{"tcp":"ok","inside":"ok","outside":"refused"}`.
- The pod sidecar always resolves `auto` (§3.5 described a bundle field; none shipped, because `AgentBundle` is in published `balerix-api` with `deny_unknown_fields`). When it resolves `deny`, it checks at start-up that the image's `balerix` supports `sandbox-exec` and refuses to start otherwise, so agent images must be built on a balerix base of this release or later.
- The operator's pod harvest Job runs sandboxed git with policy `open` (waived: the Job pod has no tmux server, agent sockets or daemon token).
- Fleet `unix_socket*` grants under `deny` (§3.4): the warning is logged when an agent's profile changes, not once per fleet.
- Foreign-architecture syscalls kill the process (seccompiler's architecture check); x32 `socket` and io_uring are refused explicitly.
