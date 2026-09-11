# Release Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Release the `balerix` CLI, the three in-tree plugins and `balerix-plugin-sdk` through release PRs: static musl binaries on GitHub Releases, multi-arch images on ghcr, and `balerix-api` + `balerix-plugin-sdk` on crates.io, with the CLI and each plugin on independent versions.

**Architecture:** Shell scripts under `scripts/release/` hold every piece of release logic and are the only thing the workflows call; `scripts/release/lib.sh` is the single definition of the four release units (core, flow, web, matrix). git-cliff computes versions and changelogs from Conventional Commits scoped by path. `release-pr.yml` keeps one release PR per unit; merging it leaves a manifest version untagged, which `release.yml` builds, packages, publishes, signs and finally tags. Two composite actions (`build-binary`, `build-image`) are shared by `release.yml` and the non-publishing `images.yml`.

**Tech Stack:** bash, git-cliff 2.14.1, cargo-edit 0.13.13, jq, GitHub Actions, Docker Buildx, cosign, trivy, hadolint, zizmor, actionlint, shellcheck, mise.

**Spec:** `docs/superpowers/specs/2026-09-11-release-pipeline-design.md`

## Global Constraints

- **Every version is exact** — `mise.toml` entries, cargo dependencies, action references (full commit SHA plus a `# vX.Y.Z` comment) and base images (by digest).
- **Run cargo and every mise-managed tool through mise:** `mise x -- …` or a `mise run` task. Release scripts assume their tools are on `PATH`; run them as `mise x -- scripts/release/<script>`.
- **Release units** (spec §3), copied verbatim:

  | Unit | Crate | Tag | Changelog | Include paths |
  |---|---|---|---|---|
  | core | `balerix` | `balerix-v<ver>` | `CHANGELOG.md` | `crates/**`, `Cargo.toml`, `Cargo.lock`, `mise.toml` |
  | flow | `balerix-plugin-flow` | `balerix-plugin-flow-v<ver>` | `plugins/flow/CHANGELOG.md` | `plugins/flow/**`, `crates/balerix-api/**`, `crates/balerix-plugin-sdk/**` |
  | web | `balerix-plugin-web` | `balerix-plugin-web-v<ver>` | `plugins/web/CHANGELOG.md` | `plugins/web/**`, `crates/balerix-api/**`, `crates/balerix-plugin-sdk/**` |
  | matrix | `balerix-plugin-matrix` | `balerix-plugin-matrix-v<ver>` | `plugins/matrix/CHANGELOG.md` | `plugins/matrix/**`, `crates/balerix-api/**`, `crates/balerix-plugin-sdk/**` |

- **Releasable commit types:** `feat`, `fix`, `perf`, `refactor`, `build`. **Skipped:** `docs`, `test`, `ci`, `chore`, `style`, `revert`.
- **Targets:** `x86_64-unknown-linux-musl` on `ubuntu-24.04`, `aarch64-unknown-linux-musl` on `ubuntu-24.04-arm`. Archive name `<crate>-v<ver>-<target>.tar.gz`; plugin package `<crate>-v<ver>-package.tar.gz`.
- **Images:** `ghcr.io/<owner>/<crate>`; tags `<ver>` at push, `<major>.<minor>` and `latest` only after release and package verification.
- **Workflows:** `permissions: {}` at the top, per-job grants only; `persist-credentials: false` on every checkout; no `pull_request_target`; `MISE_AUTO_INSTALL: "false"` and each job installs only its tools; `release.yml` uses no caches (`cache: false`).
- **Pinned action SHAs** (use exactly these):

  | Action | SHA | Version |
  |---|---|---|
  | `actions/checkout` | `3d3c42e5aac5ba805825da76410c181273ba90b1` | v7.0.1 |
  | `jdx/mise-action` | `c2a87611a18de5b3828c5652fe268e992400cb5c` | v4.3.0 |
  | `Swatinem/rust-cache` | `6323deb102c322ba6fcbdcafc7e3dddab59af2b6` | v2.9.2 |
  | `actions/upload-artifact` | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` | v7.0.1 |
  | `actions/download-artifact` | `3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c` | v8.0.1 |
  | `actions/create-github-app-token` | `bcd2ba49218906704ab6c1aa796996da409d3eb1` | v3.2.0 |
  | `docker/setup-buildx-action` | `37fe631027851001ddb9b187196cc803df7f5f0e` | v4.3.0 |
  | `docker/login-action` | `dbcb813823bdd20940b903addbd779551569679f` | v4.6.0 |
  | `docker/build-push-action` | `53b7df96c91f9c12dcc8a07bcb9ccacbed38856a` | v7.3.0 |
  | `docker/metadata-action` | `dc802804100637a589fabce1cb79ff13a1411302` | v6.2.0 |
  | `actions/attest-build-provenance` | `4d101475d8b20a2381f78447822ac1eab6504dd8` | v4.2.2 |
  | `rust-lang/crates-io-auth-action` | `c6f97d42243bad5fab37ca0427f495c86d5b1a18` | v1.0.5 |
  | `amannn/action-semantic-pull-request` | `48f256284bd46cdaab1048c3721360e808335d50` | v6.1.1 |

- **Commits** use Conventional Commit titles (this plan adopts them) and end with the line `Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t`.
- **Never create tags, releases, packages or PRs on `balerix-ai/balerix` while implementing.** Scenario tests use throwaway clones; outward-facing verification (Task 10) asks the user first.
- **`mise run check` stays green after every task.** It is slow (the e2e); redirect its output to a file under `target/tmp/` and read the file.

## Deviations from the spec (recorded here and in the spec by Task 9)

1. `scripts/release/test.sh` uses **clones**, not `git worktree`s: a worktree shares the repository's tags, and the scenarios create tags.
2. `docker/plugin/Dockerfile` takes the binary from the build context under the fixed name `plugin` instead of an `ARG PLUGIN` (exec-form `ENTRYPOINT` cannot expand an `ARG`).
3. The release App token also needs `issues: write`: labels (`release:pinned`) are an issues API.
4. `shellcheck` joins `mise run lint` for `scripts/release/*.sh`; `zizmor` runs with `--min-severity medium` (below that, it reports only `self-repository` style suggestions for the local composite actions).
5. In `release.yml`, GitHub Releases for all units in a run wait for every unit's build and images (matrix job results are aggregate); a re-run releases whatever is still untagged.

---

## File Structure

**Created**

| Path | Responsibility |
|---|---|
| `cliff.toml` | git-cliff config shared by all units: commit groups, skipped types, bump rules, changelog template |
| `scripts/release/lib.sh` | The release units: crate, tag, manifest, changelog, image, include paths, version lookup, scoped `cliff` |
| `scripts/release/prepare.sh` | Next version + manifest/lockfile/yaml/changelog edits for one unit (`status=release|none|in-progress`) |
| `scripts/release/notes.sh` | One version's section of a unit's changelog |
| `scripts/release/test.sh` | Scenario tests for the scripts against throwaway clones |
| `scripts/release/open-pr.sh` | Open, update or close one unit's release PR |
| `scripts/release/build.sh` | Static musl build, smoke test, archive |
| `scripts/release/image-context.sh` | Docker build context from a built binary |
| `scripts/release/smoke-image.sh` | Image smoke test |
| `scripts/release/plugin-mise.toml.tmpl` | Release package `mise.toml` template |
| `scripts/release/package.sh` | Plugin release package |
| `scripts/release/plan.sh` | Untagged units as step outputs |
| `scripts/release/audit.sh` | cargo audit + deny per unit |
| `scripts/release/publish-crates.sh` | Idempotent crates.io publish |
| `scripts/release/merge-image.sh` | Multi-arch index from two digests |
| `scripts/release/github-release.sh` | Draft, upload, publish (creates the tag) |
| `scripts/release/verify-package.sh` | Install and start a published package; `--flag` marks a prerelease |
| `scripts/release/promote-image.sh` | Move `<major>.<minor>` and `latest` |
| `.github/actions/build-binary/action.yml` | Composite: musl-tools, target, `build.sh` |
| `.github/actions/build-image/action.yml` | Composite: context, hadolint, build, smoke, trivy, optional push by digest |
| `.github/workflows/release-pr.yml` | Release PRs |
| `.github/workflows/release.yml` | Releases |
| `.github/workflows/pr-title.yml` | Conventional PR titles |
| `.github/workflows/images.yml` | Image builds without pushing |
| `.github/workflows/release-scripts.yml` | `mise run release-test` in CI |
| `docker/balerix/Dockerfile` | Runtime image |
| `docker/plugin/Dockerfile` | Plugin image |
| `.hadolint.yaml` | hadolint policy |
| `docs/RELEASING.md` | Setup checklist, cutting releases, recovery, verification |

**Modified**

| Path | Change |
|---|---|
| `Cargo.toml` | `version` on the `balerix-api` and `balerix-plugin-sdk` workspace dependencies; `[profile.release] strip = true`; comment |
| `crates/balerix-api/Cargo.toml`, `crates/balerix-plugin-sdk/Cargo.toml` | `publish = true` |
| `plugins/{flow,web,matrix}/Cargo.toml` | `[profile.release] strip = true`; web `version = "0.2.0"` |
| `plugins/web/Cargo.lock` | web at 0.2.0 |
| `deny.toml` | comment on `allow-wildcard-paths` |
| `scripts/plugin.sh` | `version-check` subcommand, run by `check` |
| `mise.toml` | tool pins; `lint` additions; `release-prepare`, `release-test` tasks |
| `.github/workflows/ci.yml` | SHA pins, `persist-credentials: false`, lint tools installed |
| `renovate.json` | `:semanticCommits`, `helpers:pinGitHubActionDigests` |
| `README.md`, `AGENTS.md`, `docs/THREAT-MODEL.md`, the spec | docs |

---

### Task 1: Release metadata in the manifests

Makes `balerix-api` and `balerix-plugin-sdk` publishable, fixes web's version drift, strips release binaries, and adds the guard that keeps each plugin's `balerix-plugin.yaml` version equal to its `Cargo.toml`.

**Files:**
- Modify: `Cargo.toml`, `crates/balerix-api/Cargo.toml`, `crates/balerix-plugin-sdk/Cargo.toml`, `plugins/flow/Cargo.toml`, `plugins/web/Cargo.toml`, `plugins/web/Cargo.lock`, `plugins/matrix/Cargo.toml`, `deny.toml`, `scripts/plugin.sh`, `mise.toml`

**Interfaces:**
- Produces: `scripts/plugin.sh version-check <name>` (exit 1 on mismatch, message names both versions). `balerix-api`/`balerix-plugin-sdk` workspace dependency entries carry `version = "0.1.0"` — `cargo set-version --workspace` (Task 3) rewrites them.

- [ ] **Step 1: See the guard fail before it exists**

Run: `scripts/plugin.sh version-check web; echo "exit=$?"`
Expected: `usage: …` and `exit=2` (the subcommand does not exist yet).

- [ ] **Step 2: Add `version-check` to `scripts/plugin.sh`**

Change the usage line to:

```bash
usage() { echo "usage: $0 {target-dir|build|fmt|version-check|check} <name>" >&2; exit 2; }
```

Add this function directly after the `target="$dir/target"` line:

```bash
# A plugin's balerix-plugin.yaml version is written from its Cargo.toml by
# the release scripts (Spec I §3); a hand edit to either one fails here.
version_check() {
  local cargo_version manifest_version
  cargo_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$dir/Cargo.toml" | head -n 1)
  manifest_version=$(sed -n 's/^version: //p' "$dir/package/balerix-plugin.yaml")
  if [[ $cargo_version != "$manifest_version" ]]; then
    echo "plugins/$name: Cargo.toml says $cargo_version, package/balerix-plugin.yaml says $manifest_version" >&2
    exit 1
  fi
}
```

Add a case arm before `check)`:

```bash
  version-check)
    version_check
    ;;
```

and make `version_check` the first line of the `check)` arm:

```bash
  check)
    version_check
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all --check
```

- [ ] **Step 3: Run the guard; web must fail**

Run: `for n in flow web matrix; do scripts/plugin.sh version-check $n; echo "$n exit=$?"; done`
Expected: `flow exit=0`, `plugins/web: Cargo.toml says 0.1.0, package/balerix-plugin.yaml says 0.2.0` then `web exit=1`, `matrix exit=0`.

- [ ] **Step 4: Move web to 0.2.0**

In `plugins/web/Cargo.toml`, change the `[package]` line `version = "0.1.0"` to `version = "0.2.0"`. Then:

Run: `mise x -- cargo update --manifest-path plugins/web/Cargo.toml -p balerix-plugin-web`
Expected: `Updating balerix-plugin-web v0.1.0 (…) -> v0.2.0`.

Run: `scripts/plugin.sh version-check web; echo "exit=$?"`
Expected: `exit=0`.

- [ ] **Step 5: Make the api and SDK publishable**

In `Cargo.toml`, replace the `# Nothing here is published…` comment block and the two dependency lines:

```toml
# balerix-api and balerix-plugin-sdk are published to crates.io at this
# version (Spec I §3); every other crate sets `publish = false`. A published
# crate's path dependencies need a version, so those two entries carry one,
# and scripts/release/prepare.sh moves it with the workspace version.
publish = false
```

```toml
balerix-api = { path = "crates/balerix-api", version = "0.1.0" }
```

```toml
balerix-plugin-sdk = { path = "crates/balerix-plugin-sdk", version = "0.1.0" }
```

In `crates/balerix-api/Cargo.toml` and `crates/balerix-plugin-sdk/Cargo.toml`, replace `publish.workspace = true` with `publish = true`.

In `deny.toml`, replace the comment above `allow-wildcard-paths = true` with:

```toml
# Path dependencies between private crates carry no version; the two
# published crates' path dependencies do (Spec I §3).
```

- [ ] **Step 6: Strip release binaries**

Append to `Cargo.toml` and to each of `plugins/flow/Cargo.toml`, `plugins/web/Cargo.toml`, `plugins/matrix/Cargo.toml`:

```toml

[profile.release]
strip = true
```

- [ ] **Step 7: Guard packaging in the lint tier**

In `mise.toml`, add to the end of the `[tasks.lint]` `run` list (and append ", and the published crates package cleanly" to its description):

```toml
  "cargo package --no-verify -p balerix-api -p balerix-plugin-sdk",
```

- [ ] **Step 8: Verify**

Run: `mise x -- cargo package --no-verify -p balerix-api -p balerix-plugin-sdk 2>&1 | tail -4`
Expected: two `Packaged … files` lines, no error.

Run: `mise run lint > target/tmp/task1-lint.log 2>&1; echo "exit=$?"`
Expected: `exit=0`.

Run: `mise run plugins > target/tmp/task1-plugins.log 2>&1; echo "exit=$?"`
Expected: `exit=0`.

Run: `mise run check > target/tmp/task1-check.log 2>&1; echo "exit=$?"`
Expected: `exit=0`.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock crates/balerix-api/Cargo.toml crates/balerix-plugin-sdk/Cargo.toml \
  plugins/flow/Cargo.toml plugins/web/Cargo.toml plugins/web/Cargo.lock plugins/matrix/Cargo.toml \
  deny.toml scripts/plugin.sh mise.toml
git commit -m "build: make the api and SDK publishable and guard plugin manifest versions

balerix-api and balerix-plugin-sdk get publish = true and versioned path
dependencies (Spec I §3). web moves to 0.2.0, the version its manifest
already claimed; scripts/plugin.sh check now fails on that drift. Release
binaries are stripped.

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 2: Lint workflows and release scripts; harden `ci.yml`

Adds zizmor, actionlint and shellcheck to the lint tier and brings the existing `ci.yml` to the pinning and credential rules every later workflow follows.

**Files:**
- Modify: `mise.toml`, `.github/workflows/ci.yml`, `renovate.json`

**Interfaces:**
- Produces: `mise run lint` runs `actionlint` and `zizmor --offline --min-severity medium .github`. (Task 3 adds `shellcheck` once there are scripts to check.)

- [ ] **Step 1: Pin the tools**

Append to `[tools]` in `mise.toml`, after `claude`:

```toml
# Release and workflow tooling (Spec I). Only `claude` and `gh` from this
# table reach agents (balerix-runtime's INHERITED list).
jq = "1.8.2"
git-cliff = "2.14.1"
"cargo:cargo-edit" = "0.13.13"
cosign = "3.1.3"
hadolint = "2.15.1"
trivy = "0.74.0"
zizmor = "1.30.1"
actionlint = "1.7.12"
shellcheck = "0.11.0"
```

Run: `mise install jq git-cliff cosign hadolint trivy zizmor actionlint shellcheck && mise install cargo:cargo-edit`
Expected: every tool reports installed (cargo-edit compiles for a minute or two).

- [ ] **Step 2: Add the linters to `lint`**

Append to the `[tasks.lint]` `run` list:

```toml
  "actionlint",
  "zizmor --offline --min-severity medium .github",
```

- [ ] **Step 3: See zizmor fail on today's `ci.yml`**

Run: `mise x -- zizmor --offline --min-severity medium .github 2>&1 | tail -2`
Expected: findings: `10 … unpinned-uses` errors and `artipacked` warnings.

- [ ] **Step 4: Harden `ci.yml`**

In `.github/workflows/ci.yml`:
- Replace every `- uses: actions/checkout@v4` with

  ```yaml
        - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
          with:
            persist-credentials: false
  ```
- Replace every `uses: jdx/mise-action@v2` with `uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0` (keep the existing `with:` line).
- Replace every `uses: Swatinem/rust-cache@v2` with `uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2`.
- In the `check` job, change the install line to
  `- run: mise install rust cargo:cargo-nextest cargo-insta tmux nono gh gitleaks zizmor actionlint shellcheck`.

- [ ] **Step 5: Renovate keeps the pins current**

Replace the `extends` line of `renovate.json` with:

```json
  "extends": ["config:recommended", ":semanticCommits", "helpers:pinGitHubActionDigests"],
```

- [ ] **Step 6: Verify**

Run: `mise x -- actionlint && mise x -- zizmor --offline --min-severity medium .github 2>&1 | tail -1`
Expected: actionlint prints nothing; zizmor prints `No findings to report. Good job! …`.

Run: `mise run lint > target/tmp/task2-lint.log 2>&1; echo "exit=$?"`
Expected: `exit=0`.

- [ ] **Step 7: Commit**

```bash
git add mise.toml .github/workflows/ci.yml renovate.json
git commit -m "ci: lint workflows and pin actions by commit

zizmor, actionlint and shellcheck join mise run lint. ci.yml pins every
action to a commit SHA and stops persisting checkout credentials; Renovate
keeps the digests current and writes semantic commit titles. The release
tooling Spec I needs is pinned in mise.toml.

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 3: Prepare releases — `cliff.toml`, `lib.sh`, `prepare.sh`, `notes.sh`, scenario tests

**Files:**
- Create: `cliff.toml`, `scripts/release/lib.sh`, `scripts/release/prepare.sh`, `scripts/release/notes.sh`, `scripts/release/test.sh`, `.github/workflows/release-scripts.yml`
- Modify: `mise.toml` (tasks)

**Interfaces:**
- Produces (sourced by every later script, callers `cd` to the repo root first): `UNITS`, `PLUGIN_UNITS`, `die <msg>`, `require_unit <unit>`, `unit_crate <unit>`, `unit_tag_prefix <unit>`, `unit_tag <unit> <version>`, `unit_manifest <unit>`, `unit_changelog <unit>`, `unit_image <unit>` (uses `GITHUB_REPOSITORY_OWNER`, default `balerix-ai`), `unit_paths <unit>`, `unit_version <unit>`, `tag_exists <tag>`, `last_tag <unit>`, `cliff <unit> <git-cliff args…>`.
- Produces: `prepare.sh <unit> [version]` → stdout `status=release` + `version=` `tag=` `notes=` (notes file under `${RELEASE_NOTES_DIR:-target/release-notes}/<unit>.md`), or `status=none`, or `status=in-progress` + `version=`. `notes.sh <unit> <version>` → the section body on stdout.
- Produces: mise tasks `release-prepare <unit> [version]` and `release-test`.

- [ ] **Step 1: Write the scenario tests**

Create `scripts/release/test.sh` (mode 0755):

```bash
#!/usr/bin/env bash
# Scenario tests for the release scripts (Spec I §10): prepare.sh against
# throwaway clones with synthetic commits and tags, then notes.sh and
# package.sh. Clones rather than worktrees, because a worktree shares this
# repository's tags and these tests create tags freely.
set -euo pipefail
# A failure inside $(…) — a fixture, a release step — stops the run too.
shopt -s inherit_errexit
repo="$(cd "$(dirname "$0")/../.." && pwd)"
root="$repo/target/tmp/release-test-$$"
log="$root/scripts.log"
mkdir -p "$root"
failures=0

pass() { echo "ok   $1"; }
fail() {
  echo "FAIL $1" >&2
  failures=$((failures + 1))
}
expect_eq() { if [[ $2 == "$3" ]]; then pass "$1"; else fail "$1: got '$2', expected '$3'"; fi; }
expect_grep() { if grep -qF -- "$2" "$3"; then pass "$1"; else fail "$1: '$2' not in $3"; fi; }

gitc() { git -C "$1" -c user.name=release-test -c user.email=release-test@invalid "${@:2}"; }
field() { sed -n "s/^$1=//p" <<<"$2"; }

# A clone of HEAD with no tags and no changelogs, carrying the working tree's
# release scripts so that uncommitted edits are what gets tested.
fixture() {
  local dir="$root/$1"
  git clone -q "$repo" "$dir"
  git -C "$dir" tag --list | xargs -r git -C "$dir" tag -d >/dev/null
  rm -rf "$dir/scripts/release" "$dir/CHANGELOG.md" "$dir"/plugins/*/CHANGELOG.md
  cp -R "$repo/scripts/release" "$dir/scripts/release"
  cp "$repo/cliff.toml" "$dir/cliff.toml"
  gitc "$dir" add -A
  gitc "$dir" commit -q --allow-empty -m "test: release scripts under test"
  echo "$dir"
}

