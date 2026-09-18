//! The question each agent is waiting on (Spec J §7.1). `Open` is mirrored
//! to the daemon's KV (J-8): without it a plugin restart mid-question would
//! send the operator's reply down `send_text` again, which is the silent
//! wrong answer this feature exists to remove.

use std::collections::HashMap;

use balerix_plugin_sdk::{Host, SdkError};
use serde_json::Value;

use crate::question::{self, Question, Selection};

pub const QUESTION_PREFIX: &str = "question/";

pub fn question_key(agent: &str) -> String {
    format!("{QUESTION_PREFIX}{agent}")
}

/// Where an open question stands (Spec J §7.1). Event ids are the plugin's
/// own echo message, kept for the ✅ that follows `PostToolUse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Open,
    /// An inexact match was echoed; waiting for `yes`.
    Confirming {
        selections: Vec<Selection>,
        echo: Option<String>,
    },
    /// Keys were sent. `selections` is `None` for a `skip`.
    Sent {
        selections: Option<Vec<Selection>>,
        echo: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenQuestion {
    pub questions: Vec<Question>,
    pub stage: Stage,
    /// Whether the question message reached the room. Only a message the
    /// operator can see may stand in for the `permission_prompt` that
    /// follows it (Spec J §5).
    pub posted: bool,
    /// Whether that one prompt has already been swallowed.
    pub notified: bool,
}

#[derive(Debug, Default)]
pub struct Questions {
    open: HashMap<String, OpenQuestion>,
}

impl Questions {
    /// Every mirrored question resumes as `Open`. A record that no longer
    /// parses is skipped, not an error: it must not stop the plugin loading.
    pub async fn load(host: &Host) -> Result<Self, SdkError> {
        let mut questions = Self::default();
        for key in host.kv_list(QUESTION_PREFIX).await? {
            let Some(bytes) = host.kv_get(&key).await? else {
                continue;
            };
            let parsed = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| question::parse(&v));
            match parsed {
                Some(list) => {
                    questions.open.insert(
                        key[QUESTION_PREFIX.len()..].to_string(),
                        OpenQuestion {
                            questions: list,
                            stage: Stage::Open,
                            // Neither flag is persisted: after a restart
                            // nothing is suppressed, which is the safe
                            // direction — one redundant "needs you" line.
                            posted: false,
                            notified: false,
                        },
                    );
                }
                None => tracing::warn!("matrix: bad question record {key}"),
            }
        }
        Ok(questions)
    }

    pub fn get(&self, agent: &str) -> Option<&OpenQuestion> {
        self.open.get(agent)
    }

    pub fn is_open(&self, agent: &str) -> bool {
        self.open.contains_key(agent)
    }

    pub fn set_stage(&mut self, agent: &str, stage: Stage) {
        if let Some(open) = self.open.get_mut(agent) {
            open.stage = stage;
        }
    }

    /// Records that the question message landed in the room.
    pub fn mark_posted(&mut self, agent: &str) {
        if let Some(open) = self.open.get_mut(agent) {
            open.posted = true;
        }
    }

    /// Whether this `permission_prompt` is the one the question already
    /// announced (Spec J §5). At most one per question, never once an
    /// answer is on its way, and never when the question was not shown:
    /// after a `skip` no `PostToolUse` fires, so the record lives until the
    /// next `Stop`, and every real tool permission prompt in between is
    /// Spec G's only "needs you" signal.
    pub fn suppress_permission_prompt(&mut self, agent: &str) -> bool {
        let Some(open) = self.open.get_mut(agent) else {
            return false;
        };
        if !open.posted || open.notified || matches!(open.stage, Stage::Sent { .. }) {
            return false;
        }
        open.notified = true;
        true
    }

    /// Memory first, unlike `Maps`: the record in memory is what keeps a
    /// reply off `send_text`, so it must exist even when the KV write fails.
    pub async fn open(
        &mut self,
        host: &Host,
        agent: &str,
        tool_input: &Value,
        questions: Vec<Question>,
    ) {
        self.open.insert(
            agent.to_string(),
            OpenQuestion {
                questions,
                stage: Stage::Open,
                posted: false,
                notified: false,
            },
        );
        let bytes = tool_input.to_string().into_bytes();
        if let Err(e) = host.kv_put(&question_key(agent), &bytes, false).await {
            tracing::warn!("matrix: mirroring the question for {agent}: {e}");
        }
    }

    /// No KV call when nothing is open: this runs on every `Stop`.
    pub async fn clear(&mut self, host: &Host, agent: &str) -> Option<OpenQuestion> {
        let was = self.open.remove(agent)?;
        if let Err(e) = host.kv_delete(&question_key(agent)).await {
            tracing::warn!("matrix: clearing the question for {agent}: {e}");
        }
        Some(was)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::fixtures::{color, input};
    use balerix_plugin_sdk::testing::FakeHost;
    use serde_json::json;

    async fn host() -> (FakeHost, Host) {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        (fake, host)
    }

    #[tokio::test]
    async fn an_open_question_is_mirrored_to_kv_and_survives_a_reload() {
        let (fake, host) = host().await;
        let tool_input = input(&[color()]);
        let questions = question::parse(&tool_input).unwrap();
        let mut q = Questions::default();
        assert!(!q.is_open("f/c/a"));
        q.open(&host, "f/c/a", &tool_input, questions.clone()).await;
        assert!(q.is_open("f/c/a"));
        assert_eq!(fake.kv_json("question/f/c/a"), Some(tool_input));

        q.set_stage(
            "f/c/a",
            Stage::Sent {
                selections: None,
                echo: None,
            },
        );
        let reloaded = Questions::load(&host).await.unwrap();
        assert_eq!(
            reloaded.get("f/c/a"),
            Some(&OpenQuestion {
                questions,
                stage: Stage::Open,
                posted: false,
                notified: false
            }),
            "only `Open` is persisted; the operator answers again, and the \
             notification that follows is not suppressed"
        );
    }

    #[tokio::test]
    async fn clear_returns_the_question_and_removes_the_mirror() {
        let (fake, host) = host().await;
        let tool_input = input(&[color()]);
        let mut q = Questions::default();
        q.open(
            &host,
            "f/c/a",
            &tool_input,
            question::parse(&tool_input).unwrap(),
        )
        .await;
        let was = q.clear(&host, "f/c/a").await.unwrap();
        assert_eq!(was.stage, Stage::Open);
        assert!(!q.is_open("f/c/a"));
        assert!(fake.kv_json("question/f/c/a").is_none());
        assert!(
            q.clear(&host, "f/c/a").await.is_none(),
            "nothing open, nothing to do"
        );
    }

    #[tokio::test]
    async fn a_record_that_no_longer_parses_is_ignored_on_load() {
        let (_fake, host) = host().await;
        host.kv_put("question/f/c/a", br#"{"questions":[]}"#, false)
            .await
            .unwrap();
        host.kv_put("question/f/c/b", b"\xff", false).await.unwrap();
        let q = Questions::load(&host).await.unwrap();
        assert!(!q.is_open("f/c/a") && !q.is_open("f/c/b"));
    }
}
