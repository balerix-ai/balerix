#!/usr/bin/env bash
# verify-matrix, self-contained: a local tuwunel homeserver instead of a real
# one, so the Spec G / Spec M checks can be driven and read from this machine.
#
#   scripts/verify-matrix-local.sh [up]   bring everything up (default)
#   scripts/verify-matrix-local.sh down   stop it all; files stay for a look
#
# `up` starts tuwunel (mise task tool, see mise.toml) under
# target/tmp/verify-matrix-local, registers a bot and an operator, lets the
# operator make an UNencrypted room and invite the bot, hands the credentials
# to scripts/verify-matrix.sh (which packages the plugins and writes the
# daemon config under target/tmp/verify-matrix), pins that room for the
# verify crew, then starts a daemon and brings up a two-agent fleet the way
# verify-claude does: your real $HOME for claude's credentials, the XDG roots
# under target/tmp, the shared verify-data pool so claude downloads once.
#
# The room is unencrypted on purpose: every reaction and notice is plaintext,
# so scripts/verify-matrix-room.sh can read them over the client API. The
# checks themselves are the list scripts/verify-matrix.sh prints.
#
# Environment: VERIFY_MATRIX_PORT (default 6167), BALERIX_VERIFY_TIMEOUT
# (default 10m, the `up` timeout). Needs a logged-in `claude`.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
HS_ROOT="$REPO/target/tmp/verify-matrix-local"   # homeserver, accounts, room
ROOT="$REPO/target/tmp/verify-matrix"            # verify-matrix.sh's root: config, daemon state
DATA="$REPO/target/tmp/verify-data"              # survives runs: tool installs only
PORT="${VERIFY_MATRIX_PORT:-6167}"
HS="http://127.0.0.1:$PORT"
SERVER_NAME=localhost
BOT="@balerix:$SERVER_NAME"
OPERATOR="@operator:$SERVER_NAME"
FLEET=verify
CREW=c
SOCKET=balerix-verify-matrix
UP_TIMEOUT="${BALERIX_VERIFY_TIMEOUT:-10m}"

