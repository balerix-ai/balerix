#!/usr/bin/env bash
# Opens, updates or closes one unit's release PR (Spec I §4.2). CI runs it on
# a fresh checkout of main with GH_TOKEN set to the release App's token.
#
# usage: open-pr.sh <unit> [version]    a version pins the PR at it
# RELEASE_PR_DRY_RUN=1 prints the pushes and PR edits instead of making them.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck source=scripts/release/lib.sh
source scripts/release/lib.sh

[[ $# -eq 1 || $# -eq 2 ]] || die "usage: $0 <unit> [version]"
unit=$1
forced=${2:-}
require_unit "$unit"
: "${GITHUB_REPOSITORY:?}"
branch="release/$unit"
pinned="release:pinned"

act() {
  if [[ ${RELEASE_PR_DRY_RUN:-} == 1 ]]; then
    printf 'would run:' >&2
    printf ' %q' "$@" >&2
    printf '\n' >&2
  else
    "$@"
  fi
}

pr=$(gh pr list --repo "$GITHUB_REPOSITORY" --head "$branch" --base main --state open \
  --json number,labels --jq '.[0] // empty')
if [[ -z $forced && -n $pr ]] && jq -e --arg l "$pinned" 'any(.labels[]; .name == $l)' <<<"$pr" >/dev/null; then
  echo "$unit: $branch is labelled $pinned; leaving it alone" >&2
  exit 0
fi

out=$(scripts/release/prepare.sh "$unit" ${forced:+"$forced"})
case $(sed -n 's/^status=//p' <<<"$out") in
  release) ;;
  in-progress) exit 0 ;;
  none)
    if [[ -n $pr ]]; then
      act gh pr close "$branch" --repo "$GITHUB_REPOSITORY" --delete-branch \
        --comment "Nothing left to release for \`$unit\`."
    fi
    exit 0
    ;;
  *) die "$unit: prepare.sh printed no status" ;;
esac
version=$(sed -n 's/^version=//p' <<<"$out")
notes=$(sed -n 's/^notes=//p' <<<"$out")
title="chore(release): $(unit_crate "$unit") v$version"

git switch -q -C "$branch"
git add -A
git -c user.name="github-actions[bot]" \
  -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit -q -m "$title"
[[ ${RELEASE_PR_DRY_RUN:-} == 1 ]] || : "${GH_TOKEN:?GH_TOKEN must be the release App token}"
act git push -q --force "https://x-access-token:${GH_TOKEN:-}@github.com/${GITHUB_REPOSITORY}.git" \
  "HEAD:refs/heads/$branch"

if [[ -n $pr ]]; then
  act gh pr edit "$branch" --repo "$GITHUB_REPOSITORY" --title "$title" --body-file "$notes"
else
  act gh pr create --repo "$GITHUB_REPOSITORY" --base main --head "$branch" \
    --title "$title" --body-file "$notes"
fi
if [[ -n $forced ]]; then
  act gh label create "$pinned" --repo "$GITHUB_REPOSITORY" --force \
    --color FBCA04 --description "Release PR with a hand-picked version"
  act gh pr edit "$branch" --repo "$GITHUB_REPOSITORY" --add-label "$pinned"
fi
