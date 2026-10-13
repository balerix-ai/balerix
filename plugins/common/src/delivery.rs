//! Delivery confirmation (Spec M §8.7, #85). `send_text`'s `Ok` says the
//! daemon typed the body into the pane and pressed Enter; it does not say
//! Claude took a prompt. The agent's own `UserPromptSubmit` carries the
//! text Claude took, and that is the proof. This tracker is pure: the
//! plugin feeds it what it sent and every hook event, and executes what
//! it answers (a "sent" mark, a confirmation, a note).
//!
//! Text typed mid-turn is queued by Claude and submitted only after the
//! current turn's `Stop`, so a pending prompt's clock starts at the later
//! of its send and the agent's last `Stop`, and never runs while the
//! agent is mid-turn.
//!
//! A prompt sent while Claude's TUI is starting (the first prompt, sent
//! on `SessionStart`) lands in the composer and loses its Enter (#99).
//! `nudge` answers the agents whose prompt has sat unconfirmed for
//! `NUDGE_AFTER`, and the plugin presses Enter there once more, again
//! every `NUDGE_AFTER` for up to `NUDGE_FOR`. It is stricter than
//! expiry: an Enter goes only to an agent known to be idle, with no
//! prompt or question open. Known idle means a `Stop` or a `SessionStart`
//! (not a compaction's) seen since the tracker started, and no sign of a
//! turn since; a tracker that has seen neither (a plugin restarted while
//! the agent works, a lost `UserPromptSubmit`) treats the agent as
//! mid-turn. A `Notification` other than an idle or sign-in notice, or an
//! open `AskUserQuestion`, holds the nudge until the turn ends. Any event
//! holds it for `NUDGE_AFTER`, and a gap in the plugin's own queue
//! (`lose_track`) makes every idle agent's turn unknown again.
//!
//! Every event creates an agent's state on arrival if it did not already
//! exist, so the turn and the last `Stop` are recorded even before
//! anything was ever sent to that agent. `forget` frees the entry.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use balerix_api::HookEvent;
use serde_json::Value;
use tokio::time::Instant;

/// Prompts pending per agent beyond which the oldest is evicted and
/// reported unconfirmed: a stuck agent must not grow this without bound.
pub const MAX_PENDING: usize = 64;

/// How long a prompt sits unconfirmed on an idle agent before its Enter
/// is pressed again, and the pause between two such presses.
pub const NUDGE_AFTER: Duration = Duration::from_secs(5);

/// How long after a prompt's clock starts its Enter is still pressed
/// again: what bounds a prompt whose window is zero, which never expires.
/// A cold start under nono has swallowed an Enter 20 s after
/// `SessionStart` and taken one at 60 s (#99).
pub const NUDGE_FOR: Duration = Duration::from_secs(90);

/// What a body is to Claude's input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Prose: a `UserPromptSubmit` will carry it if Claude takes it.
    Prompt,
    /// A slash command: most fire no hook, so "sent" is all a plugin can
    /// honestly say. Never tracked.
    Command,
}

pub use balerix_api::MODE_CHARS;

/// A channel's text as prose for Claude's input line: the daemon's own
/// rule ([`balerix_api::prompt_text`]), so the text tracked is the text
/// typed. Terminal controls go, then leading blanks (whitespace, controls,
/// format characters); a body that then opens with one of [`MODE_CHARS`]
/// gains a backslash, Markdown's escape for a literal character, so text
/// from a channel never opens the input line in another input mode.
/// Slash commands pass through.
pub fn as_prompt(text: &str) -> Cow<'_, str> {
    balerix_api::prompt_text(text)
}

/// A body whose first non-blank character is `/` is a command.
pub fn classify(text: &str) -> Kind {
    if text.trim_start().starts_with('/') {
        Kind::Command
    } else {
        Kind::Prompt
    }
}

/// Trimmed at both ends, each run of whitespace one space: what two
/// texts must agree on to be the same prompt.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What one hook event settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome<M> {
    /// The prompt this `UserPromptSubmit` took.
    pub confirmed: Option<M>,
    /// Older prompts it passed over: Claude submits queued text in order,
    /// so a skipped one was swallowed. Unconfirmed at once.
    pub skipped: Vec<M>,
}