target="${CARGO_TARGET_DIR:-target}"
case "$target" in /*) ;; *) target="$REPO/$target" ;; esac
BALERIX="$target/debug/balerix"

export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export XDG_DATA_HOME="$DATA"
unset BALERIX_API_URL
SERVER="$XDG_STATE_HOME/balerix/server"

say() { printf '%s\n' "$*"; }
hr() { say "----- $* -----"; }
die() { say "verify-matrix-local: $*" >&2; exit 1; }
random_hex() { od -An -N16 -tx1 /dev/urandom | tr -d ' \n'; }
wait_for() { # wait_for <seconds> <command…>: until the command succeeds
  local deadline=$((SECONDS + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then return 1; fi
    sleep 0.25
  done
}
api() { curl -sS --max-time 20 -H 'content-type: application/json' "$@"; }
homeserver_up() { curl -sS --max-time 2 "$HS/_matrix/client/versions" >/dev/null; }
plugin_ready() { "$BALERIX" plugin list 2>/dev/null | grep -q '^matrix .* ready '; }

register() { # register <localpart> <password>: prints the access token
  local session
  session="$(api -X POST "$HS/_matrix/client/v3/register" -d '{}' | jq -r .session)"
  api -X POST "$HS/_matrix/client/v3/register" -d "$(jq -n --arg u "$1" --arg p "$2" --arg t "$REG_TOKEN" --arg s "$session" \
    '{username:$u, password:$p, initial_device_display_name:"verify-matrix-local",
      auth:{type:"m.login.registration_token", token:$t, session:$s}}')" | jq -r '.access_token // empty'
}

down() {
  hr "down"
  if [ -x "$BALERIX" ] && [ -f "$SERVER/endpoint" ]; then
    "$BALERIX" down "$FLEET" --timeout 2m >/dev/null 2>&1 || true
  fi
  if [ -f "$SERVER/balerix.pid" ]; then
    local pid; pid="$(cat "$SERVER/balerix.pid")"
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do [ -f "$SERVER/balerix.pid" ] || break; sleep 0.1; done
    say "daemon $pid stopped"
  fi
  tmux -L "$SOCKET" kill-server >/dev/null 2>&1 || true
  if [ -f "$HS_ROOT/tuwunel.pid" ]; then
    local pid; pid="$(cat "$HS_ROOT/tuwunel.pid")"
    kill -TERM "$pid" 2>/dev/null || true
    rm -f "$HS_ROOT/tuwunel.pid"
    say "tuwunel $pid stopped"
  fi
  say "files kept under $HS_ROOT and $ROOT"
}

up() {
  for tool in tuwunel curl jq tmux; do command -v "$tool" >/dev/null || die "$tool is not on PATH (run through 'mise run verify-matrix-local')"; done
  [ -f "$HOME/.claude/.credentials.json" ] || say "WARNING: $HOME/.claude/.credentials.json not found; the agents will ask you to log in"

  hr "previous run"
  down
  rm -rf "$HS_ROOT"
  mkdir -p "$HS_ROOT/db"

  hr "tuwunel $(tuwunel --version 2>/dev/null | head -n 1) on $HS"
  REG_TOKEN="$(random_hex)"
  cat > "$HS_ROOT/tuwunel.toml" <<TOML
[global]
server_name = "$SERVER_NAME"
database_path = "$HS_ROOT/db"
address = ["127.0.0.1"]
port = $PORT
allow_registration = true
registration_token = "$REG_TOKEN"
allow_federation = false
allow_guest_registration = false
log = "warn"
TOML
  (tuwunel -c "$HS_ROOT/tuwunel.toml" >"$HS_ROOT/tuwunel.log" 2>&1 & echo $! > "$HS_ROOT/tuwunel.pid")
  wait_for 30 homeserver_up || { tail -n 20 "$HS_ROOT/tuwunel.log"; die "tuwunel did not answer on $HS"; }

  hr "accounts and the pinned room"
  BOT_PASS="$(random_hex)"
  register balerix "$BOT_PASS" >/dev/null || die "registering $BOT failed"
  OP_TOKEN="$(register operator "$(random_hex)")"
  [ -n "$OP_TOKEN" ] || die "registering $OPERATOR failed"
  printf '%s' "$OP_TOKEN" > "$HS_ROOT/operator.token"; chmod 600 "$HS_ROOT/operator.token"
  ROOM="$(api -X POST "$HS/_matrix/client/v3/createRoom" -H "authorization: Bearer $OP_TOKEN" \
    -d "$(jq -n --arg b "$BOT" --arg n "balerix $FLEET/$CREW (pinned, unencrypted)" '{name:$n, preset:"private_chat", visibility:"private", invite:[$b]}')" | jq -r '.room_id // empty')"
  [ -n "$ROOM" ] || die "creating the pinned room failed"
  printf '%s' "$ROOM" > "$HS_ROOT/room.id"
  printf '%s' "$HS" > "$HS_ROOT/homeserver.url"
  say "bot $BOT, operator $OPERATOR, room $ROOM"

  hr "scripts/verify-matrix.sh (packages the plugins, writes $ROOT/config)"
  MATRIX_HOMESERVER="$HS" MATRIX_USER_ID="$BOT" MATRIX_PASSWORD="$BOT_PASS" MATRIX_INVITE="$OPERATOR" \
    scripts/verify-matrix.sh >"$HS_ROOT/verify-matrix.out" 2>&1 || { tail -n 20 "$HS_ROOT/verify-matrix.out"; die "verify-matrix.sh failed"; }
  printf '      rooms:\n        %s/%s: "%s"\n' "$FLEET" "$CREW" "$ROOM" >> "$ROOT/config/balerix/plugins.yaml"
  say "pinned $FLEET/$CREW to $ROOM in $ROOT/config/balerix/plugins.yaml"

  hr "build"
  cargo build -q -p balerix || die "cargo build failed"

  hr "fleet file"
  local work="$ROOT/work" bare="$ROOT/repo.git"
  mkdir -p "$work"
  git -C "$work" init -q -b main
  printf 'hello from verify-matrix-local\n' > "$work/README"
  git -C "$work" add README
  git -C "$work" -c user.name=verify -c user.email=verify@balerix.invalid commit -q -m init
  git clone -q --bare "$work" "$bare"
  cat > "$ROOT/fleet.yaml" <<YAML
apiVersion: balerix/v1
kind: Fleet
name: $FLEET
defaults:
  claude:
    settings: {}
  tools: {}
crews:
  $CREW:
    repo: "file://$bare"
    ref: main
    git: { push: false, auth: none }
    agents:
      a: { plugins: { matrix: {} } }
      b: { plugins: { matrix: {} } }
YAML
  say "$ROOT/fleet.yaml: crew $CREW, agents a and b, each with matrix"

  hr "serve -d"
  mkdir -p "$XDG_STATE_HOME" "$XDG_DATA_HOME"
  "$BALERIX" serve -d --bind 127.0.0.1:0 --tmux-socket "$SOCKET" || { tail -n 40 "$SERVER/server.log"; die "serve -d failed"; }
  wait_for 60 plugin_ready || { "$BALERIX" plugin list; die "the matrix plugin never reached ready"; }
  "$BALERIX" plugin list

  hr "up (timeout $UP_TIMEOUT; the first run installs the agents' pinned claude)"
  "$BALERIX" up "$ROOT/fleet.yaml" --timeout "$UP_TIMEOUT" || { tail -n 20 "$SERVER/server.log"; die "up failed"; }
  wait_for 30 sh -c "scripts/verify-matrix-room.sh roots | grep -q ' b '" || true

  hr "ready"
  say "endpoint:     $(cat "$SERVER/endpoint")"
  say "room:         $ROOM"
  say "thread roots:"; scripts/verify-matrix-room.sh roots | sed 's/^/  /'
  say
  say "Drive the checks scripts/verify-matrix.sh printed (see $HS_ROOT/verify-matrix.out) with:"
  say "  scripts/verify-matrix-room.sh events [N]         the last N room events, one per line"
  say "  scripts/verify-matrix-room.sh reply <root> <body> a thread reply as the operator"
  say "  scripts/verify-matrix-room.sh send <body>         a room-level message"
  say "  tmux -L $SOCKET attach                            the agents' panes"
  say "and finish with: scripts/verify-matrix-local.sh down"
}

case "${1:-up}" in
  up) up ;;
  down) down ;;
  *) die "usage: $0 [up|down]" ;;
esac
