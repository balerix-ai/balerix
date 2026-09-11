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
  expect_grep "package: full-tag version" "version = \"balerix-plugin-flow-v$version\"" "$dir/unpacked/mise.toml"
  if grep -q '^version_prefix' "$dir/unpacked/mise.toml"; then
    fail "package: mise.toml still sets version_prefix"
  else
    pass "package: no version_prefix"
  fi
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
scenario_notes
scenario_package

if ((failures)); then
  echo "$failures check(s) failed; fixtures and $log kept" >&2
  exit 1
fi
rm -rf "$root"
echo "release scripts: all checks passed"
