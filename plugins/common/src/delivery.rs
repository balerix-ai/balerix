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
//! `UserPromptSubmit`, `Stop` and `SessionEnd` create an agent's state on
//! arrival if it did not already exist, so the mid-turn bit and the last
//! `Stop` are recorded even before anything was ever sent to that agent;
//! every other event name leaves the agent state untouched. `forget`
//! frees the entry.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use balerix_api::HookEvent;
use serde_json::Value;
use tokio::time::Instant;

/// Prompts pending per agent beyond which the oldest is evicted and
/// reported unconfirmed: a stuck agent must not grow this without bound.
pub const MAX_PENDING: usize = 64;

/// What a body is to Claude's input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Prose: a `UserPromptSubmit` will carry it if Claude takes it.
    Prompt,
    /// A slash command: most fire no hook, so "sent" is all a plugin can
    /// honestly say. Never tracked.
    Command,
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

#[derive(Debug)]
struct AgentState<M> {
    pending: VecDeque<Pending<M>>,
    in_turn: bool,
    last_stop: Option<Instant>,
}

impl<M> Default for AgentState<M> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            in_turn: false,
            last_stop: None,
        }
    }
}

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
    /// skips everything older. `Stop` and `SessionEnd` end the turn.
    /// These three events create the agent's state if it did not already
    /// exist (Spec M §8.7: the mid-turn bit and the last `Stop` are
    /// tracked unconditionally); every other event name is a no-op that
    /// never creates state.
    pub fn on_event(&mut self, event: &HookEvent, now: Instant) -> Outcome<M> {
        match event.name.as_str() {
            "UserPromptSubmit" => {
                let state = self.agents.entry(event.agent.clone()).or_default();
                state.in_turn = true;
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
            "Stop" | "SessionEnd" => {
                let state = self.agents.entry(event.agent.clone()).or_default();
                state.in_turn = false;
                state.last_stop = Some(now);
                Outcome::default()
            }
            _ => Outcome::default(),
        }
    }

    /// Every pending prompt whose window has passed since the later of
    /// its send and the agent's last `Stop`, on agents not mid-turn.
    /// Removed as they are answered.
    pub fn expire(&mut self, now: Instant) -> Vec<M> {
        let mut out = Vec::new();
        for state in self.agents.values_mut() {
            if state.in_turn {
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
}
