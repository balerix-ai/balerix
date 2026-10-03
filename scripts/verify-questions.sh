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
# Each prompt is confirmed by its UserPromptSubmit hook and Enter is pressed
# again until it is: an Enter sent right after `send-keys -l` is sometimes
# taken as a newline or dropped, as `send_text`'s are (#99), and an
# unconfirmed prompt would otherwise be submitted glued to the next case's.
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
dialog_open() { pane_has 'to navigate'; }

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
SUBMIT="$ROOT/submit.log"
jq -n --arg pre "cat >> $PRE; echo >> $PRE" --arg post "cat >> $POST; echo >> $POST" \
  --arg submit "cat >> $SUBMIT; echo >> $SUBMIT" '{
  hooks: {
    PreToolUse:  [{ matcher: "AskUserQuestion", hooks: [{ type: "command", command: $pre }] }],
    PostToolUse: [{ matcher: "AskUserQuestion", hooks: [{ type: "command", command: $post }] }],
    UserPromptSubmit: [{ hooks: [{ type: "command", command: $submit }] }]
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
fails=0      # the dialog did not answer as planned: question.rs's business
unrelated=0  # the prompt never reached the model, or the model asked something else

submit() { # submit <text>: type it, press Enter until UserPromptSubmit shows it
  local text=$1 before tries=0
  before=$(lines "$SUBMIT")
  t send-keys -t s -l -- "$text"
  sleep 0.5
  while :; do
    t send-keys -t s Enter
    wait_for 5 more_lines "$SUBMIT" "$before" && break
    # taken but its hook still running: another Enter would answer the dialog
    dialog_open && break
    tries=$((tries + 1))
    say "Enter again ($tries): ${text:0:80}" >> "$ROOT/resubmits.log"
    [ "$tries" -lt 6 ] || return 1
  done
  wait_for 5 more_lines "$SUBMIT" "$before" &&
    [ "$(grep . "$SUBMIT" | tail -n 1 | jq -r '.prompt')" = "$text" ]
}

reset() { # after a failed case: close any dialog, wait for idle, empty the composer
  t send-keys -t s Escape
  sleep 1
  wait_for 60 pane_idle
  # a quick double Escape clears the composer; on an empty one it opens Rewind
  t send-keys -t s Escape
  sleep 0.2
  t send-keys -t s Escape
  sleep 1
  if pane_has 'Rewind'; then t send-keys -t s Escape; sleep 1; fi
}

# run_case <name> <multiSelect of each question, e.g. false,true> <what to ask for>
#          <reply> <expected answers JSON, or "declined">
run_case() {
  local name=$1 shape=$2 ask=$3 reply=$4 want=$5 before_pre before_post got
  before_pre=$(lines "$PRE")
  before_post=$(lines "$POST")
  if ! submit "Use the AskUserQuestion tool exactly once, with exactly this: $ask. Give every option a two-word description. After I answer, reply with only: ok"; then
    say "FAIL $name: the prompt was never submitted on its own (see $SUBMIT)"
    pane | grep -v '^\s*$' | tail -n 12
    unrelated=$((unrelated + 1))
    reset
    return
  fi
  if ! wait_for 90 more_lines "$PRE" "$before_pre" || ! wait_for 30 dialog_open; then
    say "FAIL $name: the dialog never appeared"
    pane | grep -v '^\s*$' | tail -n 12
    fails=$((fails + 1))
    reset
    return
  fi
  sleep 1
  grep . "$PRE" | tail -n "+$((before_pre + 1))" | head -n 1 | jq '.tool_input' > "$ROOT/$name.input.json"
  got=$(jq -r '[.questions[] | .multiSelect == true | tostring] | join(",")' "$ROOT/$name.input.json")
  if [ "$got" != "$shape" ]; then
    say "FAIL $name: the model asked multiSelect $got, not $shape (see $ROOT/$name.input.json)"
    unrelated=$((unrelated + 1))
    reset
    return
  fi
  if ! "$PLAN" "$ROOT/$name.input.json" "$reply" > "$ROOT/$name.plan.json"; then
    say "FAIL $name: question_plan refused the reply"
    fails=$((fails + 1))
    reset
    return
  fi
  play "$ROOT/$name.plan.json"
  if [ "$want" = declined ]; then
    if wait_for 30 pane_has 'declined to answer'; then say "PASS $name"; else
      say "FAIL $name: claude did not report a declined question"
      fails=$((fails + 1))
      reset
      return
    fi
  elif wait_for 20 more_lines "$POST" "$before_post"; then
    got=$(grep . "$POST" | tail -n 1 | jq -S -c '.tool_response.answers')
    if [ "$got" = "$(jq -S -c . <<<"$want")" ]; then say "PASS $name: $got"; else
      say "FAIL $name: recorded $got, wanted $want (keys: $(jq -c '.steps' "$ROOT/$name.plan.json"))"
      fails=$((fails + 1))
      reset
      return
    fi
  else
    say "FAIL $name: never submitted (keys: $(jq -c '.steps' "$ROOT/$name.plan.json"))"
    pane | grep -v '^\s*$' | tail -n 12
    fails=$((fails + 1))
    reset
    return
  fi
  wait_for 60 pane_idle
  sleep 3
}

run_case single false "ONE single-select question $COLOR" "2" \
  '{"Which color?":"Green"}'
run_case single-other false "ONE single-select question $COLOR" "other: teal-ish" \
  '{"Which color?":"teal-ish"}'
run_case two false,false "TWO single-select questions in the same call: $COLOR; and $SIZE" $'blue\nmedium' \
  '{"Which color?":"Blue","Which size?":"Medium"}'
run_case two-other false,false "TWO single-select questions in the same call: $COLOR; and $SIZE" $'other: teal-ish\nlarge' \
  '{"Which color?":"teal-ish","Which size?":"Large"}'
run_case multi true "ONE question with multiSelect true, $COLORS" "red, blue" \
  '{"Which colors?":"Red, Blue"}'
run_case multi-other true "ONE question with multiSelect true, $COLORS" "green, other: a bit of gold" \
  '{"Which colors?":"Green, a bit of gold"}'
run_case mixed true,false "TWO questions in the same call: first, with multiSelect true, $COLORS; second, single-select, $SIZE" $'red, blue\nmedium' \
  '{"Which colors?":"Red, Blue","Which size?":"Medium"}'
run_case skip false "ONE single-select question $COLOR" "skip" declined

if [ "$fails" -eq 0 ] && [ "$unrelated" -eq 0 ]; then
  say "verify-questions: every dialog shape answered as planned"
else
  if [ "$fails" -gt 0 ]; then
    say "verify-questions: $fails case(s) failed — the dialog no longer matches Spec J §2; fix plugins/common/src/question.rs (plan and the test model together)"
  fi
  if [ "$unrelated" -gt 0 ]; then
    say "verify-questions: $unrelated case(s) never reached a dialog of the asked shape (prompt not submitted, or the model asked something else) — that says nothing about the dialog; run it again"
  fi
  exit 1
fi
