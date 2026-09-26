# Changelog

## 0.1.1 - 2026-09-26

### Features

- **matrix:** Post the assistant's message, split across parts (#36)
- **matrix:** Show and answer Claude's questions from the thread (Spec J) (#47)
- **common:** Extract balerix-plugin-common from matrix and web (Spec K)
- **core:** Plugin-managed fleets, owned records and a per-agent branch (Spec L)

### Bug fixes

- **examples:** Sandbox.network.mode is not a nono key (#35)
- **core:** Stop and refuse a 0.1.x agent at the first pass; a failed step gets the phase `failed` (#76)

### Build

- **matrix:** Matrix-sdk 0.18.0 -> 0.19.1, clearing the two audit advisories (#57)

## 0.1.0 - 2026-09-11

- Initial release.
