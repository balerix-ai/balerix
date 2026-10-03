# Changelog

## 0.2.1 - 2026-10-03

### Features

- **core:** Restricted settings surface for plugin-applied fleet files, the operator's fleetDefaults layer, and the fleets-gated PUT answer (Spec M §12) (#86)
- **github:** The GitHub plugin, an agent per issue or pull request (Spec M) (#93)
- **server:** Kubernetes mode and the balerix-agent sidecar over a TLS link (Spec O §6, §7) (#125)
- **operator:** The operator project, its five kinds and pure desired state; the sync and harvest Jobs (Spec O §20) (#126)

### Bug fixes

- **common:** Press Enter again while a prompt is unconfirmed, adopted by github and matrix (#99) (#101)
- **flow:** Press Enter again while a submitted send is unconfirmed (#100) (#102)
- **github:** Ignore a redelivered opening on a live row; deliver reviews in arrival order (#96) (#103)
- **github:** Write the closed row in end at once; prune closed rows after seven days (#97) (#105)
- **runtime:** Run daemon git in an agent's clone under a read-only nono profile (#68, #70) (#110)
- **runtime:** Grant git's exec-path in the git profile; a sandbox failure is never git's "no" (#109) (#112)
- **server:** Write the activation rows before the actor's first snapshot (#113) (#114)
- **runtime:** Stop ends the sandbox's whole process tree and waits for it to be empty (#107) (#117)

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
