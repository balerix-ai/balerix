//! `AskUserQuestion` (Spec J §6): parse the dialog, match a thread reply to
//! its options, plan the keystrokes. Pure: no I/O and no clock, so every
//! rule is testable, and the rules are the ones Spec J §2 measured.

use std::collections::BTreeSet;

use balerix_api::{Key, KeyStep, MAX_KEY_TEXT};
use serde_json::Value;

/// The tool whose `PreToolUse` opens a question.
pub const TOOL: &str = "AskUserQuestion";

/// One option of a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opt {
    /// What the operator picks it by: a number, the whole label or a
    /// unique prefix or word of it (Spec J §6.2).
    pub label: String,
    /// Shown beside the label; may be empty.
    pub description: String,
}

/// One question of an `AskUserQuestion` dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Verbatim: it is the key of `PostToolUse`'s `answers` map.
    pub text: String,
    /// Shown instead of `text` when it is not empty (`name`).
    pub header: String,
    /// Whether more than one option may be chosen.
    pub multi_select: bool,
    /// In display order; never empty.
    pub options: Vec<Opt>,
}

impl Question {
    /// What the thread calls this question: its header, else its text.
    pub fn name(&self) -> &str {
        if self.header.is_empty() {
            &self.text
        } else {
            &self.header
        }
    }
}

/// One question's answer. `options` is 0-based, ascending and unique. A
/// single-select holds exactly one option or `other`; a multi-select holds
/// at least one of either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Indices into the question's `options`.
    pub options: Vec<usize>,
    /// Free text from `other: …`, if any.
    pub other: Option<String>,
}

/// The outcome of matching a thread reply against the open questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matched {
    /// `exact` is true only when every item was a number or a whole label.
    Answers {
        /// One per question, in question order.
        selections: Vec<Selection>,
        exact: bool,
    },
    /// The reply declined the dialog.
    Skip,
}

/// Why a reply was not matched, as markdown for the thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

/// `tool_input` as questions; `None` for any other shape (Spec J §6.1).
pub fn parse(tool_input: &Value) -> Option<Vec<Question>> {
    let questions: Vec<Question> = tool_input
        .get("questions")?
        .as_array()?
        .iter()
        .map(|q| {
            let options: Vec<Opt> = q
                .get("options")?
                .as_array()?
                .iter()
                .map(|o| {
                    let label = o.get("label")?.as_str()?.trim().to_string();
                    (!label.is_empty()).then(|| Opt {
                        label,
                        description: text_of(o, "description"),
                    })
                })
                .collect::<Option<_>>()?;
            if options.is_empty() {
                return None;
            }
            Some(Question {
                text: q.get("question")?.as_str()?.to_string(),
                header: text_of(q, "header"),
                multi_select: q
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                options,
            })
        })
        .collect::<Option<_>>()?;
    (!questions.is_empty()).then_some(questions)
}

fn text_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Case-folded, punctuation to spaces, whitespace collapsed.
fn normalise(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn options_list(q: &Question) -> String {
    q.options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("{}. {}", i + 1, o.label))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A thread reply against the open questions (Spec J §6.2).
pub fn match_reply(questions: &[Question], reply: &str) -> Result<Matched, Refusal> {
    let reply = reply.trim();
    // `skip` declines — unless a lone question offers an option labelled
    // `skip`, which this rule would otherwise make unreachable (J-4). Then
    // it selects that option, but never exactly: the echo asks for a `yes`
    // first, so the other reading is one `no` away. A several-question
    // reply of one word could not be a positional answer anyway.
    let labelled_skip = reply.eq_ignore_ascii_case("skip")
        && questions.len() == 1
        && questions[0]
            .options
            .iter()
            .any(|o| normalise(&o.label) == "skip");
    if reply.eq_ignore_ascii_case("skip") && !labelled_skip {
        return Ok(Matched::Skip);
    }
    let parts = split(questions, reply)?;
    let mut exact = !labelled_skip;
    let mut selections = Vec::with_capacity(questions.len());
    for (q, part) in questions.iter().zip(parts) {
        let (selection, e) = match_answer(q, part)?;
        exact &= e;
        selections.push(selection);
    }
    Ok(Matched::Answers { selections, exact })
}

/// One part of the reply per question, in question order.
fn split<'a>(questions: &[Question], reply: &'a str) -> Result<Vec<&'a str>, Refusal> {
    if questions.len() == 1 {
        return Ok(vec![reply]);
    }
    let lines: Vec<&str> = reply
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // `header: answer` lines in any order — only when every line is one and
    // they cover more than a single question.
    let by_header: Option<Vec<(usize, &str)>> = lines
        .iter()
        .map(|line| {
            let (head, rest) = line.split_once(':')?;
            let head = normalise(head);
            let i = questions
                .iter()
                .position(|q| !q.header.is_empty() && normalise(&q.header) == head)?;
            Some((i, rest.trim()))
        })
        .collect();
    if let Some(pairs) = by_header.filter(|p| p.len() > 1) {
        let mut parts: Vec<Option<&str>> = vec![None; questions.len()];
        for (i, rest) in pairs {
            if parts[i].replace(rest).is_some() {
                return Err(Refusal(format!(
                    "**{}** is answered twice.",
                    questions[i].name()
                )));
            }
        }
        return parts
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                p.ok_or_else(|| Refusal(format!("no answer for **{}**.", questions[i].name())))
            })
            .collect();
    }
    if lines.len() != questions.len() {
        return Err(Refusal(format!(
            "{n} questions need {n} lines, one answer per line in order; got {}.",
            lines.len(),
            n = questions.len()
        )));
    }
    Ok(lines)
}

