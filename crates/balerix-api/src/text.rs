//! Text typed into an agent's pane: the one implementation of what
//! `send_text` strips, and of what a plugin's text becomes before it
//! reaches Claude's input line. The daemon and the plugins share it so a
//! plugin's tracked text is what was typed.

use std::borrow::Cow;

/// The characters that switch Claude Code's input line to another input
/// mode when they open it (`!` in current versions; `#` and `&` have
/// been mode keys too).
pub const MODE_CHARS: [char; 3] = ['!', '#', '&'];

/// Strips terminal control characters: every C0 control but `\n` and
/// `\t`, DEL, and every C1 control are removed; `\r\n` becomes `\n` and a
/// lone `\r` is dropped. The text is otherwise unchanged. Stripping, not
/// refusing, so a review or a reply that happens to carry one still
/// arrives. Every `send_text` goes through here.
pub fn strip_terminal_controls(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .filter(|&c| !c.is_control() || c == '\n' || c == '\t')
            .collect(),
    )
}

/// Unicode format characters (category Cf): invisible, and dropped or
/// ignored by some trims and not by others.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{600}'..='\u{605}'
            | '\u{61C}'
            | '\u{6DD}'
            | '\u{70F}'
            | '\u{890}'..='\u{891}'
            | '\u{8E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// Invisible characters Claude Code's input line drops before it reads
/// a text, beyond whitespace, controls and [`is_format`]: fillers,
/// variation selectors, tags and other default-ignorable code points.
fn is_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{34F}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180D}'
            | '\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1107F}'
            | '\u{16FE4}'
            | '\u{E0000}'
            | '\u{E0002}'..='\u{E001F}'
            | '\u{E0080}'..='\u{E0FFF}'
    )
}

/// What may precede a text's first visible character: whitespace,
/// controls, format characters and the other invisible characters the
/// input line drops ([`is_ignorable`]).
pub fn is_blank(c: char) -> bool {
    c.is_whitespace() || c.is_control() || is_format(c) || is_ignorable(c)
}

/// A plugin's text as prose for Claude's input line, in order: terminal
/// controls stripped as [`strip_terminal_controls`] does, leading blanks
/// ([`is_blank`]) dropped, and a leading [`MODE_CHARS`] character
/// escaped with a backslash, Markdown's escape for a literal character,
/// so the text never opens the input line in another input mode. A
/// visible character survives any trimming between here and the input
/// line, where a leading space would not. Idempotent: an answer passes
/// through unchanged. Slash commands are unchanged.
pub fn prompt_text(text: &str) -> Cow<'_, str> {
    let stripped = strip_terminal_controls(text);
    let body = stripped.trim_start_matches(is_blank);
    if body.starts_with(MODE_CHARS) {
        return Cow::Owned(format!("\\{body}"));
    }
    if body.len() == text.len() {
        return Cow::Borrowed(text);
    }
    Cow::Owned(body.to_string())
}

/// What `send_text` types, with `send-keys -l`, after a text it pasted
/// and before the submitting Enter (#279). Since Claude Code 2.1.283 a
/// paste Claude collapses (more than 800 characters, or four lines or
/// more) reaches the model inside `<pasted_content id="…">` tags, and the
/// model treats a turn that is only pasted text as reference material:
/// it summarises a review and asks, instead of acting on it. A short line
/// typed after the paste makes the turn a request, as the same text typed
/// was before 2.1.283. It is neutral on purpose: it claims no authorship
/// for text that may come from outside parties (an issue author, a room
/// member), and gives the paste no more standing than a typed prompt
/// (docs/THREAT-MODEL.md). Claude puts a blank line between the wrapper
/// and it; a version that does not wrap (2.1.267) joins the two, hence
/// the leading space. It opens with a space, never with `/` or a
/// [`MODE_CHARS`] character, ends without `;` (tmux's separator), and
/// holds no newline or control character,
/// so it can neither submit early nor switch the input line to another
/// mode.
pub const PASTE_LEAD: &str = " Act on the message above as this turn's request.";

/// Above this many bytes a single line is pasted with a lead. Claude
/// Code 2.1.299 and 2.1.300 wrap input of more than 800 characters, or
/// with more than two newlines (four lines); 790 characters arrive as
/// typed. Bytes count at least as many as Claude's characters, and the
/// margin below 800 is for a version that wraps sooner: a lead on a text
/// Claude would not have wrapped costs one sentence.
pub const PASTE_LEAD_ABOVE: usize = 500;

/// Whether `send_text` pastes `text` and, when it submits, types
/// [`PASTE_LEAD`] after it: any text with a newline, or longer than
/// [`PASTE_LEAD_ABOVE`], unless it is a slash command (its first visible
/// character `/`), whose arguments the lead would change.
pub fn needs_paste_lead(text: &str) -> bool {
    !text.trim_start_matches(is_blank).starts_with('/')
        && (text.contains('\n') || text.len() > PASTE_LEAD_ABOVE)
}

