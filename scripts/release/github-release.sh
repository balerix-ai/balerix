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
kind=$(unit_kind "$unit")
image=
[[ $kind == library ]] || image=$(unit_image "$unit")

[[ $kind == library || -f $assets/SHA256SUMS ]] || die "$unit: no SHA256SUMS in $assets"

notes=$(mktemp)
trap 'rm -f "$notes"' EXIT
scripts/release/notes.sh "$unit" "$version" >"$notes"

if [[ $kind == plugin ]]; then
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

if [[ $kind == library ]]; then
  cat >>"$notes" <<EOF

### Use

\`\`\`sh
cargo add $crate@$version
\`\`\`
EOF
fi

if [[ $kind != library ]]; then
  cat >>"$notes" <<EOF

### Verify

\`\`\`sh
gh attestation verify $crate-v$version-x86_64-unknown-linux-musl.tar.gz --repo $GITHUB_REPOSITORY
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify oci://$image:$version --repo $GITHUB_REPOSITORY
cosign verify $image:$version \\
  --certificate-identity https://github.com/$GITHUB_REPOSITORY/.github/workflows/release.yml@refs/heads/main \\
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
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

# A library ships no assets: no archives, no SHA256SUMS, no package.
asset_args=()
[[ $kind == library ]] || asset_args=("$assets"/*)

gh release create "$tag" --draft \
  --target "$GITHUB_SHA" \
  --title "$crate v$version" \
  --notes-file "$notes" \
  "${asset_args[@]}"
gh release edit "$tag" --draft=false --latest="$latest"
echo "$unit: published $tag" >&2
