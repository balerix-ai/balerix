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
  rm -rf "$dir/scripts/release" "$dir/CHANGELOG.md" "$dir"/plugins/*/CHANGELOG.md "$dir/charts/CHANGELOG.md"
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

# A field both charts share, and one plugin's pin, in <dir>.
charts_field_of() {
  (
    cd "$1"
    # shellcheck source=scripts/release/lib.sh
    source scripts/release/lib.sh
    charts_field "$2"
  )
}
chart_pin_of() {
  (
    cd "$1"
    # shellcheck source=scripts/release/lib.sh
    source scripts/release/lib.sh
    chart_pin "$2"
  )
}
set_chart_pin_of() {
  (
    cd "$1"
    # shellcheck source=scripts/release/lib.sh
    source scripts/release/lib.sh
    set_chart_pin "$2" "$3"
  )
}

# Releases every version the charts name, as they name them: what the
# charts gate waits for (Spec O §24.5).
release_charts_deps() {
  local dir=$1 plugin
  release "$dir" core "$(charts_field_of "$dir" appVersion)"
  for plugin in flow web matrix github; do
    release "$dir" "$plugin" "$(chart_pin_of "$dir" "$plugin")"
  done
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

project_version_of() {
  (
    cd "$1"
    # shellcheck source=scripts/release/lib.sh
    source scripts/release/lib.sh
    project_version "$2"
  )
}

# Every manifest, lockfile and plugin manifest agrees (Spec I §10).
assert_consistent() {
  local dir=$1 label=$2 core plugin project version
  core=$(manifest_version "$dir" core)
  expect_eq "$label: Cargo.lock has balerix $core" "$(lock_version "$dir/Cargo.lock" balerix)" "$core"
  for plugin in flow web matrix github; do
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
  for project in operator agent; do
    expect_eq "$label: $project Cargo.toml at core" "$(project_version_of "$dir" "$project")" "$core"
    expect_eq "$label: $project Cargo.lock has balerix-$project at core" \
      "$(lock_version "$dir/$project/Cargo.lock" "balerix-$project")" "$core"
    expect_eq "$label: $project Cargo.lock has balerix-core at core" \
      "$(lock_version "$dir/$project/Cargo.lock" balerix-core)" "$core"
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
  for unit in core flow web matrix github; do release "$dir" "$unit" 0.4.0; done
  change "$dir" crates/balerix-plugin-sdk/release-test.txt "fix(sdk): an sdk fix"
  for unit in core flow web matrix github; do
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

# A commit under operator/ or agent/ releases core (Spec O §24.4).
scenario_core_projects() {
  local dir
  dir=$(fixture core-projects)
  release "$dir" core 0.4.0
  change "$dir" operator/src/release-test.rs "fix(operator): an operator fix"
  expect_eq "operator change: core releases" "$(field status "$(prepare "$dir" core)")" release
  assert_consistent "$dir" "operator change"
  discard "$dir"
  change "$dir" agent/src/release-test.rs "fix(agent): an agent fix"
  expect_eq "agent change: core releases" "$(field status "$(prepare "$dir" core)")" release
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
  expect_eq "plan, merged initial flow PR: images" "$(field images "$out")" '[{"unit":"flow","image":"balerix-plugin-flow"}]'

  gitc "$dir" tag "$tag"
  out=$(plan "$dir")
  expect_eq "plan, flow tagged: units" "$(field units "$out")" '[]'
  expect_eq "plan, flow tagged: plugins" "$(field plugins "$out")" '[]'
}

# A merged core release builds core's three images, in build order.
scenario_plan_core_images() {
  local dir out
  dir=$(fixture plan-core)
  prepare "$dir" core >/dev/null
  gitc "$dir" add -A
  gitc "$dir" commit -qm "chore(release): core initial"
  out=$(plan "$dir")
  expect_eq "plan, merged core PR: images in build order" "$(field images "$out")" \
    '[{"unit":"core","image":"balerix"},{"unit":"core","image":"balerix-agent"},{"unit":"core","image":"balerix-operator"}]'
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
  expect_eq "hand bump, forced: manifest untouched, only yaml, pin and changelog written" \
    "$(git -C "$dir" status --porcelain | awk '{print $2}' | sort)" \
    "$(printf '%s\n' charts/balerix-daemon/values.yaml plugins/flow/CHANGELOG.md plugins/flow/package/balerix-plugin.yaml | sort)"
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

# Each image carries its own name and description, not the repository's,
# and a core image's context holds its binary under its own name.
scenario_image_context() {
  local dir="$root/image-context" out
  mkdir -p "$dir/dist"
  touch "$dir/dist/balerix" "$dir/dist/balerix-agent" "$dir/dist/balerix-operator" "$dir/dist/balerix-plugin-flow"
  out=$(GITHUB_REPOSITORY_OWNER=Example "$repo/scripts/release/image-context.sh" balerix-plugin-flow "$dir/dist" "$dir/flow" 2>>"$log")
  expect_eq "image context: flow image" "$(field image "$out")" ghcr.io/example/balerix-plugin-flow
  expect_eq "image context: flow title" "$(field title "$out")" balerix-plugin-flow
  expect_eq "image context: flow description drops the spec reference" "$(field description "$out")" \
    "The flow plugin: a per-agent state machine over hook events"
  out=$("$repo/scripts/release/image-context.sh" balerix "$dir/dist" "$dir/core" 2>>"$log")
  expect_eq "image context: core title" "$(field title "$out")" balerix
  expect_eq "image context: core description" "$(field description "$out")" \
    "Control plane and orchestrator for fleets of coding agents"
  out=$(GITHUB_REPOSITORY_OWNER=Example "$repo/scripts/release/image-context.sh" balerix-operator "$dir/dist" "$dir/operator" 2>>"$log")
  expect_eq "image context: operator image" "$(field image "$out")" ghcr.io/example/balerix-operator
  expect_eq "image context: operator Dockerfile" "$(field dockerfile "$out")" "$repo/docker/operator/Dockerfile"
  expect_eq "image context: operator description" "$(field description "$out")" \
    "The balerix operator: five custom resources reconciled into pods, claims, Secrets and Jobs"
  expect_eq "image context: operator binary" "$(ls "$dir/operator")" balerix-operator
  out=$("$repo/scripts/release/image-context.sh" balerix-agent "$dir/dist" "$dir/agent" 2>>"$log")
  expect_eq "image context: agent Dockerfile" "$(field dockerfile "$out")" "$repo/docker/agent/Dockerfile"
  expect_eq "image context: agent binary" "$(ls "$dir/agent")" balerix-agent
  expect_eq "image context: agent version is core's" "$(field version "$out")" "$(manifest_version "$repo" core)"
  if "$repo/scripts/release/image-context.sh" balerix-plugin-common "$dir/dist" "$dir/common" >/dev/null 2>>"$log"; then
    fail "image context: a library has no image, but one was assembled"
  else
    pass "image context: a library's image is refused"
  fi
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
  expect_eq "affected: an operator change is core" \
    "$(affected_by "$dir" operator/src/release-test.rs)" '["core"]'
  expect_eq "affected: an agent change is core" \
    "$(affected_by "$dir" agent/src/release-test.rs)" '["core"]'
  expect_eq "affected: an sdk change reaches every plugin" \
    "$(affected_by "$dir" crates/balerix-plugin-sdk/src/release-test.rs)" '["core","flow","web","matrix","github"]'
  expect_eq "affected: the root lockfile is core" \
    "$(affected_by "$dir" Cargo.lock)" '["core"]'
  expect_eq "affected: a plugin lockfile is that plugin" \
    "$(affected_by "$dir" plugins/web/Cargo.lock)" '["web"]'
  expect_eq "affected: a Dockerfile is every unit" \
    "$(affected_by "$dir" docker/plugin/release-test.txt)" '["core","flow","web","matrix","github"]'
  expect_eq "affected: the tool pins are every unit" \
    "$(affected_by "$dir" mise.toml)" '["core","flow","web","matrix","github"]'
  expect_eq "affected: the scan exceptions are every unit" \
    "$(affected_by "$dir" .trivyignore.yaml)" '["core","flow","web","matrix","github"]'
  expect_eq "affected: the workflow itself is every unit" \
    "$(affected_by "$dir" .github/workflows/images.yml)" '["core","flow","web","matrix","github"]'
  expect_eq "affected: docs are no unit" \
    "$(affected_by "$dir" docs/release-test.md)" '[]'
  expect_eq "affected: another workflow is no unit" \
    "$(affected_by "$dir" .github/workflows/release-test.yml)" '[]'
  expect_eq "affected: no range is every unit (the nightly)" \
    "$(field units "$(affected "$dir")")" '["core","flow","web","matrix","github"]'
  expect_eq "affected: a common change reaches the plugins built on it" \
    "$(affected_by "$dir" plugins/common/src/release-test.rs)" '["flow","web","matrix","github"]'
}

# A library unit releases like a plugin but ships crates, not a binary:
# prepare.sh on a library that must wait for core, stdout and stderr
# apart: the status on stdout, the reason on stderr.
prepare_refused() {
  local dir=$1 unit=$2 label=$3 out err needle
  err="$root/${label// /-}.err"
  # Direct, not through the prepare() helper: that helper always sends
  # stderr to $log, so a redirect at the call site cannot recapture it.
  out=$("$dir/scripts/release/prepare.sh" "$unit" 2>"$err")
  cat "$err" >>"$log"
  expect_eq "$label: status" "$(field status "$out")" none
  for needle in "${@:4}"; do
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
  prepare_refused "$dir" common "common before core" "no tag balerix-v$sdk yet" "release core $sdk first, then common"
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
  prepare_refused "$dir" common "sdk changed since balerix-v0.4.0" "changed since balerix-v0.4.0; release core first"
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
  for unit in common flow web matrix github; do release "$dir" "$unit" 0.4.0; done
  change "$dir" plugins/common/src/release-test.rs "fix(common): a shared fix"
  out=$(prepare "$dir" common)
  expect_eq "common change: common releases" "$(field status "$out")" release
  version=$(field version "$out")
  expect_eq "common change: common's manifest at $version" "$(manifest_version "$dir" common)" "$version"
  # Every in-tree plugin is built on common since flow took its delivery
  # tracker (#100); core is what a common change leaves alone.
  for unit in flow web matrix github; do
    expect_eq "common change: $unit Cargo.lock has common at $version" \
      "$(lock_version "$dir/plugins/$unit/Cargo.lock" balerix-plugin-common)" "$version"
  done
  for unit in flow web matrix github; do
    discard "$dir"
    expect_eq "common change: $unit releases" "$(field status "$(prepare "$dir" "$unit")")" release
  done
  discard "$dir"
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

# The charts name core's version and each plugin's; until every one is
# tagged, nothing is proposed, and the refusal names the missing tag.
scenario_charts_gate() {
  local dir out app
  dir=$(fixture charts-gate)
  app=$(charts_field_of "$dir" appVersion)
  prepare_refused "$dir" charts "charts before core" "balerix-v$app (appVersion)"
  release "$dir" core "$app"
  release "$dir" flow "$(chart_pin_of "$dir" flow)"
  release "$dir" web "$(chart_pin_of "$dir" web)"
  release "$dir" matrix "$(chart_pin_of "$dir" matrix)"
  prepare_refused "$dir" charts "charts before github" \
    "balerix-plugin-github-v$(chart_pin_of "$dir" github) (plugins.github.image.tag)"
  release "$dir" github "$(chart_pin_of "$dir" github)"
  out=$(prepare "$dir" charts)
  expect_eq "charts after all: status" "$(field status "$out")" release
  expect_eq "charts after all: initial version" "$(field version "$out")" "$(charts_field_of "$dir" version)"
  expect_eq "charts after all: tag" "$(field tag "$out")" "balerix-charts-v$(charts_field_of "$dir" version)"
  expect_grep "charts after all: changelog section" "## $(charts_field_of "$dir" version) - " "$dir/charts/CHANGELOG.md"
}

# At release time every pin is tagged or released by this same run, at the
# version the run releases: the charts job waits for merge-images, so the
# run's image tags exist by then (Spec O §24.5).
scenario_charts_pins_at_release() {
  local dir err pin
  dir=$(fixture charts-pins)
  err="$root/charts-pins.err"
  release "$dir" core "$(charts_field_of "$dir" appVersion)"
  release "$dir" flow "$(chart_pin_of "$dir" flow)"
  release "$dir" web "$(chart_pin_of "$dir" web)"
  release "$dir" matrix "$(chart_pin_of "$dir" matrix)"
  pin=$(chart_pin_of "$dir" github)
  expect_eq "pins: github's pin is its manifest version" "$pin" "$(manifest_version "$dir" github)"
  if "$dir/scripts/release/check-pins.sh" charts 2>"$err"; then
    fail "pins: github untagged and not in the run: passed"
  else
    pass "pins: github untagged and not in the run: fails"
  fi
  cat "$err" >>"$log"
  expect_grep "pins: names the missing tag" "balerix-plugin-github-v$pin (plugins.github.image.tag)" "$err"
  if "$dir/scripts/release/check-pins.sh" charts github 2>>"$log"; then
    pass "pins: github untagged but in the run: ok"
  else
    fail "pins: github untagged but in the run: failed"
  fi
  set_chart_pin_of "$dir" github 9.9.9
  if "$dir/scripts/release/check-pins.sh" charts github 2>"$err"; then
    fail "pins: in the run at another version: passed"
  else
    pass "pins: in the run at another version: fails"
  fi
  cat "$err" >>"$log"
  expect_grep "pins: names the pinned tag" "balerix-plugin-github-v9.9.9 (plugins.github.image.tag)" "$err"
  discard "$dir"
  release "$dir" github "$pin"
  if "$dir/scripts/release/check-pins.sh" charts 2>>"$log"; then
    pass "pins: all tagged: ok"
  else
    fail "pins: all tagged: failed"
  fi
}

# A plugin release moves its pin and nothing else in the daemon values;
# plugins.github is not credentials.github.
scenario_plugin_moves_pin() {
  local dir
  dir=$(fixture plugin-pin)
  release "$dir" github 0.1.0
  change "$dir" plugins/github/src/release-test.rs "fix: a github fix"
  prepare "$dir" github >/dev/null
  expect_eq "github release: pin moved" "$(chart_pin_of "$dir" github)" 0.1.1
  expect_eq "github release: only the pin line changed" \
    "$(git -C "$dir" diff --numstat -- charts/balerix-daemon/values.yaml | cut -f1,2)" "$(printf '1\t1')"
  expect_grep "github release: credentials.github untouched" '    secretName: ""' "$dir/charts/balerix-daemon/values.yaml"
}

# A core release moves both charts' appVersion.
scenario_core_moves_app_version() {
  local dir
  dir=$(fixture core-app-version)
  release "$dir" core 0.4.0
  expect_eq "core 0.4.0: appVersion" "$(charts_field_of "$dir" appVersion)" 0.4.0
  expect_grep "core 0.4.0: appVersion stays a quoted string" 'appVersion: "0.4.0"' "$dir/charts/balerix-daemon/Chart.yaml"
}

# A moved pin is a change even with no commit under charts/; its notes
# list the image, not "Initial release".
scenario_charts_pin_only() {
  local dir out version
  dir=$(fixture charts-pin-only)
  release_charts_deps "$dir"
  version=$(charts_field_of "$dir" version)
  release "$dir" charts "$version"
  out=$(prepare "$dir" charts)
  expect_eq "charts, nothing moved: status" "$(field status "$out")" none
  change "$dir" plugins/flow/src/release-test.rs "fix: a flow fix"
  release "$dir" flow 0.1.2
  out=$(prepare "$dir" charts)
  expect_eq "charts, flow pin moved: status" "$(field status "$out")" release
  expect_eq "charts, flow pin moved: patch" "$(field version "$out")" 0.1.1
  expect_eq "charts, flow pin moved: both charts" "$(charts_field_of "$dir" version)" 0.1.1
  expect_grep "charts, flow pin moved: Images section" '### Images' "$dir/$(field notes "$out")"
  # shellcheck disable=SC2016 # literal backticks: markdown code
  expect_grep "charts, flow pin moved: the image" '`balerix-plugin-flow` 0.1.1 → 0.1.2' "$dir/$(field notes "$out")"
  if grep -q 'Initial release' "$dir/$(field notes "$out")"; then
    fail "charts, flow pin moved: notes say Initial release"
  else
    pass "charts, flow pin moved: no Initial release line"
  fi
}

# A core minor is a chart minor, whatever the chart's own commits ask.
scenario_charts_core_minor() {
  local dir out
  dir=$(fixture charts-core-minor)
  release_charts_deps "$dir"
  release "$dir" charts "$(charts_field_of "$dir" version)"
  change "$dir" charts/balerix-daemon/release-test.txt "fix(charts): a chart fix"
  change "$dir" crates/balerix-server/release-test.txt "feat!: a breaking daemon change"
  release "$dir" core 0.3.0
  out=$(prepare "$dir" charts)
  expect_eq "charts after core 0.3.0: minor" "$(field version "$out")" 0.2.0
  expect_grep "charts after core 0.3.0: the fix listed" 'A chart fix' "$dir/$(field notes "$out")"
  # shellcheck disable=SC2016 # literal backticks: markdown code
  expect_grep "charts after core 0.3.0: the image listed" '`balerix` 0.2.0 → 0.3.0' "$dir/$(field notes "$out")"
}

# Staged as published: upstream's charts are the tree's, byte for byte.
scenario_stage_charts() {
  local dir="$root/stage-upstream" chart
  GITHUB_REPOSITORY_OWNER=balerix-ai "$repo/scripts/release/stage-charts.sh" "$dir" >/dev/null 2>>"$log"
  for chart in balerix-operator balerix-daemon; do
    if diff -r "$repo/charts/$chart" "$dir/$chart" >>"$log" 2>&1; then
      pass "stage, upstream: $chart is the tree's"
    else
      fail "stage, upstream: $chart differs from the tree"
    fi
  done
}

# A fork's charts install the fork's images, lowercased (Spec O §24.5).
scenario_stage_charts_fork() {
  local dir="$root/stage-fork" app
  app=$(charts_field_of "$repo" appVersion)
  GITHUB_REPOSITORY_OWNER=Example "$repo/scripts/release/stage-charts.sh" "$dir" >/dev/null 2>>"$log"
  expect_grep "stage, fork: operator repository" 'repository: ghcr.io/example/balerix-operator' "$dir/balerix-operator/values.yaml"
  expect_grep "stage, fork: daemon image" "daemon: \"ghcr.io/example/balerix:$app\"" "$dir/balerix-operator/values.yaml"
  expect_grep "stage, fork: agent image" "agent: \"ghcr.io/example/balerix-agent:$app\"" "$dir/balerix-operator/values.yaml"
  expect_grep "stage, fork: a plugin repository" 'repository: ghcr.io/example/balerix-plugin-github' "$dir/balerix-daemon/values.yaml"
  if grep -rq 'ghcr.io/balerix-ai/' "$dir"/*/values.yaml; then
    fail "stage, fork: a balerix-ai image is left"
  else
    pass "stage, fork: no balerix-ai image left"
  fi
}

