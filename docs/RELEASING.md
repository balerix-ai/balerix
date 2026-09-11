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
label, and pushes to `main` stop updating it. Dispatch again with a version to re-pin it, or remove the label to go back to computed versions. A forced version must be above the unit's last release, and nothing is proposed while a merged release PR still awaits its tag.

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
| one unit's GitHub Release, after other units in the same run published | those units are tagged, but their packages are not verified and their images not promoted | Use *Re-run failed jobs* on that run: it retries the failed release, then runs `verify-package` and `promote-images` for every unit. A new run or push skips units that already have tags, so their `latest` stays on the previous release until they release again. |
| `verify-package` | the release is published and flagged as a prerelease; no image's moving tags move in that run | Fix it; the next patch release follows the normal flow. |
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
