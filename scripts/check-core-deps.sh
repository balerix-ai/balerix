#!/usr/bin/env bash
# Two invariants that keep the daemon and client out of a plugin's dependency
# tree (Spec H §1, §3, §4):
#
#   1. The workspace is the seven core crates and nothing else.
#   2. `reqwest` in that workspace carries `json` and nothing else.
#
# The second is the symptom the split was made to cure; the first is the cause,
# and catches a readmitted plugin whose tree never touches `reqwest` at all.
set -euo pipefail
cd "$(dirname "$0")/.."

# Readmitting a plugin as a workspace member is the tempting fix when its build
# breaks, and it puts that plugin's whole tree back into the core feature
# resolution. A genuinely new core crate belongs in this list; a plugin does not.
expected="balerix,balerix-api,balerix-config,balerix-core,balerix-plugin-sdk,balerix-runtime,balerix-server"
members=$(cargo tree --workspace --depth 0 | sed -n 's/^\([a-z][a-z0-9-]*\) v[0-9].*/\1/p' | sort -u | paste -sd, -)
if [[ $members != "$expected" ]]; then
  echo "core workspace members are '$members'," >&2
  echo "expected '$expected' (Spec H §3)." >&2
  echo "A plugin is a standalone project under plugins/, never a member." >&2
  exit 1
fi

# Workspace-wide: cargo unifies features across every package in one
# build, so the daemon links whatever reqwest the *workspace* resolves to,
# not what `-p balerix-server` alone would resolve.
#
# Fail closed, and say what it means, if the guarded dependency ever leaves
# the core tree: `cargo tree -i` exits 101 on a package it cannot find, with
# a message about a package ID specification that reads like tooling
# breakage rather than "the thing you were guarding is gone".
if ! tree=$(cargo tree --workspace --edges features --invert reqwest) \
  || ! grep -q '^reqwest v' <<<"$tree"; then
  echo "no reqwest in the core dependency tree; update or drop this guard" >&2
  exit 1
fi

# Assert the whole feature set rather than denying a list of known-bad
# features: the set after the split is `json` alone (Spec H §10.1), so any
# future leak is covered without extending a denylist.
have=$(sed -n 's/^[^A-Za-z]*reqwest feature "\([^"]*\)".*/\1/p' <<<"$tree" | sort -u | paste -sd, -)
if [[ $have != "json" ]]; then
  echo "core reqwest features are '$have', expected 'json' (a plugin leaked into the core resolution)" >&2
  exit 1
fi
echo "core workspace clean: members match Spec H §3, reqwest features are 'json'"