impl<M> Default for Outcome<M> {
    fn default() -> Self {
        Self {
            confirmed: None,
            skipped: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct Pending<M> {
    text: String,
    marker: M,
    sent_at: Instant,
    window: Duration,
}

/// What the tracker knows of an agent's turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Turn {
    /// Nothing seen yet that says, or since a `SessionEnd`. Expiry runs;
    /// a nudge does not.
    #[default]
    Unknown,
    /// A `Stop` or a `SessionStart` seen, and no sign of a turn since.
    Idle,
    /// A `UserPromptSubmit`, a tool event or any other event seen since.
    Busy,
}

#[derive(Debug)]
struct AgentState<M> {
    pending: VecDeque<Pending<M>>,
    turn: Turn,
    /// A `Notification` that may be a dialog, until the turn ends.
    prompt_open: bool,
    /// An `AskUserQuestion` between its `PreToolUse` and `PostToolUse`.
    question_open: bool,
    last_stop: Option<Instant>,
    last_nudge: Option<Instant>,
    /// The last event of any name: a nudge waits for `NUDGE_AFTER` of quiet.
    last_event: Option<Instant>,
}

impl<M> Default for AgentState<M> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            turn: Turn::Unknown,
            prompt_open: false,
            question_open: false,
            last_stop: None,
            last_nudge: None,
            last_event: None,
        }
    }
}

impl<M> AgentState<M> {
    /// The turn is over: no dialog of it can still be open.
    fn settle(&mut self, turn: Turn) {
        self.turn = turn;
        self.prompt_open = false;
        self.question_open = false;
    }

    /// Only an agent known to be idle, with nothing open and no event
    /// for `NUDGE_AFTER`, takes an Enter.
    fn takes_enter(&self, now: Instant) -> bool {
        self.turn == Turn::Idle
            && !self.prompt_open
            && !self.question_open
            && self
                .last_event
                .is_none_or(|e| now.duration_since(e) >= NUDGE_AFTER)
    }
}

/// The tool whose dialog is a question (Spec J).
const QUESTION_TOOL: &str = "AskUserQuestion";

/// `Notification` kinds that never mean a dialog is open; any other kind,
/// or none, holds the nudge.
const QUIET_NOTIFICATIONS: [&str; 2] = ["idle_prompt", "auth_success"];

/// The prompts sent and not yet confirmed, per agent. `M` is the
/// plugin's marker for a sent prompt (a message id, a comment id).
#[derive(Debug)]
pub struct Deliveries<M> {
    agents: HashMap<String, AgentState<M>>,
}

impl<M> Default for Deliveries<M> {
    fn default() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }
}

impl<M> Deliveries<M> {
    /// Registers a prompt typed into `agent`'s pane at `now`. `window` is
    /// how long it may go unconfirmed before `expire` reports it; zero
    /// means never. Answers the oldest pending prompt when this one
    /// pushed the queue past `MAX_PENDING`; the caller reports it
    /// unconfirmed. A body that normalises to nothing is not tracked.
    pub fn sent(
        &mut self,
        agent: &str,
        text: &str,
        marker: M,
        now: Instant,
        window: Duration,
    ) -> Option<M> {
        let text = normalize(text);
        if text.is_empty() {
            return None;
        }
        let state = self.agents.entry(agent.to_string()).or_default();
        state.pending.push_back(Pending {
            text,
            marker,
            sent_at: now,
            window,
        });
        if state.pending.len() > MAX_PENDING {
            return state.pending.pop_front().map(|p| p.marker);
        }
        None
    }

