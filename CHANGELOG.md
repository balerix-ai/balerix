# Changelog

### Upgrading

- 0.2.0 gives every agent a private clone (Spec N). A fleet created by
  0.1.x is refused at the next `up` or daemon start with `created by
  balerix 0.1 as a worktree`: push unpushed work first, then `balerix down
  <fleet> --purge` and `up`. `balerix down <fleet> --keep-repos` and `up`
  also works — the old crew clone becomes the object cache and the
  branches it holds seed the new clones.

## 0.1.1 - 2026-09-23

### Features

- **matrix:** Show and answer Claude's questions from the thread (Spec J) (#47)
- **common:** Extract balerix-plugin-common from matrix and web (Spec K)

### Bug fixes

- **examples:** Sandbox.network.mode is not a nono key (#35)

## 0.1.0 - 2026-09-11

- Initial release.
