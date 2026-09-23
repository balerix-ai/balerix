//! The answer flow of Spec J §7.2 and §7.3 as pure decisions (Spec K §4).
//! No I/O and no port: a plugin's actor executes a `Decision` through its
//! own channel, under the contract in `Decision`'s docs.

use balerix_api::{KeyStep, PluginAction};
use serde_json::Value;

use crate::pending::{OpenQuestion, Stage};
use crate::question::{self, Matched, Refusal, Selection};

/// The reaction on the operator's message once a decision is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    /// The reply was accepted and acted on.
    Ack,
    /// The reply could not be acted on; the post explains why.
    Refused,
    /// The echo or the keys could not be sent.
    Failed,
    /// A held selection was confirmed and sent.
    Confirmed,
}

/// What to do with one reply while a question is open (Spec J §7.2).
///
/// **Executor contract** (every plugin honours it, Spec K §4): post `post`
/// first. If it did not land and `gates_on_post()` is true, send nothing,
/// commit `Stage::Open`, react `Failed` and count `send_failed` — J-5:
/// never send what the operator cannot see, never enter `Confirming` on a
/// reading nobody was shown. Otherwise send `send` if any; on success
/// commit `stage` (with the echo id filled in by `Stage::with_echo` when
/// `gates_on_post()` is true), react `react` and count `outcome`; on
/// failure post the daemon's error, commit `Stage::Open`, react `Failed`.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// The message to post to the thread, if any.
    pub post: Option<String>,
    /// The action to send to the agent, if any.
    pub send: Option<PluginAction>,
    /// The stage to commit once `post` and `send` (if any) have landed.
    pub stage: Stage,
    /// The reaction to leave on the operator's message.
    pub react: Option<Reaction>,
    /// The `inbound_total` outcome label, when this reply counts as one.
    pub outcome: Option<&'static str>,
}

impl Decision {
    /// Whether `post` is an echo that must be seen before anything is
    /// sent or confirmed. A refusal or a notice does not gate.
    pub fn gates_on_post(&self) -> bool {
        self.post.is_some()
            && (self.send.is_some() || matches!(self.stage, Stage::Confirming { .. }))
    }
}

fn refused(stage: Stage, post: String) -> Decision {
    Decision {
        post: Some(post),
        send: None,
        stage,
        react: Some(Reaction::Refused),
        outcome: Some("answer_refused"),
    }
}

/// The keys for `selections` (`None` for a skip) as an action, or the
/// refusal the daemon would answer with, said here first (Spec J §7.5).
fn plan_action(
    questions: &[question::Question],
    selections: Option<&[Selection]>,
    key_delay_ms: u64,
) -> Result<PluginAction, String> {
    let steps: Vec<KeyStep> = match selections {
        Some(s) => question::plan(questions, s),
        None => question::skip_plan(),
    };
    let action = PluginAction::SendKeys {
        steps,
        delay_ms: key_delay_ms,
    };
    action.validate().map_err(|reason| {
        format!("this answer needs more keystrokes than can be sent from here ({reason}); answer at the terminal.")
    })?;
    Ok(action)
}

/// Spec J §7.2 for one reply while `open` is the agent's question.
pub fn on_reply(open: &OpenQuestion, reply: &str, key_delay_ms: u64) -> Decision {
    let questions = &open.questions;
    match &open.stage {
        Stage::Sent { .. } => {
            return refused(
                open.stage.clone(),
                "an answer is already on its way; wait for the agent.".into(),
            );
        }
        Stage::Confirming { selections, echo } => {
            match reply.trim().to_ascii_lowercase().as_str() {
                "yes" | "y" => {
                    return match plan_action(questions, Some(selections), key_delay_ms) {
                        Ok(action) => Decision {
                            post: None,
                            send: Some(action),
                            stage: Stage::Sent {
                                selections: Some(selections.clone()),
                                echo: echo.clone(),
                            },
                            react: Some(Reaction::Ack),
                            outcome: Some("confirmed"),
                        },
                        Err(message) => refused(Stage::Open, message),
                    };
                }
                "no" | "n" => {
                    return Decision {
                        post: None,
                        send: None,
                        stage: Stage::Open,
                        react: Some(Reaction::Ack),
                        outcome: None,
                    };
                }
                _ => {} // anything else is a fresh answer, matched below
            }
        }
        Stage::Open => {}
    }

    match question::match_reply(questions, reply) {
        Err(Refusal(reason)) => refused(Stage::Open, reason),
        Ok(Matched::Skip) => match plan_action(questions, None, key_delay_ms) {
            Ok(action) => Decision {
                post: Some("**declining the question**".into()),
                send: Some(action),
                stage: Stage::Sent {
                    selections: None,
                    echo: None,
                },
                react: Some(Reaction::Ack),
                outcome: Some("skipped"),
            },
            Err(message) => refused(Stage::Open, message),
        },
        Ok(Matched::Answers { selections, exact }) => {
            let chosen = question::describe(questions, &selections);
            if exact {
                match plan_action(questions, Some(&selections), key_delay_ms) {
                    Ok(action) => Decision {
                        post: Some(format!("**answering** {chosen}")),
                        send: Some(action),
                        stage: Stage::Sent {
                            selections: Some(selections),
                            echo: None,
                        },
                        react: Some(Reaction::Ack),
                        outcome: Some("answered"),
                    },
                    Err(message) => refused(Stage::Open, message),
                }
            } else {
                Decision {
                    post: Some(format!(
                        "**I read that as** {chosen}. Reply **yes** to send."
                    )),
                    send: None,
                    stage: Stage::Confirming {
                        selections,
                        echo: None,
                    },
                    react: None,
                    outcome: Some("confirm_asked"),
                }
            }
        }
    }
}