# Commits a one-line change to <path> with <message>.
change() {
  local dir=$1 path=$2 message=$3
  mkdir -p "$(dirname "$dir/$path")"
  echo "$message" >>"$dir/$path"
  gitc "$dir" add -A
  gitc "$dir" commit -q -m "$message"
}

prepare() {
  local dir=$1
  shift
  "$dir/scripts/release/prepare.sh" "$@" 2>>"$log"
}

# Throws away whatever prepare.sh wrote.
discard() {
  gitc "$1" reset -q --hard
  gitc "$1" clean -qfd -e target
}

# Releases <unit> at exactly <version> the way CI would: prepare, commit, tag.
release() {
  local dir=$1 unit=$2 version=$3 out
  out=$(prepare "$dir" "$unit" "$version")
  gitc "$dir" add -A
  gitc "$dir" commit -q -m "chore(release): $unit v$version"
  gitc "$dir" tag "$(field tag "$out")"
}

manifest_version() {
  (
    cd "$1"
    # shellcheck source=scripts/release/lib.sh
    source scripts/release/lib.sh
    unit_version "$2"
  )
}

lock_version() {
  awk -v name="name = \"$2\"" '$0 == name { getline; gsub(/^version = "|"$/, ""); print; exit }' "$1"
}

# Every manifest, lockfile and plugin manifest agrees (Spec I §10).
assert_consistent() {
  local dir=$1 label=$2 core plugin version
  core=$(manifest_version "$dir" core)
  expect_eq "$label: Cargo.lock has balerix $core" "$(lock_version "$dir/Cargo.lock" balerix)" "$core"
  for plugin in flow web matrix; do
    version=$(manifest_version "$dir" "$plugin")
    expect_eq "$label: $plugin balerix-plugin.yaml matches Cargo.toml" \
      "$(sed -n 's/^version: //p' "$dir/plugins/$plugin/package/balerix-plugin.yaml")" "$version"
    expect_eq "$label: $plugin Cargo.lock matches Cargo.toml" \
      "$(lock_version "$dir/plugins/$plugin/Cargo.lock" "balerix-plugin-$plugin")" "$version"
    expect_eq "$label: $plugin Cargo.lock has the SDK at $core" \
      "$(lock_version "$dir/plugins/$plugin/Cargo.lock" balerix-plugin-sdk)" "$core"
  done
}

scenario_initial() {
  local dir version out
  dir=$(fixture initial)
  version=$(manifest_version "$dir" flow)
  out=$(prepare "$dir" flow)
  expect_eq "initial: status" "$(field status "$out")" release
  expect_eq "initial: the manifest version" "$(field version "$out")" "$version"
  expect_eq "initial: tag" "$(field tag "$out")" "balerix-plugin-flow-v$version"
  expect_grep "initial: changelog section" "## $version - " "$dir/plugins/flow/CHANGELOG.md"
  assert_consistent "$dir" initial
}

scenario_bumps_0x() {
  local dir
  dir=$(fixture bumps-0x)
  release "$dir" flow 0.4.0
  change "$dir" plugins/flow/release-test.txt "fix: a flow fix"
  expect_eq "0.x: fix bumps patch" "$(field version "$(prepare "$dir" flow)")" 0.4.1
  discard "$dir"
  change "$dir" plugins/flow/release-test.txt "feat: a flow feature"
  expect_eq "0.x: feat bumps patch" "$(field version "$(prepare "$dir" flow)")" 0.4.1
  discard "$dir"
  change "$dir" plugins/flow/release-test.txt "feat!: a breaking flow change"
  expect_eq "0.x: breaking bumps minor" "$(field version "$(prepare "$dir" flow)")" 0.5.0
  assert_consistent "$dir" "0.x breaking"
}

scenario_bumps_1x() {
  local dir
  dir=$(fixture bumps-1x)
  release "$dir" web 1.2.0
  change "$dir" plugins/web/release-test.txt "feat: a web feature"
  expect_eq "1.x: feat bumps minor" "$(field version "$(prepare "$dir" web)")" 1.3.0
  discard "$dir"
  change "$dir" plugins/web/release-test.txt "fix!: a breaking web fix"
  expect_eq "1.x: breaking bumps major" "$(field version "$(prepare "$dir" web)")" 2.0.0
}

scenario_skipped_types() {
  local dir out
  dir=$(fixture skipped)
  release "$dir" flow 0.4.0
  change "$dir" plugins/flow/release-test.txt "docs: flow docs"
  out=$(prepare "$dir" flow)
  expect_eq "docs only: status" "$(field status "$out")" none
  expect_eq "docs only: no file changed" "$(git -C "$dir" status --porcelain)" ""
}

scenario_sdk_change() {
  local dir unit
  dir=$(fixture sdk)
  for unit in core flow web matrix; do release "$dir" "$unit" 0.4.0; done
  change "$dir" crates/balerix-plugin-sdk/release-test.txt "fix(sdk): an sdk fix"
  for unit in core flow web matrix; do
    expect_eq "sdk-only change: $unit releases" "$(field status "$(prepare "$dir" "$unit")")" release
    discard "$dir"
  done
}

scenario_plugin_only() {
  local dir
  dir=$(fixture plugin-only)
  release "$dir" core 0.4.0
  release "$dir" flow 0.4.0
  change "$dir" plugins/flow/release-test.txt "fix: a flow-only fix"
  expect_eq "plugin-only change: core" "$(field status "$(prepare "$dir" core)")" none
}

scenario_core_bump() {
  local dir out
  dir=$(fixture core)
  release "$dir" core 0.4.0
  change "$dir" crates/balerix-server/release-test.txt "feat!: a breaking daemon change"
  out=$(prepare "$dir" core)
  expect_eq "core: breaking bumps minor" "$(field version "$out")" 0.5.0
  assert_consistent "$dir" "core bump"
}

scenario_in_progress() {
  local dir out
  dir=$(fixture in-progress)
  release "$dir" flow 0.4.0
  change "$dir" plugins/flow/release-test.txt "fix: a flow fix"
  prepare "$dir" flow >/dev/null
  gitc "$dir" commit -qam "chore(release): flow v0.4.1"
  out=$(prepare "$dir" flow)
  expect_eq "merged, untagged: status" "$(field status "$out")" in-progress
  expect_eq "merged, untagged: no file changed" "$(git -C "$dir" status --porcelain)" ""

  dir=$(fixture in-progress-initial)
  prepare "$dir" flow >/dev/null
  gitc "$dir" add -A
  gitc "$dir" commit -qm "chore(release): flow initial"
  expect_eq "merged initial, untagged: status" "$(field status "$(prepare "$dir" flow)")" in-progress
}

scenario_forced_released() {
  local dir
  dir=$(fixture forced)
  release "$dir" flow 0.4.0
  if prepare "$dir" flow 0.4.0 >/dev/null; then
    fail "forcing an already-released version succeeded"
  else
    pass "forcing an already-released version is refused"
  fi
}

