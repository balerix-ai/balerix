//! What the index shows (plugins spec §18.5): the agents enabled for web,
//! joined with the latest `fleets/watch` frame. The plugin never calls
//! `GET fleets`; an index that is right at all proves the watch path.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use balerix_api::{AgentPhase, FleetRecord, Timestamp, WorkspaceVersion};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub message: String,
}

/// Events kept per agent (Spec C §4.1, PC-9).
pub const EVENT_BUFFER: usize = 500;
/// A payload's serialized size beyond which only its head is kept.
pub const PAYLOAD_LIMIT: usize = 4096;

/// One entry of the activity column: a hook event, or the synthetic
/// `review_sent` divider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Per agent, from 1, never reused.
    pub seq: u64,
    pub at: Timestamp,
    pub name: String,
    pub summary: String,
    pub payload: Value,
    #[serde(default)]
    pub payload_truncated: bool,
}

/// The `events.json` body (Spec D §3.1 adds `workspace`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Events {
    pub phase: AgentPhase,
    pub events: Vec<Entry>,
    /// The worktree's version, `None` when the daemon refused or failed.
    #[serde(default)]
    pub workspace: Option<WorkspaceVersion>,
}

#[derive(Default)]
struct EventLog {
    next_seq: u64,
    entries: VecDeque<Entry>,
}

