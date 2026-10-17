#!/usr/bin/env bash
# The tools the named mise tasks pin in mise.toml, for a workflow to install
# and run: with MISE_AUTO_INSTALL=false mise never installs a task's own
# tools, and a version written into a workflow step drifts from mise.toml
# (Renovate only moves mise.toml).
#
#   task-tools.sh list    [--only <tool>] <task>...           name@version, one per line
#   task-tools.sh install [--only <tool>] <task>...           mise install them
#   task-tools.sh exec    [--only <tool>] <task>... -- <cmd>  mise x them -- <cmd>
set -euo pipefail

usage() { sed -n 's/^#   //p' "$0" >&2; exit 2; }
[[ $# -gt 0 ]] || usage
mode=$1; shift
only=""
if [[ ${1:-} == --only ]]; then only=$2; shift 2; fi
tasks=()
while [[ $# -gt 0 && $1 != -- ]]; do tasks+=("$1"); shift; done
[[ ${#tasks[@]} -gt 0 ]] || usage

mapfile -t tools < <(for task in "${tasks[@]}"; do
  mise tasks info "$task" --json |
    jq -r --arg only "$only" '.tools // {} | to_entries[]
      | select($only == "" or .key == $only)
      | "\(.key)@\(.value | if type == "object" then .version else . end)"'
done | sort -u)
if [[ ${#tools[@]} -eq 0 ]]; then echo "$0: no ${only:-tool} pinned by: ${tasks[*]}" >&2; exit 1; fi

case $mode in
  list) printf '%s\n' "${tools[@]}" ;;
  install) exec mise install "${tools[@]}" ;;
  exec) [[ ${1:-} == -- ]] || usage; shift; exec mise x "${tools[@]}" -- "$@" ;;
  *) usage ;;
esac