scenario_notes() {
  local dir notes
  dir=$(fixture notes)
  release "$dir" flow 0.4.0
  change "$dir" plugins/flow/release-test.txt "fix: the first fix"
  release "$dir" flow 0.4.1
  notes=$("$dir/scripts/release/notes.sh" flow 0.4.1 2>>"$log")
  expect_eq "notes: body only" "$(grep -c '^## ' <<<"$notes")" 0
  expect_eq "notes: carries the fix" "$(grep -c 'The first fix' <<<"$notes")" 1
  expect_eq "notes: no neighbouring section" "$(grep -c 'Initial release' <<<"$notes")" 0
  if "$dir/scripts/release/notes.sh" flow 9.9.9 >/dev/null 2>>"$log"; then
    fail "notes: a missing version succeeded"
  else
    pass "notes: a missing version is refused"
  fi
}

scenario_initial
scenario_bumps_0x
scenario_bumps_1x
scenario_skipped_types
scenario_sdk_change
scenario_plugin_only
scenario_core_bump
scenario_in_progress
scenario_forced_released
scenario_notes

if ((failures)); then
  echo "$failures check(s) failed; fixtures and $log kept" >&2
  exit 1
fi
rm -rf "$root"
echo "release scripts: all checks passed"
```

- [ ] **Step 2: Add the tasks and the script lint**

Append to the `[tasks.lint]` `run` list:

```toml
  "shellcheck -x scripts/release/*.sh",
```

Append to `mise.toml`:

```toml
[tasks.release-prepare]
description = "Prepare one release unit's next release in the working tree (what release-pr.yml runs): `mise run release-prepare flow [version]`"
usage = '''
arg "<unit>" help="core, flow, web or matrix"
arg "[version]" help="an exact version to force"
'''
run = 'scripts/release/prepare.sh "$usage_unit" ${usage_version:+"$usage_version"}'

[tasks.release-test]
description = "Scenario tests for scripts/release/ against throwaway clones under target/tmp (Spec I §10)"
run = "scripts/release/test.sh"
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `mise run release-test 2>&1 | tail -3; echo "exit=${PIPESTATUS[0]}"`
Expected: it stops in the first scenario, before any `ok` line, complaining that `cliff.toml` or `scripts/release/lib.sh` does not exist, with a non-zero exit.

- [ ] **Step 4: Write `cliff.toml`**

```toml
# git-cliff configuration shared by every release unit (Spec I §4.1).
# scripts/release/prepare.sh passes the unit's --tag-pattern and
# --include-path; nothing unit-specific lives here.

[changelog]
header = "# Changelog\n\n"
body = """
{% if version -%}
## {{ version | split(pat="-v") | last }} - {{ timestamp | date(format="%Y-%m-%d") }}
{%- else -%}
## Unreleased
{%- endif %}
{% if commits | length == 0 %}
- Initial release.
{% endif -%}
{% for group, commits in commits | group_by(attribute="group") %}
### {{ group | striptags | trim }}
{% for commit in commits %}
- {% if commit.scope %}**{{ commit.scope }}:** {% endif %}{% if commit.breaking %}[**breaking**] {% endif %}{{ commit.message | upper_first }}
{%- endfor %}
{% endfor %}
"""
trim = true
footer = ""

[git]
conventional_commits = true
filter_unconventional = true
split_commits = false
protect_breaking_commits = false
filter_commits = false
sort_commits = "oldest"
# Releasable types get a group; everything else is skipped, so it neither
# appears in a changelog nor counts toward a release.
commit_parsers = [
  { message = "^feat", group = "<!-- 0 -->Features" },
  { message = "^fix", group = "<!-- 1 -->Bug fixes" },
  { message = "^perf", group = "<!-- 2 -->Performance" },
  { message = "^refactor", group = "<!-- 3 -->Refactoring" },
  { message = "^build", group = "<!-- 4 -->Build" },
  { message = ".*", skip = true },
]

[bump]
# Cargo semver: in 0.x a breaking change bumps minor, anything else patch.
features_always_bump_minor = false
breaking_always_bump_major = false
```

- [ ] **Step 5: Write `scripts/release/lib.sh`**

```bash
# shellcheck shell=bash
# The release units (Spec I §3): the one place that knows each unit's crate,
# manifest, tag, changelog, image and include paths. Sourced by every script
# under scripts/release/, never run on its own. Callers `cd` to the
# repository root first.

# shellcheck disable=SC2034 # read by the scripts that source this file
UNITS=(core flow web matrix)
# shellcheck disable=SC2034
PLUGIN_UNITS=(flow web matrix)

die() {
  echo "$*" >&2
  exit 1
}

require_unit() {
  case ${1:-} in
    core | flow | web | matrix) ;;
    *) die "unknown release unit: '${1:-}' (expected one of: ${UNITS[*]})" ;;
  esac
}

unit_crate() {
  require_unit "$1"
  if [[ $1 == core ]]; then echo balerix; else echo "balerix-plugin-$1"; fi
}

unit_tag_prefix() { echo "$(unit_crate "$1")-v"; }

unit_tag() { echo "$(unit_tag_prefix "$1")$2"; }

unit_manifest() {
  require_unit "$1"
  if [[ $1 == core ]]; then echo Cargo.toml; else echo "plugins/$1/Cargo.toml"; fi
}

unit_changelog() {
  require_unit "$1"
  if [[ $1 == core ]]; then echo CHANGELOG.md; else echo "plugins/$1/CHANGELOG.md"; fi
}

# ghcr repositories must be lowercase; a fork's owner may not be.
unit_image() {
  local owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
  echo "ghcr.io/${owner,,}/$(unit_crate "$1")"
}

# The paths whose commits count toward the unit, one per line. The SDK and
# the api are compiled into every plugin binary, so they count for plugins.
unit_paths() {
  require_unit "$1"
  if [[ $1 == core ]]; then
    printf '%s\n' 'crates/**' Cargo.toml Cargo.lock mise.toml
  else
    printf '%s\n' "plugins/$1/**" 'crates/balerix-api/**' 'crates/balerix-plugin-sdk/**'
  fi
}

unit_version() {
  local crate
  crate=$(unit_crate "$1")
  cargo metadata --manifest-path "$(unit_manifest "$1")" --no-deps --format-version 1 |
    jq -r --arg c "$crate" '.packages[] | select(.name == $c) | .version'
}

tag_exists() { git rev-parse -q --verify "refs/tags/$1" >/dev/null; }

# The unit's newest release tag, or nothing.
last_tag() { git tag --list "$(unit_tag_prefix "$1")*" --sort=-v:refname | head -n 1; }

# git-cliff scoped to the unit: its tag pattern and its include paths.
cliff() {
  local unit=$1 path
  shift
  local args=(--config cliff.toml --tag-pattern "^$(unit_tag_prefix "$unit")")
  while IFS= read -r path; do args+=(--include-path "$path"); done < <(unit_paths "$unit")
  git-cliff "${args[@]}" "$@"
}
```

- [ ] **Step 6: Write `scripts/release/prepare.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Prepares one release unit's next release in the working tree (Spec I §4.1):
# works out the version, writes it into the manifests and lockfiles, and
# prepends the changelog section. Nothing is committed.
#
# usage: prepare.sh <unit> [version]    a version forces that exact release
#
# Prints key=value lines on stdout; progress goes to stderr.
#   status=release       files changed; version=, tag= and notes= follow
#   status=none          no releasable commit since the last tag
#   status=in-progress   the manifest version is already awaiting its tag
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 || $# -eq 2 ]] || die "usage: $0 <unit> [version]"
unit=$1
forced=${2:-}
require_unit "$unit"

prefix=$(unit_tag_prefix "$unit")
changelog=$(unit_changelog "$unit")
current=$(unit_version "$unit")
last=$(last_tag "$unit")
notes_dir=${RELEASE_NOTES_DIR:-target/release-notes}
notes="$notes_dir/$unit.md"

emit() { printf '%s=%s\n' "$@"; }
has_section() { [[ -f $changelog ]] && grep -q "^## ${1//./\\.} - " "$changelog"; }

if [[ -n $forced ]]; then
  [[ $forced =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$unit: not a release version: $forced"
  ! tag_exists "$(unit_tag "$unit" "$forced")" || die "$unit: $forced is already released"
  next=$forced
elif ! tag_exists "$(unit_tag "$unit" "$current")" && { [[ -n $last ]] || has_section "$current"; }; then
  # A release PR was merged and release.yml has not tagged it yet, or failed
  # (Spec I §8.2). Proposing anything now would release twice.
  echo "$unit: $current is awaiting its tag; release in progress" >&2
  emit status in-progress version "$current"
  exit 0
elif [[ -z $last ]]; then
  next=$current
  echo "$unit: no release tag yet; initial release of $next" >&2
else
  count=$(cliff "$unit" --unreleased --context | jq '[.[].commits[]] | length')
  if [[ $count -eq 0 ]]; then
    echo "$unit: nothing to release since $last" >&2
    emit status none
    exit 0
  fi
  next=$(cliff "$unit" --bumped-version)
  next=${next#"$prefix"}
fi

tag=$(unit_tag "$unit" "$next")

if [[ $next != "$current" ]]; then
  if [[ $unit == core ]]; then
    cargo set-version --workspace "$next" >&2
    # Plugins lock the SDK and api versions through their path dependency.
    for plugin in "${PLUGIN_UNITS[@]}"; do
      cargo update --manifest-path "plugins/$plugin/Cargo.toml" -p balerix-api -p balerix-plugin-sdk >&2
    done
  else
    cargo set-version --manifest-path "plugins/$unit/Cargo.toml" "$next" >&2
  fi
fi
if [[ $unit != core ]]; then
  sed -i "s/^version: .*/version: $next/" "plugins/$unit/package/balerix-plugin.yaml"
fi

mkdir -p "$notes_dir"
cliff "$unit" --unreleased --tag "$tag" --strip header >"$notes"
{
  printf '# Changelog\n\n%s\n' "$(<"$notes")"
  if [[ -f $changelog ]]; then
    printf '\n'
    sed '1{/^# Changelog$/d}' "$changelog" | sed '1{/^$/d}'
  fi
} >"$changelog.new"
mv "$changelog.new" "$changelog"

echo "$unit: prepared $tag" >&2
emit status release version "$next" tag "$tag" notes "$notes"
```

- [ ] **Step 7: Write `scripts/release/notes.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Prints one released version's section from the unit's CHANGELOG.md, without
# its heading (Spec I §5.6: the GitHub Release notes).
#
# usage: notes.sh <unit> <version>
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <unit> <version>"
unit=$1
version=$2
changelog=$(unit_changelog "$unit")
[[ -f $changelog ]] || die "$unit: no $changelog"

section=$(awk -v heading="## $version - " '
  index($0, heading) == 1 { found = 1; next }
  found && /^## / { exit }
  found { print }
' "$changelog")
[[ -n ${section//[[:space:]]/} ]] || die "$unit: $changelog has no section for $version"
# Trim the blank lines around the section.
printf '%s\n' "$section" | sed -e '/./,$!d' | tac | sed -e '/./,$!d' | tac
```

- [ ] **Step 8: Run the tests to see them pass**

Run: `chmod +x scripts/release/*.sh && mise run release-test 2>&1 | tail -3`
Expected: `ok   notes: a missing version is refused` then `release scripts: all checks passed`, exit 0 (about a minute).

- [ ] **Step 9: Run the scripts in CI**

Create `.github/workflows/release-scripts.yml`:

```yaml
name: release-scripts
# The release scripts' own tests (Spec I §10), whenever they could change.
on:
  push:
    branches: [main]
    paths:
      - scripts/release/**
      - scripts/plugin.sh
      - cliff.toml
      - mise.toml
      - .github/workflows/release-scripts.yml
  pull_request:
    paths:
      - scripts/release/**
      - scripts/plugin.sh
      - cliff.toml
      - mise.toml
      - .github/workflows/release-scripts.yml
permissions: {}
env:
  MISE_AUTO_INSTALL: "false"
jobs:
  release-test:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: true
      - run: mise install rust jq git-cliff shellcheck
      - run: mise install cargo:cargo-edit
      - run: mise run release-test
```

- [ ] **Step 10: Verify and commit**

Run: `mise run lint > target/tmp/task3-lint.log 2>&1; echo "exit=$?"`
Expected: `exit=0` (shellcheck now checks the four scripts).

```bash
git add cliff.toml scripts/release/lib.sh scripts/release/prepare.sh scripts/release/notes.sh \
  scripts/release/test.sh .github/workflows/release-scripts.yml mise.toml
git commit -m "feat(release): compute versions and changelogs per release unit

scripts/release/prepare.sh works out a unit's next version from
Conventional Commits on its paths (git-cliff), writes it into the
manifests, lockfiles and plugin manifest, and prepends the changelog
(Spec I §4.1). test.sh drives it through every bump rule against
throwaway clones.

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 4: Release PRs and PR-title checks

**Files:**
- Create: `scripts/release/open-pr.sh`, `.github/workflows/release-pr.yml`, `.github/workflows/pr-title.yml`

**Interfaces:**
- Consumes: `prepare.sh` (Task 3), `lib.sh`.
- Produces: `open-pr.sh <unit> [version]`; needs `GITHUB_REPOSITORY` and (unless `RELEASE_PR_DRY_RUN=1`) `GH_TOKEN`. Repository configuration read by the workflow: variable `RELEASE_APP_ID`, secret `RELEASE_APP_PRIVATE_KEY`, both in environment `release-bot`.

- [ ] **Step 1: Write `scripts/release/open-pr.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Opens, updates or closes one unit's release PR (Spec I §4.2). CI runs it on
# a fresh checkout of main with GH_TOKEN set to the release App's token.
#
# usage: open-pr.sh <unit> [version]    a version pins the PR at it
# RELEASE_PR_DRY_RUN=1 prints the pushes and PR edits instead of making them.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 || $# -eq 2 ]] || die "usage: $0 <unit> [version]"
unit=$1
forced=${2:-}
require_unit "$unit"
: "${GITHUB_REPOSITORY:?}"
branch="release/$unit"
pinned="release:pinned"

act() {
  if [[ ${RELEASE_PR_DRY_RUN:-} == 1 ]]; then
    printf 'would run:' >&2
    printf ' %q' "$@" >&2
    printf '\n' >&2
  else
    "$@"
  fi
}

pr=$(gh pr list --repo "$GITHUB_REPOSITORY" --head "$branch" --base main --state open \
  --json number,labels --jq '.[0] // empty')
if [[ -z $forced && -n $pr ]] && jq -e --arg l "$pinned" 'any(.labels[]; .name == $l)' <<<"$pr" >/dev/null; then
  echo "$unit: $branch is labelled $pinned; leaving it alone" >&2
  exit 0
fi