    /// One hook event. `UserPromptSubmit` starts a turn and, when its
    /// `prompt` matches a pending prompt, confirms the oldest such and
    /// skips everything older. `Stop` ends the turn and marks the agent
    /// idle; so does a `SessionStart` other than a compaction's, which
    /// leaves the turn as it was. `SessionEnd` ends the turn with the
    /// agent's state unknown. A `Notification` that may be a dialog holds
    /// the nudge until one of those three; an `AskUserQuestion`'s
    /// `PreToolUse` holds it until its `PostToolUse`. Every other event
    /// is a sign of a turn. Every event creates the agent's state if it
    /// did not already exist (Spec M §8.7: the turn and the last `Stop`
    /// are tracked unconditionally).
    pub fn on_event(&mut self, event: &HookEvent, now: Instant) -> Outcome<M> {
        let state = self.agents.entry(event.agent.clone()).or_default();
        state.last_event = Some(now);
        let field = |key: &str| event.payload.get(key).and_then(Value::as_str);
        match event.name.as_str() {
            "UserPromptSubmit" => {
                state.turn = Turn::Busy;
                let prompt = normalize(
                    event
                        .payload
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
                let Some(i) = state.pending.iter().position(|p| p.text == prompt) else {
                    return Outcome::default();
                };
                let skipped = state.pending.drain(..i).map(|p| p.marker).collect();
                Outcome {
                    confirmed: state.pending.pop_front().map(|p| p.marker),
                    skipped,
                }
            }
            "Stop" => {
                state.settle(Turn::Idle);
                state.last_stop = Some(now);
                Outcome::default()
            }
            "SessionEnd" => {
                state.settle(Turn::Unknown);
                state.last_stop = Some(now);
                Outcome::default()
            }
            "SessionStart" => {
                if field("source") != Some("compact") {
                    state.settle(Turn::Idle);
                }
                Outcome::default()
            }
            "Notification" => {
                if !field("notification_type").is_some_and(|k| QUIET_NOTIFICATIONS.contains(&k)) {
                    state.prompt_open = true;
                }
                Outcome::default()
            }
            name => {
                state.turn = Turn::Busy;
                if field("tool_name") == Some(QUESTION_TOOL) {
                    match name {
                        "PreToolUse" => state.question_open = true,
                        "PostToolUse" => state.question_open = false,
                        _ => {}
                    }
                }
                Outcome::default()
            }
        }
    }

    /// Every pending prompt whose window has passed since the later of
    /// its send and the agent's last `Stop`, on agents not known to be
    /// mid-turn. Removed as they are answered.
    pub fn expire(&mut self, now: Instant) -> Vec<M> {
        let mut out = Vec::new();
        for state in self.agents.values_mut() {
            if state.turn == Turn::Busy {
                continue;
            }
            let last_stop = state.last_stop;
            let mut kept = VecDeque::new();
            for p in state.pending.drain(..) {
                let start = last_stop.map_or(p.sent_at, |s| s.max(p.sent_at));
                if !p.window.is_zero() && now.duration_since(start) >= p.window {
                    out.push(p.marker);
                } else {
                    kept.push_back(p);
                }
            }
            state.pending = kept;
        }
        out
    }

    /// The agents, in name order, to press Enter on once more: those
    /// known to be idle, with no prompt or question open and no event for
    /// `NUDGE_AFTER`, holding a prompt whose clock (the later of its send
    /// and the agent's last `Stop`) started less than `NUDGE_FOR` ago and
    /// less than its window ago, `NUDGE_AFTER` after the later of that
    /// start and the agent's last nudge. One Enter submits the composer
    /// whatever it holds, so an agent is answered once however many of
    /// its prompts wait. Nothing is removed: confirmation and expiry stay
    /// what settles a prompt.
    pub fn nudge(&mut self, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        for (agent, state) in &mut self.agents {
            if !state.takes_enter(now) {
                continue;
            }
            let (last_stop, last_nudge) = (state.last_stop, state.last_nudge);
            let due = state.pending.iter().any(|p| {
                let start = last_stop.map_or(p.sent_at, |s| s.max(p.sent_at));
                let waited = now.duration_since(start);
                let since_nudge = last_nudge.map_or(waited, |n| now.duration_since(n.max(start)));
                waited < NUDGE_FOR
                    && (p.window.is_zero() || waited < p.window)
                    && since_nudge >= NUDGE_AFTER
            });
            if due {
                state.last_nudge = Some(now);
                out.push(agent.clone());
            }
        }
        out.sort();
        out
    }

    /// Events may have been lost (the plugin's queue dropped some): every
    /// idle agent's turn is unknown again, so none is nudged before its
    /// next `Stop` or `SessionStart`; a busy one stays busy. Pending
    /// prompts and their clocks stay.
    pub fn lose_track(&mut self) {
        for state in self.agents.values_mut() {
            if state.turn == Turn::Idle {
                state.turn = Turn::Unknown;
            }
        }
    }

    /// Drops everything held for `agent`, answering the markers so the
    /// caller may report or ignore them.
    pub fn forget(&mut self, agent: &str) -> Vec<M> {
        self.agents
            .remove(agent)
            .map(|s| s.pending.into_iter().map(|p| p.marker).collect())
            .unwrap_or_default()
    }

    /// How many prompts `agent` has pending.
    pub fn pending(&self, agent: &str) -> usize {
        self.agents.get(agent).map_or(0, |s| s.pending.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::event;
    use serde_json::json;

    const W: Duration = Duration::from_secs(30);

    fn submit(text: &str) -> HookEvent {
        event("f/c/a", "UserPromptSubmit", json!({ "prompt": text }))
    }

    fn stop() -> HookEvent {
        event("f/c/a", "Stop", json!({}))
    }

    /// A fresh session on `agent`: what makes a new tracker's agent idle.
    fn start<M>(d: &mut Deliveries<M>, agent: &str, now: Instant) {
        d.on_event(
            &event(agent, "SessionStart", json!({ "source": "startup" })),
            now,
        );
    }

    #[test]
    fn a_slash_command_is_a_command_and_prose_is_a_prompt() {
        assert_eq!(classify("/exit"), Kind::Command);
        assert_eq!(classify("  /compact "), Kind::Command);
        assert_eq!(classify("run /tmp/x"), Kind::Prompt);
        assert_eq!(classify(""), Kind::Prompt);
    }

    #[test]
    fn normalize_trims_and_collapses_whitespace() {
        assert_eq!(normalize("  run\n\n the\t tests  "), "run the tests");
        assert_eq!(normalize(" \n "), "");
    }

    #[test]
    fn an_exact_and_a_whitespace_differing_submit_confirm() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        assert_eq!(d.sent("f/c/a", "run the tests", 1, now, W), None);
        let o = d.on_event(&submit("run the tests"), now);
        assert_eq!(
            o,
            Outcome {
                confirmed: Some(1),
                skipped: vec![]
            }
        );
        assert_eq!(d.pending("f/c/a"), 0);

        d.sent("f/c/a", "run\n  the tests\n", 2, now, W);
        let o = d.on_event(&submit("  run the   tests"), now);
        assert_eq!(o.confirmed, Some(2));
    }

    #[test]
    fn an_unrelated_submit_is_ignored_but_starts_a_turn() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "run the tests", 1, now, W);
        let o = d.on_event(&submit("something the operator typed"), now);
        assert_eq!(o, Outcome::default());
        assert_eq!(d.pending("f/c/a"), 1);
        // mid-turn: the window does not run
        assert!(d.expire(now + Duration::from_secs(600)).is_empty());
    }