/// `prompt` less a trailing [`PASTE_LEAD`], trimmed; `None` without one.
fn strip_paste_lead(prompt: &str) -> Option<&str> {
    let body = prompt.trim_end().strip_suffix(PASTE_LEAD.trim_start())?;
    Some(body.trim())
}

/// The inside of a `<pasted_content …>` wrapper around all of `text`
/// (blanks around it allowed), closed by `</pasted_content>` with the
/// same attributes or with none; `None` when `text` is not one wrapper.
fn unwrap_pasted(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("<pasted_content")?;
    let (attrs, inner) = rest.split_once('>')?;
    if attrs.contains('<') {
        return None;
    }
    let close = format!("</pasted_content{attrs}>");
    inner
        .strip_suffix(close.as_str())
        .or_else(|| inner.strip_suffix("</pasted_content>"))
}

/// What a `UserPromptSubmit`'s `prompt` may be the submission of, least
/// stripped first (#279): the prompt itself, then without a trailing
/// [`PASTE_LEAD`], then without that and a `<pasted_content>` wrapper
/// around all of the rest (also without a lead, from a daemon before
/// #279). Match a sent text against them in this order, so text that
/// itself ends in the lead or is shaped like a wrapper is matched as it
/// was sent before anything is stripped. Whitespace is not normalised.
pub fn submitted_candidates(prompt: &str) -> Vec<&str> {
    let mut out = vec![prompt];
    let unled = strip_paste_lead(prompt);
    out.extend(unled);
    out.extend(unwrap_pasted(unled.unwrap_or(prompt)));
    out.dedup();
    out
}

