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
