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

# The version a `{ version = "…" }` dependency names in <manifest>.
dep_version_of() {
  (
    cd "$1"
    # shellcheck source=scripts/release/lib.sh
    source scripts/release/lib.sh
    dep_version "$2" "$3"
  )
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
  expect_eq "$label: common Cargo.lock has the SDK at $core" \
    "$(lock_version "$dir/plugins/common/Cargo.lock" balerix-plugin-sdk)" "$core"
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

plan() {
  "$1/scripts/release/plan.sh" 2>>"$log"
}

# plan.sh releases a unit only once its release PR is merged (Spec I §5.1).
scenario_plan() {
  local dir out tag
  dir=$(fixture plan)
  out=$(plan "$dir")
  expect_eq "plan, nothing proposed: units" "$(field units "$out")" '[]'
  expect_eq "plan, nothing proposed: plugins" "$(field plugins "$out")" '[]'
  expect_eq "plan, nothing proposed: core" "$(field core "$out")" false

  tag=$(field tag "$(prepare "$dir" flow)")
  gitc "$dir" add -A
  gitc "$dir" commit -qm "chore(release): flow initial"
  out=$(plan "$dir")
  expect_eq "plan, merged initial flow PR: units" "$(field units "$out")" '["flow"]'
  expect_eq "plan, merged initial flow PR: plugins" "$(field plugins "$out")" '["flow"]'
  expect_eq "plan, merged initial flow PR: core" "$(field core "$out")" false

  gitc "$dir" tag "$tag"
  out=$(plan "$dir")
  expect_eq "plan, flow tagged: units" "$(field units "$out")" '[]'
  expect_eq "plan, flow tagged: plugins" "$(field plugins "$out")" '[]'
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

scenario_forced_in_progress() {
  local dir out
  dir=$(fixture forced-in-progress)
  release "$dir" flow 0.4.0
  change "$dir" plugins/flow/release-test.txt "fix: a flow fix"
  prepare "$dir" flow >/dev/null
  gitc "$dir" commit -qam "chore(release): flow v0.4.1"
  out=$(prepare "$dir" flow 0.5.0)
  expect_eq "forced, in progress: status" "$(field status "$out")" in-progress
  expect_eq "forced, in progress: no file changed" "$(git -C "$dir" status --porcelain)" ""
}

scenario_forced_below_last() {
  local dir
  dir=$(fixture forced-below)
  release "$dir" flow 0.4.0
  if prepare "$dir" flow 0.3.0 >/dev/null; then
    fail "forcing a version below the last release succeeded"
  else
    pass "forcing a version below the last release is refused"
  fi
}

scenario_hand_bump() {
  local dir errfile out
  dir=$(fixture hand-bump)
  release "$dir" flow 0.4.0
  cargo set-version --manifest-path "$dir/plugins/flow/Cargo.toml" 0.5.0 >>"$log" 2>&1
  gitc "$dir" add -A
  gitc "$dir" commit -q -m "chore: hand-bump flow to 0.5.0"

  errfile="$root/hand-bump.stderr"
  if "$dir/scripts/release/prepare.sh" flow >/dev/null 2>"$errfile"; then
    fail "hand bump: prepare succeeded for a version changed outside a release PR"
  else
    pass "hand bump: prepare refuses a version changed outside a release PR"
  fi
  cat "$errfile" >>"$log"
  expect_grep "hand bump: error names the version" "flow: 0.5.0" "$errfile"
  expect_eq "hand bump: no file changed" "$(git -C "$dir" status --porcelain)" ""

  out=$(prepare "$dir" flow 0.5.0)
  expect_eq "hand bump, forced: status" "$(field status "$out")" release
  expect_eq "hand bump, forced: version" "$(field version "$out")" 0.5.0
  assert_consistent "$dir" "hand bump, forced"
  expect_grep "hand bump, forced: changelog section" "## 0.5.0 - " "$dir/plugins/flow/CHANGELOG.md"
  expect_eq "hand bump, forced: manifest untouched, only yaml and changelog written" \
    "$(git -C "$dir" status --porcelain | awk '{print $2}' | sort)" \
    "$(printf '%s\n' plugins/flow/CHANGELOG.md plugins/flow/package/balerix-plugin.yaml | sort)"
  gitc "$dir" add -A
  gitc "$dir" commit -q -m "chore(release): flow v0.5.0"
  out=$(plan "$dir")
  expect_eq "hand bump, forced: plan proposes flow" "$(field units "$out")" '["flow"]'
}

scenario_hand_bump_core() {
  local dir errfile out
  dir=$(fixture hand-bump-core)
  release "$dir" core 0.4.0
  (cd "$dir" && cargo set-version --workspace 0.5.0) >>"$log" 2>&1
  gitc "$dir" add -A
  gitc "$dir" commit -q -m "chore: hand-bump core to 0.5.0"

  errfile="$root/hand-bump-core.stderr"
  if "$dir/scripts/release/prepare.sh" core >/dev/null 2>"$errfile"; then
    fail "hand bump core: prepare succeeded for a version changed outside a release PR"
  else
    pass "hand bump core: prepare refuses a version changed outside a release PR"
  fi
  cat "$errfile" >>"$log"
  expect_grep "hand bump core: error names the version" "core: 0.5.0" "$errfile"
  expect_eq "hand bump core: no file changed" "$(git -C "$dir" status --porcelain)" ""

  # The hand-bump above only touched the core manifest and its own
  # Cargo.lock; every plugin's Cargo.lock still locks the SDK at 0.4.0
  # until prepare.sh refreshes it (Spec I §10).
  out=$(prepare "$dir" core 0.5.0)
  expect_eq "hand bump core, forced: status" "$(field status "$out")" release
  expect_eq "hand bump core, forced: version" "$(field version "$out")" 0.5.0
  assert_consistent "$dir" "hand bump core, forced"
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

scenario_package() {
  local dir="$root/package" version result pkg sha_x64 sha_arm64
  version=$(manifest_version "$repo" flow)
  mkdir -p "$dir/dist" "$dir/unpacked"
  echo x64 >"$dir/dist/balerix-plugin-flow-v$version-x86_64-unknown-linux-musl.tar.gz"
  echo arm64 >"$dir/dist/balerix-plugin-flow-v$version-aarch64-unknown-linux-musl.tar.gz"
  sha_x64=$(sha256sum "$dir/dist/balerix-plugin-flow-v$version-x86_64-unknown-linux-musl.tar.gz" | cut -d' ' -f1)
  sha_arm64=$(sha256sum "$dir/dist/balerix-plugin-flow-v$version-aarch64-unknown-linux-musl.tar.gz" | cut -d' ' -f1)
  result=$(GITHUB_REPOSITORY=example/fork "$repo/scripts/release/package.sh" flow "$dir/dist" "$dir/out" 2>>"$log")
  pkg=$(field package "$result")
  expect_eq "package: file name" "$(basename "$pkg")" "balerix-plugin-flow-v$version-package.tar.gz"
  expect_eq "package: sha256" "$(field sha256 "$result")" "$(sha256sum "$pkg" | cut -d' ' -f1)"
  tar -xzf "$pkg" -C "$dir/unpacked"
  expect_grep "package: repository" '[tools."github:example/fork"]' "$dir/unpacked/mise.toml"
  expect_grep "package: full-tag version" "version = \"balerix-plugin-flow-v$version\"" "$dir/unpacked/mise.toml"
  if grep -q '^version_prefix' "$dir/unpacked/mise.toml"; then
    fail "package: mise.toml still sets version_prefix"
  else
    pass "package: no version_prefix"
  fi
  expect_grep "package: x64 checksum" "checksum = \"sha256:$sha_x64\"" "$dir/unpacked/mise.toml"
  expect_grep "package: arm64 checksum" "checksum = \"sha256:$sha_arm64\"" "$dir/unpacked/mise.toml"
  expect_grep "package: start task" 'run = "balerix-plugin-flow"' "$dir/unpacked/mise.toml"
  expect_eq "package: manifest version" \
    "$(sed -n 's/^version: //p' "$dir/unpacked/balerix-plugin.yaml")" "$version"
  if (cd "$dir/unpacked" && MISE_TRUSTED_CONFIG_PATHS="$PWD" mise tasks ls 2>>"$log" | grep -q '^serve'); then
    pass "package: mise parses mise.toml"
  else
    fail "package: mise cannot read the rendered mise.toml"
  fi
}

# Each unit's image carries its own name and description, not the repository's.
scenario_image_context() {
  local dir="$root/image-context" out
  mkdir -p "$dir/dist"
  touch "$dir/dist/balerix" "$dir/dist/balerix-plugin-flow"
  out=$(GITHUB_REPOSITORY_OWNER=Example "$repo/scripts/release/image-context.sh" flow "$dir/dist" "$dir/flow" 2>>"$log")
  expect_eq "image context: flow image" "$(field image "$out")" ghcr.io/example/balerix-plugin-flow
  expect_eq "image context: flow title" "$(field title "$out")" balerix-plugin-flow
  expect_eq "image context: flow description drops the spec reference" "$(field description "$out")" \
    "The flow plugin: a per-agent state machine over hook events"
  out=$("$repo/scripts/release/image-context.sh" core "$dir/dist" "$dir/core" 2>>"$log")
  expect_eq "image context: core title" "$(field title "$out")" balerix
  expect_eq "image context: core description" "$(field description "$out")" \
    "Control plane and orchestrator for fleets of coding agents"
}

affected() {
  "$1/scripts/release/affected-units.sh" "${@:2}" 2>>"$log"
}

# Commits a change to <path> and prints the units affected-units.sh names
# for it, against the commit before it.
affected_by() {
  local dir=$1 path=$2 base
  base=$(gitc "$dir" rev-parse HEAD)
  change "$dir" "$path" "test: change $path"
  field units "$(affected "$dir" "$base" HEAD)"
}

# affected-units.sh names the units whose image a change can break (Spec I
# §6.5): a unit's own include paths select it, what every image shares
# selects all of them, and anything else selects none.
scenario_affected() {
  local dir
  dir=$(fixture affected)
  expect_eq "affected: a matrix source change" \
    "$(affected_by "$dir" plugins/matrix/src/release-test.rs)" '["matrix"]'
  expect_eq "affected: a core crate change" \
    "$(affected_by "$dir" crates/balerix-server/src/release-test.rs)" '["core"]'
  expect_eq "affected: an sdk change reaches every plugin" \
    "$(affected_by "$dir" crates/balerix-plugin-sdk/src/release-test.rs)" '["core","flow","web","matrix"]'
  expect_eq "affected: the root lockfile is core" \
    "$(affected_by "$dir" Cargo.lock)" '["core"]'
  expect_eq "affected: a plugin lockfile is that plugin" \
    "$(affected_by "$dir" plugins/web/Cargo.lock)" '["web"]'
  expect_eq "affected: a Dockerfile is every unit" \
    "$(affected_by "$dir" docker/plugin/release-test.txt)" '["core","flow","web","matrix"]'
  expect_eq "affected: the tool pins are every unit" \
    "$(affected_by "$dir" mise.toml)" '["core","flow","web","matrix"]'
  expect_eq "affected: the scan exceptions are every unit" \
    "$(affected_by "$dir" .trivyignore.yaml)" '["core","flow","web","matrix"]'
  expect_eq "affected: the workflow itself is every unit" \
    "$(affected_by "$dir" .github/workflows/images.yml)" '["core","flow","web","matrix"]'
  expect_eq "affected: docs are no unit" \
    "$(affected_by "$dir" docs/release-test.md)" '[]'
  expect_eq "affected: another workflow is no unit" \
    "$(affected_by "$dir" .github/workflows/release-test.yml)" '[]'
  expect_eq "affected: no range is every unit (the nightly)" \
    "$(field units "$(affected "$dir")")" '["core","flow","web","matrix"]'
  expect_eq "affected: a common change reaches the plugins built on it" \
    "$(affected_by "$dir" plugins/common/src/release-test.rs)" '["web","matrix"]'
}

# A library unit releases like a plugin but ships crates, not a binary:
# prepare.sh on a library that must wait for core, stdout and stderr
# apart: the status on stdout, the reason on stderr.
prepare_refused() {
  local dir=$1 label=$2 out err needle
  err="$root/${label// /-}.err"
  # Direct, not through the prepare() helper: that helper always sends
  # stderr to $log, so a redirect at the call site cannot recapture it.
  out=$("$dir/scripts/release/prepare.sh" common 2>"$err")
  cat "$err" >>"$log"
  expect_eq "$label: status" "$(field status "$out")" none
  for needle in "${@:3}"; do
    expect_grep "$label: says why" "$needle" "$err"
  done
  expect_eq "$label: working tree untouched" "$(git -C "$dir" status --porcelain)" ""
}

# common is proposed only once the SDK version its manifest names is
# tagged, and a core release moves that version.
scenario_common_ordering() {
  local dir out sdk
  dir=$(fixture common)
  sdk=$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-plugin-sdk)
  prepare_refused "$dir" "common before core" "no tag balerix-v$sdk yet" "release core $sdk first, then common"
  release "$dir" core "$sdk"
  out=$(prepare "$dir" common)
  expect_eq "common after core: status" "$(field status "$out")" release
  expect_eq "common after core: tag" "$(field tag "$out")" "balerix-plugin-common-v$(manifest_version "$dir" common)"
  discard "$dir"
}

# The SDK tag alone is not enough: common compiles against the core crates
# at HEAD, so a change to either since that tag waits for the next core
# release (Spec K §5).
scenario_common_waits_for_core_changes() {
  local dir out
  dir=$(fixture common-core-changes)
  release "$dir" core 0.4.0
  release "$dir" common 0.4.0
  change "$dir" crates/balerix-plugin-sdk/release-test.txt "feat(sdk): a new host call"
  prepare_refused "$dir" "sdk changed since balerix-v0.4.0" "changed since balerix-v0.4.0; release core first"
  release "$dir" core 0.5.0
  out=$(prepare "$dir" common)
  expect_eq "after core 0.5.0: common status" "$(field status "$out")" release
  expect_eq "after core 0.5.0: common's manifest names the new SDK" \
    "$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-plugin-sdk)" 0.5.0
  discard "$dir"
}

scenario_core_bump_moves_common() {
  local dir next
  dir=$(fixture common-core)
  release "$dir" core 0.4.0
  release "$dir" common 0.4.0
  change "$dir" crates/balerix-server/release-test.txt "feat!: a breaking daemon change"
  next=$(field version "$(prepare "$dir" core)")
  expect_eq "core bump: version" "$next" 0.5.0
  expect_eq "core bump: common's manifest names the new SDK" \
    "$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-plugin-sdk)" "$next"
  expect_eq "core bump: common's manifest names the new API" \
    "$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-api)" "$next"
  expect_eq "core bump: common's lockfile has the SDK at $next" \
    "$(lock_version "$dir/plugins/common/Cargo.lock" balerix-plugin-sdk)" "$next"
}

scenario_common_change_releases_dependents() {
  local dir unit out version
  dir=$(fixture common-dependents)
  release "$dir" core 0.4.0
  for unit in common flow web matrix; do release "$dir" "$unit" 0.4.0; done
  change "$dir" plugins/common/src/release-test.rs "fix(common): a shared fix"
  out=$(prepare "$dir" common)
  expect_eq "common change: common releases" "$(field status "$out")" release
  version=$(field version "$out")
  expect_eq "common change: common's manifest at $version" "$(manifest_version "$dir" common)" "$version"
  for unit in matrix web; do
    expect_eq "common change: $unit Cargo.lock has common at $version" \
      "$(lock_version "$dir/plugins/$unit/Cargo.lock" balerix-plugin-common)" "$version"
  done
  discard "$dir"
  expect_eq "common change: matrix releases" "$(field status "$(prepare "$dir" matrix)")" release
  discard "$dir"
  expect_eq "common change: web releases" "$(field status "$(prepare "$dir" web)")" release
  discard "$dir"
  expect_eq "common change: flow does not" "$(field status "$(prepare "$dir" flow)")" none
  expect_eq "common change: core does not" "$(field status "$(prepare "$dir" core)")" none
}

scenario_plan_crates() {
  local dir out
  dir=$(fixture plan-crates)
  release "$dir" core "$(dep_version_of "$dir" plugins/common/Cargo.toml balerix-plugin-sdk)"
  prepare "$dir" common >/dev/null
  gitc "$dir" add -A
  gitc "$dir" commit -qm "chore(release): common initial"
  out=$(plan "$dir")
  expect_eq "plan, merged common PR: units" "$(field units "$out")" '["common"]'
  expect_eq "plan, merged common PR: plugins" "$(field plugins "$out")" '[]'
  expect_eq "plan, merged common PR: binaries" "$(field binaries "$out")" '[]'
  expect_eq "plan, merged common PR: crates" "$(field crates "$out")" '["common"]'
  expect_eq "plan, merged common PR: core" "$(field core "$out")" false
}

scenario_initial
scenario_bumps_0x
scenario_bumps_1x
scenario_skipped_types
scenario_sdk_change
scenario_plugin_only
scenario_core_bump
scenario_in_progress
scenario_plan
scenario_forced_released
scenario_forced_in_progress
scenario_forced_below_last
scenario_hand_bump
scenario_hand_bump_core
scenario_notes
scenario_package
scenario_image_context
scenario_affected
scenario_common_ordering
scenario_common_waits_for_core_changes
scenario_core_bump_moves_common
scenario_common_change_releases_dependents
scenario_plan_crates

if ((failures)); then
  echo "$failures check(s) failed; fixtures and $log kept" >&2
  exit 1
fi
rm -rf "$root"
echo "release scripts: all checks passed"
