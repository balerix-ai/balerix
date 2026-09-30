#!/usr/bin/env bash
# The by-hand check for Phase 3 spec §8.1: a real `claude` under a real
# daemon, on a scratch state root. Prints a report to paste back.
#
# What it settles (spec §8.1 rows): SessionStart command hook with the profile
# environment (relay → Ready), HTTP hooks to loopback http:// with the literal
# header (event counts after one prompt), which `.claude.json` copy Claude
# read, whether any first-start dialog appeared (login, onboarding, folder
# trust, or the "Make auto mode your default permission mode?" offer that
# claude 2.1.282 added; #73), HOME relocation (nono's own $HOME), and that a
# slash command typed the way `send_text` types it runs (#41: `/exit` sent
# as `send-keys -l` then `Enter`, the pane exits and a SessionEnd arrives),
# and that a text sent the moment `SessionStart` arrives is submitted once
# its Enter is pressed again (#99: the first Enter is lost while Claude's
# TUI starts; section H adds a second agent and times its first start).
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
#
# BALERIX_VERIFY_BRANCH=<name> is Spec L §10's by-hand check: the scratch
# repo gets that branch (one commit ahead of main), the agent runs with
# `branch: <name>`, and the sandbox lets it push to the scratch bare, so
# a `git push` typed into the session should move origin's branch. Section
# F of the report shows the clone's HEAD and origin's tip.
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
BRANCH="${BALERIX_VERIFY_BRANCH:-}"

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
if [ -n "$BRANCH" ]; then
  git_q "$WORK" switch -q -c "$BRANCH"
  printf 'this branch is %s\n' "$BRANCH" > "$WORK/BRANCH"
  git_q "$WORK" add BRANCH
  git -C "$WORK" -c user.name=verify -c user.email=verify@balerix.invalid commit -q -m "one commit on $BRANCH" >/dev/null 2>&1
  git_q "$WORK" switch -q main
fi
git clone -q --bare "$WORK" "$BARE"
# The agent's clone has the scratch bare as `origin`; a push from inside
# the sandbox needs write on it.
SANDBOX_BLOCK=""
[ -n "$BRANCH" ] && SANDBOX_BLOCK="  sandbox: { filesystem: { allow: [\"$BARE\"] } }"
AGENT_EXTRA=""
[ -n "$BRANCH" ] && AGENT_EXTRA=", branch: \"$BRANCH\""
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
$SANDBOX_BLOCK
crews:
  $CREW:
    repo: "file://$BARE"
    ref: main
    git: { push: false, auth: none }
    agents:
      $AGENT: { plugins: { $( [ "$WEB" = 1 ] && printf 'web: {}' ) }$AGENT_EXTRA }
EOF
say "fleet file: $ROOT/fleet.yaml (repo: file://$BARE; agent branch: ${BRANCH:-<default>})"

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
  if [ -n "$BRANCH" ]; then
    say ">>> Spec L: also ask Claude to commit a change and run git push; section F below"
    say ">>> shows whether origin's $BRANCH moved."
  fi
  ONBOARD=""
  while [ -z "$ONBOARD" ]; do
    printf '>>> Did Claude show a login, onboarding, trust or "auto mode as default" prompt before its normal prompt? [y/n] '
    read -r ONBOARD < /dev/tty
    case "$ONBOARD" in y|Y|n|N) ;; *) ONBOARD="" ;; esac
  done
  sleep 2
fi

hr "B. HTTP hooks to loopback http:// with the literal header (counts after one prompt)"
metrics
hr "C. which .claude.json copy Claude read (newest mtime wins); first-start dialog seen: $ONBOARD"
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
if [ -n "$BRANCH" ]; then
  hr "F. Spec L §10: an agent on branch $BRANCH (clone HEAD should be it; a push should move origin)"
  say "clone HEAD:        $(git -C "$AGENT_DIR/workspace" symbolic-ref --short HEAD 2>&1)"
  say "clone tip:         $(git -C "$AGENT_DIR/workspace" rev-parse --short HEAD 2>&1)"
  say "origin $BRANCH tip: $(git -C "$BARE" rev-parse --short "$BRANCH" 2>&1)"
  say "origin main tip:   $(git -C "$BARE" rev-parse --short main 2>&1)"
  say "origin log of $BRANCH:"; git -C "$BARE" log --oneline "$BRANCH" 2>&1 | sed 's/^/  /'
