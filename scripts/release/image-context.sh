#!/usr/bin/env bash
# Assembles the Docker build context for one image from an already built
# binary (Spec I §6.1): nothing compiles inside Docker.
#
# usage: image-context.sh <image> <dist-dir> <context-dir>
#   <image> is a repository name from unit_images (lib.sh).
# Prints GitHub step outputs: context=, dockerfile=, image=, version=, title=,
# description=.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

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
