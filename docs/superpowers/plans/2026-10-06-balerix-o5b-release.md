# Spec O 5b: the release of balerix on Kubernetes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Release the operator and agent with core, add a `charts` release unit that publishes both charts to OCI and to the `helm-charts` index, and add `mise run verify-k8s` for the by-hand fork rehearsal.

**Architecture:** Everything sits in the Spec I release scripts (`scripts/release/*.sh`, sourced `lib.sh`) and their workflows. Core's binaries job builds three static binaries. Its image job builds three images per architecture, in order, and images are keyed by name from here on (`unit_images`). The charts unit is a unit with no crate, and its version lives in `Chart.yaml`. A gate holds it back until every version the charts pin is tagged, and pin moves count as changes. Publishing works like an image release: package and check on kind, push to OCI and sign, the shared GitHub Release, then the index commit and a verify step. `verify-k8s` is a report-printing bash script like `verify-claude.sh`.

**Tech Stack:** bash (GNU sed/awk), git-cliff, cargo-edit, helm 4.3.0, kind 0.33.0, kubectl 1.34.12, cosign, GitHub Actions (composite actions, docker/build-push-action), jq.

**Spec:** `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md`. The sections are §13, §14 and §24.4–§24.6, with §17's "done" condition. Spec I (`docs/superpowers/specs/2026-09-11-release-pipeline-design.md`) is the pipeline this extends.

**Branch:** `feat/release-k8s`, cut from `docs/spec-o-5b`, which holds this plan.

## Global Constraints

- Core's tag stays `balerix-v<ver>`. The charts tag is `balerix-charts-v<ver>`. Both charts always share one `version` and one `appVersion`.
- Static musl binaries, x86_64 and aarch64, for `balerix`, `balerix-operator` and `balerix-agent`. Core's archives `balerix-v<ver>-<target>.tar.gz` gain the two new binaries.
- The images are `ghcr.io/<owner>/balerix`, `…/balerix-agent` and `…/balerix-operator`. `<owner>` is `GITHUB_REPOSITORY_OWNER`, lowercased, and defaults to `balerix-ai`.
- On a release, `balerix-agent`'s `BASE` is the pushed `balerix` per-architecture digest, never `latest`.
- Smoke tests:
  - `--version` on both new images prints `balerix-operator <ver>` / `balerix-agent <ver>`.
  - `balerix-operator crds` prints exactly five `kind: CustomResourceDefinition` documents.
  - `balerix-agent sidecar` with no configuration exits 1 with `balerix-agent sidecar: …` on stderr.
- `prepare.sh core` writes the core version into `operator/Cargo.toml` and `agent/Cargo.toml`, updates their lockfiles, and moves both charts' `appVersion`.
- `prepare.sh <plugin>` moves `plugins.<plugin>.image.tag` in `charts/balerix-daemon/values.yaml`.
- `prepare.sh charts` answers `status=none`, naming the missing tag, until `appVersion` and every pinned plugin version are tagged.
- The chart bump is the larger of what the commits under `charts/` ask and the largest bump among the changed pins. A core minor is a chart minor.
- OCI target: `oci://ghcr.io/<owner>/charts`. Index: `<owner>/helm-charts`, served at `https://<owner>.github.io/helm-charts`. Asset URLs point at `https://github.com/<repo>/releases/download/balerix-charts-v<ver>/`.
- A dry run runs package and check and uploads the archives. Nothing is pushed, signed, tagged or committed.
- helm 4.3.0, kind 0.33.0, kubectl 1.34.12 and envtest `envtest-v1.34.1` stay task-level or explicit `mise install` pins, not `[tools]`.
- Commit messages are Conventional Commits (`feat:`, `fix:`, `docs:`, `test:`, `ci:`, `chore:`). Run `mise run check` before every commit, `mise run release-test` for any `scripts/release/` change, and `mise run lint` (shellcheck, actionlint, zizmor) for scripts and workflows.

## Review Focus

1. **`plugins.github` vs `credentials.github` in the daemon values.** Both are a two-space `github:` key. The pin reader and writer must touch only the one under `plugins:`. Pinned by `scenario_plugin_moves_pin` (Task 3), which asserts that the values diff is exactly one line.
2. **A fork whose owner has capitals** (`Example`). Every staged image repository, and the operator's `images.daemon`/`images.agent`, must be lowercase (`ghcr.io/example/…`). Pinned by `scenario_stage_charts_fork` (Task 4).
3. **A pin-only charts release** (a plugin or core release moved a pin, with no commit under `charts/`). It must propose a release, not report "nothing to release", and its changelog must list the moved images, not say "Initial release." Pinned by `scenario_charts_pin_only` (Task 3).
4. **Re-running a charts release after a partial publish** (the index already lists the version). `index-charts.sh` must skip without a second commit. **And Pages lag after the index commit:** `verify-charts.sh` must retry for up to 10 minutes before failing and flagging. Both are exercised locally in Task 4 Step 9.
5. **The agent image's base in each mode.** In the smoke build (pull request, dry run) it must be the `balerix` image built moments earlier in the same job. In the push build it must be that image's pushed digest. Pinned by `images.yml` building core on both architectures in the PR's CI, and by the Task 7 core dry run.

---

### Task 1: The operator and agent in the core unit (binaries, versions, paths, audit)

**Files:**
- Modify: `scripts/release/lib.sh` (`CORE_PROJECTS`, `unit_paths core`, `project_version`, `project_path_crates`)
- Modify: `scripts/release/prepare.sh` (core loop over the projects)
- Modify: `scripts/release/build.sh` (core builds, smoke-tests and archives three binaries)
- Modify: `scripts/release/audit.sh` (core audits the two projects)
- Modify: `.github/workflows/images.yml` (pull-request paths)
- Test: `scripts/release/test.sh` (`assert_consistent`, `scenario_affected`)

**Interfaces:**
- Produces:
  - `CORE_PROJECTS=(operator agent)` (lib.sh).
  - `project_version <project>` prints `balerix-<project>`'s version.
  - `project_path_crates <project>` prints the path-dependency crates in `<project>/Cargo.lock`, one per line, excluding the project's own crate.
  - `build.sh core <target> <out>` leaves `<out>/balerix`, `<out>/balerix-agent`, `<out>/balerix-operator` and the archive.

- [ ] **Step 1: Write the failing tests**

In `scripts/release/test.sh`, extend `assert_consistent` after the `common` line:

```bash
  for project in operator agent; do
    expect_eq "$label: $project Cargo.toml at core" "$(project_version_of "$dir" "$project")" "$core"
    expect_eq "$label: $project Cargo.lock has balerix-$project at core" \
      "$(lock_version "$dir/$project/Cargo.lock" "balerix-$project")" "$core"
    expect_eq "$label: $project Cargo.lock has balerix-core at core" \
      "$(lock_version "$dir/$project/Cargo.lock" balerix-core)" "$core"
  done
```

Add `project` to the function's `local` line. Add this helper next to `dep_version_of`:

```bash
project_version_of() {
  (
    cd "$1"
    # shellcheck source=scripts/release/lib.sh
    source scripts/release/lib.sh
    project_version "$2"
  )
}
```

In `scenario_affected`, after the "a core crate change" line:

```bash
  expect_eq "affected: an operator change is core" \
    "$(affected_by "$dir" operator/src/release-test.rs)" '["core"]'
  expect_eq "affected: an agent change is core" \
    "$(affected_by "$dir" agent/src/release-test.rs)" '["core"]'
```

Add a core-includes-the-projects scenario and call it after `scenario_core_bump`:

```bash
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
```

- [ ] **Step 2: Run them to see them fail**

Run: `mise run release-test`
Expected: FAIL. `project_version: command not found`, then "operator change: core releases: got 'none'" and the affected-units lines for operator and agent (`[]`).

- [ ] **Step 3: Implement lib.sh**

In `scripts/release/lib.sh`, after `IMAGE_UNITS`:

```bash
# The standalone projects that release with core (Spec O §13, §24.4): each
# has its own manifest, lockfile and target directory, and core's version.
# shellcheck disable=SC2034
CORE_PROJECTS=(operator agent)
```

In `unit_paths`, replace the core line:

```bash
    printf '%s\n' 'crates/**' Cargo.toml Cargo.lock mise.toml \
      'operator/**' 'agent/**' 'docker/operator/**' 'docker/agent/**'
```

After `unit_version`:

```bash
# A core project's version (balerix-<project> in <project>/Cargo.toml).
project_version() {
  cargo metadata --manifest-path "$1/Cargo.toml" --no-deps --format-version 1 |
    jq -r --arg c "balerix-$1" '.packages[] | select(.name == $c) | .version'
}

# The core crates <project> builds on by path, as its lockfile lists them:
# the balerix-* packages with no `source` line, less the project itself.
project_path_crates() {
  awk -v self="balerix-$1" '
    function flush() { if (name != "" && !src && name != self) print name; name = ""; src = 0 }
    /^\[\[package\]\]/ { flush() }
    /^name = "balerix/ { name = $3; gsub(/"/, "", name) }
    /^source = / { src = 1 }
    END { flush() }
  ' "$1/Cargo.lock"
}
```

- [ ] **Step 4: Implement prepare.sh**

In `scripts/release/prepare.sh`, after the `for plugin in "${PLUGIN_UNITS[@]}"` loop inside `if [[ $unit == core ]]` (the loop that runs `cargo update … -p balerix-api -p balerix-plugin-sdk`), add:

```bash
  # The operator and the agent release with core (Spec O §24.4): core's
  # version, and lockfiles that lock the core crates they build on by path.
  # Refresh even when the version is already right, for the reason above.
  for project in "${CORE_PROJECTS[@]}"; do
    if [[ $(project_version "$project") != "$next" ]]; then
      cargo set-version --manifest-path "$project/Cargo.toml" "$next" >&2
    fi
    mapfile -t locals < <(project_path_crates "$project")
    cargo update --manifest-path "$project/Cargo.toml" "${locals[@]/#/--package=}" >&2
  done
```

- [ ] **Step 5: Run release-test**

Run: `mise run release-test`
Expected: `release scripts: all checks passed`. If a `cargo update` line fails with "package ID specification … did not match", print `project_path_crates operator` from a sourced shell. Compare it with `grep -B1 -A2 'name = "balerix' operator/Cargo.lock`, and fix the awk, not the test.

- [ ] **Step 6: build.sh builds, smoke-tests and archives three binaries**

Replace the build block and everything after it, from `if [[ $unit == core ]]; then` (the cargo build) to the end of the file, with:

```bash
if [[ $unit == core ]]; then
  cargo build --release --locked --target "$target" -p balerix
  bins=("${CARGO_TARGET_DIR:-target}/$target/release/balerix")
  # The operator and the agent release with core (Spec O §24.4), each from
  # its own project and target directory, as scripts/operator.sh builds them.
  for project in "${CORE_PROJECTS[@]}"; do
    CARGO_TARGET_DIR="$project/target" cargo build --release --locked --target "$target" \
      --manifest-path "$project/Cargo.toml"
    bins+=("$project/target/$target/release/balerix-$project")
  done
else
  target_dir=$(scripts/plugin.sh target-dir "$unit")
  CARGO_TARGET_DIR="$target_dir" cargo build --release --locked --target "$target" \
    --manifest-path "plugins/$unit/Cargo.toml"
  bins=("$target_dir/$target/release/$crate")
fi

for bin in "${bins[@]}"; do
  # Protocol §7: static builds, so no host libc can mismatch.
  linkage=$(ldd "$bin" 2>&1 || true)
  grep -Eq 'not a dynamic executable|statically linked' <<<"$linkage" ||
    die "$bin is not statically linked: $linkage"
done

if [[ $unit == core ]]; then
  for bin in "${bins[@]}"; do
    name=$(basename "$bin")
    got=$("$bin" --version)
    [[ $got == "$name $version" ]] || die "$name --version printed '$got', expected '$name $version'"
  done
else
  # Without the daemon's environment a plugin fails in Env::from_process and
  # exits 1 with "<name>: …", which proves it starts on this architecture.
  set +e
  err=$(env -i "${bins[0]}" 2>&1 >/dev/null)
  code=$?
  set -e
  [[ $code -eq 1 ]] || die "$crate exited $code without a daemon environment, expected 1: $err"
  grep -q "^$unit: " <<<"$err" || die "$crate did not report '$unit: …': $err"
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
files=()
mkdir -p "$out"
for bin in "${bins[@]}"; do
  name=$(basename "$bin")
  cp "$bin" "$stage/$name"
  cp "$bin" "$out/$name"
  files+=("$name")
done
cp LICENSE "$stage/LICENSE"
files+=(LICENSE)
changelog=$(unit_changelog "$unit")
if [[ -f $changelog ]]; then
  cp "$changelog" "$stage/CHANGELOG.md"
  files+=(CHANGELOG.md)
fi

archive="$out/$crate-v$version-$target.tar.gz"
tar -czf "$archive" -C "$stage" "${files[@]}"
echo "$archive"
```