fi
hr "G. #41: a slash command typed the way send_text types it (send-keys -l, then Enter) runs"
# A matrix thread reply and a flow `send` step reach the pane as exactly
# these two tmux commands (TmuxRunner::send_text). #41 saw `/exit` parked
# in the input with the ✓ already sent; it never reproduced, and this is
# what a claude bump has to keep true. The fake only records what arrived;
# the real claude proves the exit (pane dead, SessionEnd counted).
TARGET="$FLEET/$CREW:$AGENT"
hook_count() { # hook_count <event> [agent]: how many the daemon has counted, 0 when none
  local n
  n="$(curl -sf "$URL/metrics" 2>/dev/null | grep "agent=\"${2:-$AGENT}\".*fleet=\"$FLEET\"" \
    | sed -n "s/^balerix_hook_events_total{.*event=\"$1\".*} //p" | head -1)"
  printf '%s' "${n:-0}"
}
session_ends() { hook_count SessionEnd; }
if tmux -L "$SOCKET" has-session -t "=$FLEET/$CREW" >/dev/null 2>&1; then
  ends_before="$(session_ends)"; ends_before="${ends_before:-0}"
  tmux -L "$SOCKET" send-keys -t "$TARGET" -l -- '/exit'
  tmux -L "$SOCKET" send-keys -t "$TARGET" Enter
  verdict=""
  for _ in $(seq 1 60); do
    if [ "$FAKE" = 1 ]; then
      if grep -qx '/exit' "$AGENT_DIR/home/fake-claude.stdin" 2>/dev/null; then
        verdict="arrived: '/exit' and its Enter reached the fake's stdin as one line (the fake does not exit)"; break
      fi
    else
      dead="$(tmux -L "$SOCKET" display-message -p -t "$TARGET" '#{pane_dead}' 2>/dev/null || echo gone)"
      ends="$(session_ends)"; ends="${ends:-0}"
      if [ "$dead" != 0 ] || [ "$ends" -gt "$ends_before" ]; then
        verdict="ran: pane dead=$dead, SessionEnd hooks $ends_before -> $ends"; break
      fi
    fi
    sleep 0.25
  done
  if [ -n "$verdict" ]; then
    say "$verdict"
  else
    say "PARKED (#41 reproduced): 15 s after Enter the pane is alive and no SessionEnd arrived; input line:"
    tmux -L "$SOCKET" capture-pane -p -t "$TARGET" 2>/dev/null | grep '^❯' | sed 's/^/  /'
    [ "$FAKE" = 1 ] && tail_file "fake-claude.stdin" "$AGENT_DIR/home/fake-claude.stdin" 5
  fi
else
  say "agent session absent; skipped"
