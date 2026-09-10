# Plugin Dependency Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the three in-tree plugins out of the cargo workspace into three standalone projects, each with its own lockfile, dependency table and supply-chain policy, so `mise run check` never builds a Matrix client.

**Architecture:** Each plugin becomes a cargo package that is its own workspace root (an empty-but-for-`resolver` `[workspace]` table), living at `plugins/<name>/`, depending on `balerix-plugin-sdk` and `balerix-api` by relative path upward into the core workspace. The core `[workspace.dependencies]` table keeps only what the seven core crates and the SDK use. A per-plugin tier of mise tasks and a CI matrix job run the plugin projects concurrently with the core `check` job, each behind its own cargo cache.

**Tech Stack:** Rust (edition 2024), cargo, mise tasks, cargo-nextest, cargo-deny, cargo-audit, GitHub Actions, `Swatinem/rust-cache`.

**Spec:** `docs/superpowers/specs/2026-09-10-plugin-dependency-isolation-design.md`

## Global Constraints

- **Every version is exact.** `mise.toml` entries and every cargo dependency. An unpinned entry is a reproducibility bug (AGENTS.md).
- **Run cargo through mise:** `mise x -- cargo …`, or via a `mise run` task. Never bare `cargo`.
- **Package metadata literals** for every standalone plugin manifest, copied from the core `[workspace.package]`: `version = "0.1.0"`, `edition = "2024"`, `rust-version = "1.98"`, `license = "Apache-2.0"`, `repository = "https://github.com/balerix-ai/balerix"`, `publish = false`.
- **Lints, verbatim in every standalone plugin manifest** (there is no workspace table to inherit from):
  ```toml
  [lints.rust]
  unsafe_code = "forbid"

  [lints.clippy]
  all = { level = "warn", priority = -1 }
  unwrap_used = "warn"
  expect_used = "warn"
  ```
- **`clippy.toml`, verbatim in every plugin project root** (clippy resolves it from the workspace root, and each plugin is now its own root):
  ```toml
  allow-unwrap-in-tests = true
  allow-expect-in-tests = true
  ```
- **Package names never change.** `balerix-plugin-flow`, `balerix-plugin-web`, `balerix-plugin-matrix`. Directory names do. Insta snapshot filenames embed the crate name, so leaving names alone keeps every `.snap` valid.
- **Move with `git mv`,** never copy-and-delete, so blame survives.
- **Packaging output does not move.** The e2e finds plugin packages at `$CARGO_TARGET_DIR/plugins/<name>/bin/balerix-plugin-<name>` by walking up from its own binary. Only the *source* of the copied binary changes.
- **Each commit leaves `mise run check` green.** The pre-commit hook runs the full check and writes a lot of output; redirect it to a file and read the file.
- **Test-count baseline, recorded before any change** — parity across the four projects at the end is the guard against silently dropping tests:

  | Project | Tests |
  |---|---|
  | core seven crates | 464 |
  | `balerix-plugin-flow` | 18 |
  | `balerix-plugin-web` | 22 |
  | `balerix-plugin-matrix` | 66 |
  | **total** | **570** |

---

## File Structure

**Created:**
- `plugins/flow/Cargo.toml`, `plugins/flow/Cargo.lock`, `plugins/flow/clippy.toml`, `plugins/flow/deny.toml`
- `plugins/web/Cargo.toml`, `plugins/web/Cargo.lock`, `plugins/web/clippy.toml`, `plugins/web/deny.toml`
- `plugins/matrix/Cargo.toml`, `plugins/matrix/Cargo.lock`, `plugins/matrix/clippy.toml`, `plugins/matrix/deny.toml`
- `scripts/plugin.sh` — the one place that knows how to build, lint and test a plugin project, and where its target directory is. Both `mise run plugin` and `scripts/package-plugins.sh` call it, so the target directory is defined once.

**Moved (`git mv`, whole directory):**
- `crates/balerix-plugin-flow/` → `plugins/flow/`
- `crates/balerix-plugin-web/` → `plugins/web/`
- `crates/balerix-plugin-matrix/` → `plugins/matrix/`

**Modified:**
- `Cargo.toml` — drop three plugin path entries plus `matrix-sdk`, `regex`, `serde_path_to_error`
- `Cargo.lock` — shrinks as a consequence
- `deny.toml` — the four matrix exemptions leave
- `mise.toml` — `plugin`, `plugins` tasks; `test`/`e2e` packaging narrowed; `audit` loops four projects
- `scripts/package-plugins.sh` — takes plugin names, builds via `scripts/plugin.sh`
- `scripts/vendor-xterm.sh` — `out=` path
- `.gitleaks.toml` — three allowlist paths
- `.gitignore` — `plugins/*/target`
- `.github/workflows/ci.yml` — new `plugins` matrix job, caches on both jobs
- `AGENTS.md`, `ARCHITECTURE.md`, `docs/THREAT-MODEL.md`, `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md`, `docs/superpowers/specs/2026-09-09-balerix-g-matrix-plugin-design.md` — paths and conventions

---

## Task 1: The flow plugin as a standalone project

The smallest plugin, and the one the e2e depends on. Doing it first proves the whole pattern: the manifest shape, the shared script, the mise tier, the CI job, and that the e2e still finds a binary built from outside the workspace.

**Files:**
- Move: `crates/balerix-plugin-flow/` → `plugins/flow/`
- Modify: `plugins/flow/Cargo.toml`
- Create: `plugins/flow/clippy.toml`, `plugins/flow/deny.toml`, `scripts/plugin.sh`
- Modify: `Cargo.toml`, `mise.toml`, `scripts/package-plugins.sh`, `.gitignore`, `.github/workflows/ci.yml`
- Test: `crates/balerix/tests/e2e.rs` (unchanged, but its `flow_journey` is the acceptance test)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `scripts/plugin.sh`, with three subcommands used by Tasks 2, 3 and 4:
  - `scripts/plugin.sh target-dir <name>` prints the absolute target directory for that plugin (`<repo>/plugins/<name>/target`).
  - `scripts/plugin.sh build <name>` builds the plugin's binary into that directory.
  - `scripts/plugin.sh check <name>` runs rustfmt check, clippy with warnings denied, and nextest for that project.
  Also produces the `mise run plugin <name>` and `mise run plugins` tasks, and the `plugins` CI job with a `plugin` matrix that Tasks 2 and 3 extend.