/// `other:` at the start of the answer or right after a comma takes the
/// rest of the answer, commas included.
fn split_other(part: &str) -> (&str, Option<&str>) {
    const MARK: &str = "other:";
    let lower = part.to_ascii_lowercase(); // same byte offsets as `part`
    let mut from = 0;
    while let Some(found) = lower[from..].find(MARK) {
        let at = from + found;
        let before = part[..at].trim_end();
        if before.is_empty() || before.ends_with(',') {
            return (
                before.trim_end_matches(',').trim_end(),
                Some(part[at + MARK.len()..].trim()),
            );
        }
        from = at + MARK.len();
    }
    (part, None)
}

fn match_answer(q: &Question, part: &str) -> Result<(Selection, bool), Refusal> {
    let (listed, other) = split_other(part);
    let other = match other {
        Some(text) => Some(check_other(q, text)?),
        None => None,
    };
    let items: Vec<&str> = if q.multi_select {
        listed
            .split(',')
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .collect()
    } else if listed.trim().is_empty() {
        Vec::new()
    } else {
        vec![listed.trim()]
    };
    // free text is never exact: a typo must not become an answer unasked
    let mut exact = other.is_none();
    let mut options = BTreeSet::new();
    for item in items {
        let (i, e) = match_item(q, item)?;
        exact &= e;
        if !options.insert(i) {
            return Err(Refusal(format!(
                "**{}** names {} twice.",
                q.name(),
                q.options[i].label
            )));
        }
    }
    let options: Vec<usize> = options.into_iter().collect();
    let chosen = options.len() + usize::from(other.is_some());
    if chosen == 0 {
        return Err(Refusal(format!(
            "no answer given for **{}**. Options: {}",
            q.name(),
            options_list(q)
        )));
    }
    if !q.multi_select && chosen > 1 {
        return Err(Refusal(format!(
            "**{}** takes one answer: an option or `other:`, not both.",
            q.name()
        )));
    }
    Ok((Selection { options, other }, exact))
}

