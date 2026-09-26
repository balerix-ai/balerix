# Changelog

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