- [ ] **Step 1: Record the baseline, so the end state can be checked against it**

Run and keep the output somewhere you can read later in the plan:

```bash
mise x -- cargo nextest list --workspace --message-format json 2>/dev/null | python3 -c "
import json,sys,collections
d=json.load(sys.stdin); c=collections.Counter()
for s in d['rust-suites'].values(): c[s['package-name']]+=len(s['testcases'])
for k in sorted(c): print(f'{k:28} {c[k]}')
print(f'{\"TOTAL\":28} {sum(c.values())}')"
grep -c '^\[\[package\]\]' Cargo.lock
grep '^name = ' Cargo.lock | sort | uniq -d | wc -l
```

Expected, matching the table in Global Constraints: 570 tests total, 524 lockfile packages, 37 duplicated crate names.

- [ ] **Step 2: Write the failing check — the core workspace must not link a TLS stack**

This is the red test for the whole plan. Create `scripts/check-core-deps.sh`:

```bash
#!/usr/bin/env bash
# The daemon and client must not inherit a plugin's dependency features.
# `reqwest` in the core workspace is declared `default-features = false`
# with only `json`; anything else means a plugin leaked into the core
# feature resolution (Spec H §1, §4).
set -euo pipefail
cd "$(dirname "$0")/.."

tree=$(cargo tree -p balerix-server --edges features --invert reqwest)
bad=()
for feature in __tls __rustls http2 gzip stream; do
  if grep -q "reqwest feature \"$feature\"" <<<"$tree"; then
    bad+=("$feature")
  fi
done

if (( ${#bad[@]} )); then
  echo "core reqwest carries plugin-only features: ${bad[*]}" >&2
  exit 1
fi
echo "core reqwest features are clean"
```

- [ ] **Step 3: Run it and watch it fail**

```bash
chmod +x scripts/check-core-deps.sh
mise x -- scripts/check-core-deps.sh
```

Expected: FAIL, `core reqwest carries plugin-only features: __tls __rustls http2 gzip stream`, exit 1. It stays failing until Task 3 moves `matrix-sdk` out; Tasks 1 and 2 do not fix it.

- [ ] **Step 4: Move the crate**

```bash
mkdir -p plugins
git mv crates/balerix-plugin-flow plugins/flow
```

- [ ] **Step 5: Rewrite `plugins/flow/Cargo.toml` as a standalone project**

Replace the whole file with this. Every version is copied verbatim from the core `[workspace.dependencies]` table.

```toml
# A standalone project, not a workspace member (Spec H). Its dependencies
# are its own: nothing here reaches the daemon's feature resolution, and
# nothing the daemon adds reaches this lockfile.
[workspace]
resolver = "3"

[package]
name = "balerix-plugin-flow"
description = "The flow plugin: a per-agent state machine over hook events (plugins spec §8.1)"
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/balerix-ai/balerix"
publish = false

[[bin]]
name = "balerix-plugin-flow"
path = "src/main.rs"

[dependencies]
balerix-api = { path = "../../crates/balerix-api" }
balerix-plugin-sdk = { path = "../../crates/balerix-plugin-sdk" }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
# Attaches the config path (`states.working.on[1].foo`) to serde's error so
# a plugin config error reads like every other balerix config error.
serde_path_to_error = "0.1.20"
# The rule matcher (plugins spec §8.1, §17.2): full-match regexes with a
# 10 KiB size limit.
regex = "1.13.1"
sha2 = "0.11.0"
hex = "0.4.3"
thiserror = "2.0.20"
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net"] }
anyhow = "1.0.104"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }

[dev-dependencies]
proptest = "1.11.0"

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
expect_used = "warn"
```

- [ ] **Step 6: Add the project's `clippy.toml` and `deny.toml`**

`plugins/flow/clippy.toml`:

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
```

`plugins/flow/deny.toml` — the strict policy, with no matrix exemptions:

```toml
[advisories]
ignore = []

[licenses]
allow = [
  "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
  "ISC", "Zlib", "Unicode-3.0", "Unicode-DFS-2016", "MPL-2.0", "CC0-1.0", "0BSD",
]

[bans]
multiple-versions = "warn"
wildcards = "deny"
# Path dependencies on the core crates carry no version; every crate is
# `publish = false`.
allow-wildcard-paths = true

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

- [ ] **Step 7: Drop flow's entries from the core workspace**

In `Cargo.toml`, delete these two lines from `[workspace.dependencies]`:

```toml
balerix-plugin-flow = { path = "crates/balerix-plugin-flow" }
```

and

```toml
# The flow plugin's rule matcher (plugins spec §8.1, §17.2): full-match
# regexes with a 10 KiB size limit. Already in the lock through
# tracing-subscriber; now a direct, exact dependency.
regex = "1.13.1"
```

Leave `serde_path_to_error` for now; web and matrix still use it from the workspace table.

- [ ] **Step 8: Create `scripts/plugin.sh`**

```bash
#!/usr/bin/env bash
# Build, lint and test one standalone plugin project (Spec H).
#
# Each plugin is its own cargo workspace with its own lockfile, so every
# invocation runs from that plugin's directory. Its target directory is
# pinned here rather than inherited, because an ambient CARGO_TARGET_DIR
# would otherwise move the binary out from under package-plugins.sh.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"

usage() { echo "usage: $0 {target-dir|build|check} <name>" >&2; exit 2; }
[[ $# -eq 2 ]] || usage
cmd=$1
name=$2
dir="$repo/plugins/$name"
[[ -d $dir ]] || { echo "no such plugin: $name" >&2; exit 1; }
target="$dir/target"

case "$cmd" in
  target-dir)
    echo "$target"
    ;;
  build)
    CARGO_TARGET_DIR="$target" cargo build -q --manifest-path "$dir/Cargo.toml"
    ;;
  check)
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all --check
    CARGO_TARGET_DIR="$target" cargo clippy --manifest-path "$dir/Cargo.toml" --all-targets -- -D warnings
    CARGO_TARGET_DIR="$target" cargo nextest run --manifest-path "$dir/Cargo.toml"
    ;;
  *)
    usage
    ;;
esac
```

