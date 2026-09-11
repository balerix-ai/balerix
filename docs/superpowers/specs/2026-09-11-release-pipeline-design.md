# Balerix — Spec I: release pipeline

**Date:** 2026-09-11
**Status:** Implemented 2026-09-11 (plan: docs/superpowers/plans/2026-09-11-release-pipeline.md)
**Scope:** release the `balerix` CLI, the three in-tree plugins and the
plugin SDK. Binaries go to GitHub Releases, multi-arch images (linux/amd64,
linux/arm64) to ghcr, and `balerix-api` + `balerix-plugin-sdk` to crates.io.
The CLI and each plugin have independent versions and release cycles. No
behaviour change to the daemon, the CLI or any plugin.

---

## 1. Problem

Nothing in this repository is released. Every crate is `publish = false`,
there are no tags, and `docs/plugin-protocol.md` §7 already describes a
release plugin package that pins its binary as a mise `github:` tool against
a release asset — an asset no pipeline produces. Versions have started to
drift by hand: `plugins/web/package/balerix-plugin.yaml` says `0.2.0` while
`plugins/web/Cargo.toml` says `0.1.0`.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| I-1 | Four release units: **core**, **flow**, **web**, **matrix**. | The CLI and each plugin release independently (owner's requirement). |
| I-2 | `balerix-api` and `balerix-plugin-sdk` share the core version and release with the CLI. | One version line for the core workspace; the SDK's crates.io version always equals the `balerix-v*` tag. A CLI-only patch republishes an unchanged SDK, which is harmless. |
| I-3 | Release PRs and releases are driven by our own scripts on **git-cliff**, not release-plz. | release-plz 0.3.165 was evaluated and reproduced three blockers against this layout (§12). git-cliff is release-plz's changelog engine; tag names and changelog paths follow release-plz conventions so a later migration is configuration. |
| I-4 | Conventional Commits, enforced on PR titles; squash merge only. | The bump and the changelog come from commit types. Squash merge makes the PR title the commit. |
| I-5 | Binaries are static musl builds on native amd64 and arm64 runners. | Protocol §7 asks for static builds; the repo is public, so `ubuntu-24.04-arm` is free; `cross` has open aws-lc-sys issues. |
| I-6 | The `balerix` image is a full runtime (git, gh, mise, nono, tmux); plugin images are the binary on distroless. | `balerix serve` needs its tools on `PATH`; a plugin is one static binary. |
| I-7 | Checksums, GitHub build-provenance attestations, and cosign keyless signatures on images. | mise verifies GitHub attestations on install; cosign serves consumers (admission policies) that verify signatures. |
| I-8 | The tag is created last, when the GitHub Release is published. | An untagged manifest version means "not released yet", which makes every failed run safely re-runnable. |
| I-9 | Moving image tags (`<major>.<minor>`, `latest`) are applied only after the release is published and its plugin package verified. | A partial run can never move `latest`. |

## 3. Release units

| Unit | Version source | Tag | Changelog | Ships |
|---|---|---|---|---|
| core | `[workspace.package] version` in `/Cargo.toml` | `balerix-v<ver>` | `CHANGELOG.md` | `balerix` binaries, `ghcr.io/balerix-ai/balerix`, crates `balerix-api` and `balerix-plugin-sdk` |
| flow | `plugins/flow/Cargo.toml` | `balerix-plugin-flow-v<ver>` | `plugins/flow/CHANGELOG.md` | binaries, `ghcr.io/balerix-ai/balerix-plugin-flow`, release package |
| web | `plugins/web/Cargo.toml` | `balerix-plugin-web-v<ver>` | `plugins/web/CHANGELOG.md` | binaries, `ghcr.io/balerix-ai/balerix-plugin-web`, release package |
| matrix | `plugins/matrix/Cargo.toml` | `balerix-plugin-matrix-v<ver>` | `plugins/matrix/CHANGELOG.md` | binaries, `ghcr.io/balerix-ai/balerix-plugin-matrix`, release package |

**Include paths.** A commit counts toward a unit only if it touches the
unit's paths:

- core: `crates/**`, `Cargo.toml`, `Cargo.lock`, `mise.toml`. `mise.toml` is
  included because `balerix-runtime` embeds it as the default tool table
  (`include_str!`), so a tool bump changes the shipped binary.
- each plugin: `plugins/<name>/**`, `crates/balerix-api/**`,
  `crates/balerix-plugin-sdk/**`. The SDK and api are compiled into the
  plugin binary, so their changes are pending in every plugin's release PR.

**Manifest changes.**

- `balerix-api` and `balerix-plugin-sdk` set `publish = true`; the other five
  core crates keep `publish = false`.
- Their `[workspace.dependencies]` entries gain `version = "<core version>"`
  next to `path` (`cargo package` requires it). `deny.toml`'s comment on
  `allow-wildcard-paths` is updated accordingly.
- `plugins/web/Cargo.toml` moves to `0.2.0` to match its manifest.
- `balerix-plugin.yaml`'s `version` is always written from `Cargo.toml` by
  the release script. `scripts/plugin.sh check` fails when the two differ.
- Each of the four projects gains `[profile.release] strip = true`.

**First release.** A unit with no tag releases the version currently in its
manifest (core `0.1.0`, flow `0.1.0`, web `0.2.0`, matrix `0.1.0`), with its
full history as the changelog.

**Independent cycles.** A release PR accumulates changes and waits. Nothing
releases until a maintainer merges it; merging one unit's PR never releases
another unit.

## 4. Release PRs

### 4.1 `scripts/release/prepare.sh <unit>`

Exposed as `mise run release-prepare <unit>` so humans and CI run the same
code. Steps:

1. Find the unit's latest tag matching `^<prefix>-v`. No tag: initial release
   (§3); the version is the manifest's.
2. Compute the next version with `git cliff --bumped-version`, using the
   shared `cliff.toml`, `--tag-pattern` for the unit and `--include-path` for
   each of its paths.
   - Releasable types: `feat`, `fix`, `perf`, `refactor`, `build`.
   - Skipped types (no release, not in the changelog): `docs`, `test`, `ci`,
     `chore`, `style`, `revert`.
   - Bump rules (cargo semver, same as release-plz): in `0.x` a breaking
     change bumps minor and anything else bumps patch; from `1.0` a breaking
     change bumps major, `feat` minor, `fix` patch.
   - No releasable commit since the tag: print `nothing to release`, exit 0,
     change nothing.
3. Apply the version.
   - core: `cargo set-version --workspace <ver>` (bumps `[workspace.package]`
     and the versioned `[workspace.dependencies]` entries), refresh the core
     `Cargo.lock`, and refresh each plugin's `Cargo.lock` for
     `balerix-api` and `balerix-plugin-sdk` (plugins lock the SDK version
     through their path dependency).
   - plugin: `cargo set-version --manifest-path plugins/<name>/Cargo.toml
     <ver>`, refresh that project's `Cargo.lock`, write `version: <ver>` into
     `plugins/<name>/package/balerix-plugin.yaml`.
4. Prepend the new section to the unit's `CHANGELOG.md` with git-cliff.
5. Print the new version and the changelog section (the workflow uses both).

Before step 2: if the manifest version has no tag yet and the changelog
already carries that version's section — a release PR was merged and
`release.yml` has not finished, or failed (§8.2); prepare.sh always writes
the section, so this also covers a merged initial release PR — the script
prints `release in progress`, exits 0 and changes nothing. The workflow
leaves that unit's PR state untouched; the next push after the tag exists
computes from it.

If instead an older tag exists (a release has shipped before) and the
manifest version has neither a tag nor that section, nobody proposed it
through a release PR — the version was changed by hand. The script dies,
naming the version. Recover by reverting the edit, or by forcing that exact
version (`release-pr`'s `workflow_dispatch` with `version` set to it, or
`release-prepare <unit> <version>` locally) to release it.

`git-cliff` and `cargo-edit` are pinned exactly in `mise.toml`.

### 4.2 `.github/workflows/release-pr.yml`

- Triggers: `push` to `main`; `workflow_dispatch` with inputs `unit` and
  optional `version`.
- A matrix over the four units, `concurrency: release-pr-<unit>`.
- Each leg runs `prepare.sh`. With changes, it force-pushes branch
  `release/<unit>` (rebuilt from `main` every run) and creates or updates the
  PR titled `chore(release): <crate> v<ver>`, with the changelog section as
  the body. With nothing to release, it closes any open `release/<unit>` PR.
- It pushes with a **GitHub App** installation token
  (`actions/create-github-app-token`) so CI runs on the release PR without
  manual approval. The token is scoped to `contents`, `pull-requests` and `issues` (labels are an issues API).
- Forcing a version: `workflow_dispatch` with `unit` and `version` sets that
  exact version and labels the PR `release:pinned`. Push-triggered runs skip
  a unit whose open PR carries the label; dispatching again with a version
  re-pins it, and removing the label resumes automatic updates.

### 4.3 Keeping commits conventional

- `.github/workflows/pr-title.yml` checks every PR title with
  `amannn/action-semantic-pull-request`, on `pull_request` (`opened`,
  `edited`, `synchronize`, `reopened`), allowing the types in §4.1; scope is
  optional.
- Repository settings: squash merge only; default squash commit message is
  the PR title.
- `renovate.json` extends `:semanticCommits`, so dependency updates are
  `fix(deps):` (releasable) and dev-dependency updates and lockfile
  maintenance are `chore(deps):` (not releasable).

## 5. Release workflow — `.github/workflows/release.yml`

Triggers: `push` to `main`; `workflow_dispatch` with `dry-run` (boolean).
`concurrency: release`, `cancel-in-progress: false`. Branch protection on
`main` requires `check` and `plugins` to pass and branches to be up to date
before merge, so a merged release PR has been tested as it lands.

### 5.1 `plan`

- For each unit, read the version (`cargo metadata`). A unit is planned when
  `<prefix>-v<ver>` does not exist on `origin` and the unit's changelog
  carries a `## <ver> - ` section (its release PR was merged); an untagged
  version without that section was never proposed and is skipped. Output the
  planned units as a JSON matrix; with none, the workflow ends.
- For each unit in the matrix, run `cargo audit` and `cargo deny check
  advisories bans sources licenses` against that unit's lockfile. A release
  cannot ship past a known advisory.

### 5.2 `build` (unit × target)

| Target | Runner |
|---|---|
| `x86_64-unknown-linux-musl` | `ubuntu-24.04` |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` |

- `rustup target add <target>`; `apt-get install musl-tools`;
  `CC_aarch64_unknown_linux_musl=musl-gcc` (needed by matrix's `aws-lc-sys`
  and bundled SQLite; core, flow and web have no C dependencies).
- `cargo build --release --locked --target <target>`: `-p balerix` for core,
  `--manifest-path plugins/<name>/Cargo.toml` for a plugin.
- Smoke test on the build runner: `file` reports the binary statically
  linked; `balerix --version` prints exactly the release version; a plugin
  run with no daemon environment exits 1 with `<name>: …` on stderr.
- Archive `<prefix>-v<ver>-<target>.tar.gz` containing the binary, `LICENSE`
  and the unit's `CHANGELOG.md`; upload as a workflow artifact.

### 5.3 `images` (unit × arch) and `merge-images` — §6

### 5.4 `package` (plugins only) — §7

### 5.5 `publish-crates` (core only)

Two jobs, so a dry run (or a fork's real run, which can never publish)
never enters environment `release` or requests OIDC:

- `publish-crates-dry-run`: runs when `dry-run`, or outside
  `balerix-ai/balerix` (a fork's real run). No environment, `contents:
  read` only. Runs `cargo publish --dry-run --locked -p balerix-api -p
  balerix-plugin-sdk`.
- `publish-crates`: runs only when `!dry-run && github.repository ==
  'balerix-ai/balerix'`, in GitHub environment `release`. Obtains a
  short-lived token with `rust-lang/crates-io-auth-action` (crates.io
  trusted publishing). Skips each crate whose version already exists in the
  crates.io index, then runs `cargo publish --locked -p balerix-api -p
  balerix-plugin-sdk` (cargo orders them by dependency).

`github-release` needs both jobs: on a fork's real run only
`publish-crates-dry-run` does anything, and it must still hold
`github-release` back the way the combined job used to.

PR-tier guard: `mise run lint` gains
`cargo package --no-verify --allow-dirty -p balerix-api -p balerix-plugin-sdk` (`--allow-dirty` so the gate also passes before a commit).

### 5.6 `github-release` (per unit)

Needs `build`, `images`/`merge-images`, `package` (plugins) and
`publish-crates` (core) to have succeeded.

1. Delete any existing draft for the tag (a previous failed run).
2. `gh release create <tag> --draft --target <sha>` with the unit's
   changelog section as notes; for plugins the notes also carry a
   ready-to-paste `plugins.yaml` entry with the package `sha256`.
3. Upload the archives, `SHA256SUMS`, and (plugins) the package.
4. Attest the uploaded files (§9).
5. `gh release edit <tag> --draft=false` — this creates the tag.

Skipped under `dry-run`, which uploads everything as workflow artifacts
instead. All units in one run wait for every unit's builds and images; a re-run releases whatever is still untagged.

### 5.7 `verify-package` (plugins only) — §7.3

### 5.8 `promote-images` (per unit)

After `github-release` (and `verify-package` for plugins) succeed:
`docker buildx imagetools create` adds `<major>.<minor>` and `latest` to the
already-pushed `<ver>` index.

## 6. Container images

### 6.1 Build mechanics

- Images reuse the binaries from §5.2; nothing compiles inside Docker.
- Each architecture builds on its native runner with
  `docker/build-push-action`, runs the smoke test (§6.4) and the scans (§9),
  then pushes **by digest only**.
- `merge-images` combines the two digests into one index tagged `<ver>`
  (`docker buildx imagetools create`), with OCI labels (version, source,
  revision) from `docker/metadata-action`, then signs and attests it (§9).
- BuildKit `sbom: true` and `provenance: mode=max`.

### 6.2 `ghcr.io/balerix-ai/balerix` — `docker/balerix/Dockerfile`

- Base `debian:trixie-slim`, pinned by digest (Renovate updates it). apt:
  `git`, `ca-certificates`, `tini`. `ENV LANG=C.UTF-8`.
- Builder stage: download a pinned `mise` release and verify its sha256; run
  `mise install` for `gh`, `nono` and `tmux` at the exact versions in the
  repository `mise.toml` (all three resolve to prebuilt binaries:
  `aqua:cli/cli`, `github:nolabs-ai/nono`, `aqua:tmux/tmux-builds`).
- Final stage copies `mise`, `gh`, `nono`, `tmux` and `balerix` into
  `/usr/local/bin`. `/usr` is inside the sandbox's system read grant, and
  `ToolPaths::discover_in` (`crates/balerix-runtime/src/tools.rs`) finds them
  on `PATH`.
- **No mise configuration is left in the image.** A system
  `/etc/mise/config.toml` would enter every agent's and plugin's mise
  resolution (see the `MISE_GLOBAL_CONFIG_FILE` gotcha in `AGENTS.md`).
- Non-root user `balerix` (uid 10001), `HOME=/home/balerix`,
  `VOLUME /home/balerix` (the XDG state, config and data roots live under it).
- `ENTRYPOINT ["tini", "--", "balerix"]`, `CMD ["serve"]`; tini reaps tmux
  and agent processes.
- Documented limits, neither a threat-model change: `serve` refuses
  non-loopback binds (`crates/balerix/src/commands/serve.rs`, P3-1), so
  clients run inside the container (`docker exec <c> balerix up …`); nono
  needs Landlock, i.e. a host kernel ≥ 5.13 and a seccomp profile that
  permits the `landlock_*` syscalls.

### 6.3 `ghcr.io/balerix-ai/balerix-plugin-<name>` — `docker/plugin/Dockerfile`

- One Dockerfile; the build context holds the binary under the fixed name `plugin` (an exec-form `ENTRYPOINT` cannot expand an `ARG`).
- Base `gcr.io/distroless/static-debian13:nonroot`, pinned by digest. It
  ships a CA bundle, which matrix needs (its `reqwest` fails to build a
  client against an empty certificate store — `AGENTS.md`).
- Contents: the binary only, as `ENTRYPOINT`.
- The daemon does not run plugins from these images; it runs the release
  package (§7) through mise inside its sandbox. The images serve hosting a
  plugin outside the daemon's host process and a future Kubernetes runner.
  The README says so.

### 6.4 Smoke tests (before each per-arch push)

- Plugin image: `docker run` with no daemon environment exits 1 with
  `<name>: …`.
- `balerix` image: `docker run … --version` prints the release version; a
  detached `serve` container answers `docker exec … balerix list`
  (polled with a timeout), proving tool discovery succeeded.

### 6.5 `.github/workflows/images.yml`

On `pull_request` touching `docker/**` or `mise.toml`, and nightly: build
both images for both architectures from musl release builds of the PR head
(as in §5.2), run §6.4 and the scans in §9, push nothing.

## 7. Plugin release package

### 7.1 Contents

Asset `balerix-plugin-<name>-v<ver>-package.tar.gz`, platform-neutral:

- `balerix-plugin.yaml` from `plugins/<name>/package/` (version already
  written by the release PR).
- `mise.toml` rendered from the single template
  `scripts/release/plugin-mise.toml.tmpl`:

```toml
[tools."github:balerix-ai/balerix"]
version = "balerix-plugin-flow-v0.1.1"
[tools."github:balerix-ai/balerix".platforms]
linux-x64   = { asset_pattern = "balerix-plugin-flow-v0.1.1-x86_64-unknown-linux-musl.tar.gz",  checksum = "sha256:<digest>" }
linux-arm64 = { asset_pattern = "balerix-plugin-flow-v0.1.1-aarch64-unknown-linux-musl.tar.gz", checksum = "sha256:<digest>" }

[tasks.serve]
run = "balerix-plugin-flow"
```

The repository (`balerix-ai/balerix`) is rendered from `github.repository`,
so a fork rehearsal (§10) resolves against the fork. The version is the
full release tag rather than a bare version with a `version_prefix`: mise
names the install directory after the version, so the full tag keeps each
plugin's install directory distinct in the daemon's shared mise data dir
(flow and matrix at the same version would otherwise share one, and the
second install would be skipped). Explicit per-platform
`asset_pattern`s make asset selection deterministic and keep mise from
choosing the package itself. The per-platform `checksum` completes the
chain: `plugins.yaml` `sha256` pins the package, the package pins each
binary archive, and mise's default `github_attestations` verification checks
the archives' provenance (§9).

`plugins/<name>/package/mise.toml` stays the development layout
(`./bin/…`); the template replaces it only inside the release tarball.

### 7.2 `package` job

Needs both `build` legs of the plugin. Renders the template with the two
archive digests, runs `cargo run -p balerix -- plugin package <dir> --out
<asset>` (the daemon's own tarball code, so the format cannot drift), adds
the package to `SHA256SUMS`, and outputs the package `sha256` for the
release notes.

### 7.3 `verify-package` job

Runs after `github-release` on both native runners (release assets can only
be resolved by tag once published):

1. Download the package; check it against `SHA256SUMS`.
2. Unpack into a clean directory with a fresh `MISE_DATA_DIR`.
3. `mise trust`, `mise install` (exercises the checksum and attestation
   checks).
4. `mise run serve` exits 1 with `<name>: …`.

On failure: `gh release edit <tag> --prerelease`, add a "package
verification failed" line to the notes, fail the workflow; `promote-images`
does not run. It requires every `verify-package` leg to succeed, so one failed verification holds back the moving image tags of every unit in that run.

## 8. Failure handling

### 8.1 Invariants

- Runs never overlap (`concurrency: release`, no cancellation).
- Every job is idempotent: crates skip published versions, drafts are
  recreated, `<ver>` image tags are overwritten, attestations are additive.
- The tag is the last thing created (I-8); `latest` moves after that (I-9).

### 8.2 Recovery

| Fails at | State left | Recovery |
|---|---|---|
| `plan` audit, build, smoke, image, scan | nothing public | Fix on `main` through a normal PR; the version is still untagged, so that push releases it. |
| after `publish-crates` | crates live, no tag | Re-run via `workflow_dispatch`; publishing skips. |
| after image `<ver>` push | unannounced `<ver>` image tag | Re-run; the tag is overwritten, `latest` untouched. |
| `verify-package` | release published as prerelease | Fix; the next patch release follows the normal flow. |
| a bad release shipped | — | Roll forward with a patch; `cargo yank` a harmful crate version; never delete or move a git tag. |

A release is built and tagged at the commit the release run checks out, not
at the release PR's merge commit. Commits merged between the release PR and
that run (a queued run replaced under `concurrency: release`, or a fix merged
to recover) ship in the release without their own changelog entry or version
bump, and the next changelog starts after the tag. Check what has landed
before merging anything behind a release PR.

## 9. Integrity and pipeline hardening

### 9.1 Artifact integrity

| Artifact | Checksum | Provenance | Signature / SBOM |
|---|---|---|---|
| binary archives, plugin package | `SHA256SUMS` | `actions/attest-build-provenance` (file subjects) | — |
| image index | digest | `actions/attest-build-provenance`, `push-to-registry: true` | `cosign sign --recursive` (keyless, GitHub OIDC); BuildKit SBOM and provenance |

Verification commands published in the release notes and `docs/RELEASING.md`:

- `gh attestation verify <file> --repo balerix-ai/balerix`
- `gh attestation verify oci://ghcr.io/balerix-ai/<image>:<ver> --repo balerix-ai/balerix`
- `cosign verify ghcr.io/balerix-ai/<image>:<ver>
  --certificate-identity https://github.com/balerix-ai/balerix/.github/workflows/release.yml@refs/heads/main
  --certificate-oidc-issuer https://token.actions.githubusercontent.com`

### 9.2 Hardening

- **Least privilege.** Every workflow sets `permissions: {}`; jobs grant only:
  `plan`, `build`, `package`: `contents: read`; image jobs: `contents: read`,
  `packages: write`, `id-token: write`, `attestations: write`;
  `publish-crates`: `contents: read`, `id-token: write`; `github-release`:
  `contents: write`, `id-token: write`, `attestations: write`;
  `verify-package`: `contents: write` (to flag a prerelease).
  `actions/checkout` uses `persist-credentials: false` wherever the job does
  not push. Build jobs hold no secrets and no OIDC permission.
- **Secrets bound to places.** Environment `release` (deployment branch
  `main` only) guards `publish-crates`; the crates.io trusted publisher is
  bound to `release.yml` and that environment. The GitHub App private key
  lives in environment `release-bot` (`main` only); the App is installed on
  this repository alone with `contents: write`, `pull-requests: write` and `issues: write`.
  ghcr uses `GITHUB_TOKEN`; there are no PATs.
- **No privileged runs on untrusted code.** No workflow uses
  `pull_request_target`. `pr-title.yml` and `images.yml` run with read-only
  tokens and no secrets. `release.yml` and `release-pr.yml` run only on push
  to `main` or dispatch.
- **Pinned actions.** Third-party actions are pinned to full commit SHAs with
  a version comment; `renovate.json` extends `helpers:pinGitHubActionDigests`
  (this also pins the existing `ci.yml`).
- **Scanners for the new surface.** `zizmor` (`--min-severity medium`), `actionlint` and `shellcheck` (over `scripts/release/`) join `mise run lint`. `hadolint` on both Dockerfiles and `trivy image` (fail on
  fixed CRITICAL/HIGH, `--ignore-unfixed`) run in `images.yml` and before
  every release image push.
- **Tools** — `git-cliff`, `cargo-edit`, `cosign`, `hadolint`, `trivy`,
  `zizmor`, `actionlint` — are pinned exactly in `mise.toml`; each job
  installs only the tools it uses (`MISE_AUTO_INSTALL=false`, as in `ci.yml`).
- **Threat model.** `docs/THREAT-MODEL.md` gains a "Release pipeline"
  section. Assets: the crates.io publish right, the ghcr namespace, tags and
  release assets, the App key. Adversaries: a malicious pull request, a
  compromised dependency or action, a leaked App key. Controls: this
  section. Out of scope: a compromise of GitHub itself, maintainer account
  takeover.

## 10. Testing

- **`scripts/release/test.sh`** (`mise run release-test`): clones `HEAD`
  under `target/tmp` (a worktree would share this repository's tags), adds
  synthetic commits and tags, and runs `prepare.sh`, `plan.sh`, `notes.sh`
  and `package.sh` against the clones:

  | Case | Expected |
  |---|---|
  | unit with no tag | initial release at the manifest version |
  | `fix:` in `0.x` | patch bump |
  | `feat:` in `0.x` | patch bump |
  | `feat!:` in `0.x` | minor bump |
  | `feat:` from `1.0` | minor bump |
  | `fix!:` from `1.0` | major bump |
  | `docs:` only | `status=none`, no file changed |
  | a change under `crates/balerix-plugin-sdk/` | `status=release` for core and every plugin |
  | a plugin-only change | core `status=none` |
  | core `feat!:` in `0.x` | minor bump |
  | manifest version untagged, changelog section present (a merged release PR) | `status=in-progress`, no file changed |
  | same, with no last tag (a merged initial release PR) | `status=in-progress` |
  | forcing an already-released version | refused |
  | forcing a version while a release is in progress | `status=in-progress` (the forced version is ignored), no file changed |
  | forcing a version below the last release | refused |
  | manifest version changed by hand to one with no tag and no changelog section, an older tag exists | `prepare.sh` dies, naming the hand-bumped version; no file changed |
  | forcing that hand-bumped version | `status=release` at that version; `plan.sh` then proposes the unit once it is committed |
  | `notes.sh` for a released version | the changelog section's body alone, no neighbouring section |
  | `notes.sh` for a version with no section | refused |
  | `package.sh` for a plugin | archive named and checksummed correctly; `mise.toml` rewritten with the full-tag version, per-architecture checksums and start task, no `version_prefix`; mise can parse the result |
  | `plan.sh`, nothing proposed | empty `units`/`plugins`, `core=false` |
  | `plan.sh`, a merged initial release PR | proposes that unit in `units` and, for a plugin, `plugins` |
  | `plan.sh`, once tagged | empty again |

  `assert_consistent`, run after the initial release, the `0.x` breaking
  bump, the core bump and the hand-bump case, checks that the core
  `Cargo.lock` carries the core version and that every plugin's
  `balerix-plugin.yaml` version, its own `Cargo.lock` entry and its
  `Cargo.lock`'s `balerix-plugin-sdk` entry all agree with its `Cargo.toml`
  and the core version. CI runs `scripts/release/test.sh` in its own job
  (`release-scripts.yml`), on push to `main` and on every pull request, when
  `scripts/release/**`, `scripts/plugin.sh`, `cliff.toml`, `mise.toml` or
  the workflow file itself changes.
- **Workflows:** `actionlint` and `zizmor` in `mise run lint`.
- **Images:** `images.yml` (§6.5).
- **Dry run:** `release.yml` with `dry-run: true` still runs `plan` with the
  same rule as a real run (§5.1): a unit is planned only when its manifest
  version has no tag and its changelog carries that version's section, i.e.
  its release PR has merged on the dispatched ref. On `main` before any
  release PR has merged that is nothing, so dispatch the dry run on the
  unit's `release/<unit>` branch (its release PR's head) to rehearse it
  before merging. For whatever `plan` finds, the run does `build`, smoke
  tests, `package`, image builds and scans (no push) and
  `cargo publish --dry-run`, and uploads every artifact.
- **Fork rehearsal:** nothing hardcodes the owner (`github.repository`
  everywhere), and `publish-crates` skips outside `balerix-ai/balerix`, so a
  full run on a fork exercises tags, releases, ghcr pushes, signing and
  `verify-package` for real.

## 11. Rollout

1. **Setup** (recorded as a checklist in `docs/RELEASING.md`): create the
   GitHub App and the `release` and `release-bot` environments; branch
   protection on `main` (required `check` and `plugins`, up-to-date
   branches); squash merge only with PR-title commit messages; a fork
   rehearsal.
2. Release **flow** first — the smallest unit and the canary for the plugin
   path.
3. Release **web**, then **matrix**.
4. **Core** last: publish `balerix-api` and `balerix-plugin-sdk` `0.1.0` by
   hand with an API token from the release commit; configure the crates.io
   trusted publisher for both; merge core's release PR (the workflow skips
   the already-published crates). After the first push of each image, make
   its ghcr package public (ghcr creates packages private).

### 11.1 Documentation

- New `docs/RELEASING.md`: setup checklist, cutting a release (merge the
  release PR), forcing a version, recovery (§8.2), verification commands
  (§9.1).
- `README.md`: an "Install" section — release binaries, `docker run` /
  `docker exec`, a `plugins.yaml` entry for a released plugin.
- `AGENTS.md`: tasks `release-prepare` and `release-test`; gotchas — PR
  titles must be Conventional Commits, the plugin yaml version is written by
  the release script, released changelog sections are not hand-edited.

## 12. Why not release-plz (yet)

Evaluated against release-plz 0.3.165 / `release-plz/action` v0.5.136 on
2026-09-10, each reproduced on a scratch copy of this layout:

- **A.** `release-plz release` only walks packages publishable per
  `Cargo.toml`, so a `publish = false` package (the CLI, every plugin) gets a
  version bump but no tag and no GitHub Release. Upstream PR
  release-plz/release-plz#3049 (open) targets this.
- **B.** `git_only` mode — the way to version a never-published package —
  fails at `cargo package` when path dependencies point at unpublished
  crates (release-plz/release-plz#2595, open).
- **C.** `--manifest-path plugins/<name>/Cargo.toml` fails with "could not
  find repository": the repository is opened at the manifest directory
  rather than discovered upward. No upstream issue found.

release-plz could handle only `balerix-api` and `balerix-plugin-sdk`, which
conflicts with I-2: a daemon-only change would need a second bot to bump the
same `[workspace.package] version`. **Revisit** once #3049 and a fix for C
ship; §3's tag names and changelog paths already follow release-plz
conventions.

## 13. Deliberately deferred

- macOS and Windows binaries (the sandbox is Landlock-only).
- cosign signatures on binary archives (attestations cover them).
- `cargo-semver-checks` on `balerix-plugin-sdk` in the release PR.
- `cargo install balerix` (publishing the five internal crates).
- A non-loopback or TLS daemon listener for container use.

## 14. Acceptance criteria

- Merging a conventional-titled PR that touches only `plugins/flow/` opens or
  updates exactly one release PR, `release/flow`.
- Merging `release/flow` produces tag `balerix-plugin-flow-v<ver>`, a GitHub
  Release with two musl archives, `SHA256SUMS` and the package; a
  multi-arch `ghcr.io/balerix-ai/balerix-plugin-flow:<ver>` that
  `cosign verify` and `gh attestation verify` accept; and a green
  `verify-package` on both architectures.
- Merging `release/core` additionally publishes `balerix-api` and
  `balerix-plugin-sdk` at the core version, and
  `docker run ghcr.io/balerix-ai/balerix:<ver> --version` prints it on both
  architectures.
- Re-running `release.yml` after any partial failure completes the release
  without manual cleanup.
- `mise run lint`, `mise run release-test` and `images.yml` pass on the
  implementing PR.