out=$(scripts/release/prepare.sh "$unit" ${forced:+"$forced"})
case $(sed -n 's/^status=//p' <<<"$out") in
  release) ;;
  in-progress) exit 0 ;;
  none)
    if [[ -n $pr ]]; then
      act gh pr close "$branch" --repo "$GITHUB_REPOSITORY" --delete-branch \
        --comment "Nothing left to release for \`$unit\`."
    fi
    exit 0
    ;;
  *) die "$unit: prepare.sh printed no status" ;;
esac
version=$(sed -n 's/^version=//p' <<<"$out")
notes=$(sed -n 's/^notes=//p' <<<"$out")
title="chore(release): $(unit_crate "$unit") v$version"

git switch -q -C "$branch"
git add -A
git -c user.name="github-actions[bot]" \
  -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit -q -m "$title"
[[ ${RELEASE_PR_DRY_RUN:-} == 1 ]] || : "${GH_TOKEN:?GH_TOKEN must be the release App token}"
act git push -q --force "https://x-access-token:${GH_TOKEN:-}@github.com/${GITHUB_REPOSITORY}.git" \
  "HEAD:refs/heads/$branch"

if [[ -n $pr ]]; then
  act gh pr edit "$branch" --repo "$GITHUB_REPOSITORY" --title "$title" --body-file "$notes"
else
  act gh pr create --repo "$GITHUB_REPOSITORY" --base main --head "$branch" \
    --title "$title" --body-file "$notes"
fi
if [[ -n $forced ]]; then
  act gh label create "$pinned" --repo "$GITHUB_REPOSITORY" --force \
    --color FBCA04 --description "Release PR with a hand-picked version"
  act gh pr edit "$branch" --repo "$GITHUB_REPOSITORY" --add-label "$pinned"
fi
```

- [ ] **Step 2: Dry-run it in a throwaway clone**

```bash
rm -rf target/tmp/open-pr && git clone -q . target/tmp/open-pr
cp -R scripts/release target/tmp/open-pr/scripts/ && cp cliff.toml target/tmp/open-pr/
git -C target/tmp/open-pr tag --list | xargs -r git -C target/tmp/open-pr tag -d
(cd target/tmp/open-pr && GITHUB_REPOSITORY=balerix-ai/balerix RELEASE_PR_DRY_RUN=1 \
  mise x -- scripts/release/open-pr.sh flow 2>&1 | grep -E '^(flow:|would run:)')
```

Expected (flow's version may differ):
```
flow: no release tag yet; initial release of 0.1.0
flow: prepared balerix-plugin-flow-v0.1.0
would run: git push -q --force https://x-access-token:@github.com/balerix-ai/balerix.git HEAD:refs/heads/release/flow
would run: gh pr create --repo balerix-ai/balerix --base main --head release/flow --title chore\(release\):\ balerix-plugin-flow\ v0.1.0 --body-file target/release-notes/flow.md
```

Run: `git -C target/tmp/open-pr log --oneline -1 && rm -rf target/tmp/open-pr`
Expected: `… chore(release): balerix-plugin-flow v0.1.0`.

- [ ] **Step 3: Write `.github/workflows/release-pr.yml`**

```yaml
name: release-pr
# Keeps one release PR per unit in step with main (Spec I §4.2). Merging a
# release PR is what releases (release.yml).
on:
  push:
    branches: [main]
  workflow_dispatch:
    inputs:
      unit:
        description: Release unit
        type: choice
        options: [core, flow, web, matrix]
        required: true
      version:
        description: Exact version to release; pins the PR until the label is removed
        type: string
        required: false
permissions: {}
env:
  MISE_AUTO_INSTALL: "false"
jobs:
  pr:
    name: release PR (${{ matrix.unit }})
    runs-on: ubuntu-24.04
    environment: release-bot
    permissions:
      contents: read
    strategy:
      fail-fast: false
      matrix:
        unit: ${{ fromJSON(github.event_name == 'workflow_dispatch' && format('["{0}"]', inputs.unit) || '["core","flow","web","matrix"]') }}
    concurrency:
      group: release-pr-${{ matrix.unit }}
      cancel-in-progress: false
    steps:
      - id: app
        uses: actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1 # v3.2.0
        with:
          app-id: ${{ vars.RELEASE_APP_ID }}
          private-key: ${{ secrets.RELEASE_APP_PRIVATE_KEY }}
          permission-contents: write
          permission-pull-requests: write
          permission-issues: write
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq git-cliff gh
      # cargo-edit has no prebuilt release; the cargo backend builds it with rust.
      - run: mise install cargo:cargo-edit
      - env:
          UNIT: ${{ matrix.unit }}
          VERSION: ${{ inputs.version }}
          GH_TOKEN: ${{ steps.app.outputs.token }}
        run: mise x -- scripts/release/open-pr.sh "$UNIT" "$VERSION"
```

- [ ] **Step 4: Write `.github/workflows/pr-title.yml`**

```yaml
name: pr-title
# Squash merges make the PR title the commit on main, and the release
# scripts read commit types from it (Spec I §4.3).
on:
  pull_request:
    types: [opened, edited, synchronize, reopened]
permissions: {}
jobs:
  conventional:
    runs-on: ubuntu-24.04
    permissions:
      pull-requests: read
    steps:
      - uses: amannn/action-semantic-pull-request@48f256284bd46cdaab1048c3721360e808335d50 # v6.1.1
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        with:
          types: |
            feat
            fix
            perf
            refactor
            build
            docs
            test
            ci
            chore
            style
            revert
          requireScope: false
```

- [ ] **Step 5: Verify and commit**

Run: `mise run lint > target/tmp/task4-lint.log 2>&1; echo "exit=$?"`
Expected: `exit=0` (actionlint and zizmor clean for both workflows).

```bash
git add scripts/release/open-pr.sh .github/workflows/release-pr.yml .github/workflows/pr-title.yml
git commit -m "ci(release): keep one release PR per unit and check PR titles

release-pr.yml runs open-pr.sh for every unit on each push to main,
pushing with the release App's token so CI runs on the PR (Spec I §4.2).
pr-title.yml holds squash-merge titles to Conventional Commits (§4.3).

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 5: Static binaries — `build.sh` and the `build-binary` action

**Files:**
- Create: `scripts/release/build.sh`, `.github/actions/build-binary/action.yml`

**Interfaces:**
- Consumes: `lib.sh`, `scripts/plugin.sh target-dir`.
- Produces: `build.sh <unit> <target> <out-dir>` → `<out-dir>/<crate>` and `<out-dir>/<crate>-v<ver>-<target>.tar.gz` (binary, `LICENSE`, `CHANGELOG.md` when present). Composite action `./.github/actions/build-binary` with inputs `unit`, `target` and output `dist`; the caller has checked out and run `mise install rust jq`.

- [ ] **Step 1: Write `scripts/release/build.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Builds one release unit's static binary for one musl target, smoke-tests it
# on this machine and archives it (Spec I §5.2). Run it on a host of the
# target's architecture: the smoke test executes the binary.
#
# usage: build.sh <unit> <target> <out-dir>
# Writes <out-dir>/<crate> (the bare binary, for images) and
# <out-dir>/<crate>-v<version>-<target>.tar.gz; prints the archive path.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 3 ]] || die "usage: $0 <unit> <target> <out-dir>"
unit=$1
target=$2
out=$3
require_unit "$unit"
case $target in
  x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) ;;
  *) die "unsupported target: $target" ;;
esac
crate=$(unit_crate "$unit")
version=$(unit_version "$unit")

# matrix's aws-lc-sys and bundled SQLite compile C. On an arm64 host cc-rs
# looks for aarch64-linux-musl-gcc, which musl-tools does not ship; musl-gcc
# is the native wrapper on both architectures. Pure-Rust units never call it.
export CC_x86_64_unknown_linux_musl=${CC_x86_64_unknown_linux_musl:-musl-gcc}
export CC_aarch64_unknown_linux_musl=${CC_aarch64_unknown_linux_musl:-musl-gcc}

if [[ $unit == core ]]; then
  cargo build --release --locked --target "$target" -p balerix
  bin="${CARGO_TARGET_DIR:-target}/$target/release/$crate"
else
  target_dir=$(scripts/plugin.sh target-dir "$unit")
  CARGO_TARGET_DIR="$target_dir" cargo build --release --locked --target "$target" \
    --manifest-path "plugins/$unit/Cargo.toml"
  bin="$target_dir/$target/release/$crate"
fi

# Protocol §7: static builds, so no host libc can mismatch.
linkage=$(ldd "$bin" 2>&1 || true)
grep -Eq 'not a dynamic executable|statically linked' <<<"$linkage" ||
  die "$bin is not statically linked: $linkage"

if [[ $unit == core ]]; then
  got=$("$bin" --version)
  [[ $got == "balerix $version" ]] || die "balerix --version printed '$got', expected 'balerix $version'"
else
  # Without the daemon's environment a plugin fails in Env::from_process and
  # exits 1 with "<name>: …", which proves it starts on this architecture.
  set +e
  err=$(env -i "$bin" 2>&1 >/dev/null)
  code=$?
  set -e
  [[ $code -eq 1 ]] || die "$crate exited $code without a daemon environment, expected 1: $err"
  grep -q "^$unit: " <<<"$err" || die "$crate did not report '$unit: …': $err"
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
files=("$crate" LICENSE)
cp "$bin" "$stage/$crate"
cp LICENSE "$stage/LICENSE"
changelog=$(unit_changelog "$unit")
if [[ -f $changelog ]]; then
  cp "$changelog" "$stage/CHANGELOG.md"
  files+=(CHANGELOG.md)
fi

mkdir -p "$out"
cp "$bin" "$out/$crate"
archive="$out/$crate-v$version-$target.tar.gz"
tar -czf "$archive" -C "$stage" "${files[@]}"
echo "$archive"
```

- [ ] **Step 2: Build flow and core for x86_64 musl locally**

Run: `mise x -- rustup target add x86_64-unknown-linux-musl`
Expected: the target installs (or is already up to date).

Run: `mise x -- scripts/release/build.sh flow x86_64-unknown-linux-musl target/tmp/dist && mise x -- scripts/release/build.sh core x86_64-unknown-linux-musl target/tmp/dist`
Expected: the last line of each is the archive path, e.g. `target/tmp/dist/balerix-plugin-flow-v0.1.0-x86_64-unknown-linux-musl.tar.gz` and `target/tmp/dist/balerix-v0.1.0-x86_64-unknown-linux-musl.tar.gz`. (Neither needs `musl-gcc`: both are pure Rust. matrix needs `musl-tools`; CI installs it.)

Run: `tar -tzf target/tmp/dist/balerix-v0.1.0-x86_64-unknown-linux-musl.tar.gz`
Expected: `balerix` and `LICENSE`.

Run: `rm -rf target/tmp/dist`

- [ ] **Step 3: Write `.github/actions/build-binary/action.yml`**

```yaml
name: build-binary
description: >-
  Build, smoke-test and archive one release unit's static musl binary on a
  runner of the target's architecture (scripts/release/build.sh). The caller
  has checked out the repository and installed rust and jq through mise.
inputs:
  unit:
    description: Release unit (core, flow, web, matrix)
    required: true
  target:
    description: x86_64-unknown-linux-musl or aarch64-unknown-linux-musl
    required: true
outputs:
  dist:
    description: Directory holding the bare binary and its archive
    value: ${{ steps.build.outputs.dist }}
runs:
  using: composite
  steps:
    - name: Install musl-tools
      shell: bash
      run: |
        sudo apt-get update -q
        sudo apt-get install -yq --no-install-recommends musl-tools
    - name: Add the Rust target
      shell: bash
      env:
        TARGET: ${{ inputs.target }}
      run: mise x -- rustup target add "$TARGET"
    - name: Build, smoke-test and archive
      id: build
      shell: bash
      env:
        UNIT: ${{ inputs.unit }}
        TARGET: ${{ inputs.target }}
      run: |
        dist="$RUNNER_TEMP/dist"
        mise x -- scripts/release/build.sh "$UNIT" "$TARGET" "$dist"
        echo "dist=$dist" >>"$GITHUB_OUTPUT"
```

- [ ] **Step 4: Verify and commit**

Run: `mise run lint > target/tmp/task5-lint.log 2>&1; echo "exit=$?"`
Expected: `exit=0`.

```bash
git add scripts/release/build.sh .github/actions/build-binary/action.yml
git commit -m "build(release): static musl binaries with an on-host smoke test

build.sh builds one unit for one musl target, proves the binary is static
and starts on this architecture, and archives it under the asset name
mise's github backend matches (Spec I §5.2).

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 6: Container images

**Files:**
- Create: `docker/balerix/Dockerfile`, `docker/plugin/Dockerfile`, `.hadolint.yaml`, `scripts/release/image-context.sh`, `scripts/release/smoke-image.sh`, `.github/actions/build-image/action.yml`, `.github/workflows/images.yml`

**Interfaces:**
- Consumes: `build-binary` (Task 5), `lib.sh` (`unit_image`, `unit_version`).
- Produces: `image-context.sh <unit> <dist-dir> <context-dir>` → outputs `context=`, `dockerfile=`, `image=`, `version=`. `smoke-image.sh <unit> <image-ref>`. Composite action `./.github/actions/build-image` with inputs `unit`, `dist`, `push` (`'true'`/`'false'`), `github-token`, output `digest`; the caller has run `mise install rust jq hadolint trivy` and, when pushing, logged in to ghcr.

- [ ] **Step 1: Write `.hadolint.yaml`**

```yaml
# DL3008 wants apt package versions pinned. The base images are pinned by
# digest and Renovate moves them; pinning Debian package versions as well
# breaks the build whenever the security archive supersedes one.
ignored:
  - DL3008
```

- [ ] **Step 2: Write `docker/balerix/Dockerfile`**

The two `MISE_SHA256_*` values are the `SHASUMS256.txt` entries of mise v2026.9.2's `linux-x64-musl` and `linux-arm64-musl` tarballs; the base digests are `debian:trixie-slim` and `gcr.io/distroless/static-debian13:nonroot` index digests as of 2026-09-11.

```dockerfile
# The balerix runtime image (Spec I §6.2): the static `balerix` plus every
# tool `serve` looks for on PATH. Built by scripts/release/image-context.sh's
# context: `balerix` (a built binary) and the repository's mise.toml.
FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS base