fn check_other(q: &Question, text: &str) -> Result<String, Refusal> {
    if text.is_empty() {
        return Err(Refusal(format!(
            "`other:` needs some text for **{}**.",
            q.name()
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(Refusal(format!(
            "`other:` for **{}** must be one line.",
            q.name()
        )));
    }
    if text.len() > MAX_KEY_TEXT {
        return Err(Refusal(format!(
            "`other:` for **{}** is limited to {MAX_KEY_TEXT} bytes.",
            q.name()
        )));
    }
    Ok(text.to_string())
}

/// The ladder: number, whole label, unique prefix, unique word. The `bool`
/// is whether the rung was exact.
fn match_item(q: &Question, item: &str) -> Result<(usize, bool), Refusal> {
    let digits = item.trim().trim_end_matches(['.', ')', ':']);
    let numeric = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
    if numeric
        && let Ok(n) = digits.parse::<usize>()
        && (1..=q.options.len()).contains(&n)
    {
        // The number counts rows. When some *other* option is labelled
        // with it the reply has two honest readings, so it is inexact and
        // the echo asks; the operator can always name the other number
        // (J-4). A number that counts to no row falls through to the
        // labels below, so options labelled `2 / 4 / 8` stay reachable.
        let shadowed = q
            .options
            .iter()
            .enumerate()
            .any(|(i, o)| i != n - 1 && normalise(&o.label) == digits);
        return Ok((n - 1, !shadowed));
    }
    let want = normalise(item);
    if want.is_empty() {
        return Err(Refusal(format!(
            "no answer given for **{}**. Options: {}",
            q.name(),
            options_list(q)
        )));
    }
    let labels: Vec<String> = q.options.iter().map(|o| normalise(&o.label)).collect();
    let candidates = |hits: &[usize]| {
        Refusal(format!(
            "\"{}\" could be {} in **{}**. Reply with the number.",
            item.trim(),
            hits.iter()
                .map(|i| format!("{}. {}", i + 1, q.options[*i].label))
                .collect::<Vec<_>>()
                .join(" or "),
            q.name()
        ))
    };
    // Labels that normalise alike (`C++` and `C#`, `A+` and `A-`) make the
    // whole-label rung a coin toss, and the rungs below cannot tell them
    // apart either — identical normalised labels hit or miss together. The
    // raw text is what is left to separate them (J-4).
    let equal: Vec<usize> = (0..labels.len()).filter(|&i| labels[i] == want).collect();
    match equal.as_slice() {
        [] => {}
        [i] => return Ok((*i, true)),
        many => {
            let raw: Vec<usize> = many
                .iter()
                .copied()
                .filter(|&i| q.options[i].label.trim().to_lowercase() == item.trim().to_lowercase())
                .collect();
            return match raw.as_slice() {
                [i] => Ok((*i, true)),
                _ => Err(candidates(many)),
            };
        }
    }
    let unique = |hits: Vec<usize>| match hits.as_slice() {
        [] => None,
        [i] => Some(Ok((*i, false))),
        many => Some(Err(candidates(many))),
    };
    let by_prefix = (0..labels.len())
        .filter(|&i| labels[i].starts_with(&want))
        .collect();
    if let Some(result) = unique(by_prefix) {
        return result;
    }
    let words: Vec<&str> = want.split(' ').collect();
    let by_words = (0..labels.len())
        .filter(|&i| words.iter().all(|w| labels[i].split(' ').any(|l| l == *w)))
        .collect();
    if let Some(result) = unique(by_words) {
        return result;
    }
    // A number that counted to no row and spells no label is a miscount,
    // and saying so is more use than "matches nothing".
    Err(Refusal(if numeric {
        format!(
            "**{}** has no option {digits}. Options: {}",
            q.name(),
            options_list(q)
        )
    } else {
        format!(
            "\"{}\" matches nothing in **{}**. Options: {}",
            item.trim(),
            q.name(),
            options_list(q)
        )
    }))
}

/// The keystrokes that answer the dialog (Spec J §6.3). Rows of a question:
/// the options, then `Type something`, then — multi-select only — `Submit`
/// or `Next`. The cursor starts on the first row of every question. Only
/// Down, Enter and text: Tab behaves differently from different rows.
pub fn plan(questions: &[Question], selections: &[Selection]) -> Vec<KeyStep> {
    fn down(steps: &mut Vec<KeyStep>, count: usize) {
        steps.extend(std::iter::repeat_n(KeyStep::Key(Key::Down), count));
    }
    let enter = KeyStep::Key(Key::Enter);
    let mut steps = Vec::new();
    for (q, s) in questions.iter().zip(selections) {
        let n = q.options.len();
        if q.multi_select {
            let mut row = 0;
            for &option in &s.options {
                down(&mut steps, option - row);
                row = option;
                steps.push(enter.clone()); // toggles
            }
            if let Some(text) = &s.other {
                down(&mut steps, n - row);
                row = n;
                steps.push(KeyStep::Text(text.clone())); // typing ticks the row
            }
            down(&mut steps, n + 1 - row);
            steps.push(enter.clone()); // `Submit` or `Next`
        } else if let Some(text) = &s.other {
            down(&mut steps, n);
            steps.push(KeyStep::Text(text.clone()));
            steps.push(enter.clone());
        } else {
            down(&mut steps, s.options[0]);
            steps.push(enter.clone());
        }
    }
    // Everything but a lone single-select question ends on the review
    // screen, with `Submit answers` highlighted.
    let lone_single = questions.len() == 1 && !questions[0].multi_select;
    if !lone_single {
        steps.push(enter);
    }
    steps
}

/// Declining the dialog: Claude sees "User declined to answer questions".
pub fn skip_plan() -> Vec<KeyStep> {
    vec![KeyStep::Key(Key::Escape)]
}

fn answer_text(q: &Question, s: &Selection) -> String {
    s.options
        .iter()
        .map(|&i| q.options[i].label.clone())
        .chain(s.other.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `Color → Blue · Size → Medium`, for the echo.
pub fn describe(questions: &[Question], selections: &[Selection]) -> String {
    questions
        .iter()
        .zip(selections)
        .map(|(q, s)| format!("{} → {}", q.name(), answer_text(q, s)))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The same line from `PostToolUse`'s `tool_response.answers`.
pub fn describe_recorded(questions: &[Question], answers: &Value) -> String {
    questions
        .iter()
        .map(|q| {
            let got = answers
                .get(&q.text)
                .and_then(Value::as_str)
                .unwrap_or("(nothing)");
            format!("{} → {got}", q.name())
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Whether Claude recorded what was intended (Spec J §7.3). A multi-select
/// answer is its labels joined with `", "`, in Claude's order, so it is
/// compared as a set.
pub fn recorded_matches(questions: &[Question], selections: &[Selection], answers: &Value) -> bool {
    questions.iter().zip(selections).all(|(q, s)| {
        let Some(got) = answers.get(&q.text).and_then(Value::as_str) else {
            return false;
        };
        let want = answer_text(q, s);
        // Labels were trimmed at parse and Claude's text is verbatim, so
        // both sides are trimmed before they are compared — items of a
        // multi-select set too. A mismatch over a stray space would be a
        // false alarm on the counter that is meant to stay at zero (§8).
        let set = |t: &str| {
            t.split(", ")
                .map(|i| i.trim().to_string())
                .collect::<BTreeSet<_>>()
        };
        got.trim() == want || (q.multi_select && set(got) == set(&want))
    })
}

/// Dialogs the tests share. Always compiled, like `FakePort` and the
/// SDK's `testing`: downstream crates build their tests on them.
pub mod fixtures {
    use serde_json::{Value, json};

    /// A single-select question, `tool_input` shape.
    pub fn color() -> Value {
        json!({ "question": "Which color?", "header": "Color", "multiSelect": false,
                "options": [
                    { "label": "Red", "description": "A warm, bold color" },
                    { "label": "Green", "description": "A calm, natural color" },
                    { "label": "Blue", "description": "A cool, serene color" } ] })
    }

    /// A second single-select question, for a several-questions dialog.
    pub fn size() -> Value {
        json!({ "question": "Which size?", "header": "Size", "multiSelect": false,
                "options": [
                    { "label": "Small", "description": "" },
                    { "label": "Medium", "description": "" },
                    { "label": "Large", "description": "" } ] })
    }

    /// `color()` with `multiSelect` on.
    pub fn colors_multi() -> Value {
        let mut q = color();
        q["question"] = json!("Which colors?");
        q["header"] = json!("Colors");
        q["multiSelect"] = json!(true);
        q
    }

    /// `questions` wrapped as an `AskUserQuestion` `tool_input`.
    pub fn input(questions: &[Value]) -> Value {
        json!({ "questions": questions })
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::{
        Matched, Opt, Question, Refusal, Selection, describe, describe_recorded, match_reply,
        parse, plan, recorded_matches, skip_plan,
    };
    use balerix_api::{Key, KeyStep};
    use serde_json::json;

    fn parsed(questions: &[serde_json::Value]) -> Vec<Question> {
        parse(&input(questions)).unwrap()
    }

    fn answers(questions: &[Question], reply: &str) -> (Vec<Selection>, bool) {
        match match_reply(questions, reply) {
            Ok(Matched::Answers { selections, exact }) => (selections, exact),
            other => panic!("{reply:?}: {other:?}"),
        }
    }

    fn option(i: usize) -> Selection {
        Selection {
            options: vec![i],
            other: None,
        }
    }

    fn refusal(questions: &[Question], reply: &str) -> String {
        match match_reply(questions, reply) {
            Err(Refusal(reason)) => reason,
            other => panic!("{reply:?} was not refused: {other:?}"),
        }
    }

    #[test]
    fn parse_reads_questions_and_refuses_anything_else() {
        let q = parsed(&[color()]);
        assert_eq!(q.len(), 1);
        assert_eq!(
            (q[0].text.as_str(), q[0].header.as_str()),
            ("Which color?", "Color")
        );
        assert!(!q[0].multi_select);
        assert_eq!(q[0].options[2].label, "Blue");
        assert_eq!(q[0].options[0].description, "A warm, bold color");
        assert_eq!(q[0].name(), "Color");
        assert!(parsed(&[colors_multi()])[0].multi_select);

        let mut headless = color();
        headless["header"] = json!("");
        assert_eq!(parsed(&[headless])[0].name(), "Which color?");

        for bad in [
            json!({}),
            json!({ "questions": [] }),
            json!({ "questions": [{ "question": "Q?", "options": [] }] }),
            json!({ "questions": [{ "question": "Q?", "options": [{ "label": " " }] }] }),
            json!({ "questions": [{ "options": [{ "label": "A" }] }] }),
            json!({ "questions": "no" }),
        ] {
            assert_eq!(parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_ladder_number_label_prefix_word() {
        let q = parsed(&[color()]);
        assert_eq!(answers(&q, "2"), (vec![option(1)], true));
        assert_eq!(answers(&q, " 3. "), (vec![option(2)], true));
        assert_eq!(answers(&q, "BLUE!"), (vec![option(2)], true));
        assert_eq!(
            answers(&q, "gre"),
            (vec![option(1)], false),
            "a unique prefix is inexact"
        );

        let mut long = color();
        long["options"][0]["label"] = json!("Red (Recommended)");
        long["options"][1]["label"] = json!("Dark green");
        let q = parsed(&[long]);
        assert_eq!(
            answers(&q, "green"),
            (vec![option(1)], false),
            "a unique word"
        );
        assert_eq!(
            answers(&q, "red"),
            (vec![option(0)], false),
            "a prefix of a longer label"
        );
        assert_eq!(answers(&q, "Red (recommended)"), (vec![option(0)], true));
    }

    #[test]
    fn ambiguity_and_no_match_are_refused_with_what_to_do() {
        let mut close = color();
        close["options"][0]["label"] = json!("Green tea");
        close["options"][1]["label"] = json!("Green apple");
        let q = parsed(&[close]);
        let r = refusal(&q, "green");
        assert!(
            r.contains("1. Green tea") && r.contains("2. Green apple"),
            "{r}"
        );

        let q = parsed(&[color()]);
        let r = refusal(&q, "purple please");
        assert!(
            r.contains("matches nothing") && r.contains("1. Red, 2. Green, 3. Blue"),
            "{r}"
        );
        assert!(refusal(&q, "7").contains("no option 7"));
        assert!(refusal(&q, "0").contains("no option 0"));
        assert!(refusal(&q, "   ").contains("no answer"));
    }

    #[test]
    fn other_is_free_text_and_never_exact() {
        let q = parsed(&[color()]);
        assert_eq!(
            answers(&q, "Other: teal-ish, really"),
            (
                vec![Selection {
                    options: vec![],
                    other: Some("teal-ish, really".into())
                }],
                false
            )
        );
        assert!(refusal(&q, "other:").contains("needs some text"));
        assert!(refusal(&q, "red, other: pink").contains("one answer"));
        assert!(refusal(&q, "other: two\nlines").contains("one line"));
        assert!(refusal(&q, &format!("other: {}", "x".repeat(1025))).contains("1024"));
    }

    #[test]
    fn multi_select_takes_a_comma_list_in_any_order() {
        let q = parsed(&[colors_multi()]);
        assert_eq!(
            answers(&q, "blue, 1"),
            (
                vec![Selection {
                    options: vec![0, 2],
                    other: None
                }],
                true
            )
        );
        assert_eq!(
            answers(&q, "2, other: a bit of gold"),
            (
                vec![Selection {
                    options: vec![1],
                    other: Some("a bit of gold".into())
                }],
                false
            )
        );
        assert!(refusal(&q, "red, 1").contains("twice"));
    }

    #[test]
    fn several_questions_take_lines_in_order_or_header_lines_in_any_order() {
        let q = parsed(&[color(), size()]);
        assert_eq!(
            answers(&q, "blue\n\n2\n"),
            (vec![option(2), option(1)], true)
        );
        assert_eq!(
            answers(&q, "size: large\nCOLOR: 1"),
            (vec![option(0), option(2)], true)
        );
        assert!(refusal(&q, "blue").contains("2 questions need 2 lines"));
        assert!(refusal(&q, "color: red\ncolor: blue").contains("twice"));
        assert!(refusal(&q, "color: red\nsize: 2\nsize: 3").contains("twice"));
        // a header line for one question only falls back to positional, which
        // then refuses on the count
        assert!(refusal(&q, "color: red").contains("2 questions need 2 lines"));
    }

    #[test]
    fn skip_declines() {
        let q = parsed(&[color(), size()]);
        assert_eq!(match_reply(&q, " Skip "), Ok(Matched::Skip));
    }

    /// J-4: a wrong guess answers a question on the operator's behalf, and
    /// labels come from the agent, so they can collide with the very rules
    /// that read them. `C++` and `C#` normalise alike, so the whole-label
    /// rung has to fall back to the raw text before it calls one of them
    /// exact.
    #[test]
    fn labels_that_normalise_alike_are_told_apart_by_their_raw_text() {
        let mut langs = color();
        langs["question"] = json!("Which language?");
        langs["header"] = json!("Language");
        langs["options"] = json!([
            { "label": "C++", "description": "" },
            { "label": "C#", "description": "" },
            { "label": "Rust", "description": "" }
        ]);
        let q = parsed(&[langs.clone()]);
        assert_eq!(answers(&q, "C++"), (vec![option(0)], true));
        assert_eq!(answers(&q, " c# "), (vec![option(1)], true));
        assert_eq!(answers(&q, "rust"), (vec![option(2)], true));
        let r = refusal(&q, "c");
        assert!(
            r.contains("1. C++") && r.contains("2. C#") && !r.contains("Rust"),
            "{r}"
        );

        // the same through a multi-select answer's comma list
        langs["multiSelect"] = json!(true);
        let q = parsed(&[langs]);
        assert_eq!(
            answers(&q, "c#, rust"),
            (
                vec![Selection {
                    options: vec![1, 2],
                    other: None
                }],
                true
            )
        );
        assert!(refusal(&q, "rust, c").contains("could be"));
    }

    /// An option labelled `skip` would otherwise be unreachable, `skip`
    /// being tested before any label is looked at. In a lone question the
    /// label wins — inexactly, so the echo asks and the other reading is
    /// one `no` away. Everywhere else `skip` keeps meaning decline.
    #[test]
    fn skip_as_a_label_selects_it_only_in_a_lone_question() {
        let mut retry = color();
        retry["question"] = json!("What now?");
        retry["header"] = json!("Next");
        retry["options"] = json!([
            { "label": "Skip", "description": "" },
            { "label": "Retry", "description": "" }
        ]);
        let q = parsed(&[retry.clone()]);
        assert_eq!(answers(&q, "skip"), (vec![option(0)], false));
        assert_eq!(answers(&q, "1"), (vec![option(0)], true));

        assert_eq!(match_reply(&parsed(&[color()]), "skip"), Ok(Matched::Skip));
        assert_eq!(
            match_reply(&parsed(&[retry, size()]), " SKIP "),
            Ok(Matched::Skip),
            "one word cannot be a positional answer to two questions"
        );
    }

    /// Options labelled with numbers: `2` counts to a row and also names a
    /// label, so it is confirmed rather than guessed; `4` and `8` count to
    /// no row at all and must still reach the labels they spell.
    #[test]
    fn a_number_counts_rows_and_a_numeric_label_is_still_reachable() {
        let mut counts = color();
        counts["question"] = json!("How many?");
        counts["header"] = json!("Count");
        counts["options"] = json!([
            { "label": "2", "description": "" },
            { "label": "4", "description": "" },
            { "label": "8", "description": "" }
        ]);
        let q = parsed(&[counts]);
        assert_eq!(
            answers(&q, "2"),
            (vec![option(1)], false),
            "row 2 is `4`, but another option is labelled `2`: ask first"
        );
        assert_eq!(
            answers(&q, "4"),
            (vec![option(1)], true),
            "there is no row 4, so the label matches"
        );
        assert_eq!(answers(&q, "8"), (vec![option(2)], true));
        assert_eq!(
            answers(&q, "1"),
            (vec![option(0)], true),
            "no option is labelled 1, so the row is unambiguous"
        );
        assert!(refusal(&q, "9").contains("no option 9"));
    }

    /// `D` Down, `E` Enter, `T` the text step `t`.
    fn keys(pattern: &str, t: &str) -> Vec<KeyStep> {
        pattern
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| match c {
                'D' => KeyStep::Key(Key::Down),
                'E' => KeyStep::Key(Key::Enter),
                'T' => KeyStep::Text(t.to_string()),
                other => panic!("{other}"),
            })
            .collect()
    }

    fn sel(options: &[usize], other: Option<&str>) -> Selection {
        Selection {
            options: options.to_vec(),
            other: other.map(str::to_string),
        }
    }

    /// Each case is a row of Spec J §2's table: the sequence that was
    /// measured against the real `claude`.
    #[test]
    fn the_plan_is_the_measured_sequence() {
        let one = parsed(&[color()]);
        assert_eq!(plan(&one, &[sel(&[1], None)]), keys("DE", ""));
        assert_eq!(plan(&one, &[sel(&[0], None)]), keys("E", ""));
        assert_eq!(
            plan(&one, &[sel(&[], Some("teal"))]),
            keys("DDD T E", "teal")
        );

        let two = parsed(&[color(), size()]);
        assert_eq!(
            plan(&two, &[sel(&[2], None), sel(&[1], None)]),
            keys("DDE DE E", "")
        );
        assert_eq!(
            plan(&two, &[sel(&[], Some("teal")), sel(&[2], None)]),
            keys("DDD T E  DDE  E", "teal")
        );

        let multi = parsed(&[colors_multi()]);
        assert_eq!(plan(&multi, &[sel(&[0, 2], None)]), keys("E DDE DDE E", ""));
        assert_eq!(
            plan(&multi, &[sel(&[1], Some("gold"))]),
            keys("DE DD T DE E", "gold")
        );
        assert_eq!(
            plan(&multi, &[sel(&[], Some("gold"))]),
            keys("DDD T DE E", "gold")
        );

        let mixed = parsed(&[colors_multi(), size()]);
        assert_eq!(
            plan(&mixed, &[sel(&[0, 2], None), sel(&[1], None)]),
            keys("E DDE DDE  DE  E", "")
        );
        assert_eq!(skip_plan(), vec![KeyStep::Key(Key::Escape)]);
    }

    #[test]
    fn describe_and_the_recorded_answers() {
        let q = parsed(&[colors_multi(), size()]);
        let chosen = [sel(&[0, 2], Some("gold")), sel(&[1], None)];
        assert_eq!(
            describe(&q, &chosen),
            "Colors → Red, Blue, gold · Size → Medium"
        );

        let same = json!({ "Which colors?": "Blue, gold, Red", "Which size?": "Medium" });
        assert!(
            recorded_matches(&q, &chosen, &same),
            "multi-select compares as a set"
        );
        let differs = json!({ "Which colors?": "Red, Blue, gold", "Which size?": "Small" });
        assert!(!recorded_matches(&q, &chosen, &differs));
        assert!(!recorded_matches(
            &q,
            &chosen,
            &json!({ "Which size?": "Medium" })
        ));
        assert!(!recorded_matches(&q, &chosen, &json!(null)));
        assert_eq!(
            describe_recorded(&q, &differs),
            "Colors → Red, Blue, gold · Size → Small"
        );
        assert_eq!(
            describe_recorded(&q, &json!({ "Which size?": "Small" })),
            "Colors → (nothing) · Size → Small"
        );
    }

    /// Labels are trimmed at parse; Claude's recorded text is verbatim. A
    /// mismatch reported over a stray space is a false alarm on the one
    /// counter that is meant to stay at zero (Spec J §8).
    #[test]
    fn recorded_answers_are_compared_with_both_sides_trimmed() {
        let one = parsed(&[color()]);
        assert!(recorded_matches(
            &one,
            &[sel(&[2], None)],
            &json!({ "Which color?": "Blue " })
        ));
        let multi = parsed(&[colors_multi()]);
        assert!(recorded_matches(
            &multi,
            &[sel(&[0, 2], None)],
            &json!({ "Which colors?": " Blue ,  Red " })
        ));
    }

    /// The dialog as Spec J §2 measured it: what each key does. `plan` is
    /// correct when, fed to this, it records exactly the selections and ends
    /// submitted. The model is only as true as `mise run verify-questions`.
    struct Dialog<'a> {
        questions: &'a [Question],
        /// The question on screen; `questions.len()` is the review screen.
        at: usize,
        row: usize,
        picked: Vec<Selection>,
        submitted: bool,
        /// A key landed somewhere the plan must never reach.
        broken: Option<String>,
    }

    impl<'a> Dialog<'a> {
        fn new(questions: &'a [Question]) -> Self {
            Self {
                questions,
                at: 0,
                row: 0,
                picked: vec![sel(&[], None); questions.len()],
                submitted: false,
                broken: None,
            }
        }

        fn advance(&mut self) {
            self.at += 1;
            self.row = 0;
            let lone_single = self.questions.len() == 1 && !self.questions[0].multi_select;
            if self.at == self.questions.len() && lone_single {
                self.submitted = true; // no review screen for one single-select
            }
        }

        fn press(&mut self, step: &KeyStep) {
            if self.submitted || self.broken.is_some() {
                self.broken = Some(format!("{step:?} after the end"));
                return;
            }
            if self.at == self.questions.len() {
                // review: `Submit answers` is highlighted
                match step {
                    KeyStep::Key(Key::Enter) => self.submitted = true,
                    other => self.broken = Some(format!("{other:?} on the review screen")),
                }
                return;
            }
            // copy the `&'a [Question]` out so `q` does not borrow `self`
            let questions = self.questions;
            let q = &questions[self.at];
            let n = q.options.len();
            let row = self.row;
            match step {
                KeyStep::Key(Key::Down) => self.row += 1,
                KeyStep::Text(t) if self.row == n => self.picked[self.at].other = Some(t.clone()),
                KeyStep::Key(Key::Enter) if self.row < n && q.multi_select => {
                    let options = &mut self.picked[self.at].options;
                    match options.iter().position(|o| *o == row) {
                        // Enter toggles
                        Some(i) => {
                            options.remove(i);
                        }
                        None => options.push(row),
                    }
                }
                KeyStep::Key(Key::Enter) if self.row < n => {
                    self.picked[self.at].options = vec![self.row];
                    self.advance();
                }
                // single-select: Enter on the typed text submits it
                KeyStep::Key(Key::Enter)
                    if self.row == n && !q.multi_select && self.picked[self.at].other.is_some() =>
                {
                    self.advance()
                }
                // multi-select: row n+1 is `Submit` or `Next`
                KeyStep::Key(Key::Enter) if self.row == n + 1 && q.multi_select => self.advance(),
                other => {
                    self.broken = Some(format!("{other:?} at row {} of {}", self.row, q.name()))
                }
            }
        }
    }

    use proptest::prelude::*;

    fn dialogs() -> impl Strategy<Value = (Vec<Question>, Vec<Selection>)> {
        prop::collection::vec(
            (any::<bool>(), 2usize..=4, any::<u8>(), any::<bool>()),
            1..=4,
        )
        .prop_map(|specs| {
            specs
                .into_iter()
                .enumerate()
                .map(|(qi, (multi, n, bits, other))| {
                    let q = Question {
                        text: format!("Q{qi}?"),
                        header: format!("H{qi}"),
                        multi_select: multi,
                        options: (0..n)
                            .map(|i| Opt {
                                label: format!("opt{i}"),
                                description: String::new(),
                            })
                            .collect(),
                    };
                    let s = if multi {
                        let options: Vec<usize> = (0..n).filter(|i| (bits >> i) & 1 == 1).collect();
                        let other = (other || options.is_empty()).then(|| "free".to_string());
                        Selection { options, other }
                    } else if other {
                        sel(&[], Some("free"))
                    } else {
                        sel(&[bits as usize % n], None)
                    };
                    (q, s)
                })
                .unzip()
        })
    }

    proptest! {
        #[test]
        fn the_plan_drives_the_model_to_exactly_the_selections((questions, selections) in dialogs()) {
            let steps = plan(&questions, &selections);
            prop_assert!(steps.len() <= balerix_api::MAX_KEY_STEPS, "{} steps", steps.len());
            let mut dialog = Dialog::new(&questions);
            for step in &steps {
                dialog.press(step);
            }
            prop_assert_eq!(&dialog.broken, &None);
            prop_assert!(dialog.submitted, "not submitted: {:?}", steps);
            prop_assert_eq!(dialog.picked, selections);
        }
    }
}