Update the header comment's "Writes" line:

```bash
# Writes <out-dir>/<binary> for each binary the unit ships (core: balerix,
# balerix-agent, balerix-operator; a plugin: its one), the bare binaries for
# images, and <out-dir>/<crate>-v<version>-<target>.tar.gz; prints the archive path.
```

- [ ] **Step 7: Run build.sh core for this host's musl target**

Run: `rustup target add x86_64-unknown-linux-musl && mise x -- scripts/release/build.sh core x86_64-unknown-linux-musl target/tmp/build-core && tar -tzf target/tmp/build-core/balerix-v*-x86_64-unknown-linux-musl.tar.gz`
Expected: the archive path, then `balerix`, `balerix-agent`, `balerix-operator`, `LICENSE` and `CHANGELOG.md`. The ring and aws-lc C code needs `musl-gcc`; if it's missing (`command -v musl-gcc`), ask the user to install `musl-tools` (`! sudo apt-get install -y musl-tools`) and don't skip the step. Remove `target/tmp/build-core` afterwards.

- [ ] **Step 8: audit.sh audits the two projects with core**

In `scripts/release/audit.sh`, inside `if [[ $unit == core ]]; then`, after the `cargo deny check` line:

```bash
    for project in "${CORE_PROJECTS[@]}"; do
      cargo audit --file "$project/Cargo.lock"
      cargo deny --manifest-path "$project/Cargo.toml" check advisories bans sources licenses
    done
```

Run: `mise x -- scripts/release/audit.sh core`
Expected: exit 0 (the nightly `mise run audit` already audits both lockfiles). If it reports an advisory, stop and report it. Don't add ignores.

- [ ] **Step 9: images.yml builds core on operator and agent changes**

In `.github/workflows/images.yml`, under `pull_request: paths:` after `- plugins/**`, add:

```yaml
      - operator/**
      - agent/**
```

- [ ] **Step 10: Lint, test, commit**

Run: `mise run lint && mise run release-test && mise run check`
Expected: all pass.

```bash
git add scripts/release/lib.sh scripts/release/prepare.sh scripts/release/build.sh scripts/release/audit.sh scripts/release/test.sh .github/workflows/images.yml
git commit -m "feat(release): the operator and agent binaries release with core (Spec O §24.4)"
```

---

### Task 2: Core's three images

**Files:**
- Modify: `scripts/release/lib.sh` (`unit_images`, `image_unit`, `image_ref`, `image_description`; delete `unit_image`)
- Modify: `scripts/release/image-context.sh`, `scripts/release/smoke-image.sh`, `scripts/release/merge-image.sh`, `scripts/release/promote-image.sh` (keyed by image)
- Modify: `scripts/release/plan.sh` (`images=` output)
- Modify: `scripts/release/github-release.sh` (Verify lists every image)
- Modify: `.github/actions/build-image/action.yml` (`image`, `base-smoke`, `base-push`)
- Create: `.github/actions/build-unit-images/action.yml`
- Modify: `.github/workflows/release.yml` (`images`, `merge-images`, `promote-images`), `.github/workflows/images.yml`
- Modify: `scripts/kind-up.sh` (images through `build.sh core` and image names)
- Modify: `.github/workflows/ci.yml` (`e2e-k8s` path filter: `scripts/release/build\.sh`, `scripts/release/lib\.sh`)
- Test: `scripts/release/test.sh` (`scenario_image_context`, `scenario_plan`)

**Interfaces:**
- Consumes: `CORE_PROJECTS`, `build.sh core` outputs (Task 1).
- Produces:
  - `unit_images <unit>` prints the image names in build order (core: `balerix`, `balerix-agent`, `balerix-operator`; a plugin: `balerix-plugin-<unit>`).
  - `image_unit <image>` prints the unit.
  - `image_ref <image>` prints `ghcr.io/<owner,,>/<image>`.
  - `image_description <image>`.
  - `image-context.sh <image> <dist> <context>`, `smoke-image.sh <image> <ref>`, `merge-image.sh <image> <digests-dir>`, `promote-image.sh <image>`.
  - `plan.sh` gains `images=[{"unit":…,"image":…},…]`.
  - The `build-unit-images` action writes `<digests>/<image>/<hex>`.

- [ ] **Step 1: Write the failing tests**

Replace `scenario_image_context` in `scripts/release/test.sh`:

```bash
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
```

In `scenario_plan`, after the "merged initial flow PR: core" line:

```bash
  expect_eq "plan, merged initial flow PR: images" "$(field images "$out")" '[{"unit":"flow","image":"balerix-plugin-flow"}]'
```

Add a core-plan scenario and call it after `scenario_plan`:

```bash
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
```

- [ ] **Step 2: Run them to see them fail**

Run: `mise run release-test`
Expected: FAIL on the operator and agent image-context lines and both `images` lines.

- [ ] **Step 3: lib.sh, images by name**

Replace `unit_image` in `scripts/release/lib.sh` with:

```bash
# The images a unit ships, by repository name, in build order (Spec O
# §24.4): core's balerix-agent is built on the balerix just built.
unit_images() {
  require_unit "$1"
  case $(unit_kind "$1") in
    core) printf '%s\n' balerix balerix-agent balerix-operator ;;
    plugin) unit_crate "$1" ;;
    *) die "$1 has no image" ;;
  esac
}

# The release unit an image belongs to.
image_unit() {
  local unit
  case $1 in
    balerix | balerix-agent | balerix-operator) echo core ;;
    balerix-plugin-*)
      unit=${1#balerix-plugin-}
      require_unit "$unit"
      [[ $(unit_kind "$unit") == plugin ]] || die "$unit is a library and has no image"
      echo "$unit"
      ;;
    *) die "unknown image: '$1'" ;;
  esac
}

# ghcr repositories must be lowercase; a fork's owner may not be.
image_ref() {
  image_unit "$1" >/dev/null
  local owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
  echo "ghcr.io/${owner,,}/$1"
}

# The image's description: its crate's, less the trailing spec reference.
image_description() {
  case $1 in
    balerix-agent | balerix-operator)
      cargo metadata --manifest-path "${1#balerix-}/Cargo.toml" --no-deps --format-version 1 |
        jq -r --arg c "$1" '.packages[] | select(.name == $c) | .description // ""' |
        sed -E 's/ \([^()]*\)$//'
      ;;
    *) unit_description "$(image_unit "$1")" ;;
  esac
}
```

Then grep for every caller of the old function: `grep -rn 'unit_image\b' scripts .github`. Each one is replaced in the steps below, and none may remain.

- [ ] **Step 4: image-context.sh by image**

Rewrite the body of `scripts/release/image-context.sh` after `source`:

```bash
[[ $# -eq 3 ]] || die "usage: $0 <image> <dist-dir> <context-dir>"
image=$1
dist=$2
context=$3
unit=$(image_unit "$image")
[[ -f $dist/$image ]] || die "$image: no binary at $dist/$image"

rm -rf "$context"
mkdir -p "$context"
case $image in
  balerix)
    cp "$dist/balerix" "$context/balerix"
    # The image installs gh, nono and tmux at the versions this file pins.
    cp mise.toml "$context/mise.toml"
    dockerfile=docker/balerix/Dockerfile
    ;;
  balerix-agent | balerix-operator)
    # Spec O §13: FROM the runtime image (agent) or distroless (operator).
    cp "$dist/$image" "$context/$image"
    dockerfile="docker/${image#balerix-}/Dockerfile"
    ;;
  *)
    cp "$dist/$image" "$context/plugin"
    dockerfile=docker/plugin/Dockerfile
    ;;
esac

echo "context=$(cd "$context" && pwd)"
echo "dockerfile=$PWD/$dockerfile"
echo "image=$(image_ref "$image")"
echo "version=$(unit_version "$unit")"
echo "title=$image"
echo "description=$(image_description "$image")"
```

Update the usage comment to `image-context.sh <image> <dist-dir> <context-dir>` and say that `<image>` is a repository name from `unit_images`.

- [ ] **Step 5: Run release-test**

Run: `mise run release-test`
Expected: the image-context lines pass. The plan lines still fail.

- [ ] **Step 6: plan.sh's images output**

In `scripts/release/plan.sh`, add `images=()` beside the other arrays. Leave the loop's `case` as it is. After it, inside the loop, add:

```bash
  if [[ $(unit_kind "$unit") == core || $(unit_kind "$unit") == plugin ]]; then
    while IFS= read -r image; do
      images+=("{\"unit\":\"$unit\",\"image\":\"$image\"}")
    done < <(unit_images "$unit")
  fi
```

Then, at the end:

```bash
# A matrix of objects: merge-images and promote-images run once per image.
echo "images=[$(IFS=,; echo "${images[*]}")]"
```

Update the header comment to list `images=` (JSON array of `{unit, image}` objects).

Run: `mise run release-test`
Expected: all checks passed.

- [ ] **Step 7: smoke-image.sh, merge-image.sh and promote-image.sh by image**

`scripts/release/smoke-image.sh`. Usage `smoke-image.sh <image> <image-ref>`. Replace the body after `source`:

```bash
[[ $# -eq 2 ]] || die "usage: $0 <image> <image-ref>"
image=$1
ref=$2
unit=$(image_unit "$image")
version=$(unit_version "$unit")

# Runs the image with <args>, expecting exit 1 and stderr lines starting
# with <prefix>: the binary started on this architecture and refused a
# missing configuration.
expect_refusal() {
  local prefix=$1 err code
  shift
  set +e
  err=$(docker run --rm "$ref" "$@" 2>&1 >/dev/null)
  code=$?
  set -e
  [[ $code -eq 1 ]] || die "$ref $* exited $code, expected 1: $err"
  grep -q "^$prefix" <<<"$err" || die "$ref $* did not report '$prefix…': $err"
}

expect_version() {
  local got
  got=$(docker run --rm "$ref" --version)
  [[ $got == "$image $version" ]] || die "$ref --version printed '$got', expected '$image $version'"
}

case $image in
  balerix-agent)
    expect_version
    # Spec O §13: the sidecar (the image's CMD) with no bundle mounted
    expect_refusal "balerix-agent sidecar: "
    ;;
  balerix-operator)
    expect_version
    count=$(docker run --rm "$ref" crds | grep -c '^kind: CustomResourceDefinition$' || true)
    [[ $count -eq 5 ]] || die "$ref crds printed $count CustomResourceDefinitions, expected 5"
    ;;
  balerix)
    expect_version
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
    ;;
  *)
    expect_refusal "$unit: "
    ;;
esac
echo "$ref: ok" >&2
```

`scripts/release/merge-image.sh`. Usage `merge-image.sh <image> <digests-dir>`, where `<digests-dir>` holds that image's per-architecture digest files. Replace the variable lines:

```bash
[[ $# -eq 2 ]] || die "usage: $0 <image> <digests-dir>"
name=$1
digests=$2
image=$(image_ref "$name")
version=$(unit_version "$(image_unit "$name")")
```

Then replace `$unit:` with `$name:` in its two `die` messages.

`scripts/release/promote-image.sh`. Usage `promote-image.sh <image>`:

```bash
[[ $# -eq 1 ]] || die "usage: $0 <image>"
name=$1
unit=$(image_unit "$name")
image=$(image_ref "$name")
version=$(unit_version "$unit")
tag=$(unit_tag "$unit" "$version")
```

Leave the rest as it is.

- [ ] **Step 8: github-release.sh's Verify lists every image**

Replace the `image=` lines near the top with nothing, and remove `image=$(unit_image "$unit")`. In the Verify block, replace the two image lines with a generated list:

````bash
if [[ $kind != library ]]; then
  {
    printf '\n### Verify\n\n```sh\n'
    printf 'gh attestation verify %s --repo %s\n' "$crate-v$version-x86_64-unknown-linux-musl.tar.gz" "$GITHUB_REPOSITORY"
    printf 'sha256sum --check --ignore-missing SHA256SUMS\n'
    while IFS= read -r name; do
      ref="$(image_ref "$name"):$version"
      printf 'gh attestation verify oci://%s --repo %s\n' "$ref" "$GITHUB_REPOSITORY"
      printf 'cosign verify %s \\\n' "$ref"
      printf '  --certificate-identity https://github.com/%s/.github/workflows/release.yml@refs/heads/main \\\n' "$GITHUB_REPOSITORY"
      printf '  --certificate-oidc-issuer https://token.actions.githubusercontent.com\n'
    done < <(unit_images "$unit")
    printf '```\n'
  } >>"$notes"
fi
````

(Task 4 narrows this condition to core and plugins when the charts kind arrives.)

- [ ] **Step 9: The build-image action takes an image and an optional base**

In `.github/actions/build-image/action.yml`:
- Rename the input `unit` to `image` ("Image name from `unit_images`: balerix, balerix-agent, balerix-operator or balerix-plugin-<name>").
- Add the inputs:

```yaml
  base-smoke:
    description: >-
      BASE for the smoke build: an image already in this runner's local
      image store (the balerix built earlier in the job). Built with the
      docker driver, the only one that reads the local store.
    required: false
    default: ""
  base-push:
    description: BASE for the push build, a registry reference (the pushed balerix digest; never latest)
    required: false
    default: ""
```

- Add the outputs:

```yaml
  image:
    description: The image repository (ghcr.io/<owner>/<image>)
    value: ${{ steps.context.outputs.image }}
  smoke:
    description: The locally loaded smoke-test tag
    value: ${{ steps.context.outputs.image }}:smoke
```

- The context step passes `IMAGE: ${{ inputs.image }}` and runs `image-context.sh "$IMAGE" "$DIST" "$RUNNER_TEMP/image-context-$IMAGE"`. A per-image directory, because one job now builds three.
- Give `setup-buildx-action` an `id: buildx`.
- In the smoke build step add:

```yaml
        builder: ${{ inputs.base-smoke != '' && 'default' || steps.buildx.outputs.name }}
        build-args: ${{ inputs.base-smoke != '' && format('BASE={0}', inputs.base-smoke) || '' }}
```

- In the push step add `build-args: ${{ inputs.base-push != '' && format('BASE={0}', inputs.base-push) || '' }}`.
- In the smoke-test step, change to `IMAGE_NAME: ${{ inputs.image }}` and `smoke-image.sh "$IMAGE_NAME" "$IMAGE:smoke"`.
- Update the description's "one release unit's image" to "one image".

- [ ] **Step 10: The build-unit-images composite action**

Create `.github/actions/build-unit-images/action.yml`:

```yaml
name: build-unit-images
description: >-
  Build, smoke-test, scan and optionally push every image of one release
  unit for this runner's architecture, in order (Spec O §24.4): core's
  balerix, then balerix-agent on it, then balerix-operator; a plugin's one
  image. When pushing, writes each pushed digest as an empty file
  <digests>/<image>/<hex>. The caller has checked out the repository,
  installed jq, hadolint and trivy through mise, and logged in to ghcr when
  pushing.
inputs:
  unit:
    description: Release unit (core, flow, web, matrix, github)
    required: true
  dist:
    description: Directory holding the unit's bare binaries (build-binary's dist)
    required: true
  push:
    description: "'true' pushes each image by digest after its checks pass"
    required: false
    default: "false"
  github-token:
    description: Token for mise's GitHub downloads inside the build
    required: true
  digests:
    description: Directory for the pushed digests (used only when pushing)
    required: false
    default: ""
runs:
  using: composite
  steps:
    - id: first
      uses: ./.github/actions/build-image
      with:
        image: ${{ inputs.unit == 'core' && 'balerix' || format('balerix-plugin-{0}', inputs.unit) }}
        dist: ${{ inputs.dist }}
        push: ${{ inputs.push }}
        github-token: ${{ inputs.github-token }}
    - id: agent
      if: inputs.unit == 'core'
      uses: ./.github/actions/build-image
      with:
        image: balerix-agent
        dist: ${{ inputs.dist }}
        push: ${{ inputs.push }}
        github-token: ${{ inputs.github-token }}
        base-smoke: ${{ steps.first.outputs.smoke }}
        base-push: ${{ inputs.push == 'true' && format('{0}@{1}', steps.first.outputs.image, steps.first.outputs.digest) || '' }}
    - id: operator
      if: inputs.unit == 'core'
      uses: ./.github/actions/build-image
      with:
        image: balerix-operator
        dist: ${{ inputs.dist }}
        push: ${{ inputs.push }}
        github-token: ${{ inputs.github-token }}
    - name: Record the pushed digests
      if: inputs.push == 'true'
      shell: bash
      env:
        DIGESTS: ${{ inputs.digests }}
        FIRST: ${{ inputs.unit == 'core' && 'balerix' || format('balerix-plugin-{0}', inputs.unit) }}
        FIRST_DIGEST: ${{ steps.first.outputs.digest }}
        AGENT_DIGEST: ${{ steps.agent.outputs.digest }}
        OPERATOR_DIGEST: ${{ steps.operator.outputs.digest }}
      run: |
        [[ -n $DIGESTS ]] || { echo "build-unit-images: push needs digests" >&2; exit 1; }
        record() {
          [[ -n $2 ]] || return 0
          mkdir -p "$DIGESTS/$1"
          touch "$DIGESTS/$1/${2#sha256:}"
        }
        record "$FIRST" "$FIRST_DIGEST"
        record balerix-agent "$AGENT_DIGEST"
        record balerix-operator "$OPERATOR_DIGEST"
```

- [ ] **Step 11: release.yml and images.yml use it**

In `.github/workflows/release.yml`, in the `images` job, replace the steps from `- id: image` through the digest upload with:

```yaml
      - uses: ./.github/actions/build-unit-images
        with:
          unit: ${{ matrix.unit }}
          dist: ${{ runner.temp }}/dist
          push: ${{ !inputs.dry-run }}
          github-token: ${{ secrets.GITHUB_TOKEN }}
          digests: ${{ runner.temp }}/digests
      - if: ${{ !inputs.dry-run }}
        uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: digest-${{ matrix.unit }}-${{ matrix.arch }}
          path: ${{ runner.temp }}/digests
          if-no-files-found: error
```

Add `images: ${{ steps.plan.outputs.images }}` to `plan`'s outputs.

In `merge-images`:
- Change the condition to `${{ !inputs.dry-run && needs.plan.outputs.images != '[]' }}`.
- Replace the matrix with `include: ${{ fromJSON(needs.plan.outputs.images) }}`.
- Download `pattern: digest-${{ matrix.unit }}-*` with `merge-multiple: true` into `${{ runner.temp }}/digests`.
- Run the merge step with `IMAGE: ${{ matrix.image }}` and `merge-image.sh "$IMAGE" "$RUNNER_TEMP/digests/$IMAGE"`.

In `promote-images`:
- Use the same `include` matrix.
- Change the condition's `needs.plan.outputs.binaries != '[]'` to `needs.plan.outputs.images != '[]'`.
- Run `promote-image.sh "$IMAGE"` with `IMAGE: ${{ matrix.image }}`.

In `.github/workflows/images.yml`, replace the `build-image` step with `./.github/actions/build-unit-images`, passing `unit`, `dist` and `github-token` (push stays `false`).

- [ ] **Step 12: kind-up builds through build.sh and the image names**

In `scripts/kind-up.sh`, replace everything from `# the daemon and agent images, from this tree` down to the plugin loop's `done` with:

```bash
# the daemon, agent and operator images (Spec O §24.4): core's three static
# binaries from scripts/release/build.sh, each image from its own context
dist="$root/dist-core"
scripts/release/build.sh core "$musl" "$dist" >/dev/null
context() { scripts/release/image-context.sh "$1" "$2" "$root/context-$1" | sed -n 's/^context=//p'; }
secret=()
[[ -n ${GITHUB_TOKEN:-} ]] && secret=(--secret "id=github_token,env=GITHUB_TOKEN")
# bash 3.2 (macOS) calls an empty "${secret[@]}" unbound under set -u
docker build ${secret[@]+"${secret[@]}"} -t balerix:e2e -f docker/balerix/Dockerfile "$(context balerix "$dist")"
docker build --build-arg BASE=balerix:e2e -t balerix-agent:e2e -f docker/agent/Dockerfile "$(context balerix-agent "$dist")"
docker build -t balerix-operator:e2e -f docker/operator/Dockerfile "$(context balerix-operator "$dist")"
# the plugin images (Spec O §23.5): flow and web as released, a static musl
# binary on distroless (docker/plugin/Dockerfile); and the fake plugin, the
# balerix image whose entrypoint is `balerix dev fake-plugin`
for unit in flow web; do
  out="$root/dist-$unit"
  scripts/release/build.sh "$unit" "$musl" "$out" >/dev/null
  docker build -t "balerix-plugin-$unit:e2e" -f docker/plugin/Dockerfile "$(context "balerix-plugin-$unit" "$out")"
done
```

Remove the now-unused native `cargo build` lines and the operator's musl build. Keep the `musl-gcc` and `rustup target add` checks. `build.sh` needs `jq` on PATH: add `jq` to the script's tool check loop. In `.github/workflows/ci.yml`, extend the `e2e-k8s` path filter's regex `scripts/release/image-context\.sh` to `scripts/release/(image-context|build|lib)\.sh`.

- [ ] **Step 13: Lint, test, commit**

Run: `grep -rn 'unit_image\b' scripts .github || true` (expect no output), then `mise run lint && mise run release-test && mise run check`.
Expected: all pass. `kind-up` and the images themselves need docker, so CI's `images` and `e2e-k8s` jobs prove them in Task 7.

```bash
git add -A scripts/release .github/actions .github/workflows scripts/kind-up.sh
git commit -m "feat(release): core ships the balerix-agent and balerix-operator images (Spec O §24.4)"
```

---

### Task 3: The `charts` release unit (versions, pins, gate, plan)

**Files:**
- Modify: `scripts/release/lib.sh` (the charts unit, chart fields, pins, bump helpers)
- Modify: `scripts/release/prepare.sh` (gate, pin moves, charts bump, Images section)
- Modify: `scripts/release/plan.sh` (`charts=`), `scripts/release/audit.sh` (charts has nothing to audit)
- Modify: `.github/workflows/release-pr.yml` (unit `charts`), `.github/workflows/release-scripts.yml` (paths `charts/**`), `mise.toml` (`release-prepare` help)
- Test: `scripts/release/test.sh`

**Interfaces:**
- Consumes: `unit_paths`, `cliff`, `tag_exists`, `has_section` (lib.sh).
- Produces (lib.sh):
  - `CHARTS=(balerix-operator balerix-daemon)` and `DAEMON_VALUES`.
  - `yaml_field <file|-> <field>`, `chart_field <chart> <field>`, `charts_field <field>`, `set_charts_field <field> <value>`.
  - `chart_pin <plugin> [values-file]` and `set_chart_pin <plugin> <version>`.
  - `bump_level <old> <new>`, `bump_version <version> <level>`, `level_rank <level>`.
  - `unit_kind charts` is `charts`, `unit_crate charts` is `balerix-charts`, and `unit_version charts` reads `Chart.yaml`.
  - `plan.sh` prints `charts=true|false`.

- [ ] **Step 1: Write the failing tests**

In `scripts/release/test.sh`:

1. Generalise `prepare_refused` to take the unit. Its signature becomes `prepare_refused <dir> <unit> <label> <needle>...`, so the body uses `"$dir/scripts/release/prepare.sh" "$unit"` and `"${@:4}"`. Update its two `common` callers to pass `common`.
2. In `fixture`, also remove `charts/CHANGELOG.md`: `rm -rf "$dir/scripts/release" "$dir/CHANGELOG.md" "$dir"/plugins/*/CHANGELOG.md "$dir/charts/CHANGELOG.md"`.
3. Add these helpers:

```bash
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

# Releases every version the charts name, as they name them: what the
# charts gate waits for (Spec O §24.5).
release_charts_deps() {
  local dir=$1 plugin
  release "$dir" core "$(charts_field_of "$dir" appVersion)"
  for plugin in flow web matrix github; do
    release "$dir" "$plugin" "$(chart_pin_of "$dir" "$plugin")"
  done
}
```

4. Add these scenarios and call them after `scenario_plan_crates`:

```bash
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
  expect_grep "charts after core 0.3.0: the image listed" '`balerix` 0.2.0 → 0.3.0' "$dir/$(field notes "$out")"
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
```

The fixture's `appVersion` is `0.2.0`. `scenario_charts_core_minor` releases core 0.3.0 from it, and `scenario_charts_pin_only` assumes flow is pinned at 0.1.1. If the pins have moved by the time this runs, take the expected values from `chart_pin_of` rather than editing the values file. The assertions above hold for the current tree: flow 0.1.1, web 0.2.1, matrix 0.1.1, github 0.1.0, charts 0.1.0.

- [ ] **Step 2: Run them to see them fail**

Run: `mise run release-test`
Expected: FAIL. The output reads "unknown release unit: 'charts'" and "charts_field: command not found".

- [ ] **Step 3: lib.sh, the charts unit**

In `scripts/release/lib.sh`:

```bash
UNITS=(core common flow web matrix github charts)
```

- `require_unit`: add `charts` to the case.
- `unit_kind`: add `charts) echo charts ;;`.
- `unit_crate`:

```bash
unit_crate() {
  require_unit "$1"
  case $1 in
    core) echo balerix ;;
    # no crate: the release's name, for its tag and its title
    charts) echo balerix-charts ;;
    *) echo "balerix-plugin-$1" ;;
  esac
}
```

- `unit_manifest`: `charts) echo charts/balerix-operator/Chart.yaml ;;`
- `unit_changelog`: `charts) echo charts/CHANGELOG.md ;;`
- `unit_paths`: add the arm `elif [[ $1 == charts ]]; then printf '%s\n' 'charts/**'` before the plugin `else`.
- `unit_version`:

```bash
unit_version() {
  if [[ $1 == charts ]]; then charts_field version; else unit_package "$1" | jq -r .version; fi
}
```

Append the chart helpers:

```bash
# The two charts (Spec O §14.1): one version and one appVersion between them.
CHARTS=(balerix-operator balerix-daemon)
DAEMON_VALUES=charts/balerix-daemon/values.yaml

# A top-level `<field>: <value>` of a YAML file (`-` is stdin), quotes dropped.
yaml_field() {
  sed -n "s/^$2: \"\{0,1\}\([^\"]*\)\"\{0,1\}\$/\1/p" "$1"
}

chart_field() { yaml_field "charts/$1/Chart.yaml" "$2"; }

# <field> of both charts; dies when they differ (they move together).
charts_field() {
  local field=$1 chart value first=
  for chart in "${CHARTS[@]}"; do
    value=$(chart_field "$chart" "$field")
    [[ -n $value ]] || die "charts/$chart/Chart.yaml has no $field"
    [[ -z $first || $value == "$first" ]] ||
      die "charts: $field is $first in ${CHARTS[0]} but $value in $chart; they move together"
    first=$value
  done
  echo "$first"
}

# Writes <field> into both Chart.yaml files; appVersion stays a quoted string.
set_charts_field() {
  local field=$1 value=$2 chart
  [[ $field != appVersion ]] || value="\"$value\""
  for chart in "${CHARTS[@]}"; do
    sed -i "s/^$field: .*/$field: $value/" "charts/$chart/Chart.yaml"
  done
}

# plugins.<plugin>.image.tag in the daemon chart's values: the plugin version
# the chart installs (Spec O §24.5). Only the key under `plugins:` counts:
# `credentials:` has a `github:` key at the same depth.
chart_pin() {
  awk -v want="  $1:" '
    /^[^ #]/ { top = $1; inside = 0 }
    top == "plugins:" && /^  [^ #]/ { inside = ($0 == want) }
    inside && /^      tag: / { sub(/^      tag: "?/, ""); sub(/"$/, ""); print; exit }
  ' "${2:-$DAEMON_VALUES}"
}

set_chart_pin() {
  local tmp="$DAEMON_VALUES.new"
  awk -v want="  $1:" -v v="$2" '
    /^[^ #]/ { top = $1; inside = 0 }
    top == "plugins:" && /^  [^ #]/ { inside = ($0 == want) }
    inside && /^      tag: / { $0 = "      tag: \"" v "\""; inside = 0 }
    { print }
  ' "$DAEMON_VALUES" >"$tmp"
  mv "$tmp" "$DAEMON_VALUES"
  [[ $(chart_pin "$1") == "$2" ]] || die "charts: could not set plugins.$1.image.tag in $DAEMON_VALUES"
}

# major, minor or patch: the largest component that differs between two
# versions; nothing when they are equal.
bump_level() {
  local -a a b
  IFS=. read -ra a <<<"$1"
  IFS=. read -ra b <<<"$2"
  if [[ ${a[0]:-} != "${b[0]:-}" ]]; then
    echo major
  elif [[ ${a[1]:-} != "${b[1]:-}" ]]; then
    echo minor
  elif [[ ${a[2]:-} != "${b[2]:-}" ]]; then
    echo patch
  fi
}

bump_version() {
  local -a v
  IFS=. read -ra v <<<"$1"
  case $2 in
    major) echo "$((v[0] + 1)).0.0" ;;
    minor) echo "${v[0]}.$((v[1] + 1)).0" ;;
    patch) echo "${v[0]}.${v[1]}.$((v[2] + 1))" ;;
    *) die "not a bump level: '$2'" ;;
  esac
}

level_rank() {
  case ${1:-} in
    major) echo 3 ;;
    minor) echo 2 ;;
    patch) echo 1 ;;
    *) echo 0 ;;
  esac
}
```

Update the header comment's list ("crate, manifest, tag, changelog, image and include paths") to mention that `charts` is a unit with no crate.

- [ ] **Step 4: prepare.sh, gate, pins, bump and notes**

In `scripts/release/prepare.sh`:

a. Add these functions after `library_blocked`:

```bash
# The charts name core's version (appVersion) and each plugin's (its image
# tag): until every one is tagged, a release would install images that do
# not exist (Spec O §24.5). A status, not a failure, as for a library.
charts_blocked() {
  local app plugin pin missing=()
  app=$(charts_field appVersion)
  tag_exists "$(unit_tag core "$app")" || missing+=("$(unit_tag core "$app") (appVersion)")
  for plugin in "${PLUGIN_UNITS[@]}"; do
    pin=$(chart_pin "$plugin")
    [[ -n $pin ]] || die "charts: $DAEMON_VALUES has no plugins.$plugin.image.tag"
    tag_exists "$(unit_tag "$plugin" "$pin")" ||
      missing+=("$(unit_tag "$plugin" "$pin") (plugins.$plugin.image.tag)")
  done
  ((${#missing[@]})) || return 1
  echo "charts: they name versions with no release tag yet: ${missing[*]}; release those first, then charts" >&2
}

# The pins that moved since <tag>, one `<image> <old> <new>` line each: a
# core or plugin release moves them in a chore(release) commit, which
# git-cliff does not count, so they count here (Spec O §24.5).
charts_pin_changes() {
  local tag=$1 plugin old new
  old=$(git show "$tag:charts/balerix-operator/Chart.yaml" | yaml_field - appVersion)
  new=$(charts_field appVersion)
  [[ $old == "$new" ]] || echo "balerix $old $new"
  for plugin in "${PLUGIN_UNITS[@]}"; do
    old=$(chart_pin "$plugin" <(git show "$tag:$DAEMON_VALUES"))
    new=$(chart_pin "$plugin")
    [[ $old == "$new" ]] || echo "$(unit_crate "$plugin") $old $new"
  done
}

# The charts' next version: the largest of the bump their own commits ask
# and each moved pin's. A core minor is a chart minor.
charts_next() {
  local lastversion=$1 count=$2 pins=$3 level= candidate image old new
  if ((count)); then
    candidate=$(cliff charts --bumped-version)
    level=$(bump_level "$lastversion" "${candidate#"$prefix"}")
  fi
  while read -r image old new; do
    [[ -n $image ]] || continue
    candidate=$(bump_level "$old" "$new")
    if (($(level_rank "$candidate") > $(level_rank "$level"))); then level=$candidate; fi
  done <<<"$pins"
  bump_version "$lastversion" "$level"
}
```

b. Add `count=-1` and `pins=` right after `notes="$notes_dir/$unit.md"`. In the main `if` chain, add after the library arm:

```bash
elif [[ $unit == charts ]] && charts_blocked; then
  emit status none
  exit 0
```

Just before the chain, after `pins=`, add (`last` is always an existing tag or empty):

```bash
if [[ $unit == charts && -n $last ]]; then pins=$(charts_pin_changes "$last"); fi
```

c. Replace the final `else` arm's body:

```bash
else
  count=$(cliff "$unit" --unreleased --context | jq '[.[].commits[]] | length')
  if [[ $count -eq 0 && -z $pins ]]; then
    echo "$unit: nothing to release since $last" >&2
    emit status none
    exit 0
  fi
  if [[ $unit == charts ]]; then
    next=$(charts_next "${last#"$prefix"}" "$count" "$pins")
  else
    next=$(cliff "$unit" --bumped-version)
    next=${next#"$prefix"}
  fi
fi
```

d. Replace the version-writing block `if [[ $next != "$current" ]]; then … fi` with:

```bash
if [[ $next != "$current" ]]; then
  case $(unit_kind "$unit") in
    core) cargo set-version --workspace "$next" >&2 ;;
    charts) set_charts_field version "$next" ;;
    *) cargo set-version --manifest-path "plugins/$unit/Cargo.toml" "$next" >&2 ;;
  esac
fi
```

e. Inside `if [[ $unit == core ]]`, after the `CORE_PROJECTS` loop, add:

```bash
  # The charts run this core version (Spec O §24.5).
  set_charts_field appVersion "$next"
```

Inside `if [[ $(unit_kind "$unit") == plugin ]]`, after the `balerix-plugin.yaml` sed, add:

```bash
  # The daemon chart installs this plugin version (Spec O §24.5).
  set_chart_pin "$unit" "$next"
```

f. After `cliff "$unit" --unreleased --tag "$tag" --strip header >"$notes"`, add:

```bash
if [[ $unit == charts ]]; then
  # A pin-only release has no commit; the template's "Initial release." is
  # wrong for it: the moved images are the change.
  [[ $count -ne 0 ]] || sed -i '/^- Initial release\.$/d' "$notes"
  if [[ -n $pins ]]; then
    {
      printf '\n### Images\n\n'
      while read -r image old new; do
        printf -- '- `%s` %s → %s\n' "$image" "$old" "$new"
      done <<<"$pins"
    } >>"$notes"
  fi
fi
```

The prepend that follows copies `$notes` into the changelog, so the Images block lands in `charts/CHANGELOG.md` too.

- [ ] **Step 5: plan.sh and audit.sh**

`plan.sh`: add `charts=false`. In the `case`, add `charts) charts=true ;;`. Echo `charts=$charts` after `core=`, and document it in the header comment.

`audit.sh`: as the first line inside the loop, after `require_unit`:

```bash
  if [[ $unit == charts ]]; then echo "charts: no Rust dependencies to audit" >&2; continue; fi
```

- [ ] **Step 6: Run release-test**

Run: `mise run release-test`
Expected: `release scripts: all checks passed`. A failing pin scenario usually means the awk `top` tracking is off. Print `chart_pin github charts/balerix-daemon/values.yaml` from a sourced shell; it must print the github tag, not an empty line.

- [ ] **Step 7: Workflows and the task help**

`.github/workflows/release-pr.yml`: add `charts` to the `unit` choice options and to the default matrix list, after `github`.
`.github/workflows/release-scripts.yml`: add `- charts/**` to both `paths` lists.
`mise.toml` `[tasks.release-prepare]`: the arg help becomes `"core, common, flow, web, matrix, github or charts"`.

- [ ] **Step 8: Lint, check, commit**

Run: `mise run lint && mise run release-test && mise run check`

```bash
git add scripts/release .github/workflows/release-pr.yml .github/workflows/release-scripts.yml mise.toml
git commit -m "feat(release): the charts release unit, gated on the versions it pins (Spec O §24.5)"
```