FROM base AS tools
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG TARGETARCH
ARG MISE_VERSION=2026.9.2
ARG MISE_SHA256_AMD64=9b75416600bb52ef12c54bc64d85f2f756c4d8980a981a7878ada273f56acda9
ARG MISE_SHA256_ARM64=df427c304c48c10323fe9ed5ec99e7f39bdc01d1842e304cd3f15ea5a6049320
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/* \
  && case "$TARGETARCH" in \
       amd64) arch=x64; sha="$MISE_SHA256_AMD64" ;; \
       arm64) arch=arm64; sha="$MISE_SHA256_ARM64" ;; \
       *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
     esac \
  && curl -fsSLo /tmp/mise.tar.gz \
       "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-${arch}-musl.tar.gz" \
  && echo "$sha  /tmp/mise.tar.gz" | sha256sum --check - \
  && tar -xzf /tmp/mise.tar.gz -C /tmp \
  && install -m 0755 /tmp/mise/bin/mise /usr/local/bin/mise
# gh, nono and tmux at exactly the versions the repository pins. The config
# stays in this stage: a system mise config in the final image would enter
# every agent's and plugin's tool resolution.
COPY mise.toml /build/mise.toml
WORKDIR /build
ENV MISE_DATA_DIR=/opt/mise MISE_CACHE_DIR=/tmp/mise-cache MISE_TRUSTED_CONFIG_PATHS=/build
RUN --mount=type=secret,id=github_token,env=GITHUB_TOKEN \
  mise install gh nono tmux \
  && mkdir /out \
  && for tool in gh nono tmux; do install -m 0755 "$(mise which "$tool")" "/out/$tool"; done

FROM base
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates git tini \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --uid 10001 --create-home --shell /bin/bash balerix
COPY --from=tools /usr/local/bin/mise /usr/local/bin/mise
COPY --from=tools /out/ /usr/local/bin/
COPY --chmod=0755 balerix /usr/local/bin/balerix
ENV LANG=C.UTF-8 HOME=/home/balerix
USER 10001:10001
WORKDIR /home/balerix
# The XDG state, config and data roots all live under $HOME.
VOLUME /home/balerix
ENTRYPOINT ["/usr/bin/tini", "--", "balerix"]
CMD ["serve"]
```

- [ ] **Step 3: Write `docker/plugin/Dockerfile`**

```dockerfile
# A plugin image (Spec I §6.3): one static plugin binary. The context from
# scripts/release/image-context.sh holds it as `plugin`, whichever plugin it
# is. distroless/static carries the CA bundle matrix's HTTP client needs.
# The daemon does not run plugins from images; it runs the release package.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:1c2c046bc09ed40fad370b599a0b1ae7987f55b01e247cf27a7c27cd97e5bbc7
COPY --chmod=0755 plugin /usr/local/bin/balerix-plugin
ENTRYPOINT ["/usr/local/bin/balerix-plugin"]
```

- [ ] **Step 4: Lint the Dockerfiles**

Run: `mise x -- hadolint docker/balerix/Dockerfile docker/plugin/Dockerfile; echo "exit=$?"`
Expected: no output, `exit=0`.

- [ ] **Step 5: Write `scripts/release/image-context.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Assembles the Docker build context for one unit's image from an already
# built binary (Spec I §6.1): nothing compiles inside Docker.
#
# usage: image-context.sh <unit> <dist-dir> <context-dir>
# Prints GitHub step outputs: context=, dockerfile=, image=, version=.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 3 ]] || die "usage: $0 <unit> <dist-dir> <context-dir>"
unit=$1
dist=$2
context=$3
crate=$(unit_crate "$unit")
[[ -f $dist/$crate ]] || die "$unit: no binary at $dist/$crate"

rm -rf "$context"
mkdir -p "$context"
if [[ $unit == core ]]; then
  cp "$dist/$crate" "$context/balerix"
  # The image installs gh, nono and tmux at the versions this file pins.
  cp mise.toml "$context/mise.toml"
  dockerfile=docker/balerix/Dockerfile
else
  cp "$dist/$crate" "$context/plugin"
  dockerfile=docker/plugin/Dockerfile
fi

echo "context=$(cd "$context" && pwd)"
echo "dockerfile=$PWD/$dockerfile"
echo "image=$(unit_image "$unit")"
echo "version=$(unit_version "$unit")"
```

- [ ] **Step 6: Check the context locally**

```bash
mkdir -p target/tmp/ctx-dist && printf 'x' > target/tmp/ctx-dist/balerix-plugin-flow && printf 'x' > target/tmp/ctx-dist/balerix
mise x -- scripts/release/image-context.sh flow target/tmp/ctx-dist target/tmp/ctx-flow
mise x -- scripts/release/image-context.sh core target/tmp/ctx-dist target/tmp/ctx-core
ls target/tmp/ctx-flow target/tmp/ctx-core && rm -rf target/tmp/ctx-*
```

Expected: the first prints `context=…/target/tmp/ctx-flow`, `dockerfile=…/docker/plugin/Dockerfile`, `image=ghcr.io/balerix-ai/balerix-plugin-flow`, `version=0.1.0`; the second `dockerfile=…/docker/balerix/Dockerfile`, `image=ghcr.io/balerix-ai/balerix`; `ls` shows `plugin` and `balerix mise.toml`.

- [ ] **Step 7: Write `scripts/release/smoke-image.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Smoke-tests one unit's locally loaded image (Spec I §6.4).
#
# usage: smoke-image.sh <unit> <image-ref>
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <unit> <image-ref>"
unit=$1
ref=$2
require_unit "$unit"
version=$(unit_version "$unit")

if [[ $unit != core ]]; then
  set +e
  err=$(docker run --rm "$ref" 2>&1 >/dev/null)
  code=$?
  set -e
  [[ $code -eq 1 ]] || die "$ref exited $code without a daemon environment, expected 1: $err"
  grep -q "^$unit: " <<<"$err" || die "$ref did not report '$unit: …': $err"
  echo "$ref: ok" >&2
  exit 0
fi

got=$(docker run --rm "$ref" --version)
[[ $got == "balerix $version" ]] || die "$ref --version printed '$got', expected 'balerix $version'"

# `serve` discovers git, gh, mise, nono and tmux on PATH before it listens,
# so a `list` that answers proves the image carries every tool.
name="balerix-smoke-$$"
docker run -d --name "$name" "$ref" serve >/dev/null
ok=false
for _ in $(seq 60); do
  if docker exec "$name" balerix list >/dev/null 2>&1; then
    ok=true
    break
  fi
  sleep 1
done
if ! $ok; then
  docker logs "$name" >&2 || true
  docker rm -f "$name" >/dev/null
  die "$ref: \`balerix list\` never answered inside the container"
fi
docker rm -f "$name" >/dev/null
echo "$ref: ok" >&2
```

- [ ] **Step 8: Write `.github/actions/build-image/action.yml`**

```yaml
name: build-image
description: >-
  Build one release unit's image for this runner's architecture from an
  already built binary, smoke-test and scan it, and optionally push it by
  digest (Spec I §6). The caller has checked out the repository, installed
  jq, hadolint and trivy through mise, and logged in to ghcr when pushing.
inputs:
  unit:
    description: Release unit (core, flow, web, matrix)
    required: true
  dist:
    description: Directory holding the unit's bare binary (build-binary's dist)
    required: true
  push:
    description: "'true' pushes the image by digest after the checks pass"
    required: false
    default: "false"
  github-token:
    description: Token for mise's GitHub downloads inside the build
    required: true
outputs:
  digest:
    description: The pushed per-architecture digest (empty when not pushing)
    value: ${{ steps.push.outputs.digest }}
runs:
  using: composite
  steps:
    - name: Assemble the build context
      id: context
      shell: bash
      env:
        UNIT: ${{ inputs.unit }}
        DIST: ${{ inputs.dist }}
      run: mise x -- scripts/release/image-context.sh "$UNIT" "$DIST" "$RUNNER_TEMP/image-context" >>"$GITHUB_OUTPUT"
    - name: Lint the Dockerfile
      shell: bash
      env:
        DOCKERFILE: ${{ steps.context.outputs.dockerfile }}
      run: mise x -- hadolint "$DOCKERFILE"
    - uses: docker/setup-buildx-action@37fe631027851001ddb9b187196cc803df7f5f0e # v4.3.0
    - name: Labels
      id: meta
      uses: docker/metadata-action@dc802804100637a589fabce1cb79ff13a1411302 # v6.2.0
      with:
        images: ${{ steps.context.outputs.image }}
        labels: |
          org.opencontainers.image.version=${{ steps.context.outputs.version }}
    - name: Build for the smoke test and scan
      uses: docker/build-push-action@53b7df96c91f9c12dcc8a07bcb9ccacbed38856a # v7.3.0
      with:
        context: ${{ steps.context.outputs.context }}
        file: ${{ steps.context.outputs.dockerfile }}
        labels: ${{ steps.meta.outputs.labels }}
        load: true
        tags: ${{ steps.context.outputs.image }}:smoke
        secrets: |
          github_token=${{ inputs.github-token }}
    - name: Smoke test
      shell: bash
      env:
        UNIT: ${{ inputs.unit }}
        IMAGE: ${{ steps.context.outputs.image }}
      run: mise x -- scripts/release/smoke-image.sh "$UNIT" "$IMAGE:smoke"
    - name: Scan
      shell: bash
      env:
        IMAGE: ${{ steps.context.outputs.image }}
      run: >-
        mise x -- trivy image --exit-code 1 --severity CRITICAL,HIGH
        --ignore-unfixed --no-progress "$IMAGE:smoke"
    - name: Push by digest
      id: push
      if: inputs.push == 'true'
      uses: docker/build-push-action@53b7df96c91f9c12dcc8a07bcb9ccacbed38856a # v7.3.0
      with:
        context: ${{ steps.context.outputs.context }}
        file: ${{ steps.context.outputs.dockerfile }}
        labels: ${{ steps.meta.outputs.labels }}
        sbom: true
        provenance: mode=max
        outputs: type=image,name=${{ steps.context.outputs.image }},push-by-digest=true,name-canonical=true,push=true
        secrets: |
          github_token=${{ inputs.github-token }}
```

- [ ] **Step 9: Write `.github/workflows/images.yml`**

```yaml
name: images
# Builds, smoke-tests and scans every unit's image on both architectures
# without pushing, so a Dockerfile or tool-pin break shows up before release
# day (Spec I §6.5).
on:
  pull_request:
    paths:
      - docker/**
      - mise.toml
      - .hadolint.yaml
      - scripts/release/**
      - .github/actions/**
      - .github/workflows/images.yml
  schedule:
    - cron: "43 3 * * *"
  workflow_dispatch:
permissions: {}
env:
  MISE_AUTO_INSTALL: "false"
jobs:
  image:
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: read
    strategy:
      fail-fast: false
      matrix:
        unit: [core, flow, web, matrix]
        arch: [amd64, arm64]
        include:
          - arch: amd64
            target: x86_64-unknown-linux-musl
            runner: ubuntu-24.04
          - arch: arm64
            target: aarch64-unknown-linux-musl
            runner: ubuntu-24.04-arm
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: true
      - run: mise install rust jq hadolint trivy
      - id: build
        uses: ./.github/actions/build-binary
        with:
          unit: ${{ matrix.unit }}
          target: ${{ matrix.target }}
      - uses: ./.github/actions/build-image
        with:
          unit: ${{ matrix.unit }}
          dist: ${{ steps.build.outputs.dist }}
          github-token: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **Step 10: Verify and commit**

Run: `mise run lint > target/tmp/task6-lint.log 2>&1; echo "exit=$?"`
Expected: `exit=0`.

The image builds themselves run on GitHub's runners (no Docker locally); Task 10 exercises `images.yml`.

```bash
git add .hadolint.yaml docker scripts/release/image-context.sh scripts/release/smoke-image.sh \
  .github/actions/build-image/action.yml .github/workflows/images.yml
git commit -m "build(release): runtime and plugin images from the release binaries

The balerix image carries git, gh, mise, nono and tmux at the repository's
pins with no mise config left behind; plugin images are the binary on
distroless (Spec I §6). images.yml builds, smoke-tests and scans both
architectures on pull requests and nightly without pushing.

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 7: The plugin release package

**Files:**
- Create: `scripts/release/plugin-mise.toml.tmpl`, `scripts/release/package.sh`
- Modify: `scripts/release/test.sh`

**Interfaces:**
- Consumes: `lib.sh`; `balerix plugin package <dir> --out <file>` (existing CLI).
- Produces: `package.sh <plugin> <dist-dir> <out-dir>` → stdout `package=<abs path>` and `sha256=<hex>`. `BALERIX_BIN` overrides the `cargo run -p balerix` invocation.

- [ ] **Step 1: Add the package scenario to `scripts/release/test.sh`**

Insert this function after the `scenario_notes() { … }` function (before the block of scenario calls), and add the call `scenario_package` on its own line after the `scenario_notes` call:

```bash
scenario_package() {
  local dir="$root/package" version result pkg sha_x64
  version=$(manifest_version "$repo" flow)
  mkdir -p "$dir/dist" "$dir/unpacked"
  echo x64 >"$dir/dist/balerix-plugin-flow-v$version-x86_64-unknown-linux-musl.tar.gz"
  echo arm64 >"$dir/dist/balerix-plugin-flow-v$version-aarch64-unknown-linux-musl.tar.gz"
  sha_x64=$(sha256sum "$dir/dist/balerix-plugin-flow-v$version-x86_64-unknown-linux-musl.tar.gz" | cut -d' ' -f1)
  result=$(GITHUB_REPOSITORY=example/fork "$repo/scripts/release/package.sh" flow "$dir/dist" "$dir/out" 2>>"$log")
  pkg=$(field package "$result")
  expect_eq "package: file name" "$(basename "$pkg")" "balerix-plugin-flow-v$version-package.tar.gz"
  expect_eq "package: sha256" "$(field sha256 "$result")" "$(sha256sum "$pkg" | cut -d' ' -f1)"
  tar -xzf "$pkg" -C "$dir/unpacked"
  expect_grep "package: repository" '[tools."github:example/fork"]' "$dir/unpacked/mise.toml"
  expect_grep "package: tag prefix" 'version_prefix = "balerix-plugin-flow-v"' "$dir/unpacked/mise.toml"
  expect_grep "package: x64 checksum" "checksum = \"sha256:$sha_x64\"" "$dir/unpacked/mise.toml"
  expect_grep "package: start task" 'run = "balerix-plugin-flow"' "$dir/unpacked/mise.toml"
  expect_eq "package: manifest version" \
    "$(sed -n 's/^version: //p' "$dir/unpacked/balerix-plugin.yaml")" "$version"
  if (cd "$dir/unpacked" && MISE_TRUSTED_CONFIG_PATHS="$PWD" mise tasks ls 2>>"$log" | grep -q '^serve'); then
    pass "package: mise parses mise.toml"
  else
    fail "package: mise cannot read the rendered mise.toml"
  fi
}
```

- [ ] **Step 2: Run the tests to see the new scenario fail**

Run: `mise run release-test 2>&1 | tail -3; echo "exit=${PIPESTATUS[0]}"`
Expected: every earlier scenario prints `ok`, then the run stops in `scenario_package` (`scripts/release/package.sh: No such file or directory` is in `target/tmp/release-test-*/scripts.log`) with a non-zero exit.

- [ ] **Step 3: Write `scripts/release/plugin-mise.toml.tmpl`**

```toml
# The release package layout (plugin-protocol §7, Spec I §7): the plugin
# binary is a mise tool pinned to this release's assets, and the package
# carries no binary of its own. Rendered by scripts/release/package.sh.
[tools."github:@REPO@"]
version = "@VERSION@"
version_prefix = "@CRATE@-v"

