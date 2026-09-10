#!/usr/bin/env bash
# The by-hand check for Phase 3 spec §8.1: a real `claude` under a real
# daemon, on a scratch state root. Prints a report to paste back.
#
# What it settles (spec §8.1 rows): SessionStart command hook with the profile
# environment (relay → Ready), HTTP hooks to loopback http:// with the literal
# header (event counts after one prompt), which `.claude.json` copy Claude
# read, whether onboarding appeared, and HOME relocation (nono's own $HOME).
#
# Your real $HOME stays: the client needs ~/.claude for credentials and the
# host settings layer. Only the three XDG roots move: config and state to
# target/tmp/verify-claude, wiped every run, and data to target/tmp/verify-data,
# kept across runs so the pinned claude (a 200 MB download into the shared
# MISE_DATA_DIR) is fetched once per version, not once per run. Your real
# balerix state is untouched. Nothing secret is printed: no settings.json,
# no hosts.yml, no token, no hook secret.
#
# BALERIX_VERIFY_FAKE=1 swaps in `balerix dev fake-claude`, an empty tool table
# and no host defaults — the maintainers' self-test of this script.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$REPO/target/tmp/verify-claude"
DATA="$REPO/target/tmp/verify-data"   # survives runs: tool installs only
FLEET=verify
CREW=c
AGENT=a
SOCKET="balerix-verify-$$"
UP_TIMEOUT="${BALERIX_VERIFY_TIMEOUT:-10m}"
FAKE="${BALERIX_VERIFY_FAKE:-0}"

export XDG_CONFIG_HOME="$ROOT/xdg/config"
export XDG_STATE_HOME="$ROOT/xdg/state"
export XDG_DATA_HOME="$DATA"
unset BALERIX_API_URL
STATE="$XDG_STATE_HOME/balerix"
SERVER="$STATE/server"
AGENT_DIR="$STATE/fleets/$FLEET/crews/$CREW/agents/$AGENT"

say() { printf '%s\n' "$*"; }
hr() { say "----- $* -----"; }
tail_file() { # tail_file <label> <path> [lines]
  if [ -f "$2" ]; then hr "$1 (last ${3:-20} lines of $2)"; tail -n "${3:-20}" "$2"; else hr "$1: $2 absent"; fi
}
stat_mtime() { stat -c '%y %n' "$1" 2>/dev/null || say "absent: $1"; }
git_q() { git -C "$1" "${@:2}" >/dev/null 2>&1; }

cleanup() {
  hr "teardown"
  if [ -f "$SERVER/balerix.pid" ]; then
    "$BALERIX" down "$FLEET" --keep --timeout 2m >/dev/null 2>&1 || say "down --keep failed or fleet absent (fine)"
    pid="$(cat "$SERVER/balerix.pid" 2>/dev/null || true)"
    if [ -n "$pid" ]; then
      kill -TERM "$pid" 2>/dev/null || true
      for _ in $(seq 1 50); do [ -f "$SERVER/balerix.pid" ] || break; sleep 0.1; done
      [ -f "$SERVER/balerix.pid" ] && kill -KILL "$pid" 2>/dev/null || true
    fi
  fi
  tmux -L "$SOCKET" kill-server >/dev/null 2>&1 || true
  say "state kept for inspection under $STATE (delete target/tmp/verify-claude when done;"
  say "tool installs stay in target/tmp/verify-data so the next run skips the download)"
}
trap cleanup EXIT

hr "preflight"
for t in git mise nono tmux; do command -v "$t" >/dev/null || { say "missing tool on PATH: $t (run: mise install)"; exit 2; }; done
if [ "$FAKE" = 0 ]; then
  [ -f "$HOME/.claude/.credentials.json" ] || say "WARNING: $HOME/.claude/.credentials.json not found; Claude will ask you to log in"
fi
say "host claude: $(command -v claude || echo 'not on host PATH (the agent installs its own pinned claude)')"
say "nono: $(nono --version 2>/dev/null | head -1)   tmux: $(tmux -V)   mise: $(mise --version 2>/dev/null | head -1)"

