#!/usr/bin/env bash
# A dry run's charts check with the run's own images (#167): a dry run
# pushes no images, so a pin this run moves (core's appVersion, a plugin's
# image tag) names a tag ghcr does not have. Loads the archives
# save-images.sh wrote in the images job, re-tags each image under the
# exact reference the staged charts name (`<image_ref>:<version>`, a fork's
# owner included) and loads it into the kind cluster kind-up.sh made. The
# operator chart's pull policy and Kubernetes' default for a versioned tag
# are both IfNotPresent, so those pods use the loaded image; pins this run
# does not move still pull from ghcr. A plugin check-charts.sh does not
# enable (outside CHARTS_CHECK_PLUGINS) is skipped: nothing would run it.
#
# usage: kind-load-pins.sh [--check] <archives-dir> < pins
#   pins: check-pins.sh --dry-run's lines, `<unit> <version> <field>`.
# With --check, prints the path of each archive a pin needs that is
# missing, one per line, and loads nothing. Without it, needs docker and
# kind, fails on a missing archive or an image whose version label is not
# the pinned version, and deletes each archive once docker has loaded it.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

check=
if [[ ${1:-} == --check ]]; then
  check=1
  shift
fi
[[ $# -eq 1 ]] || die "usage: $0 [--check] <archives-dir> < pins"
archives=$1
# kind-up.sh's cluster
cluster=balerix-e2e

units=()
versions=()
while read -r unit version _field; do
  [[ -n $unit ]] || continue
  require_unit "$unit"
  if [[ $(unit_kind "$unit") == plugin && " ${CHARTS_CHECK_PLUGINS[*]} " != *" $unit "* ]]; then
    echo "charts: $unit $version is not installed by the check; not loaded" >&2
    continue
  fi
  units+=("$unit")
  versions+=("$version")
done

if [[ -n $check ]]; then
  for unit in ${units[@]+"${units[@]}"}; do
    [[ -f $archives/$unit.tar.gz ]] || echo "$archives/$unit.tar.gz"
  done
  exit 0
fi

# every archive first: a missing one loads nothing and deletes nothing
for i in "${!units[@]}"; do
  [[ -f $archives/${units[$i]}.tar.gz ]] ||
    die "charts: no image archive $archives/${units[$i]}.tar.gz for ${units[$i]} ${versions[$i]}"
done
loaded=()
for i in "${!units[@]}"; do
  unit=${units[$i]}
  version=${versions[$i]}
  archive="$archives/$unit.tar.gz"
  docker load --input "$archive" >&2
  rm -f "$archive"   # the runner's disk: docker holds the image now
  while read -r image; do
    ref=$(image_ref "$image")
    label=$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}}' "$ref:smoke")
    [[ $label == "$version" ]] ||
      die "charts: $ref:smoke from $archive is version '$label', but the charts pin $version"
    docker tag "$ref:smoke" "$ref:$version"
    loaded+=("$ref:$version")
  done < <(unit_images "$unit")
done
((${#loaded[@]} == 0)) || kind load docker-image --name "$cluster" "${loaded[@]}" >&2
for ref in ${loaded[@]+"${loaded[@]}"}; do echo "charts: loaded $ref into kind" >&2; done
df -h / "$archives" >&2 || true
