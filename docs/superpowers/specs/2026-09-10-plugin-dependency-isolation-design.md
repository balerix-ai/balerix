# Balerix — Spec H: plugin dependency isolation

**Date:** 2026-09-10
**Status:** Approved in brainstorm 2026-09-10
**Scope:** split the three in-tree plugins out of the cargo workspace into
three standalone projects, each with its own lockfile, dependency table and
supply-chain policy, so the workspace table describes the daemon and client
alone. Also: per-tier cargo caching in CI. No behaviour change to any
shipped artifact.

---

## 1. Problem

`matrix-sdk` landed with Spec G and made every workspace-wide build roughly
three times slower. `mise run lint`, `mise run test` and therefore
`mise run check` — the pre-commit gate — all pass `--workspace`, so all three
pay for it. Measured on a 24-core machine into an empty target directory:

| Selection | Cold build | Target size |
|---|---|---|
| `--workspace --exclude balerix-plugin-matrix --all-targets` | 47 s | 6.4 GB |
| `--workspace --all-targets` | 143 s | 12 GB |
| flow, web and matrix alone | 121 s | 6.3 GB |
| flow and web alone | 24 s | 1.9 GB |

Three distinct causes sit behind that number, and only the first is obvious.

**Tree size.** The lockfile went from 273 packages to 524 when `matrix-sdk`
was added (commit `63c5404`). The costly members are native: `aws-lc-sys`
builds BoringSSL, `libsqlite3-sys` compiles SQLite from C under
`bundled-sqlite`, and `vodozemac` sits under the whole `ruma` proc-macro
family.

**Feature unification reaches the daemon.** Cargo unifies features across
every member selected by one invocation. Under `--workspace`,
`balerix-server` and `balerix` link a `reqwest` carrying `__rustls`,
`__rustls-aws-lc-rs`, `__tls`, `http2`, `gzip` and `stream`, none of which
they asked for. The workspace manifest's own comment on that dependency still
reads "No TLS provider, no HTTP/2, no proxy discovery". That claim is false
today in exactly the builds the gate runs.

**Duplicate versions.** 37 crate names now resolve to two versions each,
including `sha2`, `rand`, `chacha20poly1305`, `thiserror` and
`tokio-tungstenite`. The daemon's own crypto stack compiles twice.

An aggravating factor is independent of matrix: CI caches mise tools but never
the cargo target directory, so every pull request pays a full cold build of
whichever tree it is given.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| H-1 | Not Bazel. Stay on cargo. | Of the four triggers in `devkit:developer-environment`, none holds. The repo is single-language, the worst cold build is two minutes, a target cache covers the remote-cache need, and hermeticity is actively wrong here: `test-it` and the e2e exist precisely to exercise real `git`, `mise`, `nono`, `tmux` and Landlock. The tiers also lean on cargo-native tools with no ruleset equivalent (`nextest`, `insta`, `mutants`, `audit`, `deny`). |
| H-2 | Each plugin is its own project with its own `Cargo.lock`, not one shared plugin workspace. | Isolation across plugins, not only between plugins and the daemon. A `matrix-sdk` resolution can then never move `flow`'s crate versions either. It also makes each plugin's distribution story honest: a directory that happens to use `balerix-plugin-sdk`. |
| H-3 | Plugins depend on `balerix-plugin-sdk` and `balerix-api` by relative path. | Nothing in this repo is published, and a path dependency fails the plugin tier in the same commit that breaks the SDK. A registry or git dependency would defer that signal behind a release step. |
| H-4 | `balerix-plugin-sdk` stays in the core workspace. | The `balerix` binary depends on it for `dev fake-plugin`, and `balerix-server`'s integration tests use it in process. The core workspace needs it either way. |
| H-5 | `mise run check` covers the core workspace only. Plugins get their own tier, run concurrently in CI. | The gate a developer feels before every commit should not carry a Matrix client. A plugin break still fails the pull request, in a job of its own. |
| H-6 | Shared crates may resolve to different versions in different projects. | Plugins are separate processes behind a wire protocol. Nothing requires a plugin to agree with the daemon on a tokio patch release. |
| H-7 | The matrix supply-chain exemptions move to the matrix project. | Today they weaken the policy for the daemon and client too, which is broader than the accepted risk. |
| H-8 | Add per-tier cargo caching to CI in the same change. | The split makes four independent caches possible, each invalidated only by its own lockfile. A `matrix-sdk` bump stops rebuilding the daemon. |