fi
hr "H. #99: a text sent the moment SessionStart arrives is submitted (UserPromptSubmit confirms it)"
# The github first prompt and a flow `send` on SessionStart reach the pane
# while Claude's TUI is still starting: the text lands in the composer and
# its Enter is lost. The delivery tracker presses Enter again every
# NUDGE_AFTER while the prompt is unconfirmed (plugins/common's
# `delivery.rs`); this does the same by hand and times it. It needs a
# first start, which agent $AGENT spent on the dialog check above (a text
# and an Enter typed at a first-start dialog would answer it), and a
# restart proved too warm to lose the Enter, so the fleet gains agent
# $AGENT_B here. How long Enter is swallowed varies between cold starts
# (1 s to 10 s here, over 20 s in the github check), so zero resent Enters
# on one run does not mean the race is gone. The text has several lines so that it travels as the
# first prompt does: `load-buffer -`, `paste-buffer -p -d`, then `Enter`.
NUDGE="${BALERIX_VERIFY_NUDGE:-5}"                 # seconds between Enters
STARTUP_WAIT="${BALERIX_VERIFY_STARTUP_WAIT:-60}"  # seconds to wait for the submit
AGENT_B=b
B_DIR="$STATE/fleets/$FLEET/crews/$CREW/agents/$AGENT_B"
B_TARGET="$FLEET/$CREW:$AGENT_B"
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
secs() { printf '%d.%d' $(( $1 / 1000 )) $(( $1 % 1000 / 100 )); }
sed "s|^      $AGENT: .*|&\n      $AGENT_B: { plugins: { $( [ "$WEB" = 1 ] && printf 'web: {}' ) } }|" \
  "$ROOT/fleet.yaml" > "$ROOT/fleet-h.yaml"
# shellcheck disable=SC2086
if ! "$BALERIX" update "$ROOT/fleet-h.yaml" $HOST_FLAG --no-wait; then
  say "update with agent $AGENT_B was refused; skipped"
else
  started=""
  for _ in $(seq 1 3000); do
    if [ "$(hook_count SessionStart "$AGENT_B")" -gt 0 ]; then started="$(now_ms)"; break; fi
    sleep 0.1
  done
  if [ -z "$started" ]; then
    say "no SessionStart from agent $AGENT_B within 5 min; skipped"
    "$BALERIX" status "$FLEET" || true
    tail_file "agent $AGENT_B tmux.log" "$B_DIR/logs/tmux.log" 20
  else
    printf 'Reply with the single word ready.\n\nThe lines below only make this text a paste,\nas the first prompt of the github plugin is:\n\n- one\n- two\n' \
      | tmux -L "$SOCKET" load-buffer -b balerix-verify-h -
    tmux -L "$SOCKET" paste-buffer -p -d -b balerix-verify-h -t "$B_TARGET"
    tmux -L "$SOCKET" send-keys -t "$B_TARGET" Enter
    sent="$(now_ms)"; last="$sent"; resent=0; verdict=""
    say "SessionStart seen; text and Enter sent $(secs $(( sent - started ))) s later"
    while [ $(( $(now_ms) - sent )) -lt $(( STARTUP_WAIT * 1000 )) ]; do
      if [ "$FAKE" = 1 ]; then
        if grep -q '^- two$' "$B_DIR/home/fake-claude.stdin" 2>/dev/null; then
          verdict="arrived: the text and its Enter reached the fake's stdin (the fake reads from the start and fires no UserPromptSubmit)"; break
        fi
      elif [ "$(hook_count UserPromptSubmit "$AGENT_B")" -gt 0 ]; then
        verdict="submitted: UserPromptSubmit $(secs $(( $(now_ms) - sent ))) s after the send, after $resent resent Enter(s)"; break
      fi
      if [ "$FAKE" = 0 ] && [ $(( $(now_ms) - last )) -ge $(( NUDGE * 1000 )) ]; then
        tmux -L "$SOCKET" send-keys -t "$B_TARGET" Enter
        last="$(now_ms)"; resent=$(( resent + 1 ))
        say "  Enter resent $(secs $(( last - sent ))) s after the send"
      fi
      sleep 0.25
    done
    if [ -n "$verdict" ]; then
      say "$verdict"
      [ "$FAKE" = 0 ] && [ "$resent" = 0 ] && say "(the first Enter was taken: #99 did not reproduce on this start)"
    else
      say "NOT SUBMITTED (#99): no UserPromptSubmit ${STARTUP_WAIT} s after the send and $resent resent Enter(s); input line:"
      tmux -L "$SOCKET" capture-pane -p -t "$B_TARGET" 2>/dev/null | grep '^❯' | sed 's/^/  /'
    fi
  fi
fi
say "=============================== END REPORT ==============================="
exit 0