---

### Task 4: Publishing the charts

**Files:**
- Create: `scripts/release/stage-charts.sh`, `scripts/release/package-charts.sh`, `scripts/release/check-charts.sh`, `scripts/release/push-charts.sh`, `scripts/release/index-charts.sh`, `scripts/release/verify-charts.sh`
- Modify: `scripts/release/lib.sh` (`flag_prerelease`), `scripts/release/verify-package.sh` (uses it), `scripts/release/github-release.sh` (charts notes)
- Modify: `scripts/kind-up.sh` (`cluster` mode), `mise.toml` (`kind-up` help)
- Modify: `.github/workflows/release.yml` (jobs `charts`, `charts-publish`, `charts-index`, `charts-verify`; `github-release` needs and checksums)
- Test: `scripts/release/test.sh` (`scenario_stage_charts`, `scenario_stage_charts_fork`)

**Interfaces:**
- Consumes: `CHARTS`, `charts_field`, `unit_version charts`, `unit_tag` (Task 3).
- Produces:
  - `stage-charts.sh <out>` prints the staged chart dirs.
  - `package-charts.sh <out>` writes `<out>/<chart>-<ver>.tgz` and prints the paths.
  - `check-charts.sh <archives>`.
  - `push-charts.sh <archives>` prints `<chart>=<ref>@<digest>` lines.
  - `index-charts.sh <archives> <helm-charts-dir>`.
  - `verify-charts.sh [--flag]`.
  - `flag_prerelease <tag> <marker> <advice>` (lib.sh).
  - `kind-up.sh cluster`.

- [ ] **Step 1: Write the failing tests**

In `scripts/release/test.sh`, add and call after `scenario_plan_charts`:

```bash
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
```

- [ ] **Step 2: Run them to see them fail**

Run: `mise run release-test`
Expected: FAIL (`stage-charts.sh: No such file or directory`).

- [ ] **Step 3: stage-charts.sh**

Create `scripts/release/stage-charts.sh` (mode 0755):

```bash
#!/usr/bin/env bash
# Copies both charts to <out-dir> as they will be published (Spec O §24.5).
# On a fork (GITHUB_REPOSITORY_OWNER other than balerix-ai) every image
# repository in the values becomes the fork's, so a fork's charts install
# its own images. The operator names the daemon and agent images itself
# (ghcr.io/balerix-ai/… at its own version) when `images` is empty, so a
# fork's operator chart sets both to the fork's at the charts' appVersion.
#
# usage: stage-charts.sh <out-dir>
# Prints the staged chart directories, one per line.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <out-dir>"
out=$1
owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
owner=${owner,,}

rm -rf "$out"
mkdir -p "$out"
for chart in "${CHARTS[@]}"; do cp -R "charts/$chart" "$out/$chart"; done

if [[ $owner != balerix-ai ]]; then
  app=$(charts_field appVersion)
  for chart in "${CHARTS[@]}"; do
    sed -i "s|ghcr\.io/balerix-ai/|ghcr.io/$owner/|g" "$out/$chart/values.yaml"
  done
  values="$out/balerix-operator/values.yaml"
  sed -i \
    -e "s|^  daemon: \"\"\$|  daemon: \"ghcr.io/$owner/balerix:$app\"|" \
    -e "s|^  agent: \"\"\$|  agent: \"ghcr.io/$owner/balerix-agent:$app\"|" \
    "$values"
  grep -qx "  daemon: \"ghcr.io/$owner/balerix:$app\"" "$values" || die "charts: could not set images.daemon in $values"
  grep -qx "  agent: \"ghcr.io/$owner/balerix-agent:$app\"" "$values" || die "charts: could not set images.agent in $values"
fi

for chart in "${CHARTS[@]}"; do echo "$out/$chart"; done
```

Run: `mise run release-test`. Expected: the stage scenarios pass.

- [ ] **Step 4: package-charts.sh**

Create `scripts/release/package-charts.sh` (0755):

```bash
#!/usr/bin/env bash
# Packages both charts as stage-charts.sh stages them (Spec O §24.5 step 2):
# <out-dir>/<chart>-<version>.tgz each. The shared github-release job writes
# SHA256SUMS over them and attests them.
#
# usage: package-charts.sh <out-dir>     needs helm on PATH
# Prints the archive paths, one per line.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <out-dir>"
out=$1
version=$(unit_version charts)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mapfile -t dirs < <(scripts/release/stage-charts.sh "$stage/charts")
mkdir -p "$out"
for dir in "${dirs[@]}"; do
  helm lint --strict "$dir" >&2
  helm package "$dir" --destination "$out" >&2
  archive="$out/$(basename "$dir")-$version.tgz"
  [[ -f $archive ]] || die "charts: helm package wrote no $archive"
  echo "$archive"
done
```

Run: `mise x helm@4.3.0 -- scripts/release/package-charts.sh target/tmp/charts-pkg && tar -tzf target/tmp/charts-pkg/balerix-daemon-0.1.0.tgz | head`
Expected: two paths, and the daemon archive lists `balerix-daemon/Chart.yaml`. If `mise x helm@4.3.0` hits EACCES under `~/.local/share/mise`, use the 5a workaround: a scratch helm 4.3.0 on PATH, from https://get.helm.sh/helm-v4.3.0-linux-amd64.tar.gz checked against its `.sha256sum`. Don't override `MISE_DATA_DIR`.

- [ ] **Step 5: kind-up's cluster mode**

In `scripts/kind-up.sh`:
- The usage becomes `kind-up.sh [up|cluster|down]`. The header says `cluster` creates the cluster and the shared class only, with no images: the charts release check installs the published images.
- Make the tool check depend on the mode:

```bash
mode=${1:-up}
tools=(kind kubectl docker)
[[ $mode != up ]] || tools+=(cargo rustup jq)
for tool in "${tools[@]}"; do
  command -v "$tool" >/dev/null || { echo "kind-up: $tool is not on PATH" >&2; exit 2; }
done

case "$mode" in
  down) …unchanged… ;;
  up | cluster) ;;
  *) echo "usage: $0 [up|cluster|down]" >&2; exit 2 ;;
esac
```

- After the `rollout status` line, add:

```bash
if [[ $mode == cluster ]]; then
  echo "kind-up: cluster $name ready, no images loaded; KUBECONFIG=$KUBECONFIG"
  exit 0
fi
```

In `mise.toml`, the `kind-up` usage help becomes `"up, cluster (no images) or down"`.

- [ ] **Step 6: check-charts.sh**

Create `scripts/release/check-charts.sh` (0755):

```bash
#!/usr/bin/env bash
# Installs the packaged charts on the kind cluster `kind-up.sh cluster` made,
# with the published images they pin (Spec O §24.5 step 1): the operator
# chart cluster-wide into balerix-system, the daemon chart with flow and web
# enabled. Passes when the Daemon and both Plugins are Ready. No Fleet: one
# needs claude credentials (scripts/verify-k8s.sh covers it). The images
# must be pullable without credentials: public on ghcr.
#
# usage: check-charts.sh <archives-dir>     needs helm, kubectl
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <archives-dir>"
archives=$1
version=$(unit_version charts)
export KUBECONFIG="${KUBECONFIG:-${CARGO_TARGET_DIR:-target}/tmp/kind/kubeconfig}"
ns=charts-check

diagnose() {
  echo "----- charts check failed; the cluster as it stands -----"
  kubectl get daemons.balerix.ai,plugins.balerix.ai,pods,jobs,pvc -A -o wide || true
  kubectl -n "$ns" describe daemons.balerix.ai,plugins.balerix.ai || true
  kubectl -n balerix-system logs deploy/balerix-operator --tail=200 || true
  kubectl get events -A --sort-by=.lastTimestamp | tail -n 60 || true
}
trap 'diagnose >&2' ERR

helm upgrade --install balerix-operator "$archives/balerix-operator-$version.tgz" \
  --namespace balerix-system --create-namespace --wait --timeout 5m
helm upgrade --install balerix "$archives/balerix-daemon-$version.tgz" \
  --namespace "$ns" --create-namespace \
  --set name=default --set plugins.flow.enabled=true --set plugins.web.enabled=true \
  --wait --timeout 5m
# the pool Job installs claude and gh in the cluster first: minutes
kubectl -n "$ns" wait daemons.balerix.ai/default --for=condition=Ready --timeout=20m
kubectl -n "$ns" wait plugins.balerix.ai/flow plugins.balerix.ai/web --for=condition=Ready --timeout=10m
echo "charts: balerix-charts $version installs; the Daemon and the flow and web Plugins are Ready" >&2
```

- [ ] **Step 7: push-charts.sh, index-charts.sh, verify-charts.sh and flag_prerelease**

Add to `lib.sh`:

```bash
# Flags <tag>'s GitHub Release as a prerelease, putting <marker>, this
# runner's architecture, a link to this run and <advice> at the top of its
# notes; once (Spec I §7.3).
flag_prerelease() {
  local tag=$1 marker=$2 advice=$3 body run file
  body=$(gh release view "$tag" --json body --jq .body)
  if grep -qF "$marker" <<<"$body"; then return 0; fi
  run="${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY/actions/runs/${GITHUB_RUN_ID:-}"
  file="${RUNNER_TEMP:-/tmp}/notes-$tag.md"
  printf '%s on %s ([run](%s)); %s\n\n%s\n' "$marker" "$(uname -m)" "$run" "$advice" "$body" >"$file"
  gh release edit "$tag" --prerelease --notes-file "$file"
}
```

In `verify-package.sh`, replace the `--flag` block's body with `flag_prerelease "$tag" "> **Package verification failed**" "do not install this release."` followed by `exit 0`. The notes come out the same as today.

Create `scripts/release/push-charts.sh` (0755):

```bash
#!/usr/bin/env bash
# Pushes both chart archives to oci://ghcr.io/<owner>/charts and cosign-signs
# each by digest (Spec O §24.5 step 3). An archive the registry already holds
# pushes again as a no-op there; it is signed again (a second signature on a
# re-run is harmless). helm and cosign must be logged in to ghcr.io.
#
# usage: push-charts.sh <archives-dir>
# Prints <chart>=<repository>@<digest> lines.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 ]] || die "usage: $0 <archives-dir>"
archives=$1
version=$(unit_version charts)
owner=${GITHUB_REPOSITORY_OWNER:-balerix-ai}
registry="ghcr.io/${owner,,}/charts"

for chart in "${CHARTS[@]}"; do
  archive="$archives/$chart-$version.tgz"
  [[ -f $archive ]] || die "charts: no $archive"
  out=$(helm push "$archive" "oci://$registry" 2>&1) || die "helm push $archive: $out"
  printf '%s\n' "$out" >&2
  digest=$(sed -n 's/^Digest: \(sha256:[0-9a-f]\{64\}\)$/\1/p' <<<"$out")
  [[ -n $digest ]] || die "$chart: helm push printed no digest: $out"
  cosign sign --yes "$registry/$chart@$digest"
  echo "$chart=$registry/$chart@$digest"
done
```

Create `scripts/release/index-charts.sh` (0755):

```bash
#!/usr/bin/env bash
# Adds both charts' release to <owner>/helm-charts' index.yaml and pushes it
# (Spec O §14.3 step 5, §24.5). It runs after the GitHub Release, whose
# asset URLs the index names. An index that already lists both archives is
# left alone, so a re-run makes no second commit. Nobody edits it by hand.
#
# usage: index-charts.sh <archives-dir> <helm-charts-checkout>
#   GITHUB_REPOSITORY names this repository; GH_TOKEN can push to helm-charts.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <archives-dir> <helm-charts-checkout>"
archives=$1
pages=$2
: "${GITHUB_REPOSITORY:?}"
version=$(unit_version charts)
tag=$(unit_tag charts "$version")
base="https://github.com/$GITHUB_REPOSITORY/releases/download/$tag"
index="$pages/index.yaml"

listed=0
for chart in "${CHARTS[@]}"; do
  if [[ -f $index ]] && grep -qF "$base/$chart-$version.tgz" "$index"; then listed=$((listed + 1)); fi
done
if ((listed == ${#CHARTS[@]})); then
  echo "charts: $index already lists $tag" >&2
  exit 0
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
for chart in "${CHARTS[@]}"; do cp "$archives/$chart-$version.tgz" "$stage/"; done
merge=()
[[ ! -f $index ]] || merge=(--merge "$index")
helm repo index "$stage" --url "$base" "${merge[@]}"
cp "$stage/index.yaml" "$index"

git -C "$pages" add index.yaml
git -C "$pages" -c user.name="github-actions[bot]" \
  -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit -q -m "chore: index $tag"
: "${GH_TOKEN:?GH_TOKEN must be the release App token}"
git -C "$pages" push -q "https://x-access-token:${GH_TOKEN}@github.com/${GITHUB_REPOSITORY%%/*}/helm-charts.git" HEAD:main
echo "charts: indexed $tag in ${GITHUB_REPOSITORY%%/*}/helm-charts" >&2
```