## 3. Layout

The three plugin crates move by `git mv`, so blame survives:

```
crates/balerix-plugin-flow    ->  plugins/flow
crates/balerix-plugin-web     ->  plugins/web
crates/balerix-plugin-matrix  ->  plugins/matrix
```

Package names are unchanged (`balerix-plugin-<name>`), which keeps the
packaging loop a straight mapping from directory name to crate name.

The root workspace keeps seven crates: `balerix`, `balerix-api`,
`balerix-core`, `balerix-config`, `balerix-runtime`, `balerix-server`,
`balerix-plugin-sdk`. Its `members = ["crates/*"]` glob no longer matches any
plugin, so no `exclude` entry is needed. `scripts/check-core-deps.sh` asserts
that member list, so a plugin readmitted through the glob fails the gate even
if its tree never touches `reqwest`.

Each plugin manifest becomes a standalone project:

- an empty `[workspace]` table, which makes the package its own workspace
  root and gives it its own `Cargo.lock` (verified by probe: an upward path
  dependency to the SDK resolves, and the probe independently picked
  `reqwest` 0.13.5 while the core lock holds 0.13.4);
- literal `version`, `edition`, `rust-version`, `license`, `repository` and
  `publish` in `[package]`, since there is no workspace to inherit from;
- package-level `[lints.rust]` and `[lints.clippy]` carrying the same
  `unsafe_code = "forbid"` and the same clippy set the workspace table gives
  today;
- its own `clippy.toml` (`allow-unwrap-in-tests`, `allow-expect-in-tests`),
  because clippy resolves that file from the workspace root;
- its own `deny.toml` (§6);
- its own `target/`, added to `.gitignore` as `plugins/*/target`;
- the root `.config/nextest.toml` profile, which nextest also resolves from
  the workspace root: the plugin tiers pass `--config-file` so a hung plugin
  test still dies on the root profile's slow-timeout rather than running to
  the CI job limit.

Two path references move with the crates: the three vendored xterm entries in
`.gitleaks.toml`, and `out=` in `scripts/vendor-xterm.sh`.
`scripts/verify-matrix.sh` points at the packaging *output* directory, which
does not move.

## 4. Dependency ownership

Exactly three dependency lines leave `[workspace.dependencies]`, plus the
three now-unused plugin path entries (nothing in the workspace depends on a
plugin crate).

| Dependency | Moves to | Note |
|---|---|---|
| `matrix-sdk` | `plugins/matrix` | its only user |
| `regex` | `plugins/flow` | its only user |
| `serde_path_to_error` | all three plugins | each with its own exact version |
| `balerix-plugin-{flow,web,matrix}` | deleted | unused workspace entries |

Everything else stays, because the SDK or the daemon genuinely uses it. That
includes `axum`, `prometheus`, `tokio-tungstenite`, `futures-util`, `anyhow`,
`tracing-subscriber`, `sha2`, `hex`, `insta` and `proptest` — each of which a
plugin also uses, and each of which a plugin now declares for itself at an
exact version.

The result is the stated goal: the workspace table describes common
dependencies and the core daemon and client, and nothing else.

The `reqwest` comment in the workspace manifest becomes true again. After the
split the core resolution cannot see `matrix-sdk`, so no TLS provider, no
HTTP/2 and no compression reach the daemon.

## 5. Build and test tiers

