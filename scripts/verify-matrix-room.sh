#!/usr/bin/env bash
# The operator's view of verify-matrix-local's pinned room, over the client
# API (the room is unencrypted so everything is readable).
#
#   scripts/verify-matrix-room.sh events [N]           last N timeline events
#   scripts/verify-matrix-room.sh roots                 agent -> thread root event id
#   scripts/verify-matrix-room.sh reply <root> <body>   thread reply; prints the event id
#   scripts/verify-matrix-room.sh send <body>           room-level message; prints the event id
#   scripts/verify-matrix-room.sh members               who has joined
#
# Event lines are tab-separated: time, type, sender, event id, body or
# reaction key, relation type, related event id.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
HS_ROOT="$REPO/target/tmp/verify-matrix-local"
for f in homeserver.url operator.token room.id; do
  [ -f "$HS_ROOT/$f" ] || { echo "verify-matrix-room: $HS_ROOT/$f missing; run scripts/verify-matrix-local.sh first" >&2; exit 1; }
done
HS="$(cat "$HS_ROOT/homeserver.url")"
TOKEN="$(cat "$HS_ROOT/operator.token")"
ROOM="$(cat "$HS_ROOT/room.id")"

api() { curl -sS --max-time 20 -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
messages() { api "$HS/_matrix/client/v3/rooms/$ROOM/messages?dir=b&limit=${1:-30}"; }
txn() { date +%s%N; }

case "${1:-}" in
  events)
    messages "${2:-30}" | jq -r '.chunk | reverse | .[]
      | [(.origin_server_ts/1000|strftime("%H:%M:%S")), .type, .sender, .event_id,
         (.content.body // .content["m.relates_to"].key // ""),
         (.content["m.relates_to"].rel_type // ""),
         (.content["m.relates_to"].event_id // "")] | @tsv' ;;
  roots)
    messages 100 | jq -r '.chunk[] | select(.type=="m.room.message" and (.content.body // "" | test("session .* started")))
      | [(.content.body | capture("^\\*\\*(?<a>[^*]+)\\*\\*").a), .event_id] | @tsv' | sort -u | sed 's/\t/ /' ;;
  reply)
    [ $# -eq 3 ] || { echo "usage: $0 reply <root> <body>" >&2; exit 1; }
    api -X PUT "$HS/_matrix/client/v3/rooms/$ROOM/send/m.room.message/$(txn)" -d "$(jq -n --arg r "$2" --arg b "$3" \
      '{msgtype:"m.text", body:$b, "m.relates_to":{rel_type:"m.thread", event_id:$r, is_falling_back:true, "m.in_reply_to":{event_id:$r}}}')" | jq -r .event_id ;;
  send)
    [ $# -eq 2 ] || { echo "usage: $0 send <body>" >&2; exit 1; }
    api -X PUT "$HS/_matrix/client/v3/rooms/$ROOM/send/m.room.message/$(txn)" -d "$(jq -n --arg b "$2" '{msgtype:"m.text", body:$b}')" | jq -r .event_id ;;
  members)
    api "$HS/_matrix/client/v3/rooms/$ROOM/joined_members" | jq -r '.joined | keys[]' ;;
  *) echo "usage: $0 events [N] | roots | reply <root> <body> | send <body> | members" >&2; exit 1 ;;
esac