hr "build"
(cd "$REPO" && cargo build -q -p balerix) || { say "cargo build failed"; exit 2; }
# Under CARGO_TARGET_DIR the binary is not in ./target (same rule as
# package-plugins.sh); a relative value is taken from the repo root.
target="${CARGO_TARGET_DIR:-target}"
case "$target" in /*) ;; *) target="$REPO/$target" ;; esac
BALERIX="$target/debug/balerix"
# The web plugin, when `mise run package-plugins` has assembled it: the
# by-hand check of plugins spec §14 ("a browser shows a live terminal").
WEB_PKG="$target/plugins/web"
if [ -x "$WEB_PKG/bin/balerix-plugin-web" ]; then WEB=1; else WEB=0; say "web plugin not packaged (mise run package-plugins); skipping the browser step"; fi

hr "scratch root"
# A previous run (interrupted, or still parked at the prompt) leaves its
# daemon and tmux session behind; stop them before the root is wiped, or
# they outlive their endpoint file and squat the agent's windows.
if [ -f "$SERVER/balerix.pid" ]; then
  old="$(cat "$SERVER/balerix.pid" 2>/dev/null || true)"
  say "stopping the previous run's daemon (pid ${old:-?})"
  "$BALERIX" down "$FLEET" --keep --timeout 1m >/dev/null 2>&1 || true
  [ -n "$old" ] && kill -TERM "$old" 2>/dev/null || true
  for _ in $(seq 1 50); do [ -f "$SERVER/balerix.pid" ] || break; sleep 0.1; done
  [ -n "$old" ] && [ -f "$SERVER/balerix.pid" ] && kill -KILL "$old" 2>/dev/null || true
fi
for sock in /tmp/tmux-"$(id -u)"/balerix-verify-*; do
  [ -S "$sock" ] && tmux -S "$sock" kill-server >/dev/null 2>&1 || true
done
rm -rf "$ROOT"
mkdir -p "$ROOT/xdg/config/balerix" "$XDG_STATE_HOME" "$XDG_DATA_HOME"
if [ "$WEB" = 1 ]; then
  printf 'plugins:\n  - name: web\n    source: "%s"\n' "$WEB_PKG" > "$ROOT/xdg/config/balerix/plugins.yaml"
fi
if [ "$FAKE" = 1 ]; then
  printf '[tools]\n' > "$ROOT/xdg/config/balerix/mise.toml"   # nothing to download in the self-test
fi
WORK="$ROOT/work"; BARE="$ROOT/repo.git"
mkdir -p "$WORK"
git_q "$WORK" init -q -b main
printf 'hello from balerix verify\n' > "$WORK/README"
git_q "$WORK" add README
git -C "$WORK" -c user.name=verify -c user.email=verify@balerix.invalid commit -q -m init >/dev/null 2>&1
git clone -q --bare "$WORK" "$BARE"
if [ "$FAKE" = 1 ]; then
  CLAUDE_BLOCK="    binary: \"$BALERIX\"
    args: [dev, fake-claude, \"--verbose\"]
    settings: {}"
  HOST_FLAG=--no-host-defaults
else
  CLAUDE_BLOCK="    settings: {}"
  HOST_FLAG=
fi
cat > "$ROOT/fleet.yaml" <<EOF
apiVersion: balerix/v1
kind: Fleet
name: $FLEET
defaults:
  claude:
$CLAUDE_BLOCK
  tools: {}
crews:
  $CREW:
    repo: "file://$BARE"
    ref: main
    git: { push: false, auth: none }
    agents:
      $AGENT: { plugins: { $( [ "$WEB" = 1 ] && printf 'web: {}' ) } }
EOF
say "fleet file: $ROOT/fleet.yaml (repo: file://$BARE)"

hr "serve -d"
"$BALERIX" serve -d --bind 127.0.0.1:0 --tmux-socket "$SOCKET" || { say "serve -d failed"; tail_file server.log "$SERVER/server.log" 40; exit 1; }
URL="$(cat "$SERVER/endpoint")"
say "endpoint: $URL"

hr "up (timeout $UP_TIMEOUT; the first run installs the agent's pinned claude)"
# shellcheck disable=SC2086
if "$BALERIX" up "$ROOT/fleet.yaml" $HOST_FLAG --timeout "$UP_TIMEOUT"; then
  UP=ok
else
  UP=failed
fi

# The login URL is single use and lives 60 s from the moment it is minted,
# so it is minted right when you are told to open it (fake mode: once, as
# the smoke check). A second visit to a spent URL says why it was refused,
# and server.log records every attempt with the Host and Sec-Fetch-Site it
# arrived with.
browser_login() {
  hr "browser terminal (plugins spec §14)"
  LOGIN="$("$BALERIX" plugin open web 2>/dev/null || true)"
  if [ -n "$LOGIN" ]; then
    say ">>> Open this once in a browser, now (valid 60 s; it becomes a session cookie):"
    say ">>>     $LOGIN"
    say ">>> Then click 'review' beside $AGENT: comment on a line, add a summary, Send review."
    say ">>> The verdict (Spec C §8): the agent's terminal shows ONE pasted message, and the"
    say ">>> activity column shows 'review sent' followed by the agent's tool calls."
    say ">>> Spec D: with the review page open, edit a file in the agent's worktree (or let"
    say ">>> the agent do it): the diff updates within a few seconds and 'updated N s ago'"
    say ">>> flashes; with a comment box open the update waits until Save or Cancel."
    say ">>> Through a reverse proxy: replace only the origin ($URL), keep the path and"
    say ">>> query. If the proxy signs you in first, or the code lapses, mint a fresh one:"
    say ">>>     XDG_STATE_HOME=$XDG_STATE_HOME $BALERIX plugin open web"
  else
    say "plugin open web failed; see $SERVER/server.log"
  fi
}
if [ "$WEB" = 1 ] && [ "$UP" = ok ] && [ "$FAKE" = 1 ]; then browser_login; fi

metrics() { curl -sf "$URL/metrics" 2>/dev/null | grep '^balerix_hook_events_total' || say "(no hook events counted yet)"; }

say
say "=============================== REPORT (paste everything from here) ==============================="
say "date: $(date -u +%FT%TZ)   branch: $(git -C "$REPO" rev-parse --short HEAD)   fake: $FAKE   web: $WEB   up: $UP"
hr "A. SessionStart command hook with the profile environment (relay → Ready)"
"$BALERIX" status "$FLEET" || true
metrics
if [ "$UP" = failed ]; then
  tail_file "agent tmux.log" "$AGENT_DIR/logs/tmux.log" 40
  tail_file "agent nono.log" "$AGENT_DIR/logs/nono.log" 20
  tail_file "server.log" "$SERVER/server.log" 30
  say "=============================== END REPORT ==============================="
  exit 1
fi

if [ "$FAKE" = 1 ]; then
  ONBOARD="n (fake)"
  sleep 2
else
  say
  if [ "$WEB" = 1 ]; then browser_login; fi
  say
  say ">>> Now attach in another terminal:"
  say ">>>     tmux -L $SOCKET attach -t $FLEET/$CREW"
  if [ "$WEB" = 1 ]; then say ">>> or click $AGENT on the browser page above and type there."; fi
  say ">>> Wait for Claude's prompt, type one message (e.g. \"say hi\"), wait for the reply,"
  say ">>> detach with Ctrl-b then d, and come back here."
  ONBOARD=""
  while [ -z "$ONBOARD" ]; do
    printf '>>> Did Claude show a login, onboarding or trust prompt before its normal prompt? [y/n] '
    read -r ONBOARD < /dev/tty
    case "$ONBOARD" in y|Y|n|N) ;; *) ONBOARD="" ;; esac
  done
  sleep 2
fi

hr "B. HTTP hooks to loopback http:// with the literal header (counts after one prompt)"
metrics
hr "C. which .claude.json copy Claude read (newest mtime wins); onboarding seen: $ONBOARD"
stat_mtime "$AGENT_DIR/home/.claude.json"
stat_mtime "$AGENT_DIR/home/.claude/.claude.json"
say "home/.claude contents:"; ls -la "$AGENT_DIR/home/.claude" 2>/dev/null | sed 's/^/  /'
say "home/.claude/projects:"; find "$AGENT_DIR/home/.claude/projects" -maxdepth 2 2>/dev/null | sed 's/^/  /'
hr "D. HOME relocation: nono's own \$HOME (should hold only nono's state, nothing of Claude's)"
find "$AGENT_DIR/nono" -maxdepth 3 2>/dev/null | sed 's/^/  /'
say "top-level agent dir:"; ls -la "$AGENT_DIR" 2>/dev/null | sed 's/^/  /'
hr "E. status and logs"
"$BALERIX" status "$FLEET" || true
tail_file "agent nono.log" "$AGENT_DIR/logs/nono.log" 10
tail_file "server.log" "$SERVER/server.log" 15
say "=============================== END REPORT ==============================="
exit 0