/// Seconds since the epoch, for the synthetic entries the plugin itself
/// appends (hook events carry the daemon's `received_at`).
pub fn now() -> Timestamp {
    Timestamp(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

fn cut(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

/// One line per event for the column (Spec C §4.3's table).
pub fn summarize(name: &str, payload: &Value) -> String {
    let text = |v: &Value| v.as_str().map(str::to_string);
    let line = match name {
        "PreToolUse" | "PostToolUse" => {
            let tool = text(&payload["tool_name"]);
            let input = &payload["tool_input"];
            match tool.as_deref() {
                Some("Bash") => text(&input["command"])
                    .map(|c| format!("Bash: {c}"))
                    .or(tool),
                Some(t @ ("Edit" | "Write" | "Read" | "MultiEdit")) => text(&input["file_path"])
                    .map(|p| format!("{t} {p}"))
                    .or(tool),
                Some(_) => tool,
                None => None,
            }
        }
        "Notification" => text(&payload["message"]),
        "UserPromptSubmit" => {
            text(&payload["prompt"]).map(|p| p.lines().next().unwrap_or("").to_string())
        }
        "Stop" => Some("turn ended".into()),
        "SubagentStop" => Some("subagent ended".into()),
        _ => None,
    };
    cut(&line.unwrap_or_else(|| name.to_string()), 200)
}

/// A payload over `PAYLOAD_LIMIT` serialized bytes becomes
/// `{ "truncated": true, "head": <first PAYLOAD_LIMIT bytes> }`.
pub fn cut_payload(payload: Value) -> (Value, bool) {
    let text = payload.to_string();
    if text.len() <= PAYLOAD_LIMIT {
        return (payload, false);
    }
    let mut end = PAYLOAD_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (
        serde_json::json!({ "truncated": true, "head": &text[..end] }),
        true,
    )
}

#[derive(Default)]
struct Inner {
    enabled: BTreeSet<String>,
    fleets: Vec<FleetRecord>,
    events: BTreeMap<String, EventLog>,
}

#[derive(Default)]
pub struct Cache {
    inner: Mutex<Inner>,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `activate`: listed when enabled, hidden otherwise. The pair stays
    /// active either way; only the listing changes.
    pub fn set_enabled(&self, agent: &str, enabled: bool) {
        let mut i = self.lock();
        if enabled {
            i.enabled.insert(agent.to_string());
        } else {
            i.enabled.remove(agent);
        }
    }

    /// `deactivate`: forget the agent. Purges the listing flag and the
    /// event buffer; any per-agent state added later is purged here and
    /// kept there.
    pub fn remove(&self, agent: &str) {
        let mut i = self.lock();
        i.enabled.remove(agent);
        i.events.remove(agent);
    }

    pub fn is_enabled(&self, agent: &str) -> bool {
        self.lock().enabled.contains(agent)
    }

    /// One `fleets/watch` frame: the whole list, replaced.
    pub fn set_fleets(&self, fleets: Vec<FleetRecord>) {
        self.lock().fleets = fleets;
    }

    pub fn rows(&self) -> Vec<AgentRow> {
        let i = self.lock();
        rows_of(&i.enabled, &i.fleets)
    }

    /// Appends one entry for an enabled agent, evicting the oldest past
    /// `EVENT_BUFFER`; `None` when the agent is not enabled (dropped).
    pub fn push_event(
        &self,
        agent: &str,
        at: Timestamp,
        name: &str,
        summary: String,
        payload: Value,
    ) -> Option<u64> {
        let mut i = self.lock();
        if !i.enabled.contains(agent) {
            return None;
        }
        let log = i.events.entry(agent.to_string()).or_default();
        log.next_seq += 1;
        let seq = log.next_seq;
        let (payload, payload_truncated) = cut_payload(payload);
        log.entries.push_back(Entry {
            seq,
            at,
            name: name.to_string(),
            summary,
            payload,
            payload_truncated,
        });
        while log.entries.len() > EVENT_BUFFER {
            log.entries.pop_front();
        }
        Some(seq)
    }

    /// The entries with `seq > after`, oldest first, and the agent's phase.
    pub fn events_after(&self, agent: &str, after: u64) -> Events {
        let i = self.lock();
        let events = i
            .events
            .get(agent)
            .map(|log| {
                log.entries
                    .iter()
                    .filter(|e| e.seq > after)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Events {
            phase: phase_in(&i.fleets, agent),
            events,
            workspace: None,
        }
    }

    pub fn phase_of(&self, agent: &str) -> AgentPhase {
        phase_in(&self.lock().fleets, agent)
    }
}

fn phase_in(fleets: &[FleetRecord], id: &str) -> AgentPhase {
    fleets
        .iter()
        .find_map(|f| f.status.agents.get(id))
        .map_or(AgentPhase::Pending, |s| s.phase)
}

/// The enabled agents in id order with their phase from the fleets;
/// an enabled agent no fleet knows yet is `pending` with no message.
/// Agent ids are fleet-prefixed (`<fleet>/<crew>/<agent>`), so at most
/// one fleet knows an id; the first match is the only one.
pub fn rows_of(enabled: &BTreeSet<String>, fleets: &[FleetRecord]) -> Vec<AgentRow> {
    enabled
        .iter()
        .map(|id| {
            let status = fleets.iter().find_map(|f| f.status.agents.get(id));
            AgentRow {
                id: id.clone(),
                phase: status.map_or(AgentPhase::Pending, |s| s.phase),
                message: status.map(|s| s.message.clone()).unwrap_or_default(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::FleetSpec;
    use std::collections::BTreeMap;

    fn fleet(name: &str, agents: &[(&str, AgentPhase, &str)]) -> FleetRecord {
        let mut r = FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
            ..Default::default()
        });
        for (id, phase, message) in agents {
            let s = r.status.entry(id);
            s.phase = *phase;
            s.message = (*message).to_string();
        }
        r
    }

    #[test]
    fn rows_are_the_enabled_agents_with_phases_from_the_fleets() {
        let c = Cache::new();
        c.set_enabled("f/c/b", true);
        c.set_enabled("f/c/a", true);
        c.set_enabled("f/c/z", false);
        assert!(c.is_enabled("f/c/a") && !c.is_enabled("f/c/z"));
        assert_eq!(
            c.rows(),
            vec![
                AgentRow {
                    id: "f/c/a".into(),
                    phase: AgentPhase::Pending,
                    message: String::new()
                },
                AgentRow {
                    id: "f/c/b".into(),
                    phase: AgentPhase::Pending,
                    message: String::new()
                },
            ],
            "no fleets yet: pending"
        );
        c.set_fleets(vec![
            fleet(
                "f",
                &[
                    ("f/c/a", AgentPhase::Ready, ""),
                    ("f/c/c", AgentPhase::Ready, ""),
                ],
            ),
            fleet("g", &[("f/c/b", AgentPhase::Dead, "exit 1")]),
        ]);
        let rows = c.rows();
        assert_eq!(rows[0].phase, AgentPhase::Ready);
        assert_eq!(
            (rows[1].phase, rows[1].message.as_str()),
            (AgentPhase::Dead, "exit 1")
        );
        assert_eq!(rows.len(), 2, "c is not enabled");
        c.remove("f/c/a");
        c.set_enabled("f/c/b", false);
        assert!(c.rows().is_empty());
        assert_eq!(
            serde_json::to_value(AgentRow {
                id: "x".into(),
                phase: AgentPhase::Ready,
                message: "m".into()
            })
            .unwrap(),
            serde_json::json!({ "id": "x", "phase": "ready", "message": "m" })
        );
    }

    #[test]
    fn events_are_buffered_per_enabled_agent_with_a_running_seq() {
        use serde_json::json;
        let c = Cache::new();
        assert_eq!(
            c.push_event(
                "f/c/a",
                Timestamp(1),
                "Stop",
                "turn ended".into(),
                json!({})
            ),
            None,
            "not enabled: dropped"
        );
        c.set_enabled("f/c/a", true);
        for i in 1..=(EVENT_BUFFER as u64 + 3) {
            let seq = c
                .push_event(
                    "f/c/a",
                    Timestamp(i),
                    "PreToolUse",
                    format!("Bash: cmd{i}"),
                    json!({ "i": i }),
                )
                .unwrap();
            assert_eq!(seq, i);
        }
        let all = c.events_after("f/c/a", 0);
        assert_eq!(all.phase, AgentPhase::Pending);
        assert_eq!(
            all.events.len(),
            EVENT_BUFFER,
            "the oldest three were evicted"
        );
        assert_eq!(all.events[0].seq, 4);
        assert_eq!(all.events.last().unwrap().seq, EVENT_BUFFER as u64 + 3);
        let tail = c.events_after("f/c/a", EVENT_BUFFER as u64 + 1);
        assert_eq!(
            tail.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![EVENT_BUFFER as u64 + 2, EVENT_BUFFER as u64 + 3]
        );
        assert!(c.events_after("f/c/a", u64::MAX).events.is_empty());
        assert!(
            c.events_after("f/c/zzz", 0).events.is_empty(),
            "unknown agent: empty, not a panic"
        );
        let e = &tail.events[0];
        assert_eq!(
            serde_json::to_value(e).unwrap(),
            json!({ "seq": e.seq, "at": e.at.0, "name": "PreToolUse", "summary": format!("Bash: cmd{}", e.seq),
                    "payload": { "i": e.seq }, "payload_truncated": false })
        );
        c.remove("f/c/a");
        assert!(
            c.events_after("f/c/a", 0).events.is_empty(),
            "deactivate drops the buffer"
        );
    }

    #[test]
    fn summaries_follow_the_hook_name_and_payloads_are_cut() {
        use serde_json::json;
        assert_eq!(
            summarize(
                "PreToolUse",
                &json!({ "tool_name": "Bash", "tool_input": { "command": "cargo test" } })
            ),
            "Bash: cargo test"
        );
        assert_eq!(
            summarize(
                "PostToolUse",
                &json!({ "tool_name": "Edit", "tool_input": { "file_path": "src/lib.rs" } })
            ),
            "Edit src/lib.rs"
        );
        assert_eq!(
            summarize("PreToolUse", &json!({ "tool_name": "WebFetch" })),
            "WebFetch"
        );
        assert_eq!(summarize("PreToolUse", &json!({})), "PreToolUse");
        assert_eq!(
            summarize("Notification", &json!({ "message": "needs input" })),
            "needs input"
        );
        assert_eq!(
            summarize(
                "UserPromptSubmit",
                &json!({ "prompt": "first line\nsecond" })
            ),
            "first line"
        );
        assert_eq!(summarize("Stop", &json!({})), "turn ended");
        assert_eq!(summarize("SubagentStop", &json!({})), "subagent ended");
        assert_eq!(
            summarize("SessionStart", &json!({ "source": "startup" })),
            "SessionStart"
        );
        assert_eq!(summarize("Whatever", &json!(null)), "Whatever");
        let long = "x".repeat(300);
        let s = summarize("Notification", &json!({ "message": long }));
        assert_eq!(s.chars().count(), 200);
        let (v, cut) = cut_payload(json!({ "small": 1 }));
        assert_eq!((v, cut), (json!({ "small": 1 }), false));
        let big = json!({ "blob": "y".repeat(PAYLOAD_LIMIT) });
        let (v, cut) = cut_payload(big);
        assert!(cut);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["head"].as_str().unwrap().len(), PAYLOAD_LIMIT);
        assert!(now().0 > 1_700_000_000);
    }

    mod props {
        use super::super::*;
        use proptest::prelude::*;

        proptest! {
            /// `events_after(after)` is exactly the kept entries with
            /// `seq > after`, in order, for any number of pushes.
            #[test]
            fn events_after_returns_exactly_the_entries_above_after(
                n in 0usize..(EVENT_BUFFER + 50),
                after in 0u64..600,
            ) {
                let c = Cache::new();
                c.set_enabled("f/c/a", true);
                for i in 0..n {
                    c.push_event("f/c/a", Timestamp(i as u64), "Stop", "turn ended".into(), Value::Null);
                }
                let got: Vec<u64> = c.events_after("f/c/a", after).events.iter().map(|e| e.seq).collect();
                let first_kept = (n as u64).saturating_sub(EVENT_BUFFER as u64) + 1;
                let want: Vec<u64> = (1..=n as u64).filter(|s| *s > after && *s >= first_kept).collect();
                prop_assert_eq!(got, want);
            }
        }
    }
}