`lint`, `test` and `check` operate on the one remaining workspace. The
`--workspace` flags stay; they simply no longer select a Matrix client.

The e2e still needs the flow and web binaries on disk, so `test` and `e2e`
keep their packaging dependency, narrowed to those two.
`scripts/package-plugins.sh` takes plugin names as arguments and defaults to
all three, so building by hand and `verify-matrix` are unaffected. It builds
each plugin inside its own project and copies the binary into
`$CARGO_TARGET_DIR/plugins/<name>/bin/`, which is where the e2e already looks.
The packaging output location does not change; only the source of the binary
does.

New tasks:

- `plugin <name>` — lint and test one plugin project.
- `plugins` — the same for all three.

`audit` loops over all four projects rather than one. `mutants` is unchanged;
it targets the reconciler.

Expected cold cost, extrapolated from §1:

| Tier | Cold |
|---|---|
| `check`, core plus flow and web packaging | about 47 s |
| `plugin flow` | about 20 s |
| `plugin web` | about 20 s |
| `plugin matrix` | about 115 s |

The local gate drops from 143 s to about 47 s. Total CPU across all tiers
rises, because shared crates now compile in four places; that is the price of
H-2 and it is paid concurrently in CI.

## 6. Supply-chain policy

`matrix-sdk` currently forces the whole repository to accept four things:

- `RUSTSEC-2026-0247` (`bitmaps`, unmaintained, via `imbl` and `eyeball-im`);
- `RUSTSEC-2026-0173` (`proc-macro-error2`, unmaintained, via `aquamarine`);
- the `CDLA-Permissive-2.0` licence (`webpki-roots`, `webpki-root-certs`);
- the `BSL-1.0` licence (`xxhash-rust`, via `growable-bloom-filter`).

All four move to `plugins/matrix/deny.toml`, with their existing reasons. The
core `deny.toml` returns to a strict allow list; `plugins/flow` and
`plugins/web` get a copy of that strict policy. The accepted risk stays scoped
to the one artifact that carries it, and a future daemon dependency that
quietly needs one of those licences fails instead of passing.

`docs/THREAT-MODEL.md`'s supply-chain row names one committed lockfile and one
deny policy; it is updated to describe four of each. `gitleaks` is repo-wide
and unaffected beyond the allowlist paths in §3.

## 7. CI

`check` keeps its shape and now covers the core workspace. A new `plugins` job
runs a three-way matrix over flow, web and matrix, each linting and testing
one project. The two jobs run concurrently, so a pull request's wall clock is
set by the matrix plugin rather than by the sum.

Each job gets a cargo registry and target cache keyed on its own lockfile,
with the plugin job keyed additionally on which plugin it is. Four independent
caches.

**Risk.** GitHub allows ten gigabytes of cache per repository and evicts
least-recently-used entries past it. Four Rust caches can approach that. If it
bites, the mitigation is to let the plugin jobs restore on pull requests but
write only on pushes to the default branch.

## 8. Conventions

`AGENTS.md` currently says a new dependency goes in `[workspace.dependencies]`
with an exact version. That becomes per-project: a core dependency goes in the
workspace table, a plugin dependency in that plugin's manifest, both exact,
both with the reason in the commit. The ports-and-adapters paragraph gains the
plugin project layout, and the gotchas gain the reason plugins are not
workspace members.

`ARCHITECTURE.md` and the Spec B and Spec G crate paths are updated to the new
directories.

## 9. Migration

Five commits, each leaving `mise run check` green.

1. **flow.** Move it out; add the task and CI plumbing; teach
   `package-plugins.sh` its argument. The smallest plugin proves the pattern
   end to end, including that the e2e still finds its binary.
2. **web.** Also carries the `.gitleaks.toml` paths and `vendor-xterm.sh`.
3. **matrix.** Also splits `deny.toml` and cleans the core policy.
4. **CI caches.**
5. **Docs and conventions** (§8).

## 10. Verification