Create `scripts/release/verify-charts.sh` (0755):

```bash
#!/usr/bin/env bash
# Pulls both charts through the published index, the way a user adds the
# repository, and checks them against the release's SHA256SUMS (Spec O
# §14.3 step 6). GitHub Pages serves an index commit a minute or more after
# the push, so it retries for ten minutes before failing.
#
# usage: verify-charts.sh           verify
#        verify-charts.sh --flag    mark the charts release as a prerelease
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -le 1 ]] || die "usage: $0 [--flag]"
: "${GITHUB_REPOSITORY:?}"
version=$(unit_version charts)
tag=$(unit_tag charts "$version")
if [[ ${1:-} == --flag ]]; then
  flag_prerelease "$tag" "> **Chart verification failed**" "do not install these charts."
  exit 0
fi
[[ $# -eq 0 ]] || die "usage: $0 [--flag]"

owner=${GITHUB_REPOSITORY%%/*}
url="https://${owner,,}.github.io/helm-charts"
work=$(mktemp -d "${RUNNER_TEMP:-/tmp}/verify-charts-XXXXXX")
export HELM_CONFIG_HOME="$work/config" HELM_CACHE_HOME="$work/cache" HELM_DATA_HOME="$work/data"
gh release download "$tag" --repo "$GITHUB_REPOSITORY" --pattern SHA256SUMS --dir "$work"
for chart in "${CHARTS[@]}"; do
  grep -q " $chart-$version\.tgz\$" "$work/SHA256SUMS" || die "charts: $tag's SHA256SUMS lists no $chart-$version.tgz"
done

pull_all() {
  helm repo add --force-update balerix "$url" >/dev/null 2>&1 || return 1
  helm repo update balerix >/dev/null 2>&1 || return 1
  rm -rf "$work/pulled"
  mkdir "$work/pulled"
  local chart
  for chart in "${CHARTS[@]}"; do
    helm pull "balerix/$chart" --version "$version" --destination "$work/pulled" >/dev/null 2>&1 || return 1
  done
}
for attempt in $(seq 20); do
  if pull_all; then break; fi
  ((attempt < 20)) || die "charts: $url never offered $tag's charts"
  echo "charts: $url does not offer $version yet; retrying in 30 s" >&2
  sleep 30
done
(cd "$work/pulled" && sha256sum --check --ignore-missing ../SHA256SUMS)
echo "charts: $url serves $tag's archives as released" >&2
```

- [ ] **Step 8: github-release.sh's charts notes**

In `scripts/release/github-release.sh`:
- Change the Verify block's condition (Task 2) from `$kind != library` to `$kind == core || $kind == plugin`.
- Add a charts block before it:

````bash
if [[ $kind == charts ]]; then
  owner=${GITHUB_REPOSITORY%%/*}
  owner=${owner,,}
  cat >>"$notes" <<EOF

### Install

\`\`\`sh
helm repo add balerix https://$owner.github.io/helm-charts
helm install balerix-operator balerix/balerix-operator --version $version \\
  --namespace balerix-system --create-namespace
helm install balerix balerix/balerix-daemon --version $version --namespace <namespace>
\`\`\`

Or from \`oci://ghcr.io/$owner/charts/balerix-operator\` and \`oci://ghcr.io/$owner/charts/balerix-daemon\`.

### Verify

\`\`\`sh
gh attestation verify balerix-operator-$version.tgz --repo $GITHUB_REPOSITORY
sha256sum --check --ignore-missing SHA256SUMS
cosign verify ghcr.io/$owner/charts/balerix-operator:$version \\
  --certificate-identity https://github.com/$GITHUB_REPOSITORY/.github/workflows/release.yml@refs/heads/main \\
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
\`\`\`
EOF
fi
````

- [ ] **Step 9: Exercise the index and verify scripts locally**

No network publishing. In the scratchpad:
1. Package the charts (`package-charts.sh "$S/arch"`).
2. `git init -q -b main "$S/pages"`.
3. Run `GITHUB_REPOSITORY=example/balerix GH_TOKEN=x mise x helm@4.3.0 -- scripts/release/index-charts.sh "$S/arch" "$S/pages"` with `git` push pointed at a local bare remote. To do that, temporarily export `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=url.$S/remote.git.insteadOf GIT_CONFIG_VALUE_0=https://x-access-token:x@github.com/example/helm-charts.git` after `git init -q --bare "$S/remote.git"`.
4. Check `index.yaml` lists both charts with URLs under `https://github.com/example/balerix/releases/download/balerix-charts-v0.1.0/`.
5. Run it a second time. Expected: "already lists" and `git -C "$S/pages" log --oneline | wc -l` is still 1.

Record the commands and outputs for the PR body. `verify-charts.sh` needs Pages, so the fork rehearsal proves it, not this step. Run `bash -n` and shellcheck on it here.

- [ ] **Step 10: release.yml's charts jobs**

In `.github/workflows/release.yml`:

a. Add `charts: ${{ steps.plan.outputs.charts }}` to `plan`'s outputs.

b. Add after `package`:

```yaml
  charts:
    # Spec O §24.5 steps 1 and 2: the archives as they will be published (a
    # fork's carry its own image repositories), linted, rendered and
    # validated (§24.3), and installed on kind with the published images.
    needs: plan
    if: needs.plan.outputs.charts == 'true'
    runs-on: ubuntu-24.04
    timeout-minutes: 60
    permissions:
      contents: read
    env:
      BALERIX_REQUIRE_TOOLS: "1"
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: >-
          mise install rust jq cargo:cargo-nextest helm@4.3.0 kind@0.33.0 kubectl@1.34.12
          github:kubernetes-sigs/controller-tools@envtest-v1.34.1
      - name: Package
        run: mise x helm@4.3.0 -- scripts/release/package-charts.sh "$RUNNER_TEMP/charts"
      - name: Lint, render and validate
        run: mise run charts
      - name: A kind cluster with the shared class
        run: mise run kind-up cluster
      - name: Install the archives with the published images
        run: mise x helm@4.3.0 kubectl@1.34.12 -- scripts/release/check-charts.sh "$RUNNER_TEMP/charts"
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: archive-charts-helm
          path: ${{ runner.temp }}/charts/*.tgz
          if-no-files-found: error

  charts-publish:
    # Step 3: OCI, signed by digest. Never on a dry run.
    needs: [plan, charts]
    if: ${{ !inputs.dry-run && needs.plan.outputs.charts == 'true' }}
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      packages: write
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
      - run: mise install jq cosign helm@4.3.0
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          name: archive-charts-helm
          path: ${{ runner.temp }}/charts
      - uses: docker/login-action@dbcb813823bdd20940b903addbd779551569679f # v4.6.0
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - name: Log helm in to ghcr
        env:
          ACTOR: ${{ github.actor }}
          TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: mise x helm@4.3.0 -- helm registry login ghcr.io --username "$ACTOR" --password-stdin <<<"$TOKEN"
      - run: mise x helm@4.3.0 -- scripts/release/push-charts.sh "$RUNNER_TEMP/charts"
```

c. In `github-release`:
- Add `charts, charts-publish` to `needs`.
- Change the package download's condition to `${{ contains(fromJSON(needs.plan.outputs.plugins), matrix.unit) }}`.
- Change the Checksums step's `run` to:

```yaml
        run: |
          shopt -s nullglob
          sha256sum -- *.tar.gz *.tgz >SHA256SUMS
```

The `archive-${{ matrix.unit }}-*` pattern already matches `archive-charts-helm`.

d. Add after `github-release`:

```yaml
  charts-index:
    # Step 5, after the tag: the index names the release's asset URLs.
    needs: [plan, github-release]
    if: ${{ !cancelled() && needs.github-release.result == 'success' && needs.plan.outputs.charts == 'true' }}
    runs-on: ubuntu-24.04
    environment: release-bot
    permissions:
      contents: read
    steps:
      - id: app
        uses: actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1 # v3.2.0
        with:
          client-id: ${{ vars.RELEASE_APP_CLIENT_ID }}
          private-key: ${{ secrets.RELEASE_APP_PRIVATE_KEY }}
          owner: ${{ github.repository_owner }}
          repositories: helm-charts
          permission-contents: write
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          repository: ${{ github.repository_owner }}/helm-charts
          token: ${{ steps.app.outputs.token }}
          path: helm-charts
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install jq helm@4.3.0
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          name: archive-charts-helm
          path: ${{ runner.temp }}/charts
      - env:
          GH_TOKEN: ${{ steps.app.outputs.token }}
        run: mise x helm@4.3.0 -- scripts/release/index-charts.sh "$RUNNER_TEMP/charts" helm-charts

  charts-verify:
    needs: [plan, charts-index]
    if: ${{ !cancelled() && needs.charts-index.result == 'success' }}
    runs-on: ubuntu-24.04
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: jdx/mise-action@c2a87611a18de5b3828c5652fe268e992400cb5c # v4.3.0
        with:
          version: 2026.9.2
          install: false
          cache: false
      - run: mise install jq gh helm@4.3.0
      - env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: mise x helm@4.3.0 -- scripts/release/verify-charts.sh
      - if: failure()
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: mise x helm@4.3.0 -- scripts/release/verify-charts.sh --flag
```

- [ ] **Step 11: Lint, test, commit**

Run: `mise run lint && mise run release-test && mise run check`
Expected: all pass. zizmor and actionlint are part of `lint`. Fix their findings. Don't suppress them, unless an existing job in the file carries the same suppression for the same reason.

```bash
git add -A scripts/release scripts/kind-up.sh mise.toml .github/workflows/release.yml
git commit -m "feat(release): publish the charts to OCI and the helm-charts index (Spec O §24.5)"
```

---

### Task 5: `mise run verify-k8s`

**Files:**
- Create: `scripts/verify-k8s.sh`
- Modify: `mise.toml` (task `verify-k8s`)

**Interfaces:**
- Consumes: `charts/` (tree mode), the published index (index mode), the CRDs `*.balerix.ai/v1alpha1`. Objects are named `<fleet>-<crew>-<agent>`, and the harvest Job is `<agent object>-harvest`. The pod's agent container has tmux at socket `/balerix/run/tmux.sock`, with a window named after the agent, and the agent's HOME under `/balerix/agent`.
- Produces: `mise run verify-k8s [-- --from tree|index | -- down]`.

- [ ] **Step 1: Write the script**

Create `scripts/verify-k8s.sh` (0755):

```bash
#!/usr/bin/env bash
# The by-hand check of Spec O §24.6 with the real `claude`: both charts on
# the cluster KUBECONFIG names, from this tree (--from tree, the default)
# or from the published index (--from index); a Fleet of
# examples/payments.yaml's shape; and the e2e journey's checks walked with a
# real Claude. Prints a report to paste back. `down` removes what it made.
# Not part of any CI tier.
#
# The Fleet is examples/payments.yaml with three changes, kept in step with
# that file by hand: the repo is a git server pod in the namespace (no
# GitHub access needed), git push is off, and the runner is a pod.
#
# Needs: KUBECONFIG; a ReadWriteMany storage class, BALERIX_VERIFY_SHARED_CLASS
# (default: the cluster's default class); your claude login,
# ~/.claude/.credentials.json or BALERIX_VERIFY_CLAUDE_CREDENTIALS. For
# --from index: BALERIX_VERIFY_OWNER (default balerix-ai; the index at
# https://<owner>.github.io/helm-charts) and optionally
# BALERIX_VERIFY_VERSION (default the newest). The cluster pulls the
# published images: they must be public. The operator release watches only
# this script's namespace; the definitions it installs are cluster-wide, so
# use a cluster with no other balerix operator. Nothing secret is printed.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$REPO/target/tmp/verify-k8s"
SYSTEM=balerix-verify-system
NS=balerix-verify
FROM=tree
MODE=up
CREDS="${BALERIX_VERIFY_CLAUDE_CREDENTIALS:-$HOME/.claude/.credentials.json}"
OWNER="${BALERIX_VERIFY_OWNER:-balerix-ai}"
CLASS="${BALERIX_VERIFY_SHARED_CLASS:-}"
PROMPT="Reply with the single word: ready."
FLOW_TEXT="Run the tests and fix any failures."
FAILED=()

while (($#)); do
  case $1 in
    --from) FROM=${2:-}; shift 2 ;;
    down) MODE=down; shift ;;
    *) echo "usage: $0 [--from tree|index] | down" >&2; exit 2 ;;
  esac
done
[[ $FROM == tree || $FROM == index ]] || { echo "--from is tree or index" >&2; exit 2; }

say() { printf '%s\n' "$*"; }
hr() { say "----- $* -----"; }
ok() { say "PASS $*"; }
bad() { say "FAIL $*"; FAILED+=("$*"); }
k() { kubectl "$@"; }
kn() { kubectl -n "$NS" "$@"; }
# wait_until <seconds> <command…>: polls every 5 s
wait_until() {
  local deadline=$((SECONDS + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}
in_agent() { kn exec "$1" -c agent -- sh -c "$2"; }

if [[ $MODE == down ]]; then
  hr "down"
  kn delete fleets.balerix.ai --all --wait --timeout=10m 2>/dev/null || true
  helm uninstall balerix -n "$NS" 2>/dev/null || true
  k delete namespace "$NS" --wait --timeout=10m 2>/dev/null || true
  helm uninstall balerix-operator -n "$SYSTEM" 2>/dev/null || true
  k delete namespace "$SYSTEM" --wait --timeout=5m 2>/dev/null || true
  say "the CustomResourceDefinitions stay (helm.sh/resource-policy: keep);"
  say "delete them with: kubectl delete crd agents.balerix.ai crews.balerix.ai daemons.balerix.ai fleets.balerix.ai plugins.balerix.ai"
  rm -rf "$ROOT"
  exit 0
fi

hr "preflight"
for t in kubectl helm jq; do command -v "$t" >/dev/null || { say "missing tool on PATH: $t"; exit 2; }; done
k version 2>/dev/null | sed 's/^/  /' || { say "kubectl cannot reach the cluster in KUBECONFIG=${KUBECONFIG:-~/.kube/config}"; exit 2; }
[[ $(k auth can-i create customresourcedefinitions) == yes ]] || { say "this account cannot create CustomResourceDefinitions"; exit 2; }
[[ -f $CREDS ]] || { say "no claude credentials at $CREDS (log in with claude, or set BALERIX_VERIFY_CLAUDE_CREDENTIALS)"; exit 2; }
if [[ -n $CLASS ]]; then
  k get storageclass "$CLASS" >/dev/null || { say "no storage class $CLASS"; exit 2; }
else
  say "shared class: the cluster's default ($(k get storageclass -o json | jq -r '[.items[] | select(.metadata.annotations["storageclass.kubernetes.io/is-default-class"] == "true") | .metadata.name] | join(",")'));"
  say "  it must serve ReadWriteMany; set BALERIX_VERIFY_SHARED_CLASS otherwise"
fi
rm -rf "$ROOT" && mkdir -p "$ROOT"

hr "A. charts (--from $FROM)"
version_args=()
if [[ $FROM == tree ]]; then
  op_chart="$REPO/charts/balerix-operator"
  daemon_chart="$REPO/charts/balerix-daemon"
else
  export HELM_CONFIG_HOME="$ROOT/helm/config" HELM_CACHE_HOME="$ROOT/helm/cache" HELM_DATA_HOME="$ROOT/helm/data"
  helm repo add balerix-verify "https://${OWNER,,}.github.io/helm-charts" >/dev/null || { say "cannot add the index for $OWNER"; exit 2; }
  helm repo update balerix-verify >/dev/null
  op_chart=balerix-verify/balerix-operator
  daemon_chart=balerix-verify/balerix-daemon
  [[ -z ${BALERIX_VERIFY_VERSION:-} ]] || version_args=(--version "$BALERIX_VERIFY_VERSION")
fi
helm upgrade --install balerix-operator "$op_chart" "${version_args[@]}" \
  --namespace "$SYSTEM" --create-namespace --set "watchNamespaces={$NS}" --wait --timeout 5m ||
  { say "the operator chart did not install"; exit 1; }
helm list -n "$SYSTEM" | sed 's/^/  /'
operator_json=$(k -n "$SYSTEM" get deploy balerix-operator -o json)
say "  operator image: $(jq -r '.spec.template.spec.containers[0].image' <<<"$operator_json")"
app=$(helm list -n "$SYSTEM" -o json | jq -r '.[] | select(.name == "balerix-operator") | .app_version')
agent_image=$(jq -r '.spec.template.spec.containers[0].args as $a | ($a | index("--agent-image")) as $i | if $i then $a[$i + 1] else empty end' <<<"$operator_json")
agent_image=${agent_image:-ghcr.io/balerix-ai/balerix-agent:$app}
say "  agent image: $agent_image"

k create namespace "$NS" --dry-run=client -o yaml | k apply -f - >/dev/null
kn create secret generic claude-credentials --from-file=credentials.json="$CREDS" \
  --dry-run=client -o yaml | kn apply -f - >/dev/null

# the repository: a git server pod seeded with one commit, as the e2e journey's
kn apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: { name: git, labels: { app: git } }
spec:
  containers:
    - name: git
      image: $agent_image
      command: [bash, -c, "set -e; git init -q --bare /srv/repo.git; exec git daemon --base-path=/srv --export-all --enable=receive-pack --reuseaddr --listen=0.0.0.0 /srv"]
      ports: [{ containerPort: 9418 }]
      volumeMounts: [{ name: srv, mountPath: /srv }, { name: tmp, mountPath: /tmp }]
  volumes: [{ name: srv, emptyDir: {} }, { name: tmp, emptyDir: {} }]
---
apiVersion: v1
kind: Service
metadata: { name: git }
spec: { selector: { app: git }, ports: [{ port: 9418, targetPort: 9418 }] }
EOF
kn wait pod/git --for=condition=Ready --timeout=5m >/dev/null || { say "the git pod never became Ready"; exit 1; }
kn exec git -- bash -c 'set -e; cd /tmp && rm -rf w && git clone -q /srv/repo.git w && cd w && echo hi > README && git add . && git -c user.name=t -c user.email=t@t commit -qm init && git push -q origin HEAD:main' ||
  { say "could not seed the repository"; exit 1; }

jq -n --arg class "$CLASS" '{
  name: "default",
  credentials: { claude: { secretName: "claude-credentials" } },
  storage: { shared: { storageClassName: $class } },
  plugins: { flow: { enabled: true }, web: { enabled: true } }
}' >"$ROOT/daemon-values.json"
helm upgrade --install balerix "$daemon_chart" "${version_args[@]}" --namespace "$NS" \
  -f "$ROOT/daemon-values.json" --wait --timeout 5m || { say "the daemon chart did not install"; exit 1; }
helm list -n "$NS" | sed 's/^/  /'

hr "B. Daemon and Plugins"
if kn wait daemons.balerix.ai/default --for=condition=Ready --timeout=20m >/dev/null; then ok "Daemon Ready"; else bad "Daemon Ready"; fi
if kn wait plugins.balerix.ai/flow plugins.balerix.ai/web --for=condition=Ready --timeout=10m >/dev/null; then ok "flow and web Ready"; else bad "flow and web Ready"; fi

hr "C. the Fleet, real claude in two pods"
kn apply -f - >/dev/null <<EOF
apiVersion: balerix.ai/v1alpha1
kind: Fleet
metadata: { name: payments }
spec:
  daemon: default
  retain: None
  defaults:
    claude:
      settings: { model: sonnet, permissions: { allow: ["Bash(git *)"] } }
      args: ["--verbose"]
      resume: true
    sandbox:
      network: { block: false }
    tools: { node: "22.11.0" }
    env: { RUST_LOG: info }
    runner: { type: pod }
    plugins:
      web: {}
  crews:
    backend:
      repo: git://git.$NS.svc:9418/repo.git
      ref: main
      git: { push: false, auth: none }
      defaults:
        tools: { python: "3.12.8" }
      agents:
        alice:
          plugins:
            flow:
              initial: working
              states:
                working:
                  on:
                    - event: PreToolUse
                      match: { /tool_input/command: "rm -rf.*" }
                      respond: { decision: block, reason: "no recursive deletes" }
                    - event: Stop
                      goto: review
                      send: { text: "$FLOW_TEXT" }
                review:
                  on:
                    - event: Stop
                      goto: done
                done: {}
        bob:
          claude: { settings: { model: opus } }
EOF
if kn wait fleets.balerix.ai/payments --for=condition=Ready --timeout=30m >/dev/null; then
  ok "Fleet Ready: both agents' SessionStart arrived from the pods"
else
  bad "Fleet Ready"
  kn get fleets.balerix.ai,crews.balerix.ai,agents.balerix.ai,pods,jobs -o wide
fi
kn get agents.balerix.ai -o custom-columns=NAME:.metadata.name,PHASE:.status.phase,READY:'.status.conditions[?(@.type=="Ready")].status' | sed 's/^/  /'

hr "D. a flow rule on alice's Stop"
alice=payments-backend-alice
window=$(in_agent "$alice" "tmux -u -S /balerix/run/tmux.sock list-windows -a -F '#{window_id} #{window_name}'" 2>/dev/null | awk '$2 == "alice" { print $1; exit }')
transcript_has() { in_agent "$alice" "grep -rlF '$1' /balerix/agent --include='*.jsonl' 2>/dev/null | head -n 1" | grep -q .; }
if [[ -z $window ]]; then
  bad "alice's tmux window not found"
else
  in_agent "$alice" "tmux -u -S /balerix/run/tmux.sock send-keys -t '$window' -l '$PROMPT'"
  # closed loop (#99): Claude's TUI can swallow an Enter while it starts
  submitted=false
  for _ in $(seq 12); do
    in_agent "$alice" "tmux -u -S /balerix/run/tmux.sock send-keys -t '$window' Enter"
    if wait_until 10 transcript_has "$PROMPT"; then submitted=true; break; fi
  done
  if ! $submitted; then
    bad "the prompt never reached alice's transcript"
  elif wait_until 300 transcript_has "$FLOW_TEXT"; then
    ok "flow sent '$FLOW_TEXT' after alice's first Stop"
  else
    bad "flow's text never reached alice's transcript"
  fi
fi

hr "E. evict alice: same claim, Ready again, home and session intact"
in_agent "$alice" "touch /balerix/agent/verify-k8s-marker" || true
uid=$(kn get pod "$alice" -o jsonpath='{.metadata.uid}')
kn delete pod "$alice" --wait=false >/dev/null
new_ready() {
  [[ $(kn get pod "$alice" -o jsonpath='{.metadata.uid}') != "$uid" ]] &&
    kn wait pod "$alice" --for=condition=Ready --timeout=1s &&
    kn wait agents.balerix.ai/"$alice" --for=condition=Ready --timeout=1s
}
if wait_until 900 new_ready; then
  ok "alice's new pod Ready"
  if in_agent "$alice" "test -f /balerix/agent/verify-k8s-marker"; then ok "the claim kept the marker"; else bad "marker gone"; fi
  if transcript_has "$FLOW_TEXT"; then ok "the session transcript survived"; else bad "transcript gone"; fi
else
  bad "alice's new pod never Ready"
fi

hr "F. drop bob: harvested into the crew cache"
: >"$ROOT/harvest"
(
  for _ in $(seq 400); do
    m=$(kn get pods -l job-name=payments-backend-bob-harvest -o jsonpath='{.items[*].status.containerStatuses[*].state.terminated.message}' 2>/dev/null)
    if [[ -n $m ]]; then printf '%s\n' "$m" >"$ROOT/harvest"; exit 0; fi
    sleep 2
  done
) &
watcher=$!
kn patch fleets.balerix.ai payments --type merge -p '{"spec":{"crews":{"backend":{"agents":{"bob":null}}}}}' >/dev/null
if wait_until 900 sh -c "! kubectl -n $NS get agents.balerix.ai payments-backend-bob"; then ok "bob's Agent gone"; else bad "bob's Agent still there"; fi
kill "$watcher" 2>/dev/null; wait "$watcher" 2>/dev/null
branch=$(sed -n 's/^harvested //p' "$ROOT/harvest" | head -n 1)
if [[ -z $branch ]]; then
  bad "no 'harvested <branch>' message seen from the harvest Job ($(cat "$ROOT/harvest"))"
else
  kn run probe --image="$agent_image" --restart=Never --overrides='{"spec":{"containers":[{"name":"probe","image":"'"$agent_image"'","command":["sleep","infinity"],"volumeMounts":[{"name":"shared","mountPath":"/balerix/volume"}]}],"volumes":[{"name":"shared","persistentVolumeClaim":{"claimName":"balerix-default-shared"}}]}}' >/dev/null
  kn wait pod/probe --for=condition=Ready --timeout=5m >/dev/null
  if kn exec probe -- git --git-dir=/balerix/volume/fleets/payments/crews/backend/repo/.git branch --list "$branch" | grep -q .; then
    ok "branch $branch in the crew cache"
  else
    bad "branch $branch not in the crew cache"
  fi
  kn delete pod probe --wait=false >/dev/null
fi

hr "G. delete the Fleet"
if kn delete fleets.balerix.ai payments --wait --timeout=10m >/dev/null; then ok "Fleet deleted"; else bad "Fleet delete timed out"; fi

hr "report"
if ((${#FAILED[@]})); then
  say "${#FAILED[@]} check(s) failed:"
  printf '  %s\n' "${FAILED[@]}"
  say "state kept: namespace $NS; \`mise run verify-k8s -- down\` removes it"
  exit 1
fi
say "verify-k8s: every check passed (--from $FROM)"
say "\`mise run verify-k8s -- down\` removes the namespaces and both releases"
```

- [ ] **Step 2: The task**

In `mise.toml`, after `[tasks.verify-github]`:

```toml
[tasks.verify-k8s]
description = "Spec O §24.6's by-hand check with the real claude on the cluster KUBECONFIG names: both charts (`-- --from tree`, the default, or `-- --from index`), a payments-shaped Fleet, the e2e journey's checks; prints a report. `-- down` removes it. Not part of any CI tier"
tools = { kubectl = "1.34.12", helm = "4.3.0" }
run = "scripts/verify-k8s.sh"
```

- [ ] **Step 3: Static checks and a dry preflight**

Run: `bash -n scripts/verify-k8s.sh && mise run lint`
Expected: clean. shellcheck may flag `SC2016` on the single-quoted `#{…}` tmux formats. Those are intended literals, so add a `# shellcheck disable=SC2016` line directly above each one.

Run: `KUBECONFIG=/nonexistent scripts/verify-k8s.sh; echo "exit=$?"`
Expected: the preflight header, then "kubectl cannot reach the cluster…" and `exit=2`. Nothing is created.

This host has no cluster that allows CRDs, so the full run happens in the rehearsal (see the Handoff).

- [ ] **Step 4: Commit**

```bash
git add scripts/verify-k8s.sh mise.toml
git commit -m "feat: mise run verify-k8s, the by-hand check on a real cluster (Spec O §24.6)"
```

---

### Task 6: Documentation

**Files:**
- Modify: `docs/RELEASING.md`, `docs/THREAT-MODEL.md`, `AGENTS.md`
- Modify: `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` (§24.8; §14.4 cross-reference)

- [ ] **Step 1: RELEASING.md**

- **Release units table.**
  - The core row ships "`balerix`, `balerix-operator` and `balerix-agent` binaries; `ghcr.io/balerix-ai/balerix`, `…/balerix-agent` and `…/balerix-operator`; `balerix-api` and `balerix-plugin-sdk` on crates.io".
  - Add the charts row: "`balerix-charts-v<ver>` | both charts at one version: `oci://ghcr.io/balerix-ai/charts/balerix-operator` and `…/balerix-daemon`, and the index at `https://balerix-ai.github.io/helm-charts`".
- **A new subsection "The charts unit", after the library paragraphs.**
  - The charts name core's version (`appVersion`, moved by core's release PR) and each plugin's (`plugins.<plugin>.image.tag` in `charts/balerix-daemon/values.yaml`, moved by that plugin's release PR).
  - `prepare.sh charts` proposes nothing until all of them are tagged, and names the missing tag.
  - A moved pin counts as a change, with the larger bump winning (a core minor is a chart minor).
  - The changelog lists the moved images under `### Images`.
  - The release runs in this order:
    1. package (a fork's archives name the fork's images);
    2. `mise run charts`, then install on kind with the published images (Daemon, flow and web Ready);
    3. push to OCI and sign;
    4. the GitHub Release with the archives;
    5. index commit to `helm-charts`;
    6. verify through the index. A failure here flags the release as a prerelease.
  - The first charts release must follow a core release that ships the operator image. `balerix-v0.2.0` predates it, and the check job would fail on it.
- **Recovery table.** Add the rows:
  - "charts check" → nothing public; fix through a normal PR.
  - "after `charts-publish`" → the OCI charts exist with no tag; re-run.
  - "`charts-index`" → tagged, not indexed; re-run (it skips an index that already lists it).
  - "`charts-verify`" → flagged as a prerelease; fix the index or Pages, then the next release.
- **Dry run.** A dry run of the charts unit packages, checks and uploads the archives.
- **One-time setup.** Add §14.4's steps:
  - GitHub Pages on `balerix-ai/helm-charts` from `main`.
  - Install the `balerix-release` App on `helm-charts` too, with *Contents: read and write*.
  - After the first pushes, make `balerix-operator`, `balerix-agent` and `charts/*` public (the charts check pulls the images anonymously).
  - Add `charts` to branch protection.
  - The first releases go: core (the first with the operator), github, then charts.
- **A new section "Rehearsing Kubernetes on forks" (§24.6).**
  - Fork both repositories under one account and do the one-time setup there.
  - Make the fork's ghcr packages public as each first appears.
  - Merge the release PRs for core, then github, then charts.
  - Then run `mise run verify-k8s -- --from index` with `BALERIX_VERIFY_OWNER=<account>` against a cluster with a ReadWriteMany class.
  - Paste the report into the rehearsal record.

- [ ] **Step 2: THREAT-MODEL.md**

Extend the line "**The release App's private key** — can push branches and edit pull requests on this repository." with: "and push to `helm-charts`, whose `index.yaml` every `helm repo add` trusts for archive URLs. The archives themselves are checked against the attested `SHA256SUMS`, and the OCI charts are cosign-signed."

- [ ] **Step 3: AGENTS.md**

- In the `kind-up` entry, add: "`mise run kind-up cluster` makes the cluster and class only, no images (what the charts release check uses)."
- Add a `verify-k8s` entry next to the other `verify-*` tasks, from the task's description.
- In the `release-test` entry, if one exists, add the charts scenarios. If none exists, leave it.

- [ ] **Step 4: Spec §24.8**

Append to the Spec O doc, after §24.7:

```markdown
### 24.8 Decided by the 5b plan

- **Images by name.** `lib.sh` `unit_images` lists a unit's images in
  build order. Image scripts take an image name, and `merge-images` and
  `promote-images` run once per image. One composite action
  (`build-unit-images`) builds a unit's images in order. Digest artifacts
  stay one per unit and architecture, holding `<image>/<digest>`.
- **The agent's base.** The smoke build uses the docker driver with `BASE`
  set to the `balerix` just loaded on the runner. The push build uses the
  `balerix` per-architecture digest just pushed.
- **One archive.** Core's `balerix-v<ver>-<target>.tar.gz` holds all three
  binaries. `build.sh` checks each one's linkage and `--version`.
  `kind-up` builds its images through `build.sh core`, so `e2e-k8s` runs
  the static release binaries.
- **The charts job packages first** and then checks the archives it
  packaged: `mise run charts`, then an install on `kind-up cluster`. What
  passes the check is what gets published.
- **Pins in the changelog.** A charts release lists moved pins under
  `### Images`. A pin-only release carries no "Initial release." line.
- **OCI re-runs.** Pushing an archive that is already there is a no-op
  at the registry, and a re-run signs it again. The index step skips when
  `index.yaml` already lists both archive URLs.
- **verify-charts waits for Pages** for up to 10 minutes before it fails
  and flags.
- **verify-k8s** is a report-printing script, like `verify-claude.sh`. Its
  Fleet is `examples/payments.yaml` with the repo replaced by a git server
  pod, push off and a pod runner. It types the first prompt into alice's
  tmux window, re-pressing Enter until the transcript has it (#99). It
  reads flow's text from Claude's own transcript.
- **The first charts release** follows the first core release that ships
  `balerix-operator`. `appVersion` 0.2.0 has no operator image, and the
  check job would fail on it.
```

In §14.4, add "(and §24.5's order)" to the RELEASING sentence.

- [ ] **Step 5: Commit**

```bash
git add docs AGENTS.md
git commit -m "docs: releasing the operator, agent and charts; the Kubernetes rehearsal (Spec O §24)"
```

---

### Task 7: Push, CI, a core dry run

**Files:** none new.

- [ ] **Step 1: The whole local gate**

Run: `mise run check && mise run release-test && mise run lint && mise run agent && mise run operator`
Expected: all pass. For `charts` locally, use the 5a workaround: `scripts/operator.sh charts` with a scratch helm on PATH inside `mise exec github:kubernetes-sigs/controller-tools@envtest-v1.34.1`.

- [ ] **Step 2: Push and open a draft PR**

```bash
git push -u origin feat/release-k8s
gh pr create --draft --base main --title "feat: release the operator, agent and charts (Spec O 5b, §24.4–§24.6)" --body-file <body>
```

The body lists the tasks, the §24.8 decisions and Task 4 Step 9's local index run. It adds that the rehearsal (forks, a cluster, `verify-k8s --from index`) is the user's and is still to come.

- [ ] **Step 3: CI**

Watch with `gh pr checks --watch`. Expected green:
- `check`, `release-test`, `charts`.
- `images` for every unit on both architectures. Core's leg builds and smoke-tests three images: the agent from the local `balerix:smoke`, the operator's `crds` count, and the agent's sidecar refusal.
- `e2e-k8s`, with kind-up now building through `build.sh core`.

Diagnose any red job from its log with superpowers:systematic-debugging before changing anything.

- [ ] **Step 4: A core release dry run (publishes nothing)**

```bash
git switch -c dry-run/core-5b
scripts/release/prepare.sh core
git add -A && git commit -qm "chore(release): dry run of core with the operator and agent"
git push -u origin dry-run/core-5b
gh workflow run release.yml --ref dry-run/core-5b -f dry-run=true
```

Watch the run. Expected:
- `plan` plans core with `images` listing three.
- `audit` passes for core, operator and agent.
- `build` on both architectures produces archives holding three binaries.
- `images` builds, smoke-tests and scans three images per architecture.
- `merge-images`, `github-release` and `promote-images` are skipped.

Afterwards delete the branch locally and on origin (`git push origin --delete dry-run/core-5b`) and switch back to `feat/release-k8s`.

- [ ] **Step 5: Report**

Report to the user: CI status, the dry run's link and result, and that the PR is ready to leave draft. Merging is their call.

---

## Handoff: the fork rehearsal (§17's done condition; the user's)

These steps are not implementer tasks. They need the user's accounts and a cluster:

1. Fork `balerix-ai/balerix` and `balerix-ai/helm-charts` under one account. Do RELEASING's one-time setup on the forks: the App on both, environments, Pages on the `helm-charts` fork, and branch protection.
2. Merge the fork's core release PR and watch `release.yml`. Make `balerix`, `balerix-agent` and `balerix-operator` public on the fork's ghcr.
3. Merge the fork's github release PR.
4. Merge the fork's charts release PR. The check installs on kind, then the job publishes OCI, makes the Release, commits the index and verifies. Make `charts/*` public.
5. On a cluster with a ReadWriteMany class, run `BALERIX_VERIFY_OWNER=<account> mise run verify-k8s -- --from index`. Every check must pass. Paste the report.

Sub-project 5 is done when step 5 passes.