scenario_plan_charts() {
  local dir out
  dir=$(fixture plan-charts)
  release_charts_deps "$dir"
  prepare "$dir" charts >/dev/null
  gitc "$dir" add -A
  gitc "$dir" commit -qm "chore(release): charts initial"
  out=$(plan "$dir")
  expect_eq "plan, merged charts PR: units" "$(field units "$out")" '["charts"]'
  expect_eq "plan, merged charts PR: charts" "$(field charts "$out")" true
  expect_eq "plan, merged charts PR: binaries" "$(field binaries "$out")" '[]'
  expect_eq "plan, merged charts PR: images" "$(field images "$out")" '[]'
}

scenario_initial
scenario_bumps_0x
scenario_bumps_1x
scenario_skipped_types
scenario_sdk_change
scenario_plugin_only
scenario_core_bump
scenario_core_projects
scenario_in_progress
scenario_plan
scenario_plan_core_images
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
scenario_charts_gate
scenario_charts_pins_at_release
scenario_plugin_moves_pin
scenario_core_moves_app_version
scenario_charts_pin_only
scenario_charts_core_minor
scenario_plan_charts
scenario_stage_charts
scenario_stage_charts_fork

if ((failures)); then
  echo "$failures check(s) failed; fixtures and $log kept" >&2
  exit 1
fi
rm -rf "$root"
echo "release scripts: all checks passed"
