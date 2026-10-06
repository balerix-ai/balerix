#!/usr/bin/env bash
# Smoke-tests one locally loaded image (Spec I §6.4).
#
# usage: smoke-image.sh <image> <image-ref>
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

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
