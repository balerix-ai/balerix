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

#[cfg(test)]
mod tests {
    use super::*;

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