/// What the `PostToolUse` that closes `open` says about the answer
/// (Spec J §7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Recorded answers equal the intended ones: react `Confirmed` on the echo.
    Confirmed { echo: Option<String> },
    /// They differ: post this and count `answers_mismatched`.
    Mismatch { message: String },
    /// Nobody answered from the channel: post this when the question was shown.
    AnsweredAtTerminal { message: String },
    /// A skip was sent, or nothing to compare.
    Nothing,
}

/// Spec J §7.3 for the `PostToolUse` that closes `open`.
pub fn on_closed(open: &OpenQuestion, answers: &Value) -> Verdict {
    let recorded = question::describe_recorded(&open.questions, answers);
    match &open.stage {
        Stage::Sent {
            selections: Some(selections),
            echo,
        } => {
            if question::recorded_matches(&open.questions, selections, answers) {
                Verdict::Confirmed { echo: echo.clone() }
            } else {
                Verdict::Mismatch {
                    message: format!(
                        "**recorded answer differs** — Claude recorded {recorded}; you chose {}. Tell the agent if that matters.",
                        question::describe(&open.questions, selections)
                    ),
                }
            }
        }
        Stage::Sent {
            selections: None, ..
        } => Verdict::Nothing,
        Stage::Open | Stage::Confirming { .. } => Verdict::AnsweredAtTerminal {
            message: format!("**answered at the terminal** {recorded}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::fixtures::{color, colors_multi, input, size};
    use balerix_api::Key;
    use serde_json::json;

    fn open(questions: &[Value]) -> OpenQuestion {
        OpenQuestion {
            questions: question::parse(&input(questions)).unwrap(),
            stage: Stage::Open,
            posted: true,
            notified: false,
        }
    }

    fn keys(d: &Decision) -> Vec<KeyStep> {
        match &d.send {
            Some(PluginAction::SendKeys { steps, .. }) => steps.clone(),
            other => panic!("expected send_keys, got {other:?}"),
        }
    }

    #[test]
    fn an_exact_reply_echoes_and_plans_the_keys() {
        let d = on_reply(&open(&[color()]), "3", 100);
        assert_eq!(d.post.as_deref(), Some("**answering** Color → Blue"));
        assert_eq!(
            keys(&d),
            vec![
                KeyStep::Key(Key::Down),
                KeyStep::Key(Key::Down),
                KeyStep::Key(Key::Enter)
            ]
        );
        assert!(matches!(
            d.send,
            Some(PluginAction::SendKeys { delay_ms: 100, .. })
        ));
        assert!(matches!(
            d.stage,
            Stage::Sent {
                selections: Some(_),
                echo: None
            }
        ));
        assert_eq!(d.react, Some(Reaction::Ack));
        assert_eq!(d.outcome, Some("answered"));
        assert!(d.gates_on_post());
    }

    #[test]
    fn the_agents_key_delay_is_used() {
        let d = on_reply(&open(&[color()]), "1", 250);
        assert!(matches!(
            d.send,
            Some(PluginAction::SendKeys { delay_ms: 250, .. })
        ));
    }

    #[test]
    fn an_inexact_reply_asks_first() {
        let d = on_reply(&open(&[color()]), "gre", 100);
        assert_eq!(
            d.post.as_deref(),
            Some("**I read that as** Color → Green. Reply **yes** to send.")
        );
        assert!(d.send.is_none());
        assert!(matches!(d.stage, Stage::Confirming { echo: None, .. }));
        assert_eq!(d.react, None);
        assert_eq!(d.outcome, Some("confirm_asked"));
        assert!(d.gates_on_post());
    }

    #[test]
    fn yes_sends_the_held_selection_without_a_second_echo() {
        let mut o = open(&[color()]);
        let asked = on_reply(&o, "gre", 100);
        o.stage = asked.stage.with_echo(Some("$echo".into()));
        let d = on_reply(&o, "YES", 100);
        assert!(d.post.is_none());
        assert_eq!(
            keys(&d),
            vec![KeyStep::Key(Key::Down), KeyStep::Key(Key::Enter)]
        );
        assert!(matches!(&d.stage, Stage::Sent { echo: Some(e), .. } if e == "$echo"));
        assert_eq!(d.react, Some(Reaction::Ack));
        assert_eq!(d.outcome, Some("confirmed"));
    }

    #[test]
    fn no_drops_the_confirmation_and_another_reply_is_matched_fresh() {
        let mut o = open(&[color()]);
        o.stage = on_reply(&o, "gre", 100).stage;
        let d = on_reply(&o, "n", 100);
        assert!(d.post.is_none() && d.send.is_none());
        assert_eq!(d.stage, Stage::Open);
        assert_eq!(d.react, Some(Reaction::Ack));
        assert_eq!(d.outcome, None);
        let d = on_reply(&o, "2", 100);
        assert_eq!(d.post.as_deref(), Some("**answering** Color → Green"));
    }

    #[test]
    fn prose_is_refused_with_the_option_list() {
        let d = on_reply(&open(&[color()]), "purple please", 100);
        let post = d.post.clone().unwrap();
        assert!(post.contains("Red") && post.contains("Blue"), "{post}");
        assert!(d.send.is_none());
        assert_eq!(d.stage, Stage::Open);
        assert_eq!(d.react, Some(Reaction::Refused));
        assert_eq!(d.outcome, Some("answer_refused"));
        assert!(
            !d.gates_on_post(),
            "a refusal that fails to post still refuses"
        );
    }

    #[test]
    fn skip_declines_with_one_escape() {
        let d = on_reply(&open(&[color()]), "skip", 100);
        assert_eq!(d.post.as_deref(), Some("**declining the question**"));
        assert_eq!(keys(&d), vec![KeyStep::Key(Key::Escape)]);
        assert!(matches!(
            d.stage,
            Stage::Sent {
                selections: None,
                echo: None
            }
        ));
        assert_eq!(d.outcome, Some("skipped"));
    }

    #[test]
    fn a_reply_while_keys_are_on_their_way_is_refused_and_the_stage_kept() {
        let mut o = open(&[color()]);
        o.stage = Stage::Sent {
            selections: None,
            echo: Some("$e".into()),
        };
        let d = on_reply(&o, "2", 100);
        assert_eq!(
            d.post.as_deref(),
            Some("an answer is already on its way; wait for the agent.")
        );
        assert!(d.send.is_none());
        assert_eq!(d.stage, o.stage);
        assert_eq!(d.react, Some(Reaction::Refused));
        assert_eq!(d.outcome, Some("answer_refused"));
    }

    #[test]
    fn one_reply_answers_several_questions() {
        let d = on_reply(&open(&[colors_multi(), size()]), "1, 3\n2", 100);
        assert_eq!(
            d.post.as_deref(),
            Some("**answering** Colors → Red, Blue · Size → Medium")
        );
        assert_eq!(d.outcome, Some("answered"));
    }

    #[test]
    fn a_plan_too_long_for_the_delay_is_refused_before_anything_is_sent() {
        let options: Vec<Value> = (1..=70)
            .map(|i| json!({ "label": format!("o{i}"), "description": "" }))
            .collect();
        let q = json!({ "question": "Which?", "header": "H", "multiSelect": false, "options": options });
        let d = on_reply(&open(&[q]), "70", 500);
        assert!(
            d.post
                .unwrap()
                .starts_with("this answer needs more keystrokes than can be sent from here (")
        );
        assert!(d.send.is_none());
        assert_eq!(d.stage, Stage::Open);
        assert_eq!(d.react, Some(Reaction::Refused));
        assert_eq!(d.outcome, Some("answer_refused"));
    }

    #[test]
    fn a_sent_answer_is_confirmed_or_reported_when_it_differs() {
        let mut o = open(&[color()]);
        o.stage = on_reply(&o, "3", 100).stage.with_echo(Some("$echo".into()));
        assert_eq!(
            on_closed(&o, &json!({ "Which color?": "Blue" })),
            Verdict::Confirmed {
                echo: Some("$echo".into())
            }
        );
        assert_eq!(
            on_closed(&o, &json!({ "Which color?": "Red" })),
            Verdict::Mismatch {
                message: "**recorded answer differs** — Claude recorded Color → Red; you chose Color → Blue. Tell the agent if that matters.".into()
            }
        );
    }

    #[test]
    fn an_answer_given_at_the_terminal_is_reported_and_a_skip_says_nothing() {
        let o = open(&[color()]);
        assert_eq!(
            on_closed(&o, &json!({ "Which color?": "Green" })),
            Verdict::AnsweredAtTerminal {
                message: "**answered at the terminal** Color → Green".into()
            }
        );
        let mut skipped = open(&[color()]);
        skipped.stage = Stage::Sent {
            selections: None,
            echo: None,
        };
        assert_eq!(on_closed(&skipped, &json!({})), Verdict::Nothing);
    }
}