[tools."github:@REPO@".platforms]
linux-x64 = { asset_pattern = "@CRATE@-v@VERSION@-x86_64-unknown-linux-musl.tar.gz", checksum = "sha256:@SHA256_X64@" }
linux-arm64 = { asset_pattern = "@CRATE@-v@VERSION@-aarch64-unknown-linux-musl.tar.gz", checksum = "sha256:@SHA256_ARM64@" }

[tasks.serve]
run = "@CRATE@"
```

- [ ] **Step 4: Write `scripts/release/package.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Assembles one plugin's release package (Spec I §7.2): its manifest plus a
# mise.toml that pins both architectures' archives by checksum, tarred by
# the daemon's own `balerix plugin package`.
#
# usage: package.sh <plugin> <dist-dir> <out-dir>
#   <dist-dir> holds <crate>-v<version>-<target>.tar.gz for both targets.
# Prints package=<path> and sha256=<digest> on stdout.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 3 ]] || die "usage: $0 <plugin> <dist-dir> <out-dir>"
unit=$1
dist=$2
out=$3
[[ $unit != core ]] || die "core has no plugin package"
require_unit "$unit"
crate=$(unit_crate "$unit")
version=$(unit_version "$unit")
repo=${GITHUB_REPOSITORY:-balerix-ai/balerix}

x64="$dist/$crate-v$version-x86_64-unknown-linux-musl.tar.gz"
arm64="$dist/$crate-v$version-aarch64-unknown-linux-musl.tar.gz"
for archive in "$x64" "$arm64"; do
  [[ -f $archive ]] || die "$unit: missing $archive"
done

manifest="plugins/$unit/package/balerix-plugin.yaml"
manifest_version=$(sed -n 's/^version: //p' "$manifest")
[[ $manifest_version == "$version" ]] ||
  die "$unit: $manifest says $manifest_version, Cargo.toml says $version"

pkg=$(mktemp -d)
trap 'rm -rf "$pkg"' EXIT
cp "$manifest" "$pkg/balerix-plugin.yaml"
sed -e "s|@REPO@|$repo|g" \
  -e "s|@CRATE@|$crate|g" \
  -e "s|@VERSION@|$version|g" \
  -e "s|@SHA256_X64@|$(sha256sum "$x64" | cut -d' ' -f1)|g" \
  -e "s|@SHA256_ARM64@|$(sha256sum "$arm64" | cut -d' ' -f1)|g" \
  scripts/release/plugin-mise.toml.tmpl >"$pkg/mise.toml"
! grep -q '@[A-Z0-9_]*@' "$pkg/mise.toml" || die "$unit: unrendered placeholder in mise.toml"

mkdir -p "$out"
asset="$(cd "$out" && pwd)/$crate-v$version-package.tar.gz"
if [[ -n ${BALERIX_BIN:-} ]]; then
  "$BALERIX_BIN" plugin package "$pkg" --out "$asset" >&2
else
  cargo run -q --locked -p balerix -- plugin package "$pkg" --out "$asset" >&2
fi

echo "package=$asset"
echo "sha256=$(sha256sum "$asset" | cut -d' ' -f1)"
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `mise run release-test 2>&1 | tail -3`
Expected: `ok   package: mise parses mise.toml` then `release scripts: all checks passed`.

- [ ] **Step 6: Verify and commit**

Run: `mise run lint > target/tmp/task7-lint.log 2>&1; echo "exit=$?"`
Expected: `exit=0`.

```bash
git add scripts/release/plugin-mise.toml.tmpl scripts/release/package.sh scripts/release/test.sh
git commit -m "feat(release): assemble plugin release packages

package.sh renders a mise.toml that pins both architectures' archives by
checksum and tars it with the manifest through balerix plugin package, so
the format is the daemon's own (Spec I §7).

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 8: The release workflow

**Files:**
- Create: `scripts/release/plan.sh`, `scripts/release/audit.sh`, `scripts/release/publish-crates.sh`, `scripts/release/merge-image.sh`, `scripts/release/github-release.sh`, `scripts/release/verify-package.sh`, `scripts/release/promote-image.sh`, `.github/workflows/release.yml`

**Interfaces:**
- Consumes: everything above.
- Produces: `plan.sh` → `units=`, `plugins=` (JSON arrays), `core=`. `audit.sh <unit>…`. `publish-crates.sh [--dry-run]` (needs `CARGO_REGISTRY_TOKEN` otherwise). `merge-image.sh <unit> <digests-dir>` → `image=`, `digest=`. `github-release.sh <unit> <assets-dir>` (needs `GITHUB_REPOSITORY`, `GITHUB_SHA`, `GH_TOKEN`). `verify-package.sh <plugin> [--flag]`. `promote-image.sh <unit>`. Repository configuration: environment `release`.

- [ ] **Step 1: Write `scripts/release/plan.sh`** (mode 0755) and check it

```bash
#!/usr/bin/env bash
# Which release units have a manifest version with no tag yet (Spec I §5.1).
# Prints GitHub step outputs: units= and plugins= (JSON arrays), core=true|false.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