/// The text a plugin sent, from the `prompt` of the `UserPromptSubmit`
/// that took it: the most stripped of [`submitted_candidates`] (a
/// trailing [`PASTE_LEAD`] and a `<pasted_content>` wrapper gone, #279).
/// A prompt with neither is borrowed unchanged.
pub fn submitted_text(prompt: &str) -> Cow<'_, str> {
    Cow::Borrowed(submitted_candidates(prompt).pop().unwrap_or(prompt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lead_is_one_short_line_that_opens_no_mode() {
        let visible = PASTE_LEAD.trim_start_matches(is_blank);
        assert!(!visible.starts_with('/') && !visible.starts_with(MODE_CHARS));
        assert!(!PASTE_LEAD.starts_with('/') && !PASTE_LEAD.starts_with(MODE_CHARS));
        assert_eq!(strip_terminal_controls(PASTE_LEAD), PASTE_LEAD);
        assert!(!PASTE_LEAD.contains('\n'));
        // tmux takes a trailing `;` as its command separator
        assert!(!PASTE_LEAD.ends_with(';'));
        assert!(!needs_paste_lead(PASTE_LEAD), "never collapsed itself");
        assert_eq!(prompt_text(visible), visible, "nothing to escape");
    }

    #[test]
    fn a_lead_follows_multi_line_and_long_text_but_not_commands() {
        assert!(needs_paste_lead("a\nb"));
        assert!(needs_paste_lead(&"x".repeat(PASTE_LEAD_ABOVE + 1)));
        assert!(!needs_paste_lead(&"x".repeat(PASTE_LEAD_ABOVE)));
        assert!(!needs_paste_lead("run the tests"));
        assert!(!needs_paste_lead("/compact keep\nthe plan"));
        assert!(!needs_paste_lead(" \u{200B}/compact keep\nthe plan"));
        assert!(needs_paste_lead("\\/x\ny"), "an escaped slash is prose");
    }

    #[test]
    fn the_submitted_text_drops_the_lead_and_the_wrapper() {
        // what claude 2.1.299's UserPromptSubmit carried
        let wrapped = format!(
            "\n\n<pasted_content id=\"74ce\">\nline one\n\nline two\n</pasted_content id=\"74ce\">\n\n{PASTE_LEAD}"
        );
        assert_eq!(submitted_text(&wrapped).trim(), "line one\n\nline two");
        // 2.1.267: as typed, the lead joined on
        assert_eq!(
            submitted_text(&format!("line one\nline two{PASTE_LEAD}")),
            "line one\nline two"
        );
        // a bare closing tag
        assert_eq!(
            submitted_text(&format!(
                "<pasted_content id=\"9\">\nx\ny\n</pasted_content>{PASTE_LEAD}"
            ))
            .trim(),
            "x\ny"
        );
        // least stripped first
        let led = format!("x{PASTE_LEAD}");
        assert_eq!(submitted_candidates(&led), vec![led.as_str(), "x"]);
        assert_eq!(submitted_candidates("plain"), vec!["plain"]);
        // a wrapper without the lead (a daemon from before #279)
        assert_eq!(
            submitted_text("<pasted_content id=\"1\">\nx\ny\n</pasted_content id=\"1\">").trim(),
            "x\ny"
        );
        for t in [
            "run the tests",
            "<pasted_content id=\"1\">x</pasted_content id=\"2\">",
            "<pasted_content id=\"1\">a</pasted_content id=\"1\"> and more",
            "",
        ] {
            assert!(
                matches!(submitted_text(t), Cow::Borrowed(b) if b == t),
                "{t:?}"
            );
        }
    }

    #[test]
    fn terminal_controls_are_stripped() {
        assert!(matches!(
            strip_terminal_controls("plain\ttext\nok"),
            Cow::Borrowed("plain\ttext\nok")
        ));
        assert_eq!(
            strip_terminal_controls("a\u{1b}[31mb\u{7}c\u{85}d\u{9b}e\u{7f}f\u{0}g"),
            "a[31mbcdefg"
        );
        assert_eq!(
            strip_terminal_controls("one\r\ntwo\rthree\n"),
            "one\ntwothree\n"
        );
        assert_eq!(
            strip_terminal_controls("ä € \u{a0}"),
            "ä € \u{a0}",
            "not controls"
        );
        assert_eq!(strip_terminal_controls("!x"), "!x", "nothing else");
    }

    #[test]
    fn a_leading_mode_character_is_escaped() {
        assert_eq!(prompt_text("!ls"), "\\!ls");
        assert_eq!(prompt_text("# notes\nmore"), "\\# notes\nmore");
        assert_eq!(prompt_text("&x"), "\\&x");
        assert_eq!(prompt_text("  \n\t!ls"), "\\!ls");
        assert_eq!(prompt_text("\u{FEFF}\u{200B} !ls"), "\\!ls");
        assert_eq!(prompt_text("\u{7}!x"), "\\!x");
        assert_eq!(prompt_text("\u{1b}\u{85}\r\u{2066}#x"), "\\#x");
        assert_eq!(prompt_text("\u{E0020}&x"), "\\&x");
    }

    /// One character from each invisible range, ahead of a command and of
    /// a mode character: the command is seen as one, the mode escaped.
    const INVISIBLES: [char; 33] = [
        '\u{AD}',
        '\u{600}',
        '\u{61C}',
        '\u{6DD}',
        '\u{70F}',
        '\u{890}',
        '\u{8E2}',
        '\u{180E}',
        '\u{34F}',
        '\u{115F}',
        '\u{1160}',
        '\u{17B4}',
        '\u{17B5}',
        '\u{180C}',
        '\u{180F}',
        '\u{200D}',
        '\u{202C}',
        '\u{2065}',
        '\u{3164}',
        '\u{FE0F}',
        '\u{FEFF}',
        '\u{FFA0}',
        '\u{FFF4}',
        '\u{FFFA}',
        '\u{1107F}',
        '\u{110BD}',
        '\u{16FE4}',
        '\u{1D175}',
        '\u{E0000}',
        '\u{E0001}',
        '\u{E0010}',
        '\u{E0050}',
        '\u{E0ABC}',
    ];

    #[test]
    fn every_invisible_range_is_a_blank() {
        for c in INVISIBLES {
            assert!(is_blank(c), "U+{:04X}", c as u32);
            assert_eq!(
                prompt_text(&format!("{c}/config x")),
                "/config x",
                "U+{:04X}",
                c as u32
            );
            assert_eq!(
                prompt_text(&format!("{c}{c}!x")),
                "\\!x",
                "U+{:04X}",
                c as u32
            );
        }
        assert!(!is_blank('x') && !is_blank('/') && !is_blank('!'));
    }

    #[test]
    fn leading_blanks_go_and_controls_are_stripped_elsewhere() {
        assert_eq!(prompt_text("  /exit"), "/exit");
        assert_eq!(prompt_text("\u{7}hi\u{7} there"), "hi there");
        assert_eq!(prompt_text(" \n "), "");
    }

    #[test]
    fn other_text_is_borrowed_unchanged_and_escaping_is_idempotent() {
        for t in [
            "run the tests",
            "/compact",
            "a!b",
            "x\n!y",
            "",
            "@a hi",
            "\\!x",
        ] {
            assert!(
                matches!(prompt_text(t), Cow::Borrowed(b) if b == t),
                "{t:?}"
            );
        }
        for t in ["!x", " #y", "\u{7}&z"] {
            let once = prompt_text(t).into_owned();
            assert_eq!(prompt_text(&once), once.as_str());
        }
    }
}