- [ ] **Step 9: Teach `scripts/package-plugins.sh` about standalone projects**

Replace the whole file:

```bash
#!/usr/bin/env bash
# Assembles each named plugin as a directory source under
# $CARGO_TARGET_DIR/plugins/ (target/plugins/ by default; plugins spec
# §17.6): the project's package/ files plus the freshly built binary in
# bin/. The e2e locates the built binary and this output directory the
# same way, relative to $CARGO_TARGET_DIR, so both must agree.
#
# Plugins are standalone projects (Spec H), so the binary is built inside
# plugins/<name>/ and copied here; only the output location is shared.
# With no arguments, every in-tree plugin.
set -euo pipefail
cd "$(dirname "$0")/.."

names=("$@")
(( ${#names[@]} )) || names=(flow web matrix)

out_root="${CARGO_TARGET_DIR:-target}"

for name in "${names[@]}"; do
  crate="balerix-plugin-$name"
  scripts/plugin.sh build "$name"
  built="$(scripts/plugin.sh target-dir "$name")/debug/$crate"
  out="$out_root/plugins/$name"
  rm -rf "$out"
  mkdir -p "$out/bin"
  cp "$built" "$out/bin/$crate"
  cp "plugins/$name/package/mise.toml" "plugins/$name/package/balerix-plugin.yaml" "$out/"
  echo "packaged $name -> $out"
done
```

- [ ] **Step 10: Add the plugin tier to `mise.toml`**

Add these two tasks, and change `package-plugins`' description to say it takes names:

```toml
[tasks.plugin]
description = "Lint and test one standalone plugin project: `mise run plugin matrix`"
run = "scripts/plugin.sh check {{arg(name='name')}}"

[tasks.plugins]
description = "Lint and test every standalone plugin project (its own tier; not part of `check`)"
run = [
  "scripts/plugin.sh check flow",
  "scripts/plugin.sh check web",
  "scripts/plugin.sh check matrix",
]
```

Change `test` and `e2e` so they package only what the e2e needs:

```toml
[tasks.test]
description = "Unit + integration tests for the core workspace"
depends = ["package-plugins flow web"]
run = "cargo nextest run --workspace"
```

```toml
[tasks.e2e]
description = "End-to-end journey against a real daemon, git, mise, nono and tmux (fails, not skips, without tools)"
depends = ["package-plugins flow web"]
env = { BALERIX_REQUIRE_TOOLS = "1" }
run = "cargo nextest run -p balerix --test e2e"
```

And give `package-plugins` its argument:

```toml
[tasks.package-plugins]
description = "Build the named in-tree plugins (default: all) and assemble them as directory sources under target/plugins/<name>/"
run = "scripts/package-plugins.sh {{arg(name='names', var=true, default='flow web matrix')}}"
```

- [ ] **Step 11: Verify mise passes the task arguments through**

`depends` with arguments is the one piece of mise syntax this plan relies on that is not already used in the repo. Prove it before going further:

```bash
chmod +x scripts/plugin.sh
mise run package-plugins flow 2>&1 | tail -5
```

Expected: `packaged flow -> target/plugins/flow`, and no `web` or `matrix` line.

Then:

```bash
mise run test 2>&1 | grep -E '^packaged'
```

Expected: exactly two lines, `flow` and `web`. If `depends = ["package-plugins flow web"]` does not pass arguments in the installed mise version, replace it with an explicit `run` prelude in `test` and `e2e`: put `"scripts/package-plugins.sh flow web"` as the first entry of a `run` array and drop the `depends`. Record which form you used.

- [ ] **Step 12: Ignore the plugin target directories**

Append to `.gitignore`:

```
plugins/*/target
```

- [ ] **Step 13: Add the plugins job to CI**

In `.github/workflows/ci.yml`, after the `check` job, add:

```yaml
  # Each plugin is a standalone project with its own lockfile (Spec H), so
  # each gets its own job and its own cache. They run concurrently with
  # `check`, which no longer builds any of them.
  plugins:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        plugin: [flow]
    steps:
      - uses: actions/checkout@v4
      - uses: jdx/mise-action@v2
        with: { version: 2026.9.2, install: false, cache: true }
      - run: mise install rust cargo:cargo-nextest
      - run: mise run plugin ${{ matrix.plugin }}
```

The matrix lists only `flow` for now; Tasks 2 and 3 add `web` and `matrix` as they land, so CI is green at every commit.

- [ ] **Step 14: Run the plugin tier and the core gate**

```bash
mise run plugin flow
mise run check > /tmp/check-flow.log 2>&1; echo "exit=$?"; tail -20 /tmp/check-flow.log
```