json() {
  if (($#)); then printf '%s\n' "$@" | jq -R . | jq -cs .; else echo '[]'; fi
}

units=()
plugins=()
core=false
for unit in "${UNITS[@]}"; do
  tag=$(unit_tag "$unit" "$(unit_version "$unit")")
  if tag_exists "$tag"; then
    echo "$unit: $tag exists" >&2
    continue
  fi
  echo "$unit: releasing $tag" >&2
  units+=("$unit")
  if [[ $unit == core ]]; then core=true; else plugins+=("$unit"); fi
done

echo "units=$(json "${units[@]}")"
echo "plugins=$(json "${plugins[@]}")"
echo "core=$core"
```

Run: `mise x -- scripts/release/plan.sh`
Expected (no release tags exist yet): stderr `core: releasing balerix-v0.1.0` … `matrix: releasing balerix-plugin-matrix-v0.1.0`; stdout
```
units=["core","flow","web","matrix"]
plugins=["flow","web","matrix"]
core=true
```

- [ ] **Step 2: Write `scripts/release/audit.sh`** (mode 0755) and check it

```bash
#!/usr/bin/env bash
# Dependency advisories and policy for the units about to be released
# (Spec I §5.1): the nightly `mise run audit`, as a gate, per unit.
#
# usage: audit.sh <unit>...
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

(($#)) || die "usage: $0 <unit>..."
for unit in "$@"; do
  require_unit "$unit"
  echo "== $unit" >&2
  if [[ $unit == core ]]; then
    cargo audit
    cargo deny check advisories bans sources licenses
  else
    cargo audit --file "plugins/$unit/Cargo.lock"
    cargo deny --manifest-path "plugins/$unit/Cargo.toml" check advisories bans sources licenses
  fi
done
```

Run: `mise x -- scripts/release/audit.sh flow > target/tmp/task8-audit.log 2>&1; echo "exit=$?"`
Expected: `exit=0` (the same checks `mise run audit` runs nightly for flow). If an advisory published since the last nightly fails it, stop and report it rather than adding an ignore.

- [ ] **Step 3: Write `scripts/release/publish-crates.sh`** (mode 0755) and dry-run it

```bash
#!/usr/bin/env bash
# Publishes balerix-api and balerix-plugin-sdk at the core version (Spec I
# §5.5), skipping any that crates.io already has, so a re-run is safe.
#
# usage: publish-crates.sh [--dry-run]
# CARGO_REGISTRY_TOKEN must be set unless --dry-run is given.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

dry_run=false
case ${1:-} in
  "") ;;
  --dry-run) dry_run=true ;;
  *) die "usage: $0 [--dry-run]" ;;
esac

version=$(unit_version core)

# The sparse index path of a crate name of four or more characters.
published() {
  local name=$1
  curl -fsS "https://index.crates.io/${name:0:2}/${name:2:2}/$name" 2>/dev/null |
    jq -e --arg v "$version" 'select(.vers == $v)' >/dev/null
}

crates=()
for crate in balerix-api balerix-plugin-sdk; do
  if published "$crate"; then
    echo "$crate $version is already on crates.io; skipping" >&2
  else
    crates+=(-p "$crate")
  fi
done
((${#crates[@]})) || exit 0

if $dry_run; then
  cargo publish --dry-run --locked "${crates[@]}"
else
  : "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN must come from crates-io-auth-action}"
  cargo publish --locked "${crates[@]}"
fi
```

Run: `mise x -- scripts/release/publish-crates.sh --dry-run 2>&1 | tail -4`
Expected: `Packaging balerix-api …`, `Packaging balerix-plugin-sdk …`, verification builds, and `warning: aborting upload due to dry run`; exit 0.

- [ ] **Step 4: Write `scripts/release/merge-image.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Combines one unit's two per-architecture image digests into a multi-arch
# index tagged <version> (Spec I §6.1). Moving tags come later
# (promote-image.sh).
#
# usage: merge-image.sh <unit> <digests-dir>
#   <digests-dir> holds one empty file per pushed digest, named by its hex.
# Prints GitHub step outputs: image=, digest=.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <unit> <digests-dir>"
unit=$1
digests=$2
image=$(unit_image "$unit")
version=$(unit_version "$unit")

refs=()
for file in "$digests"/*; do
  refs+=("$image@sha256:$(basename "$file")")
done
((${#refs[@]} == 2)) || die "$unit: expected 2 per-architecture digests, found ${#refs[@]}"

docker buildx imagetools create --tag "$image:$version" "${refs[@]}" >&2
digest=$(docker buildx imagetools inspect "$image:$version" --format '{{json .Manifest}}' | jq -r .digest)
[[ $digest == sha256:* ]] || die "$unit: could not read the index digest of $image:$version"

echo "image=$image"
echo "digest=$digest"
```

- [ ] **Step 5: Write `scripts/release/github-release.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Creates and publishes one unit's GitHub Release (Spec I §5.6). Publishing
# creates the tag, which is the last thing a release does (I-8). A draft
# left by an earlier failed run is deleted first.
#
# usage: github-release.sh <unit> <assets-dir>
#   <assets-dir> holds the archives, SHA256SUMS and, for a plugin, its package.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <unit> <assets-dir>"
unit=$1
assets=$2
: "${GITHUB_REPOSITORY:?}" "${GITHUB_SHA:?}"
crate=$(unit_crate "$unit")
version=$(unit_version "$unit")
tag=$(unit_tag "$unit" "$version")

[[ -f $assets/SHA256SUMS ]] || die "$unit: no SHA256SUMS in $assets"

notes=$(mktemp)
trap 'rm -f "$notes"' EXIT
scripts/release/notes.sh "$unit" "$version" >"$notes"

if [[ $unit != core ]]; then
  package="$crate-v$version-package.tar.gz"
  [[ -f $assets/$package ]] || die "$unit: no $package in $assets"
  sha=$(sha256sum "$assets/$package" | cut -d' ' -f1)
  cat >>"$notes" <<EOF

### Install

Download \`$package\` next to your \`plugins.yaml\` (\`\$XDG_CONFIG_HOME/balerix/\`) and add:

\`\`\`yaml
plugins:
  - name: $unit
    source: ./$package
    sha256: "$sha"
\`\`\`
EOF
fi

# Drafts have no tag yet, so they cannot be looked up by tag name.
gh api "repos/$GITHUB_REPOSITORY/releases" --paginate \
  --jq ".[] | select(.draft and .tag_name == \"$tag\") | .id" |
  while read -r id; do
    echo "$unit: deleting draft release $id left by an earlier run" >&2
    gh api -X DELETE "repos/$GITHUB_REPOSITORY/releases/$id"
  done

# The newest core release is the repository's "latest"; plugins never are.
latest=false
[[ $unit != core ]] || latest=true

gh release create "$tag" --draft \
  --target "$GITHUB_SHA" \
  --title "$crate v$version" \
  --notes-file "$notes" \
  "$assets"/*
gh release edit "$tag" --draft=false --latest="$latest"
echo "$unit: published $tag" >&2
```

- [ ] **Step 6: Write `scripts/release/verify-package.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Installs one plugin's published release package the way the daemon does
# and starts it (Spec I §7.3): mise must resolve, checksum and attest the
# binary for this architecture, and the binary must run.
#
# usage: verify-package.sh <plugin>           verify
#        verify-package.sh <plugin> --flag    mark the release as a prerelease
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 || $# -eq 2 ]] || die "usage: $0 <plugin> [--flag]"
unit=$1
[[ $unit != core ]] || die "core has no plugin package"
require_unit "$unit"
: "${GITHUB_REPOSITORY:?}"
crate=$(unit_crate "$unit")
version=$(unit_version "$unit")
tag=$(unit_tag "$unit" "$version")
package="$crate-v$version-package.tar.gz"

if [[ ${2:-} == --flag ]]; then
  marker="> **Package verification failed**"
  body=$(gh release view "$tag" --json body --jq .body)
  if ! grep -qF "$marker" <<<"$body"; then
    run="${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY/actions/runs/${GITHUB_RUN_ID:-}"
    printf '%s on %s ([run](%s)); do not install this release.\n\n%s\n' \
      "$marker" "$(uname -m)" "$run" "$body" >"${RUNNER_TEMP:-/tmp}/notes-$unit.md"
    gh release edit "$tag" --prerelease --notes-file "${RUNNER_TEMP:-/tmp}/notes-$unit.md"
  fi
  exit 0
fi
[[ $# -eq 1 ]] || die "usage: $0 <plugin> [--flag]"

# Outside the repository, so no project mise.toml above it is discovered.
work=$(mktemp -d "${RUNNER_TEMP:-/tmp}/verify-$unit-XXXXXX")
gh release download "$tag" --repo "$GITHUB_REPOSITORY" \
  --pattern "$package" --pattern SHA256SUMS --dir "$work"
(cd "$work" && sha256sum --check --ignore-missing SHA256SUMS)

mkdir "$work/package"
tar -xzf "$work/$package" -C "$work/package"

export MISE_DATA_DIR="$work/data" MISE_CACHE_DIR="$work/cache" MISE_STATE_DIR="$work/state"
export MISE_GLOBAL_CONFIG_FILE="$work/global.toml"
: >"$MISE_GLOBAL_CONFIG_FILE"
cd "$work/package"
mise trust
mise install

set +e
err=$(env -u BALERIX_PLUGIN_TOKEN mise run serve 2>&1 >/dev/null)
code=$?
set -e
[[ $code -ne 0 ]] || die "$unit: the packaged plugin started without a daemon environment"
grep -q "^$unit: " <<<"$err" || die "$unit: the packaged plugin did not report '$unit: …': $err"
echo "$unit: $package installs and runs on $(uname -m)" >&2
```

- [ ] **Step 7: Write `scripts/release/promote-image.sh`** (mode 0755)

```bash
#!/usr/bin/env bash
# Points <major>.<minor> and latest at one unit's released <version> index
# (Spec I §5.8, I-9). A release flagged as a prerelease (verify-package.sh
# failed) keeps its tags where they are.
#
# usage: promote-image.sh <unit>
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <unit>"
unit=$1
image=$(unit_image "$unit")
version=$(unit_version "$unit")
tag=$(unit_tag "$unit" "$version")

prerelease=$(gh release view "$tag" --json isPrerelease --jq .isPrerelease)
if [[ $prerelease != false ]]; then
  echo "::warning::$tag is flagged as a prerelease; $image keeps its moving tags"
  exit 0
fi

docker buildx imagetools create \
  --tag "$image:${version%.*}" \
  --tag "$image:latest" \
  "$image:$version"
```

- [ ] **Step 8: Write `.github/workflows/release.yml`**

```yaml
name: release
# Releases every unit whose manifest version has no tag yet (Spec I §5).
# Merging a release PR is what makes a version untagged, so that is what
# triggers a release; every job is safe to re-run.
on:
  push:
    branches: [main]
  workflow_dispatch:
    inputs:
      dry-run:
        description: Build, test, scan and package everything; publish nothing
        type: boolean
        default: false
permissions: {}
concurrency:
  group: release
  cancel-in-progress: false
env:
  # Each job installs only the tools it uses.
  MISE_AUTO_INSTALL: "false"
jobs:
  plan:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    outputs:
      units: ${{ steps.plan.outputs.units }}
      plugins: ${{ steps.plan.outputs.plugins }}
      core: ${{ steps.plan.outputs.core }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq cargo:cargo-audit aqua:EmbarkStudios/cargo-deny
      - id: plan
        run: mise x -- scripts/release/plan.sh >>"$GITHUB_OUTPUT"
      - name: Audit the units being released
        if: steps.plan.outputs.units != '[]'
        env:
          UNITS: ${{ steps.plan.outputs.units }}
        run: |
          mapfile -t units < <(jq -r '.[]' <<<"$UNITS")
          mise x -- scripts/release/audit.sh "${units[@]}"

  build:
    needs: plan
    if: needs.plan.outputs.units != '[]'
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: read
    strategy:
      fail-fast: false
      matrix:
        unit: ${{ fromJSON(needs.plan.outputs.units) }}
        arch: [amd64, arm64]
        include:
          - arch: amd64
            target: x86_64-unknown-linux-musl
            runner: ubuntu-24.04
          - arch: arm64
            target: aarch64-unknown-linux-musl
            runner: ubuntu-24.04-arm
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq
      - id: build
        uses: ./.github/actions/build-binary
        with:
          unit: ${{ matrix.unit }}
          target: ${{ matrix.target }}
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: archive-${{ matrix.unit }}-${{ matrix.arch }}
          path: ${{ steps.build.outputs.dist }}/*.tar.gz
          if-no-files-found: error
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: binary-${{ matrix.unit }}-${{ matrix.arch }}
          path: |
            ${{ steps.build.outputs.dist }}/*
            !${{ steps.build.outputs.dist }}/*.tar.gz
          if-no-files-found: error

  images:
    needs: [plan, build]
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: read
      packages: write
    strategy:
      fail-fast: false
      matrix:
        unit: ${{ fromJSON(needs.plan.outputs.units) }}
        arch: [amd64, arm64]
        include:
          - arch: amd64
            runner: ubuntu-24.04
          - arch: arm64
            runner: ubuntu-24.04-arm
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq hadolint trivy
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          name: binary-${{ matrix.unit }}-${{ matrix.arch }}
          path: ${{ runner.temp }}/dist
      - if: ${{ !inputs.dry-run }}
        uses: docker/login-action@dbcb813823bdd20940b903addbd779551569679f # v4.6.0
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - id: image
        uses: ./.github/actions/build-image
        with:
          unit: ${{ matrix.unit }}
          dist: ${{ runner.temp }}/dist
          push: ${{ !inputs.dry-run }}
          github-token: ${{ secrets.GITHUB_TOKEN }}
      - if: ${{ !inputs.dry-run }}
        env:
          DIGEST: ${{ steps.image.outputs.digest }}
        run: |
          mkdir -p "$RUNNER_TEMP/digests"
          touch "$RUNNER_TEMP/digests/${DIGEST#sha256:}"
      - if: ${{ !inputs.dry-run }}
        uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: digest-${{ matrix.unit }}-${{ matrix.arch }}
          path: ${{ runner.temp }}/digests/*
          if-no-files-found: error

  merge-images:
    needs: [plan, images]
    if: ${{ !inputs.dry-run }}
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      packages: write
      id-token: write
      attestations: write
    strategy:
      fail-fast: false
      matrix:
        unit: ${{ fromJSON(needs.plan.outputs.units) }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq cosign
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          pattern: digest-${{ matrix.unit }}-*
          merge-multiple: true
          path: ${{ runner.temp }}/digests
      - uses: docker/login-action@dbcb813823bdd20940b903addbd779551569679f # v4.6.0
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - uses: docker/setup-buildx-action@37fe631027851001ddb9b187196cc803df7f5f0e # v4.3.0
      - id: merge
        env:
          UNIT: ${{ matrix.unit }}
        run: mise x -- scripts/release/merge-image.sh "$UNIT" "$RUNNER_TEMP/digests" >>"$GITHUB_OUTPUT"
      - name: Sign
        env:
          REF: ${{ steps.merge.outputs.image }}@${{ steps.merge.outputs.digest }}
        run: mise x -- cosign sign --yes --recursive "$REF"
      - uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2
        with:
          subject-name: ${{ steps.merge.outputs.image }}
          subject-digest: ${{ steps.merge.outputs.digest }}
          push-to-registry: true

  package:
    needs: [plan, build]
    if: needs.plan.outputs.plugins != '[]'
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    strategy:
      fail-fast: false
      matrix:
        unit: ${{ fromJSON(needs.plan.outputs.plugins) }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          pattern: archive-${{ matrix.unit }}-*
          merge-multiple: true
          path: ${{ runner.temp }}/dist
      - env:
          UNIT: ${{ matrix.unit }}
        run: mise x -- scripts/release/package.sh "$UNIT" "$RUNNER_TEMP/dist" "$RUNNER_TEMP/package"
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: package-${{ matrix.unit }}
          path: ${{ runner.temp }}/package/*.tar.gz
          if-no-files-found: error

  publish-crates:
    needs: [plan, build]
    if: needs.plan.outputs.core == 'true'
    runs-on: ubuntu-24.04
    environment: release
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq
      - if: ${{ inputs.dry-run || github.repository != 'balerix-ai/balerix' }}
        run: mise x -- scripts/release/publish-crates.sh --dry-run
      - if: ${{ !inputs.dry-run && github.repository == 'balerix-ai/balerix' }}
        id: auth
        uses: rust-lang/crates-io-auth-action@c6f97d42243bad5fab37ca0427f495c86d5b1a18 # v1.0.5
      - if: ${{ !inputs.dry-run && github.repository == 'balerix-ai/balerix' }}
        env:
          CARGO_REGISTRY_TOKEN: ${{ steps.auth.outputs.token }}
        run: mise x -- scripts/release/publish-crates.sh

  github-release:
    needs: [plan, build, merge-images, package, publish-crates]
    if: >-
      ${{ always() && !inputs.dry-run && needs.plan.outputs.units != '[]'
      && needs.build.result == 'success' && needs.merge-images.result == 'success'
      && !contains(needs.*.result, 'failure') && !contains(needs.*.result, 'cancelled') }}
    runs-on: ubuntu-24.04
    permissions:
      contents: write
      id-token: write
      attestations: write
    strategy:
      fail-fast: false
      matrix:
        unit: ${{ fromJSON(needs.plan.outputs.units) }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq gh
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          pattern: archive-${{ matrix.unit }}-*
          merge-multiple: true
          path: ${{ runner.temp }}/assets
      - if: matrix.unit != 'core'
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          name: package-${{ matrix.unit }}
          path: ${{ runner.temp }}/assets
      - name: Checksums
        working-directory: ${{ runner.temp }}/assets
        run: sha256sum -- *.tar.gz >SHA256SUMS
      - uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2
        with:
          subject-checksums: ${{ runner.temp }}/assets/SHA256SUMS
      - env:
          UNIT: ${{ matrix.unit }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: mise x -- scripts/release/github-release.sh "$UNIT" "$RUNNER_TEMP/assets"

  verify-package:
    needs: [plan, github-release]
    if: ${{ always() && needs.github-release.result == 'success' && needs.plan.outputs.plugins != '[]' }}
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: write
    strategy:
      fail-fast: false
      matrix:
        unit: ${{ fromJSON(needs.plan.outputs.plugins) }}
        runner: [ubuntu-24.04, ubuntu-24.04-arm]
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq gh
      - env:
          UNIT: ${{ matrix.unit }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: mise x -- scripts/release/verify-package.sh "$UNIT"
      - if: failure()
        env:
          UNIT: ${{ matrix.unit }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: mise x -- scripts/release/verify-package.sh "$UNIT" --flag

  promote-images:
    needs: [plan, github-release, verify-package]
    if: ${{ always() && needs.github-release.result == 'success' }}
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      packages: write
    strategy:
      fail-fast: false
      matrix:
        unit: ${{ fromJSON(needs.plan.outputs.units) }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install rust jq gh
      - uses: docker/login-action@dbcb813823bdd20940b903addbd779551569679f # v4.6.0
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - uses: docker/setup-buildx-action@37fe631027851001ddb9b187196cc803df7f5f0e # v4.3.0
      - env:
          UNIT: ${{ matrix.unit }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: mise x -- scripts/release/promote-image.sh "$UNIT"
```

- [ ] **Step 9: Verify and commit**

Run: `mise run lint > target/tmp/task8-lint.log 2>&1; echo "exit=$?"`
Expected: `exit=0` — shellcheck over all seventeen scripts, actionlint and zizmor over every workflow and action.

Run: `mise run release-test 2>&1 | tail -1`
Expected: `release scripts: all checks passed`.

```bash
git add scripts/release/plan.sh scripts/release/audit.sh scripts/release/publish-crates.sh \
  scripts/release/merge-image.sh scripts/release/github-release.sh scripts/release/verify-package.sh \
  scripts/release/promote-image.sh .github/workflows/release.yml
git commit -m "ci(release): release untagged units on push to main

release.yml audits, builds and smoke-tests every untagged unit on both
architectures, pushes and signs its images, publishes the api and SDK,
packages plugins, and publishes the GitHub Release last, which creates the
tag. Plugin packages are then installed through mise on both
architectures before latest moves (Spec I §5-§9).

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 9: Documentation and the threat model

**Files:**
- Create: `docs/RELEASING.md`
- Modify: `README.md`, `AGENTS.md`, `docs/THREAT-MODEL.md`, `docs/superpowers/specs/2026-09-11-release-pipeline-design.md`

- [ ] **Step 1: Write `docs/RELEASING.md`**

````markdown
# Releasing

Design: `docs/superpowers/specs/2026-09-11-release-pipeline-design.md`
(Spec I). This file is the operator's view.

## Release units

| Unit | Tag | Ships |
|---|---|---|
| core | `balerix-v<ver>` | `balerix` binaries, `ghcr.io/balerix-ai/balerix`, `balerix-api` and `balerix-plugin-sdk` on crates.io |
| flow | `balerix-plugin-flow-v<ver>` | binaries, `ghcr.io/balerix-ai/balerix-plugin-flow`, release package |
| web | `balerix-plugin-web-v<ver>` | binaries, `ghcr.io/balerix-ai/balerix-plugin-web`, release package |
| matrix | `balerix-plugin-matrix-v<ver>` | binaries, `ghcr.io/balerix-ai/balerix-plugin-matrix`, release package |

Binaries are static musl builds for Linux x86_64 and aarch64.

## Cutting a release

1. Merge pull requests with Conventional Commit titles. `feat`, `fix`,
   `perf`, `refactor` and `build` are releasable; `docs`, `test`, `ci`,
   `chore`, `style` and `revert` are not. A `!` (`feat!:`) or a
   `BREAKING CHANGE:` footer is breaking.
2. `release-pr.yml` keeps a PR named `chore(release): <crate> v<ver>` open
   for each unit with releasable changes, on branch `release/<unit>`. It is
   rebuilt from `main` on every push.
3. Merge that PR when you want the release. `release.yml` sees an untagged
   version and releases it; the tag appears when the GitHub Release is
   published, at the end.

Bump rules follow cargo semver: in `0.x` a breaking change bumps minor and
anything else bumps patch; from `1.0` a breaking change bumps major, `feat`
minor and `fix` patch.

## Forcing a version

Run the `release-pr` workflow by hand with `unit` and `version` (for
example `1.0.0`). The PR gets that exact version and the `release:pinned`
label, and pushes to `main` stop updating it. Dispatch again to refresh it,
or remove the label to go back to computed versions.

To see what CI would propose, locally: `mise run release-prepare <unit>`
(then `git checkout -- . && git clean -fd CHANGELOG.md plugins/*/CHANGELOG.md`
to throw the edits away).

## Dry run

Run the `release` workflow by hand with `dry-run` checked. It plans, audits,
builds, smoke-tests, packages, builds and scans images, and runs
`cargo publish --dry-run`, then uploads every artifact to the run. Nothing is
pushed, published or tagged.

## Recovery

Every job is idempotent and the tag is created last, so re-running
`release.yml` (Re-run failed jobs, or `workflow_dispatch`) finishes a partial
release.

| Fails at | State left | Do |
|---|---|---|
| audit, build, smoke test, image build or scan | nothing public | Fix on `main` through a normal PR; the version is still untagged, so that merge releases it. |
| after `publish-crates` | crates live, no tag | Re-run; publishing skips versions crates.io already has. |
| after an image push | an unannounced `<ver>` image tag | Re-run; the tag is overwritten and `latest` has not moved. |
| `verify-package` | the release is published and flagged as a prerelease | Fix it; the next patch release follows the normal flow. |
| a bad release shipped | — | Roll forward with a patch release. `cargo yank` a harmful crate version. Never delete or move a tag. |

While a merged release PR waits for its tag, `release-pr.yml` reports
`release in progress` for that unit and leaves it alone.

## Verifying what you downloaded

```sh
gh attestation verify balerix-v0.2.0-x86_64-unknown-linux-musl.tar.gz --repo balerix-ai/balerix
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify oci://ghcr.io/balerix-ai/balerix:0.2.0 --repo balerix-ai/balerix
cosign verify ghcr.io/balerix-ai/balerix:0.2.0 \
  --certificate-identity https://github.com/balerix-ai/balerix/.github/workflows/release.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

A plugin release package pins each architecture's archive by checksum, and
mise checks the archives' GitHub attestations when the daemon installs it.

## One-time setup

Nothing releases until all of this is done.

1. **GitHub App** `balerix-release`, installed on this repository only, with
   repository permissions *Contents: read and write*, *Pull requests: read
   and write* and *Issues: read and write*. Store its App ID as the variable
   `RELEASE_APP_ID` and a private key as the secret
   `RELEASE_APP_PRIVATE_KEY`, both in environment `release-bot`.
2. **Environments** `release-bot` and `release`, each with deployment
   branches limited to `main`.
3. **Branch protection** on `main`: require `check`, `plugins` (all three
   legs) and `conventional` (pr-title) to pass, and require branches to be
   up to date before merging.
4. **Merge settings:** allow squash merging only, with *Default commit
   message: Pull request title*.
5. **Fork rehearsal:** on a fork, do steps 1–4, merge one plugin's release
   PR and watch `release.yml` publish, sign, verify and promote. Crates are
   never published from a fork.
6. **First releases**, in order: flow, then web, then matrix.
7. **crates.io bootstrap** before merging core's first release PR: from that
   PR's head commit, `mise x -- cargo publish --locked -p balerix-api -p
   balerix-plugin-sdk` with a personal API token; then on crates.io add a
   trusted publisher for each crate: repository `balerix-ai/balerix`,
   workflow `release.yml`, environment `release`. Revoke the token. Merge the
   PR; `publish-crates` will skip the versions that already exist.
8. **ghcr visibility:** after each image's first push, set its package to
   public (ghcr creates packages private).

## Pins that move together

- `docker/balerix/Dockerfile` pins mise by version and by the two sha256s
  from that release's `SHASUMS256.txt`; update all three at once.
- Base images are pinned by digest and action references by commit SHA;
  Renovate proposes both.
````

- [ ] **Step 2: Add an Install section to `README.md`**

Insert before `## Quickstart`:

```markdown
## Install
Releases are on the [releases page](https://github.com/balerix-ai/balerix/releases),
one tag line per component (`balerix-v…`, `balerix-plugin-<name>-v…`); every
asset is attested (`gh attestation verify <file> --repo balerix-ai/balerix`).
`docs/RELEASING.md` covers verification and the release process.

- **CLI and daemon:** download `balerix-v<ver>-<arch>-unknown-linux-musl.tar.gz`
  (static; x86_64 or aarch64) and put `balerix` on `PATH`. `serve` also needs
  `git`, `gh`, `mise`, `nono` and `tmux` on `PATH` and a Landlock kernel (5.13+).
- **Container:** `docker run -d --name balerix -v balerix:/home/balerix
  ghcr.io/balerix-ai/balerix:<ver>` runs the daemon with its tools. It listens on
  loopback inside the container only, so run the client there too:
  `docker exec balerix balerix up fleet.yaml`. Landlock must be allowed by the
  container's seccomp profile.
- **Plugins:** download `balerix-plugin-<name>-v<ver>-package.tar.gz` next to
  `$XDG_CONFIG_HOME/balerix/plugins.yaml` and add the entry printed in that
  release's notes (`source: ./<file>` plus its `sha256`). The daemon installs the
  plugin binary through mise for the host's architecture.
- **Plugin SDK:** `cargo add balerix-plugin-sdk`.

```

- [ ] **Step 3: Update `AGENTS.md`**

Under `## Tasks (`mise run <task>`)`, after the `vendor-xterm` bullet, add:

```markdown
- `release-prepare <unit> [version]` — what `release-pr.yml` runs: works
  out one release unit's next version (core, flow, web, matrix) and writes
  it into the manifests, lockfiles, plugin manifest and changelog. Commits
  nothing. `docs/RELEASING.md` is the release process.
- `release-test` — scenario tests for `scripts/release/` against throwaway
  clones under `target/tmp`; CI runs it when the scripts change.
```

At the end of `## Gotchas`, add:

```markdown
- PR titles are Conventional Commits (`pr-title.yml`) and PRs are
  squash-merged, so the title is the commit the release scripts read.
  `feat`/`fix`/`perf`/`refactor`/`build` release the units whose paths the
  change touches; `docs`/`test`/`ci`/`chore`/`style`/`revert` never do. A
  change under `crates/balerix-api/` or `crates/balerix-plugin-sdk/` counts
  for core and for every plugin.
- Don't bump a version by hand. A plugin's `package/balerix-plugin.yaml`
  `version` is written from its `Cargo.toml` by
  `scripts/release/prepare.sh`, and `mise run plugin <name>` fails when the
  two differ. A version on `main` with no matching tag
  (`<crate>-v<version>`) means "release pending": `release.yml` releases it
  on the next push and `prepare.sh` answers `in-progress` for it.
- Released `CHANGELOG.md` sections are read back by
  `scripts/release/notes.sh` for the GitHub Release; don't edit them by
  hand.
- `scripts/release/test.sh` clones the repository instead of using `git
  worktree`: a worktree shares this repository's tags, and the scenarios
  create and delete tags.
- The release tools in `mise.toml` (git-cliff, cargo-edit, cosign, trivy,
  hadolint, zizmor, actionlint, shellcheck, jq) never reach agents: the
  embedded table is filtered to `claude` and `gh`
  (`balerix-runtime/src/toolchain.rs` `INHERITED`).
- `docker/balerix/Dockerfile` must not leave a mise config in the final
  image: a system `/etc/mise/config.toml` would enter every agent's and
  plugin's tool resolution. The `mise.toml` it installs from stays in the
  `tools` build stage.
- `release.yml` builds and tests each binary on a runner of its own
  architecture (`ubuntu-24.04-arm` for aarch64) because the smoke tests run
  the binary. On an arm64 host cc-rs wants `aarch64-linux-musl-gcc`, which
  `musl-tools` lacks; `scripts/release/build.sh` sets
  `CC_aarch64_unknown_linux_musl=musl-gcc` for matrix's C dependencies.
- zizmor runs with `--min-severity medium`; release workflows keep
  `cache: false` on mise-action (a cache restored into a job that publishes
  is a poisoning path).
```

- [ ] **Step 4: Add the release pipeline to `docs/THREAT-MODEL.md`**

Append at the end of the file:

```markdown

## Release pipeline (Spec I)

### Assets
- **The crates.io publish right** for `balerix-api` and `balerix-plugin-sdk` — anything published there lands in plugin authors' builds.
- **The ghcr namespace** `ghcr.io/balerix-ai/*` — images operators run with their credentials mounted.
- **Tags and release assets** — the binaries and plugin packages operators and the daemon's mise install.
- **The release App's private key** — can push branches and edit pull requests on this repository.

### Trust boundaries
- **Pull request ↔ CI** — a pull request's code runs in `ci.yml`, `images.yml`, `release-scripts.yml` and `pr-title.yml` with a read-only token and no secrets. **Untrusted input.**
- **`main` ↔ release workflows** — `release-pr.yml` and `release.yml` run only on push to `main` or dispatch, and hold the App token, OIDC and write permissions.
- **Build job ↔ publishing jobs** — build and image jobs hand artifacts to jobs that sign, publish and tag.
- **CI ↔ registries** — crates.io through trusted publishing (OIDC, 30-minute token), ghcr through `GITHUB_TOKEN`, GitHub Releases through `GITHUB_TOKEN`.

### Adversaries
- **A malicious pull request** — wants secrets, a poisoned cache or artifact that a release later picks up, or a workflow run with write permissions.
- **A compromised dependency or action** — code in a `build.rs`, a crate, a pinned tool or a third-party action running in a release job.
- **A leaked App key or registry token.**

### Out of scope / accepted risks
- **A compromise of GitHub, crates.io or ghcr themselves.**
- **A maintainer account takeover** — a maintainer can merge a release PR; branch protection and reviews are the control, not this pipeline.
- **A malicious dependency that passes `cargo audit` and `cargo deny`** is built into the release: the build jobs have no secrets or OIDC, but their output is what gets signed. Attestations say where an artifact was built, not that its inputs were benign.

### Mitigations
| Threat | Control | Where |
|---|---|---|
| A pull request reaching secrets or write tokens | no `pull_request_target`; PR workflows have read-only tokens and no secrets; release workflows trigger only on `main` or dispatch; the App key and the crates.io trusted publisher are bound to `main`-only environments (`release-bot`, `release`) | `.github/workflows/*.yml` |
| Over-broad job permissions | `permissions: {}` per workflow, least grant per job; build jobs hold no secrets and no `id-token`; `persist-credentials: false` on every checkout; the App token requests only `contents`, `pull-requests`, `issues` | `.github/workflows/release.yml`, `release-pr.yml` |
| A mutable action or tool changing under us | actions pinned by commit SHA, base images by digest, every tool exact in `mise.toml`, mise in the image checked by sha256; Renovate proposes bumps as reviewable PRs | `mise.toml`, `docker/*/Dockerfile`, `renovate.json` |
| Cache poisoning into a release | `release.yml` restores no caches; GitHub scopes caches written by pull requests to their own ref | `.github/workflows/release.yml` |
| Workflow injection and misconfiguration | `zizmor` and `actionlint` in `mise run lint`; untrusted values reach `run:` only through `env:` | `mise.toml` |
| Shipping a known advisory | `cargo audit` and `cargo deny` gate every unit before it builds | `scripts/release/audit.sh` |
| Vulnerable image contents | `hadolint`, and `trivy image` failing on fixed CRITICAL/HIGH, before any push and nightly | `.github/actions/build-image/action.yml`, `images.yml` |
| A tampered or substituted artifact | `SHA256SUMS` and GitHub build-provenance attestations on every archive and package; cosign keyless signatures and attestations on every image index; the plugin package pins each archive's sha256 and mise verifies attestations at install | `release.yml`, `scripts/release/package.sh` |
| A leaked long-lived registry token | none exist: crates.io trusted publishing, `GITHUB_TOKEN` for ghcr and releases; the one bootstrap API token is revoked after the first publish (`docs/RELEASING.md`) | `release.yml` |
| A half-finished release looking complete | the tag is created last; `latest` image tags move only after the release and its package verification; a failed package verification flags the release as a prerelease | `scripts/release/github-release.sh`, `verify-package.sh`, `promote-image.sh` |
```

- [ ] **Step 5: Record the deviations in the spec**

In `docs/superpowers/specs/2026-09-11-release-pipeline-design.md`:
- §4.2: after "…pushes with a **GitHub App** installation token", add the sentence: "The token is scoped to `contents`, `pull-requests` and `issues` (labels are an issues API)."
- §6.3: replace "One Dockerfile, `ARG PLUGIN`." with "One Dockerfile; the build context holds the binary under the fixed name `plugin` (an exec-form `ENTRYPOINT` cannot expand an `ARG`)."
- §9.2 **Scanners** bullet: replace "`zizmor` and `actionlint` join `mise run lint`." with "`zizmor` (`--min-severity medium`), `actionlint` and `shellcheck` (over `scripts/release/`) join `mise run lint`."
- §10 first bullet: replace "creates a `git worktree` of `HEAD` under `target/tmp`" with "clones `HEAD` under `target/tmp` (a worktree would share this repository's tags)".
- §5.6: append "All units in one run wait for every unit's builds and images; a re-run releases whatever is still untagged."
- Change `**Status:**` to `Implemented 2026-09-11 (plan: docs/superpowers/plans/2026-09-11-release-pipeline.md)`.

- [ ] **Step 6: Commit**

```bash
git add docs/RELEASING.md README.md AGENTS.md docs/THREAT-MODEL.md docs/superpowers/specs/2026-09-11-release-pipeline-design.md
git commit -m "docs: the release process, install paths and the release threat model

docs/RELEASING.md carries the one-time setup, cutting and forcing releases,
recovery and verification. README gains an install section, AGENTS.md the
release tasks and gotchas, the threat model a release pipeline section,
and the spec the deviations the implementation made.

Claude-Session: https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t"
```

---

### Task 10: Verification

- [ ] **Step 1: The full local gate**

Run each, redirecting output under `target/tmp/`, and read the logs:
- `mise run lint` → exit 0
- `mise run release-test` → `release scripts: all checks passed`
- `mise run plugins` → exit 0
- `mise run check` → exit 0

- [ ] **Step 2: Scope check against the spec**

Walk spec §14's acceptance criteria and §3–§11 and confirm each is covered by a file from the File Structure table. Report anything missing instead of claiming completion.

- [ ] **Step 3: CI on GitHub (ask the user first)**

`images.yml`, `release-scripts.yml` and `pr-title.yml` only run on GitHub. Ask the user whether to push the branch and open a **draft** PR titled `ci: release pipeline (Spec I)`. With their yes:

```bash
git push -u origin release-pipeline
gh pr create --draft --base main --title "ci: release pipeline (Spec I)" --body-file target/tmp/pr-body.md
```

where `target/tmp/pr-body.md` summarises the change and ends with the line `https://claude.ai/code/session_01Qjnjmywx95u61XMtTtpF6t`. Expected: `check`, `plugins`, `release-scripts`, `pr-title` and all eight `images` legs pass. `release.yml` and `release-pr.yml` do not run on pull requests.

- [ ] **Step 4: Hand over the one-time setup**

Tell the user the release pipeline cannot run until the `docs/RELEASING.md` "One-time setup" checklist is done (GitHub App, environments, branch protection, merge settings, fork rehearsal, first crates.io publish), and that nothing has been released.
