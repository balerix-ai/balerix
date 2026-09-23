#!/usr/bin/env bash
# The by-hand check for Spec J: the real pinned `claude`, in tmux, answered
# with the key plans common's `question.rs` (plugins/common) produces, and the
# recorded answers read back from the PostToolUse hook.
#
# What it settles: that the dialog still behaves as Spec J §2 measured. The
# property test in plugins/common/src/question.rs proves the plans against a
# model of the dialog; this proves the model. Run it after bumping `claude`
# in mise.toml.
#
# Your real $HOME stays, for claude's credentials. Everything else lives
# under target/tmp/verify-questions, wiped every run. Costs a few short
# model turns. Not part of any CI tier.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$REPO/target/tmp/verify-questions"
WORK="$ROOT/work"
SOCKET="balerix-verify-questions-$$"
DELAY="${BALERIX_KEY_DELAY:-0.1}"
PLAN="$REPO/plugins/matrix/target/debug/examples/question_plan"

say() { printf '%s\n' "$*"; }
t() { tmux -L "$SOCKET" "$@"; }
pane() { t capture-pane -p -t s 2>/dev/null; }
lines() { if [ -f "$1" ]; then grep -c . "$1"; else echo 0; fi; }

cleanup() { t kill-server >/dev/null 2>&1; }
trap cleanup EXIT

wait_for() { # wait_for <seconds> <command…>: until the command succeeds
  local deadline=$((SECONDS + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then return 1; fi
    sleep 0.25
  done
}
pane_has() { pane | grep -q -- "$1"; }
pane_idle() { ! pane | grep -q 'to navigate\|esc to interrupt'; }
more_lines() { [ "$(lines "$1")" -gt "$2" ]; }

play() { # play <plan.json>: one tmux command per step, paced
  local step key
  while IFS= read -r step; do
    key=$(jq -r '.key // empty' <<<"$step")
    case "$key" in
      up) t send-keys -t s Up ;;
      down) t send-keys -t s Down ;;
      enter) t send-keys -t s Enter ;;
      escape) t send-keys -t s Escape ;;
      "") t send-keys -t s -l -- "$(jq -r '.text' <<<"$step")" ;;
      *) say "unknown key: $key"; return 1 ;;
    esac
    sleep "$DELAY"
  done < <(jq -c '.steps[]' "$1")
}

rm -rf "$ROOT"
mkdir -p "$WORK/.claude"
git -C "$WORK" init -q
PRE="$ROOT/pre.log"
POST="$ROOT/post.log"
jq -n --arg pre "cat >> $PRE; echo >> $PRE" --arg post "cat >> $POST; echo >> $POST" '{
  hooks: {
    PreToolUse:  [{ matcher: "AskUserQuestion", hooks: [{ type: "command", command: $pre }] }],
    PostToolUse: [{ matcher: "AskUserQuestion", hooks: [{ type: "command", command: $post }] }]
  } }' > "$WORK/.claude/settings.json"

say "building the plugin's question_plan example"
CARGO_TARGET_DIR="$REPO/plugins/matrix/target" cargo build -q \
  --manifest-path "$REPO/plugins/matrix/Cargo.toml" --example question_plan ||
  { say "cargo build failed"; exit 2; }

say "starting $(claude --version 2>/dev/null || echo claude) in tmux"
t new-session -d -s s -x 110 -y 40 -c "$WORK" claude
wait_for 30 pane_has '❯' || { say "claude never showed a prompt"; pane | tail -n 15; exit 2; }
sleep 1
if pane_has 'trust this folder'; then
  t send-keys -t s Down
  sleep 0.5
  t send-keys -t s Enter
  sleep 3
  wait_for 30 pane_has '❯' || { say "no prompt after trusting the folder"; exit 2; }
fi
sleep 1

COLOR="'Which color?' header 'Color' options Red, Green, Blue"
SIZE="'Which size?' header 'Size' options Small, Medium, Large"
COLORS="'Which colors?' header 'Colors' options Red, Green, Blue"
fails=0

run_case() { # run_case <name> <what to ask for> <reply> <expected answers JSON, or "declined">
  local name=$1 ask=$2 reply=$3 want=$4 before_pre before_post got
  before_pre=$(lines "$PRE")
  before_post=$(lines "$POST")
  t send-keys -t s -l -- "Use the AskUserQuestion tool exactly once, with exactly this: $ask. Give every option a two-word description. After I answer, reply with only: ok"
  t send-keys -t s Enter
  if ! wait_for 90 more_lines "$PRE" "$before_pre" || ! wait_for 30 pane_has 'to navigate'; then
    say "FAIL $name: the dialog never appeared"
    fails=$((fails + 1))
    return
  fi
  sleep 1
  grep . "$PRE" | tail -n 1 | jq '.tool_input' > "$ROOT/$name.input.json"
  if ! "$PLAN" "$ROOT/$name.input.json" "$reply" > "$ROOT/$name.plan.json"; then
    say "FAIL $name: question_plan refused the reply"
    t send-keys -t s Escape
    fails=$((fails + 1))
    return
  fi
  play "$ROOT/$name.plan.json"
  if [ "$want" = declined ]; then
    if wait_for 30 pane_has 'declined to answer'; then say "PASS $name"; else
      say "FAIL $name: claude did not report a declined question"
      fails=$((fails + 1))
    fi
  elif wait_for 20 more_lines "$POST" "$before_post"; then
    got=$(grep . "$POST" | tail -n 1 | jq -S -c '.tool_response.answers')
    if [ "$got" = "$(jq -S -c . <<<"$want")" ]; then say "PASS $name: $got"; else
      say "FAIL $name: recorded $got, wanted $want (keys: $(jq -c '.steps' "$ROOT/$name.plan.json"))"
      fails=$((fails + 1))
    fi
  else
    say "FAIL $name: never submitted (keys: $(jq -c '.steps' "$ROOT/$name.plan.json"))"
    pane | grep -v '^\s*$' | tail -n 12
    t send-keys -t s Escape
    fails=$((fails + 1))
  fi
  wait_for 60 pane_idle
  sleep 3
}

run_case single "ONE single-select question $COLOR" "2" \
  '{"Which color?":"Green"}'
run_case single-other "ONE single-select question $COLOR" "other: teal-ish" \
  '{"Which color?":"teal-ish"}'
run_case two "TWO single-select questions in the same call: $COLOR; and $SIZE" $'blue\nmedium' \
  '{"Which color?":"Blue","Which size?":"Medium"}'
run_case two-other "TWO single-select questions in the same call: $COLOR; and $SIZE" $'other: teal-ish\nlarge' \
  '{"Which color?":"teal-ish","Which size?":"Large"}'
run_case multi "ONE question with multiSelect true, $COLORS" "red, blue" \
  '{"Which colors?":"Red, Blue"}'
run_case multi-other "ONE question with multiSelect true, $COLORS" "green, other: a bit of gold" \
  '{"Which colors?":"Green, a bit of gold"}'
run_case mixed "TWO questions in the same call: first, with multiSelect true, $COLORS; second, single-select, $SIZE" $'red, blue\nmedium' \
  '{"Which colors?":"Red, Blue","Which size?":"Medium"}'
run_case skip "ONE single-select question $COLOR" "skip" declined

if [ "$fails" -eq 0 ]; then
  say "verify-questions: every dialog shape answered as planned"
else
  say "verify-questions: $fails case(s) failed — the dialog no longer matches Spec J §2; fix plugins/common/src/question.rs (plan and the test model together)"
  exit 1
fi