    #[test]
    fn a_later_prompt_taken_first_skips_the_earlier_one() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "first", 1, now, W);
        d.sent("f/c/a", "second", 2, now, W);
        d.sent("f/c/a", "third", 3, now, W);
        let o = d.on_event(&submit("second"), now);
        assert_eq!(
            o,
            Outcome {
                confirmed: Some(2),
                skipped: vec![1]
            }
        );
        assert_eq!(d.pending("f/c/a"), 1, "the third still waits");
    }

    #[test]
    fn expiry_counts_from_the_send_when_the_agent_is_idle() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, W);
        assert!(d.expire(now + Duration::from_secs(29)).is_empty());
        assert_eq!(d.expire(now + Duration::from_secs(30)), vec![1]);
        assert_eq!(d.pending("f/c/a"), 0);
    }

    #[test]
    fn expiry_waits_for_the_turn_to_end_then_counts_from_the_stop() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.on_event(&submit("earlier work"), now); // mid-turn (nothing pending yet)
        d.sent("f/c/a", "x", 1, now, W);
        assert!(
            d.expire(now + Duration::from_secs(600)).is_empty(),
            "mid-turn"
        );
        let stopped = now + Duration::from_secs(600);
        d.on_event(&stop(), stopped);
        assert!(d.expire(stopped + Duration::from_secs(29)).is_empty());
        assert_eq!(d.expire(stopped + Duration::from_secs(30)), vec![1]);
    }

    #[test]
    fn a_turn_that_began_before_the_first_send_still_holds_the_window() {
        // The agent was never sent to, so the tracker has no state for it
        // when its `UserPromptSubmit` arrives; the turn must be recorded
        // anyway, or a prompt routed mid-turn expires while Claude works.
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.on_event(&submit("earlier work"), now);
        d.sent("f/c/a", "x", 1, now, W);
        assert!(
            d.expire(now + Duration::from_secs(600)).is_empty(),
            "mid-turn"
        );
        let stopped = now + Duration::from_secs(600);
        d.on_event(&stop(), stopped);
        assert_eq!(d.expire(stopped + W), vec![1]);
    }

    #[test]
    fn session_end_clears_the_turn_so_expiry_can_run() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.on_event(&submit("earlier"), now);
        d.sent("f/c/a", "x", 1, now, W);
        d.on_event(&event("f/c/a", "SessionEnd", json!({})), now);
        assert_eq!(d.expire(now + W), vec![1]);
    }

    #[test]
    fn a_zero_window_never_expires_and_still_confirms() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        assert!(d.expire(now + Duration::from_secs(86_400)).is_empty());
        assert_eq!(d.on_event(&submit("x"), now).confirmed, Some(1));
    }

    #[test]
    fn an_idle_agent_is_nudged_once_its_prompt_is_five_seconds_unconfirmed() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, W);
        assert!(d.nudge(now + Duration::from_secs(4)).is_empty());
        assert_eq!(d.nudge(now + Duration::from_secs(5)), vec!["f/c/a"]);
        assert_eq!(d.pending("f/c/a"), 1, "a nudge settles nothing");
    }

    #[test]
    fn the_next_nudge_waits_five_seconds_from_the_last() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, W);
        assert_eq!(d.nudge(now + Duration::from_secs(7)), vec!["f/c/a"]);
        assert!(d.nudge(now + Duration::from_secs(11)).is_empty());
        assert_eq!(d.nudge(now + Duration::from_secs(12)), vec!["f/c/a"]);
    }

    #[test]
    fn no_nudge_mid_turn_and_the_wait_restarts_at_the_stop() {
        // An Enter pressed mid-turn would answer whatever dialog is open.
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.on_event(&submit("earlier work"), now);
        d.sent("f/c/a", "x", 1, now, W);
        assert!(d.nudge(now + Duration::from_secs(5)).is_empty(), "mid-turn");
        let stopped = now + Duration::from_secs(600);
        d.on_event(&stop(), stopped);
        assert!(d.nudge(stopped + Duration::from_secs(4)).is_empty());
        assert_eq!(d.nudge(stopped + Duration::from_secs(5)), vec!["f/c/a"]);
    }

    #[test]
    fn a_confirmed_prompt_is_not_nudged() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, W);
        d.on_event(&submit("x"), now + Duration::from_secs(1));
        d.on_event(&stop(), now + Duration::from_secs(2));
        assert!(d.nudge(now + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn a_prompt_past_its_window_is_not_nudged() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, W);
        assert!(d.nudge(now + W).is_empty(), "expiry's, not a nudge's");
        assert_eq!(d.expire(now + W), vec![1]);
    }

    #[test]
    fn several_pending_prompts_are_one_enter() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "first", 1, now, W);
        d.sent("f/c/a", "second", 2, now + Duration::from_secs(1), W);
        assert_eq!(d.nudge(now + Duration::from_secs(6)), vec!["f/c/a"]);
        assert!(d.nudge(now + Duration::from_secs(7)).is_empty());
    }

    #[test]
    fn a_prompt_that_never_expires_is_nudged_for_ninety_seconds_and_no_longer() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        let nudged: Vec<u64> = (1..=100)
            .map(|i| 5 * i)
            .filter(|s| !d.nudge(now + Duration::from_secs(*s)).is_empty())
            .collect();
        assert_eq!(nudged.first(), Some(&5));
        assert_eq!(nudged.last(), Some(&85));
        assert_eq!(nudged.len(), 17, "every five seconds in between");
        assert_eq!(d.on_event(&submit("x"), now).confirmed, Some(1));
    }

    #[test]
    fn a_long_window_is_nudged_past_thirty_seconds() {
        // a cold start under nono swallowed an Enter 20 s after `SessionStart`
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::from_secs(90));
        for s in [5, 10, 15, 20, 25, 30, 35] {
            assert_eq!(
                d.nudge(now + Duration::from_secs(s)),
                vec!["f/c/a"],
                "at {s} s"
            );
        }
    }

    #[test]
    fn every_idle_agent_with_a_stale_prompt_is_nudged_in_name_order() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        for agent in ["f/c/a", "f/c/b", "f/c/late"] {
            start(&mut d, agent, now);
        }
        d.sent("f/c/b", "y", 2, now, W);
        d.sent("f/c/a", "x", 1, now, W);
        d.sent("f/c/late", "z", 3, now + Duration::from_secs(3), W);
        assert_eq!(
            d.nudge(now + Duration::from_secs(5)),
            vec!["f/c/a", "f/c/b"]
        );
    }

    #[test]
    fn a_blank_body_is_not_tracked() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        assert_eq!(d.sent("f/c/a", " \n ", 1, now, W), None);
        assert_eq!(d.pending("f/c/a"), 0);
        assert_eq!(d.on_event(&submit(""), now), Outcome::default());
    }

    #[test]
    fn the_queue_is_bounded_and_evicts_the_oldest() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        for i in 0..MAX_PENDING {
            assert_eq!(d.sent("f/c/a", &format!("p{i}"), i, now, W), None);
        }
        assert_eq!(d.sent("f/c/a", "one more", MAX_PENDING, now, W), Some(0));
        assert_eq!(d.pending("f/c/a"), MAX_PENDING);
    }

    #[test]
    fn forget_drops_an_agent_and_answers_its_markers() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, W);
        d.sent("f/c/b", "y", 2, now, W);
        assert_eq!(d.forget("f/c/a"), vec![1]);
        assert_eq!(d.forget("f/c/a"), Vec::<u32>::new());
        assert_eq!(d.pending("f/c/b"), 1);
        assert_eq!(
            d.on_event(&event("f/c/a", "Stop", json!({})), now),
            Outcome::default()
        );
    }
    fn notification(kind: &str) -> HookEvent {
        event(
            "f/c/a",
            "Notification",
            json!({ "message": "Claude is waiting", "notification_type": kind }),
        )
    }

    fn question(name: &str) -> HookEvent {
        event(
            "f/c/a",
            name,
            json!({ "tool_name": "AskUserQuestion", "tool_input": {} }),
        )
    }

    /// Every five seconds for `secs`, whether an Enter was answered.
    fn any_nudge<M>(d: &mut Deliveries<M>, from: Instant, secs: u64) -> bool {
        (1..=secs / 5).any(|i| !d.nudge(from + Duration::from_secs(5 * i)).is_empty())
    }

    #[test]
    fn a_new_tracker_never_nudges_before_it_has_seen_the_agent_idle() {
        // The tracker starts with the agent's turn unknown: it may be
        // mid-turn, so no Enter goes to it. Expiry still runs.
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        assert!(!any_nudge(&mut d, now, 200));
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, W);
        assert_eq!(d.expire(now + W), vec![1]);
    }

    #[test]
    fn a_new_tracker_with_a_reply_and_a_notification_never_nudges() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "check the logs", 1, now, Duration::ZERO);
        d.on_event(
            &notification("elicitation_dialog"),
            now + Duration::from_secs(1),
        );
        assert!(!any_nudge(&mut d, now, 200));
        // nor once a tool event says the agent is working
        let mut d = Deliveries::default();
        d.sent("f/c/a", "check the logs", 1, now, Duration::ZERO);
        d.on_event(
            &event("f/c/a", "PreToolUse", json!({ "tool_name": "Read" })),
            now,
        );
        d.on_event(&notification("elicitation_dialog"), now);
        assert!(!any_nudge(&mut d, now, 200));
    }

    #[test]
    fn nudging_starts_after_a_stop() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        assert!(d.nudge(now + Duration::from_secs(5)).is_empty());
        let stopped = now + Duration::from_secs(10);
        d.on_event(&stop(), stopped);
        assert!(d.nudge(stopped + Duration::from_secs(4)).is_empty());
        assert_eq!(d.nudge(stopped + Duration::from_secs(5)), vec!["f/c/a"]);
    }

    #[test]
    fn a_notification_holds_the_nudge_until_the_turn_ends() {
        // An idle agent by the tracker's account, then a notification: the
        // events between were lost, and the notification wins.
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        d.on_event(&notification("elicitation_dialog"), now);
        assert!(!any_nudge(&mut d, now, 60));
        let stopped = now + Duration::from_secs(60);
        d.on_event(&stop(), stopped);
        assert_eq!(d.nudge(stopped + Duration::from_secs(5)), vec!["f/c/a"]);
    }

    #[test]
    fn a_notification_of_no_known_kind_holds_the_nudge() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        d.on_event(
            &event("f/c/a", "Notification", json!({ "message": "?" })),
            now,
        );
        assert!(!any_nudge(&mut d, now, 60));
    }

    #[test]
    fn an_idle_notice_does_not_hold_the_nudge() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        d.on_event(&notification("idle_prompt"), now);
        assert_eq!(d.nudge(now + Duration::from_secs(5)), vec!["f/c/a"]);
    }

    #[test]
    fn an_open_question_holds_the_nudge_until_the_turn_ends() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        d.on_event(&question("PreToolUse"), now);
        assert!(!any_nudge(&mut d, now, 60));
        // answered, but the turn goes on
        d.on_event(&question("PostToolUse"), now + Duration::from_secs(60));
        assert!(!any_nudge(&mut d, now + Duration::from_secs(60), 60));
        let stopped = now + Duration::from_secs(120);
        d.on_event(&stop(), stopped);
        assert_eq!(d.nudge(stopped + Duration::from_secs(5)), vec!["f/c/a"]);
    }

    #[test]
    fn any_tool_event_is_a_turn() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, W);
        d.on_event(
            &event("f/c/a", "PostToolUse", json!({ "tool_name": "Read" })),
            now,
        );
        assert!(!any_nudge(&mut d, now, 60));
        assert!(
            d.expire(now + Duration::from_secs(600)).is_empty(),
            "mid-turn"
        );
    }

    #[test]
    fn a_compaction_start_is_not_idle_and_a_session_end_is_unknown() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.on_event(&submit("earlier"), now);
        d.on_event(
            &event("f/c/a", "SessionStart", json!({ "source": "compact" })),
            now,
        );
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        assert!(!any_nudge(&mut d, now, 60));

        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        d.on_event(&event("f/c/a", "SessionEnd", json!({})), now);
        assert!(!any_nudge(&mut d, now, 60));
    }

    #[test]
    fn a_leading_mode_character_is_escaped() {
        assert_eq!(as_prompt("!ls"), "\\!ls");
        assert_eq!(as_prompt("# notes\nmore"), "\\# notes\nmore");
        assert_eq!(as_prompt("&x"), "\\&x");
        assert_eq!(as_prompt("  \n\t!ls"), "\\!ls", "leading blanks go");
        assert_eq!(as_prompt("\u{FEFF}\u{200B} !ls"), "\\!ls");
        assert_eq!(
            as_prompt("\u{7}!x"),
            "\\!x",
            "a control is not a first character"
        );
        assert_eq!(classify(&as_prompt("!ls")), Kind::Prompt);
    }

    #[test]
    fn other_text_and_slash_commands_are_unchanged() {
        for t in [
            "run the tests",
            "/compact",
            "a!b",
            "x\n!y",
            "",
            "@a hi",
            "\\!x",
        ] {
            assert!(matches!(as_prompt(t), Cow::Borrowed(b) if b == t), "{t:?}");
        }
        assert_eq!(as_prompt("  /exit"), "/exit");
        assert_eq!(classify(&as_prompt("  /exit")), Kind::Command);
    }

    #[test]
    fn a_gap_in_the_events_makes_an_idle_agent_unknown() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        d.lose_track();
        assert!(!any_nudge(&mut d, now, 60));
        let stopped = now + Duration::from_secs(60);
        d.on_event(&stop(), stopped);
        assert_eq!(d.nudge(stopped + Duration::from_secs(5)), vec!["f/c/a"]);
    }

    #[test]
    fn a_gap_leaves_a_busy_agent_busy() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        d.on_event(&submit("earlier"), now);
        d.sent("f/c/a", "x", 1, now, W);
        d.lose_track();
        assert!(
            d.expire(now + Duration::from_secs(600)).is_empty(),
            "mid-turn"
        );
    }

    #[test]
    fn any_event_holds_the_nudge_for_five_seconds() {
        let now = Instant::now();
        let mut d = Deliveries::default();
        start(&mut d, "f/c/a", now);
        d.sent("f/c/a", "x", 1, now, Duration::ZERO);
        let late = now + Duration::from_secs(8);
        d.on_event(&notification("idle_prompt"), late);
        assert!(d.nudge(now + Duration::from_secs(9)).is_empty());
        assert!(d.nudge(late + Duration::from_secs(4)).is_empty());
        assert_eq!(d.nudge(late + Duration::from_secs(5)), vec!["f/c/a"]);
    }
}