Expected: the plugin tier passes 18 tests. `mise run check` passes, and its nextest summary now shows 552 tests rather than 570 (570 minus flow's 18).

- [ ] **Step 15: Run the e2e, which is the real acceptance test for this task**

```bash
mise run e2e > /tmp/e2e-flow.log 2>&1; echo "exit=$?"; tail -30 /tmp/e2e-flow.log
```

Expected: PASS. This proves the e2e still finds `target/plugins/flow/bin/balerix-plugin-flow` even though it was built outside the workspace. If it reports `target/plugins/flow (run mise run package-plugins)`, the copy destination in `package-plugins.sh` is wrong; it must remain `${CARGO_TARGET_DIR:-target}/plugins/<name>`, not the plugin's own target directory.

- [ ] **Step 16: Commit**

```bash
git add -A
git commit -F - <<'MSG'
Make the flow plugin a standalone project

First of three (Spec H). flow moves to plugins/flow with its own
Cargo.lock, dependency table, lints, clippy.toml and deny.toml, so its
resolution is independent of the daemon's and of the other plugins'.
regex, its only user, leaves [workspace.dependencies] with it.

scripts/plugin.sh is the one place that knows where a plugin's target
directory is; package-plugins.sh builds through it and keeps copying into
$CARGO_TARGET_DIR/plugins/<name>, which is where the e2e looks, so the
packaging contract is unchanged. test and e2e now package only flow and
web, the two the journey actually needs.

scripts/check-core-deps.sh is added failing on purpose: the daemon's
reqwest still carries matrix-sdk's rustls, HTTP/2, gzip and stream through
workspace feature unification. It goes green in the third commit.
MSG
```

---

## Task 2: The web plugin as a standalone project

Same shape as Task 1, plus two path references that live outside the crate: the gitleaks allowlist and the vendoring script.

**Files:**
- Move: `crates/balerix-plugin-web/` → `plugins/web/`
- Modify: `plugins/web/Cargo.toml`
- Create: `plugins/web/clippy.toml`, `plugins/web/deny.toml`
- Modify: `Cargo.toml`, `.gitleaks.toml`, `scripts/vendor-xterm.sh`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `scripts/plugin.sh` and the `plugins` CI job from Task 1.
- Produces: nothing new; extends the CI matrix to `[flow, web]`.

- [ ] **Step 1: Move the crate**

```bash
git mv crates/balerix-plugin-web plugins/web
```

The vendored assets and the `include_bytes!("../assets/…")` calls in `src/routes.rs` are relative to the source file, so they move intact and need no edit.

- [ ] **Step 2: Rewrite `plugins/web/Cargo.toml` as a standalone project**

```toml
# A standalone project, not a workspace member (Spec H).
[workspace]
resolver = "3"

[package]
name = "balerix-plugin-web"
description = "The web plugin: agents' terminals in a browser (plugins spec §18.5)"
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/balerix-ai/balerix"
publish = false

[[bin]]
name = "balerix-plugin-web"
path = "src/main.rs"

[dependencies]
balerix-api = { path = "../../crates/balerix-api" }
balerix-plugin-sdk = { path = "../../crates/balerix-plugin-sdk" }
axum = { version = "0.8.9", features = ["ws"] }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
serde_path_to_error = "0.1.20"
thiserror = "2.0.20"
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net"] }
anyhow = "1.0.104"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
sha2 = "0.11.0"
hex = "0.4.3"
tracing = "0.1.44"

[dev-dependencies]
tokio-tungstenite = "0.30.0"
futures-util = { version = "0.3.34", default-features = false, features = ["sink", "std"] }
reqwest = { version = "0.13.4", default-features = false, features = ["json"] }
proptest = "1.11.0"

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
expect_used = "warn"
```

- [ ] **Step 3: Add the project's `clippy.toml` and `deny.toml`**

`plugins/web/clippy.toml`:

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
```

`plugins/web/deny.toml` — identical to `plugins/flow/deny.toml`:

```toml
[advisories]
ignore = []

[licenses]
allow = [
  "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
  "ISC", "Zlib", "Unicode-3.0", "Unicode-DFS-2016", "MPL-2.0", "CC0-1.0", "0BSD",
]

[bans]
multiple-versions = "warn"
wildcards = "deny"
# Path dependencies on the core crates carry no version; every crate is
# `publish = false`.
allow-wildcard-paths = true

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

- [ ] **Step 4: Drop web's entry from the core workspace**

In `Cargo.toml`, delete from `[workspace.dependencies]`:

```toml
balerix-plugin-web = { path = "crates/balerix-plugin-web" }
```

- [ ] **Step 5: Fix the gitleaks allowlist paths**

In `.gitleaks.toml`, the three regexes and the prose above them name the old directory. Change the paths to:

```toml
paths = [
  '''^plugins/web/assets/xterm\.js$''',
  '''^plugins/web/assets/xterm\.css$''',
  '''^plugins/web/assets/addon-fit\.js$''',
]
```

and update the comment's `crates/balerix-plugin-web/assets/` to `plugins/web/assets/`.

- [ ] **Step 6: Prove the allowlist still matches**

If the paths were wrong, gitleaks would flag the minified xterm bundle, whose `t.FourKeyMap=void 0` reads to the entropy heuristic as a secret. Stage the move first, so gitleaks has something to scan:

```bash
git add -A
mise x -- gitleaks git --pre-commit --staged --redact
```

Expected: no findings. A `generic-api-key` finding in `plugins/web/assets/xterm.js` means the allowlist regexes still point at the old directory.

- [ ] **Step 7: Fix the vendoring script's output path**

In `scripts/vendor-xterm.sh`, change:

```bash
out=crates/balerix-plugin-web/assets
```

to:

```bash
out=plugins/web/assets
```

and update the path in the comment on line 3.

Then verify it against the committed files, offline:

```bash
mise x -- scripts/vendor-xterm.sh --check
```

Expected: every digest verifies. A "no such file or directory" means the path edit is wrong.

- [ ] **Step 8: Extend the CI matrix and run both tiers**

In `.github/workflows/ci.yml`, change the plugins job matrix to:

```yaml
        plugin: [flow, web]
```

Then locally:

```bash
mise run plugin web
mise run check > /tmp/check-web.log 2>&1; echo "exit=$?"; tail -20 /tmp/check-web.log
```

Expected: the web tier passes 22 tests; `mise run check` passes with 530 tests (552 minus web's 22).

- [ ] **Step 9: Run the e2e, which exercises the web plugin too**

```bash
mise run e2e > /tmp/e2e-web.log 2>&1; echo "exit=$?"; tail -30 /tmp/e2e-web.log
```

Expected: PASS. The web journey needs `target/plugins/web/bin/balerix-plugin-web`, so this proves the packaging path for the second plugin.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -F - <<'MSG'
Make the web plugin a standalone project

Second of three (Spec H). web moves to plugins/web with its own
Cargo.lock, dependency table, lints, clippy.toml and deny.toml.

Two path references live outside the crate and move with it: the three
vendored xterm files in the gitleaks allowlist, which are excepted by
exact path, and the output directory in scripts/vendor-xterm.sh. The
include_bytes! calls are relative to the source file, so they need no
edit. `scripts/vendor-xterm.sh --check` verifies every digest offline
against the moved files.
MSG
```

---

## Task 3: The matrix plugin as a standalone project

The task the whole plan exists for. It also splits the supply-chain policy, which is where the core `deny.toml` gets its strictness back.

**Files:**
- Move: `crates/balerix-plugin-matrix/` → `plugins/matrix/`
- Modify: `plugins/matrix/Cargo.toml`
- Create: `plugins/matrix/clippy.toml`, `plugins/matrix/deny.toml`
- Modify: `Cargo.toml`, `deny.toml`, `mise.toml`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `scripts/plugin.sh` and the `plugins` CI job.
- Produces: a green `scripts/check-core-deps.sh`, the red test written in Task 1.

- [ ] **Step 1: Move the crate**

```bash
git mv crates/balerix-plugin-matrix plugins/matrix
```

The insta snapshots under `src/snapshots/` move with it. Their filenames embed the crate name, which is unchanged, so they stay valid.

- [ ] **Step 2: Rewrite `plugins/matrix/Cargo.toml` as a standalone project**

```toml
# A standalone project, not a workspace member (Spec H). This is the
# reason the split exists: matrix-sdk's tree is larger than the rest of
# the repository put together, and workspace feature unification was
# handing the daemon its rustls, HTTP/2 and gzip.
[workspace]
resolver = "3"

[package]
name = "balerix-plugin-matrix"
description = "The matrix plugin: agent events to a Matrix room per crew, a thread per agent session, and thread replies back as send_text (Spec G)"
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/balerix-ai/balerix"
publish = false

[[bin]]
name = "balerix-plugin-matrix"
path = "src/main.rs"

[dependencies]
balerix-api = { path = "../../crates/balerix-api" }
balerix-plugin-sdk = { path = "../../crates/balerix-plugin-sdk" }
# The Matrix client (Spec G-9). Default features carry `e2e-encryption`
# (the crew rooms are encrypted), `sqlite` (the state and crypto store the
# plugin keeps under its scratch dir) and `automatic-room-key-forwarding`;
# `markdown` gives the `body` plus `formatted_body` pair every message the
# plugin sends carries, and `bundled-sqlite` compiles SQLite in so no
# system libsqlite3 is needed. TLS is not a choice here: matrix-sdk
# exposes no TLS feature and depends on reqwest's `rustls`
# unconditionally, which is exactly why this project has its own lockfile.
matrix-sdk = { version = "0.18.0", features = ["markdown", "bundled-sqlite"] }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
serde_path_to_error = "0.1.20"
thiserror = "2.0.20"
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net"] }
anyhow = "1.0.104"
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }

[dev-dependencies]
insta = { version = "1.48.0", features = ["yaml", "json"] }

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
expect_used = "warn"
```

- [ ] **Step 3: Add the project's `clippy.toml`**

`plugins/matrix/clippy.toml`:

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
```

- [ ] **Step 4: Move the four supply-chain exemptions into `plugins/matrix/deny.toml`**

Create it with the strict base plus exactly the four entries that leave the core policy, reasons intact:

```toml
# The matrix plugin's own policy. These four entries were repository-wide
# until Spec H; they belong to the one artifact that actually carries the
# risk. Everything else matches the core and sibling-plugin policies.
[advisories]
ignore = [
  # `bitmaps`, archived upstream, reaches us through
  # imbl -> eyeball-im -> matrix-sdk (the observable collections the SDK
  # publishes room lists through).
  { id = "RUSTSEC-2026-0247", reason = "bitmaps is unmaintained; a transitive dependency of matrix-sdk through imbl, with no upgrade available" },
  # `proc-macro-error2` reaches us through aquamarine, the diagram macro
  # matrix-sdk uses in its own rustdoc. Build-time only; nothing it expands
  # ends up in the binary.
  { id = "RUSTSEC-2026-0173", reason = "proc-macro-error2 is unmaintained; a build-time dependency of matrix-sdk's rustdoc macro (aquamarine), with no upgrade available" },
]

[licenses]
allow = [
  "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
  "ISC", "Zlib", "Unicode-3.0", "Unicode-DFS-2016", "MPL-2.0", "CC0-1.0", "0BSD",
  # The Mozilla root certificate bundle, as data: `webpki-roots` (a direct
  # dependency of matrix-sdk) and `webpki-root-certs` (through
  # rustls-platform-verifier, which reqwest's `rustls` feature pulls in).
  # A permissive data licence with no copyleft, whose only condition on
  # redistribution is that its disclaimer travels with the data.
  "CDLA-Permissive-2.0",
  # `xxhash-rust`, through growable-bloom-filter in matrix-sdk-base. The
  # Boost licence: OSI approved, FSF free, permissive, and it does not even
  # require the notice in a binary distribution.
  "BSL-1.0",
]

[bans]
multiple-versions = "warn"
wildcards = "deny"
# Path dependencies on the core crates carry no version; every crate is
# `publish = false`.
allow-wildcard-paths = true

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

- [ ] **Step 5: Strip the core `deny.toml` back to strict**

Replace the whole of the repo-root `deny.toml`:

```toml
# The core workspace's policy: the daemon, the client and the plugin SDK.
# Plugins carry their own (Spec H), so a plugin's accepted risk no longer
# widens what the daemon may depend on.
[advisories]
ignore = []

[licenses]
allow = [
  "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
  "ISC", "Zlib", "Unicode-3.0", "Unicode-DFS-2016", "MPL-2.0", "CC0-1.0", "0BSD",
]

[bans]
multiple-versions = "warn"
wildcards = "deny"
# Workspace path dependencies carry no version; every crate is `publish = false`.
allow-wildcard-paths = true

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

- [ ] **Step 6: Check the stripped core policy before relying on it**

This is the risk the spec names in §10: removing `CDLA-Permissive-2.0` and `BSL-1.0` assumes nothing else in the daemon's tree needs them.

```bash
mise x -- cargo deny check advisories bans sources licenses 2>&1 | tail -20
```

Expected: PASS. If a licence is rejected, do not restore the blanket entry. Add it back with a reason naming the real dependency, for example `"BSL-1.0", # <crate>, through <path>`, and note it in the commit message.

- [ ] **Step 7: Drop matrix's entries from the core workspace**

In `Cargo.toml`, delete from `[workspace.dependencies]` the plugin path entry:

```toml
balerix-plugin-matrix = { path = "crates/balerix-plugin-matrix" }
```

the whole `matrix-sdk` block including its comment, and now that no core crate uses it, `serde_path_to_error` with its comment.

Then confirm nothing in the core workspace still references them:

```bash
grep -rn 'serde_path_to_error\|matrix-sdk\|regex' crates/*/Cargo.toml
```

Expected: no output.

- [ ] **Step 8: Make the red test from Task 1 go green**

```bash
mise x -- scripts/check-core-deps.sh
```

Expected: PASS, `core reqwest features are clean`. This was failing since Task 1 step 3.

- [ ] **Step 9: Record the shrink**

```bash
grep -c '^\[\[package\]\]' Cargo.lock
grep '^name = ' Cargo.lock | sort | uniq -d | wc -l
```

Expected: the package count drops from 524 back toward 273, and the 37 duplicated crate names largely disappear. Write the actual two numbers into the commit message rather than predicting them.

- [ ] **Step 10: Extend the CI matrix and the audit task**

In `.github/workflows/ci.yml`:

```yaml
        plugin: [flow, web, matrix]
```

In `mise.toml`, replace the `audit` task so it covers all four projects:

```toml
[tasks.audit]
description = "Nightly tier: dependency CVEs and policy, for the core workspace and each standalone plugin project"
run = [
  "cargo audit",
  "cargo deny check advisories bans sources licenses",
  "cargo audit --file plugins/flow/Cargo.lock",
  "cargo deny --manifest-path plugins/flow/Cargo.toml check advisories bans sources licenses",
  "cargo audit --file plugins/web/Cargo.lock",
  "cargo deny --manifest-path plugins/web/Cargo.toml check advisories bans sources licenses",
  "cargo audit --file plugins/matrix/Cargo.lock",
  "cargo deny --manifest-path plugins/matrix/Cargo.toml check advisories bans sources licenses",
]
```

- [ ] **Step 11: Prove cargo-deny reads each project's own policy**

`cargo deny --manifest-path` must pick up the `deny.toml` next to that manifest, not the repo root's. The matrix project is the one that would fail if it did not:

```bash
mise x -- cargo deny --manifest-path plugins/matrix/Cargo.toml check licenses 2>&1 | tail -10
```

Expected: PASS. A rejection of `CDLA-Permissive-2.0` or `BSL-1.0` means the wrong policy file was read; add `--config plugins/matrix/deny.toml` to the four plugin invocations in the `audit` task and re-run.

- [ ] **Step 12: Run every tier**

```bash
mise run plugin matrix
mise run plugins > /tmp/plugins.log 2>&1; echo "exit=$?"; tail -20 /tmp/plugins.log
mise run check > /tmp/check-matrix.log 2>&1; echo "exit=$?"; tail -20 /tmp/check-matrix.log
mise run audit > /tmp/audit.log 2>&1; echo "exit=$?"; tail -20 /tmp/audit.log
```

Expected: the matrix tier passes 66 tests. `mise run plugins` passes 106 across the three (18 plus 22 plus 66). `mise run check` passes 464. `mise run audit` passes for all four projects. 464 plus 106 is 570, the recorded baseline.

- [ ] **Step 13: Commit**

```bash
git add -A
git commit -F - <<'MSG'
Make the matrix plugin a standalone project and split the deny policy

Third of three (Spec H), and the one the split exists for. matrix moves
to plugins/matrix with its own Cargo.lock, so matrix-sdk's tree no longer
takes part in the core workspace's resolution. matrix-sdk and
serde_path_to_error leave [workspace.dependencies] with it, which leaves
that table describing the daemon, the client and the SDK alone.

scripts/check-core-deps.sh, added failing in the first commit of this
series, now passes: the core reqwest carries `json` and nothing else, so
the comment above it in the workspace manifest is true again.

The four exemptions matrix-sdk needed were repository-wide and are now
the matrix project's alone: RUSTSEC-2026-0247, RUSTSEC-2026-0173,
CDLA-Permissive-2.0 and BSL-1.0. The core policy is strict again, and a
future daemon dependency that needs one of those fails instead of
passing.

The core lockfile drops from 524 packages to <N>, and duplicated crate
names from 37 to <M>.
MSG
```

Replace `<N>` and `<M>` with the numbers from Step 9 before committing.

---

## Task 4: Per-tier cargo caching in CI

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the `plugins` matrix job from Tasks 1 to 3.
- Produces: nothing other tasks depend on.

- [ ] **Step 1: Confirm the cache action is absent today**

```bash
grep -n 'rust-cache\|actions/cache' .github/workflows/ci.yml
```

Expected: no output. Every job compiles from scratch on every run, which is the aggravating factor named in the spec's §1.

- [ ] **Step 2: Add a cache to the `check` job**

In `.github/workflows/ci.yml`, between the `mise install` step and the `mise run check` step of the `check` job:

```yaml
      # The core workspace, keyed on its own Cargo.lock. A plugin's
      # dependency bump no longer invalidates this (Spec H §7).
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: ". -> target"
```

- [ ] **Step 3: Add a cache to each plugin job**

In the `plugins` job, between `mise install` and `mise run plugin`:

```yaml
      # One cache per plugin project: each has its own lockfile and its own
      # target directory under plugins/<name>/target (scripts/plugin.sh).
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: "plugins/${{ matrix.plugin }} -> target"
          key: ${{ matrix.plugin }}
```

- [ ] **Step 4: Note the cache-size risk in the workflow**

Above the `plugins` job, add:

```yaml
  # GitHub evicts least-recently-used cache entries past 10 GB per
  # repository, and four Rust caches can approach that. If eviction starts
  # thrashing, give the plugin jobs `save-if: ${{ github.ref ==
  # 'refs/heads/main' }}` so pull requests restore but do not write.
```

- [ ] **Step 5: Validate the workflow parses**

PyYAML is present in this environment (`python3 -c "import yaml"` succeeds):

```bash
python3 -c "
import yaml
d = yaml.safe_load(open('.github/workflows/ci.yml'))
print(sorted(d['jobs']))
print(d['jobs']['plugins']['strategy']['matrix'])
print([s.get('uses') for s in d['jobs']['check']['steps']])
"
```

Expected: `['audit', 'check', 'mutants', 'plugins']`; `{'plugin': ['flow', 'web', 'matrix']}`; and `Swatinem/rust-cache@v2` among the `check` job's steps.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/ci.yml
git commit -F - <<'MSG'
Cache each tier's cargo target directory

CI cached mise tools but never the target directory, so every pull
request compiled its whole tree from scratch. Now that the four projects
have four lockfiles (Spec H), each job gets a cache keyed on its own: a
matrix-sdk bump stops rebuilding the daemon, and a daemon change stops
rebuilding matrix-sdk.

The 10 GB per-repository cache ceiling is the thing to watch with four
Rust caches; the workflow records the mitigation, which is to let the
plugin jobs restore on pull requests but write only on main.
MSG
```

---

## Task 5: Conventions and documentation

**Files:**
- Modify: `AGENTS.md`, `ARCHITECTURE.md`, `docs/THREAT-MODEL.md`, `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md`, `docs/superpowers/specs/2026-09-09-balerix-g-matrix-plugin-design.md`

**Interfaces:**
- Consumes: the finished layout from Tasks 1 to 4.
- Produces: nothing other tasks depend on.

- [ ] **Step 1: Find every stale path**

```bash
grep -rn 'crates/balerix-plugin-flow\|crates/balerix-plugin-web\|crates/balerix-plugin-matrix' \
  AGENTS.md ARCHITECTURE.md README.md docs/ --include='*.md'
```

Expected before the edits: hits in `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (four, at the flow crate, the flow tests directory, the web crate and the web assets), `docs/THREAT-MODEL.md` (two, both naming the vendored xterm assets) and `docs/superpowers/specs/2026-09-09-balerix-g-matrix-plugin-design.md` (one). Rewrite each to the new directory. Expected after: no output.

- [ ] **Step 2: Update the dependency convention in `AGENTS.md`**

Replace this bullet under Conventions:

```markdown
- New Cargo dependencies are a deliberate decision: add to
  `[workspace.dependencies]` with an exact version and say why in the commit.
```

with:

```markdown
- New Cargo dependencies are a deliberate decision, and they land in the
  project that uses them: a core dependency in the root
  `[workspace.dependencies]`, a plugin dependency in that plugin's own
  manifest. Exact version either way, and say why in the commit.
```

- [ ] **Step 3: Add the plugin layout to `AGENTS.md`'s ports paragraph**

Append to the bullet that ends "Plugin crates (`balerix-plugin-flow`) depend on `balerix-plugin-sdk` and `balerix-api` only.":

```markdown
  Plugins are not workspace members: each is a standalone project under
  `plugins/<name>/` with its own `Cargo.lock`, dependency table, lints,
  `clippy.toml` and `deny.toml`, reaching the SDK by relative path. That is
  what keeps a plugin's dependency tree out of the daemon's feature
  resolution.
```

- [ ] **Step 4: Add the tasks to `AGENTS.md`'s task list**

After the `test-it` entry:

```markdown
- `plugin <name>` — lint and test one standalone plugin project
  (`mise run plugin matrix`). `plugins` does all three. Neither is part of
  `check`; CI runs them as their own concurrent jobs.
```

And replace the `package-plugins` entry with:

```markdown
- `package-plugins [names…]` — builds the named in-tree plugins inside
  their own projects and assembles each as a directory source under
  `target/plugins/<name>/` (under `CARGO_TARGET_DIR` when set). No names
  means all three. `test` and `e2e` ask it for `flow web`, the two the
  journey needs; the flow e2e skips (fails under `BALERIX_REQUIRE_TOOLS`)
  without it.
```

- [ ] **Step 5: Add the gotcha that explains why**

Under Gotchas in `AGENTS.md`:

```markdown
- `mise run check` covers the core workspace only. Cargo unifies features
  across every member one invocation selects, so while the plugins were
  members, a `--workspace` build handed `balerix-server` a `reqwest` with
  rustls, HTTP/2, gzip and stream that it never asked for, and tripled the
  gate's cold build. `scripts/check-core-deps.sh` fails if that ever comes
  back. A plugin change is caught by `mise run plugins`, not by `check`.
```

- [ ] **Step 6: Update the threat model's supply-chain row**

In `docs/THREAT-MODEL.md`, the Supply chain row names "committed `Cargo.lock`" and one `deny.toml`. Change those two phrases to:

```
four committed `Cargo.lock` files (the core workspace and each standalone
plugin project); `cargo audit` + `cargo deny` over all four (`mise run
audit`), each plugin under its own policy so a plugin's accepted risk does
not widen the daemon's
```

and change the vendored-asset path to `plugins/web/assets/VENDOR.md`. Change the artefact column from `` `mise.toml`, `deny.toml` `` to `` `mise.toml`, `deny.toml`, `plugins/*/deny.toml` ``.

- [ ] **Step 7: Fix the crate list in `ARCHITECTURE.md`**

The bullet list around lines 43 to 58 names the crates. Two edits.

First, the `balerix-plugin-flow` bullet ends with "Plugin crates depend on the SDK and `api` only." Replace that sentence with:

```markdown
  Plugin crates depend on the SDK and `api` only, and are not workspace
  members: each is a standalone project under `plugins/<name>/` with its
  own lockfile and dependency table (Spec H).
```

Second, the list stops at `balerix-plugin-web` and never mentions matrix. Add after the web bullet:

```markdown
- `balerix-plugin-matrix` — the third in-tree plugin: a Matrix room per
  crew and a thread per agent session, with thread replies coming back as
  `send_text` (Spec G). Its `matrix-sdk` tree is larger than the rest of
  the repository put together, which is why plugins stopped being
  workspace members.
```

- [ ] **Step 8: Give rust-analyzer all four projects**

Spec §12. rust-analyzer discovers only the root workspace, so the plugin
sources would show as unowned files. Create `.vscode/settings.json`:

```json
{
  "rust-analyzer.linkedProjects": [
    "Cargo.toml",
    "plugins/flow/Cargo.toml",
    "plugins/web/Cargo.toml",
    "plugins/matrix/Cargo.toml"
  ]
}
```

- [ ] **Step 9: Note the Renovate follow-up in the spec**

Spec §12 asks for confirmation after Renovate's first scheduled run, which
cannot happen during this work. Renovate's cargo manager detects every
`Cargo.toml`, so no config change is expected. Change the second bullet of
the spec's §12 to record what to check and when:

```markdown
- Renovate's cargo manager detects every `Cargo.toml`, so four lockfiles
  should need no config change. `renovate.json` sets `lockFileMaintenance`
  for Monday mornings; confirm after the first run that all four lockfiles
  are being maintained, and add `"enabledManagers"` scoping only if one is
  missed.
```

- [ ] **Step 10: Verify the docs are consistent with reality**

```bash
grep -rn 'crates/balerix-plugin' . --include='*.md' --include='*.toml' --include='*.sh' --include='*.yml' | grep -v '^./target'
```

Expected: no output.

- [ ] **Step 11: Commit**

```bash
git add -A
git commit -F - <<'MSG'
Say where plugin dependencies go now

The convention was "add to [workspace.dependencies]"; it is now
per-project, because plugins are standalone projects (Spec H). AGENTS.md
gains the layout, the two new tasks and the gotcha that explains why
`check` no longer covers plugins: workspace feature unification was
handing the daemon a plugin's reqwest features, and
scripts/check-core-deps.sh now fails if that returns.

THREAT-MODEL.md's supply-chain row described one lockfile and one deny
policy; there are four of each. The stale crate paths in Spec B, Spec G
and the threat model follow the directories.
MSG
```

---

## Task 6: Measure the result

The spec promises measured verification, not assertion. This task produces the numbers and appends them to the spec, so the claim and its evidence live together.

**Files:**
- Modify: `docs/superpowers/specs/2026-09-10-plugin-dependency-isolation-design.md`

**Interfaces:**
- Consumes: the finished state from Tasks 1 to 5.
- Produces: nothing.

- [ ] **Step 1: Time the core gate cold, against the 143 s baseline**

```bash
S=/workspace/target/tmp/verify; rm -rf $S; mkdir -p $S
s=$SECONDS
CARGO_TARGET_DIR=$S/core mise x -- cargo build --workspace --all-targets -q
echo "core cold: $((SECONDS-s))s"
du -sh $S/core
```

Expected: about 47 s and about 6.4 GB, against 143 s and 12 GB before the split. Record the actual figures.

- [ ] **Step 2: Time the matrix project cold, so the other side of the trade is on the record**

```bash
s=$SECONDS
CARGO_TARGET_DIR=$S/matrix mise x -- cargo build --manifest-path plugins/matrix/Cargo.toml --all-targets -q
echo "matrix cold: $((SECONDS-s))s"
du -sh $S/matrix
rm -rf $S
```

Expected: about 115 s. This is the job that now sets CI wall clock, and it runs concurrently with a 47 s core job rather than serially inside it.

- [ ] **Step 3: Confirm test-count parity against the recorded baseline**

`cargo nextest list` prints a tree, not a count, so count the JSON test cases the same way the baseline did:

```bash
count() { python3 -c "
import json,sys
print(sum(len(s['testcases']) for s in json.load(sys.stdin)['rust-suites'].values()))"; }

echo -n "core:   "; mise x -- cargo nextest list --workspace --message-format json 2>/dev/null | count
for p in flow web matrix; do
  echo -n "$p: "
  mise x -- cargo nextest list --manifest-path plugins/$p/Cargo.toml --message-format json 2>/dev/null | count
done
```

Expected: 464 for the core workspace, then 18, 22 and 66. The four must sum to 570, the number recorded in Task 1 Step 1. A shortfall means a test file did not move or a target is no longer compiled.

- [ ] **Step 4: Confirm the isolation invariants one last time**

```bash
mise x -- scripts/check-core-deps.sh
grep -c '^\[\[package\]\]' Cargo.lock
grep '^name = ' Cargo.lock | sort | uniq -d | wc -l
ls plugins/*/Cargo.lock
```

Expected: clean features; the core package count near 273; far fewer than 37 duplicated names; three plugin lockfiles present and committed.

- [ ] **Step 5: Append the results to the spec**

Add a section to `docs/superpowers/specs/2026-09-10-plugin-dependency-isolation-design.md`, after §10:

```markdown
## 10.1 Measured result

| Measure | Before | After |
|---|---|---|
| `check` cold build | 143 s | <actual> |
| core target size | 12 GB | <actual> |
| core lockfile packages | 524 | <actual> |
| core duplicated crate names | 37 | <actual> |
| matrix project cold build | (inside the 143 s) | <actual> |
| tests, core / flow / web / matrix | 570 in one tier | <actual> |

Core `reqwest` features after the split: `json` only.
```

Replace each `<actual>` with the figure measured in Steps 1 to 4. Leave no angle brackets in the committed file.

- [ ] **Step 6: Final full run**

```bash
mise run check > /tmp/final-check.log 2>&1; echo "check=$?"
mise run plugins > /tmp/final-plugins.log 2>&1; echo "plugins=$?"
mise run e2e > /tmp/final-e2e.log 2>&1; echo "e2e=$?"
mise run audit > /tmp/final-audit.log 2>&1; echo "audit=$?"
```

Expected: all four exit 0. `balerix-runtime`'s `processes_with_arg_pair` and `reap_kills` tests are known to fail under host load in the parallel suite and pass alone; if either fails, rerun that crate with `--no-fail-fast` before treating it as a regression.

- [ ] **Step 7: Commit**

```bash
git add docs/superpowers/specs/2026-09-10-plugin-dependency-isolation-design.md
git commit -F - <<'MSG'
Record what the split actually measured

The spec promised numbers rather than assertions. §10.1 has them: the
cold core gate, the core target size, the lockfile package and duplicate
counts, the matrix project's own cold build, and test-count parity across
the four projects against the 570 recorded before the first move.
MSG
```