Measured, not asserted. Baseline recorded before starting:

| Project | Tests |
|---|---|
| core seven crates | 464 |
| `balerix-plugin-flow` | 18 |
| `balerix-plugin-web` | 22 |
| `balerix-plugin-matrix` | 66 |
| total | 570 |

Test-count parity across the four projects afterwards is the guard against
silently dropping a plugin's tests.

Then, on the finished state:

- **Feature isolation.** `cargo tree -p balerix-server -e features -i reqwest`
  in the core workspace shows no `rustls`, `http2`, `gzip` or `stream`.
- **Lockfile shrink.** Core package count moves from 524 back toward 273, and
  the 37 duplicate-version crate names largely disappear. Record the actual
  numbers rather than predicting them.
- **Timed cold build.** The core gate against the 143 s baseline.
- **Green tiers.** `mise run e2e`, and `mise run audit` across all four
  projects.

**Risk to check before committing step 3.** Removing the licence allowances
from the core policy assumes nothing else in the daemon's tree needs them. If
something does, the entry stays with a reason naming the real dependency
instead of matrix.

## 10.1 Measured result

| Measure | Before | After |
|---|---|---|
| `check` cold build | 143 s | 57 s |
| core target size | 12 GB | 5.4 GB |
| core lockfile packages | 524 | 270 |
| core duplicated crate names | 37 | 16 |
| matrix project cold build | (inside the 143 s) | 154 s |
| tests, core / flow / web / matrix | 570 in one tier | 464 / 18 / 22 / 66 (sum 570) |

Core `reqwest` features after the split: `json` only.

Measured on a 24-core, 125 GiB machine, matching the core count of the
24-core machine described in §1 that produced the "before" column and the
brief's extrapolated "after" estimates (about 47 s core, about 115 s
matrix). The core cold build (57 s) and the matrix cold build (154 s) both
ran somewhat slower here than those estimates — plausibly disk speed or
background load on this host rather than a difference in what gets
compiled — but the shape of the result is unchanged: the core gate dropped
from 143 s to 57 s (a 60% cut), and the matrix build that used to be inside
that 143 s now runs as its own 154 s job, concurrently with core rather than
serially inside it.

## 11. Deliberately deferred

- **Publishing the SDK.** H-3 keeps path dependencies. Publishing
  `balerix-api` and `balerix-plugin-sdk` so plugins depend by version is the
  natural next step if an out-of-tree plugin author ever appears, and it is
  not needed to get the isolation.
- **Sharing one target directory across projects.** Each project keeps its
  own. Sharing would cut disk at the cost of lock contention and a `cargo
  clean` that reaches further than intended.
- **A non-Rust plugin.** The plugin package format already delegates each
  plugin's build to its own mise task behind the process protocol, so this
  needs no shared build graph and no change here. It is the condition that
  would reopen H-1.

## 12. Editor and bot follow-ups

Neither blocks the work; both are consequences worth confirming.

- rust-analyzer sees only the root workspace by default. A linked-projects
  setting listing all four is a small convenience.
- Renovate's cargo manager detects every `Cargo.toml`, so four lockfiles
  should need no config change. `renovate.json` sets `lockFileMaintenance`
  for Monday mornings; confirm after the first run that all four lockfiles
  are being maintained, and add `"enabledManagers"` scoping only if one is
  missed.

## 13. Done when

- `plugins/{flow,web,matrix}` are standalone projects, each with its own
  lockfile, dependency table, lints, `clippy.toml` and `deny.toml`.
- `[workspace.dependencies]` names only what the core crates and the SDK use.
- `mise run check` builds no Matrix client, and the core `reqwest` carries no
  TLS, HTTP/2 or compression features.
- `mise run plugins` lints and tests all three; CI runs them concurrently with
  `check`, each behind its own cargo cache.
- The four matrix supply-chain exemptions live only in the matrix project.
- Test counts across the four projects sum to the recorded baseline.
