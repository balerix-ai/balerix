//! `AskUserQuestion` (Spec J §6): parse the dialog, match a thread reply to
//! its options, plan the keystrokes. Pure: no I/O and no clock, so every
//! rule is testable, and the rules are the ones Spec J §2 measured.

use std::collections::BTreeSet;

use balerix_api::MAX_KEY_TEXT;
use serde_json::Value;

/// The tool whose `PreToolUse` opens a question.
pub const TOOL: &str = "AskUserQuestion";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opt {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Verbatim: it is the key of `PostToolUse`'s `answers` map.
    pub text: String,
    pub header: String,
    pub multi_select: bool,
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
    pub options: Vec<usize>,
    pub other: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matched {
    /// `exact` is true only when every item was a number or a whole label.
    Answers {
        selections: Vec<Selection>,
        exact: bool,
    },
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
    if reply.eq_ignore_ascii_case("skip") {
        return Ok(Matched::Skip);
    }
    let parts = split(questions, reply)?;
    let mut exact = true;
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
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        return match digits.parse::<usize>() {
            Ok(n) if (1..=q.options.len()).contains(&n) => Ok((n - 1, true)),
            _ => Err(Refusal(format!(
                "**{}** has no option {digits}. Options: {}",
                q.name(),
                options_list(q)
            ))),
        };
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
    if let Some(i) = labels.iter().position(|l| *l == want) {
        return Ok((i, true));
    }
    let unique = |hits: Vec<usize>| match hits.as_slice() {
        [] => None,
        [i] => Some(Ok((*i, false))),
        many => Some(Err(Refusal(format!(
            "\"{}\" could be {} in **{}**. Reply with the number.",
            item.trim(),
            many.iter()
                .map(|i| format!("{}. {}", i + 1, q.options[*i].label))
                .collect::<Vec<_>>()
                .join(" or "),
            q.name()
        )))),
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
    Err(Refusal(format!(
        "\"{}\" matches nothing in **{}**. Options: {}",
        item.trim(),
        q.name(),
        options_list(q)
    )))
}

#[cfg(test)]
pub(crate) mod fixtures {
    use serde_json::{Value, json};

    pub fn color() -> Value {
        json!({ "question": "Which color?", "header": "Color", "multiSelect": false,
                "options": [
                    { "label": "Red", "description": "A warm, bold color" },
                    { "label": "Green", "description": "A calm, natural color" },
                    { "label": "Blue", "description": "A cool, serene color" } ] })
    }

    pub fn size() -> Value {
        json!({ "question": "Which size?", "header": "Size", "multiSelect": false,
                "options": [
                    { "label": "Small", "description": "" },
                    { "label": "Medium", "description": "" },
                    { "label": "Large", "description": "" } ] })
    }

    pub fn colors_multi() -> Value {
        let mut q = color();
        q["question"] = json!("Which colors?");
        q["header"] = json!("Colors");
        q["multiSelect"] = json!(true);
        q
    }

    pub fn input(questions: &[Value]) -> Value {
        json!({ "questions": questions })
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use crate::question::{Matched, Question, Refusal, Selection, match_reply, parse};
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
}
