#!/usr/bin/env bash
# Smoke-tests one unit's locally loaded image (Spec I §6.4).
#
# usage: smoke-image.sh <unit> <image-ref>
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 2 ]] || die "usage: $0 <unit> <image-ref>"
unit=$1
ref=$2
require_unit "$unit"
version=$(unit_version "$unit")

if [[ $unit != core ]]; then
  set +e
  err=$(docker run --rm "$ref" 2>&1 >/dev/null)
  code=$?
  set -e
  [[ $code -eq 1 ]] || die "$ref exited $code without a daemon environment, expected 1: $err"
  grep -q "^$unit: " <<<"$err" || die "$ref did not report '$unit: …': $err"
  echo "$ref: ok" >&2
  exit 0
fi

got=$(docker run --rm "$ref" --version)
[[ $got == "balerix $version" ]] || die "$ref --version printed '$got', expected 'balerix $version'"

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
echo "$ref: ok" >&2
