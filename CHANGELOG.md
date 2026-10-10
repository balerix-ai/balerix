# Changelog

### Upgrading

- `balerix_plugin_events_dropped_total` has a second label, `reason`
  (`overflow`, `not_ready` or `unacknowledged`), so its label set is now
  `{plugin, reason}` (#13). A query or alert that matched the series on
  `{plugin}` alone still matches; one that compares series by their full
  label set (`on(plugin)` is fine, a bare `/` or `==` against another
  `{plugin}` metric is not) needs `sum by (plugin)` first.

## 0.2.3 - 2026-10-09

### Bug fixes

- **runtime:** Bind the socket probe's sockets in a short directory (#196)

## 0.2.2 - 2026-10-09

### Features

- Git and sandbox batch (#118, #108, #111) (#173)

### Bug fixes

- Supervisor batch (#148, #120, #123, #122, #121) (#158)
- Plugin batch (#17, #42, #20, #49, #48) (#159)
- Daemon batch (#9, #11, #10, #12, #116) (#166)
- Plugin host batch (#170, #15, #14, #168, #6, #1, #169) (#172)
- Toolchain batch (#24, #23, #22, #25) (#180)
- Workspace reader batch (#174, #18, #19, #20) (#181)
- Plugin hardening batch (#2, #3, #4, #5, #7) (#188)
- **runtime:** Keep sandboxed processes off Unix sockets outside their grants (#189)

### Upgrading

- New `[sandbox] unix_sockets` setting in the daemon's `config.toml`
  (`auto` | `mediate` | `deny` | `open`, default `auto`) controls which
  Unix sockets agents, plugins and the daemon's sandboxed git can reach.
  On a host where nono's pathname mediation does not work
  (`kernel.yama.ptrace_scope = 2`, restricted containers) `auto` resolves
  to `deny`: sandboxed processes can no longer create Unix sockets (a
  local postgres, docker or `git fsmonitor` in an agent's workspace fails
  with "address family not supported") unless you set `open`. Agents
  already running keep their old profile until their next launch: restart
  them with `balerix down` / `balerix up`, or restart the daemon, which
  re-renders on its first pass. In Kubernetes mode the pod sidecar always
  resolves `auto` and, when that is `deny`, refuses to start unless the
  image's `balerix` supports `sandbox-exec`: build agent images on a
  balerix base of this release or later.
- The daemon now refuses an inexact tool version (`latest`, `22`, `22.x`,
  …) in a fleet's, crew's or agent's `tools` on every fleet it is posted,
  not only in `balerix up`'s client-side check (#24). A stored fleet
  record written by a client that bypassed that check and holding such a
  version fails every pass after the upgrade with `<fleet>: invalid
  stored spec: <path>: expected an exact version, got "…"`; re-apply it
  with exact versions (`mise latest <tool>@<version>` names one), or
  `balerix down` it. `down` is enough on one machine only: in Kubernetes
  mode a record whose spec no longer converts sends its agents no Stop,
  so re-apply it there instead.

## 0.2.1 - 2026-10-07

### Features

- **core:** Restricted settings surface for plugin-applied fleet files, the operator's fleetDefaults layer, and the fleets-gated PUT answer (Spec M §12) (#86)
- **github:** The GitHub plugin, an agent per issue or pull request (Spec M) (#93)
- **server:** Kubernetes mode and the balerix-agent sidecar over a TLS link (Spec O §6, §7) (#125)
- **operator:** The operator project, its five kinds and pure desired state; the sync and harvest Jobs (Spec O §20) (#126)
- **operator:** The controllers, envtest tests, the images, kind and e2e-k8s (Spec O §21) (#127)
- Plugins without a cluster (Spec O 4a, §23.1–§23.3) (#143)
- Plugins on a cluster (Spec O 4b, §23.4–§23.9) (#144)
- The Helm charts (Spec O 5a, §24.1–§24.3) (#145)
- Release the operator, agent and charts (Spec O 5b, §24.4–§24.6) (#146)

### Bug fixes

- **common:** Press Enter again while a prompt is unconfirmed, adopted by github and matrix (#99) (#101)
- **flow:** Press Enter again while a submitted send is unconfirmed (#100) (#102)
- **github:** Ignore a redelivered opening on a live row; deliver reviews in arrival order (#96) (#103)
- **github:** Write the closed row in end at once; prune closed rows after seven days (#97) (#105)
- **runtime:** Run daemon git in an agent's clone under a read-only nono profile (#68, #70) (#110)
- **runtime:** Grant git's exec-path in the git profile; a sandbox failure is never git's "no" (#109) (#112)
- **server:** Write the activation rows before the actor's first snapshot (#113) (#114)
- **runtime:** Stop ends the sandbox's whole process tree and waits for it to be empty (#107) (#117)
- **operator:** Serve the controllers' watches from an unpooled client (#129) (#139)
- **operator:** Hardening — request bound, timeout back-off, Job failure, crew lock, CA check (Spec O §22) (#141)
- Flake batch (#136, #137, #142), release-PR CI, claude 2.1.292 (#147)

## 0.2.0 - 2026-09-27

### Features

- **core:** Plugin-managed fleets, owned records and a per-agent branch (Spec L)
- **runtime:** [**breaking**] A private clone per agent (Spec N) (#66)

### Bug fixes

- **cli_serve:** Poll for the pid file as well as the endpoint on SIGTERM (#55)
- **runtime:** Detect a changed agent branch against a marker, not HEAD (#65)
- **runtime:** Decline claude's auto-mode default offer in the seeded .claude.json (#75)
- **core:** Stop and refuse a 0.1.x agent at the first pass; a failed step gets the phase `failed` (#76)
- **runtime:** Keep no clone-time default branch in the crew cache (#78)
- **runtime:** Refuse a promisor remote in the clone's config on any git version (#80)
- **runtime:** A git error is never a "no" in the clone and cache paths; trust only the workspace (#81)
- **server:** Refuse a removed plugin's apply; purge the fleets listed after the sync; threat-model example (#82)

### Upgrading

- 0.2.0 gives every agent a private clone (Spec N). Before upgrading,
  `balerix down <fleet>` every fleet (`--keep-repos` keeps its branches;
  they seed the new clones), then `up` on 0.2.0. A 0.1.x agent left
  running is stopped at the 0.2.0 daemon's first pass (its start, or the
  next `up`), never adopted under its old sandbox, and refused with
  `created by balerix 0.1 as a worktree`; `balerix status` shows it as
  `failed` with that message, and its worktree is left in place — push
  unpushed work from it, then `balerix down <fleet> --purge` (or
  `--keep-repos`) and `up`. From 0.2.0 only an agent's assigned branch
  survives `down --keep-repos`, a removal or a `branch` change; other local
  branches and the working tree go with the clone.
- An agent whose materialize or start step failed now has the phase
  `failed` (previously it kept its last phase and only the message said
  so); it is retried every pass and returns to the usual phases on the
  next success. Anything matching on phase strings should expect the new
  value.
- The pinned `claude` is now 2.1.283. From that version an interactive
  session starts in auto mode unless a settings file sets
  `permissions.defaultMode`, so an agent whose fleet and host settings set
  none now runs under the auto-mode classifier rather than in manual mode.
  Set `permissions.defaultMode` in the fleet's `settings` (or your host
  `~/.claude/settings.json`, which every agent inherits) to choose. The
  one-time "Make auto mode your default permission mode?" offer that
  2.1.282 added is declined in the agent's seeded `.claude.json` (#73).

## 0.1.1 - 2026-09-23

### Features

- **matrix:** Show and answer Claude's questions from the thread (Spec J) (#47)
- **common:** Extract balerix-plugin-common from matrix and web (Spec K)

### Bug fixes

- **examples:** Sandbox.network.mode is not a nono key (#35)

## 0.1.0 - 2026-09-11

- Initial release.
