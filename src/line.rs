//! Processing of raw lines into styled text which is safe to display.
//!
//! The processing happens in the following order:
//!
//! 1. Sanitising: the raw bytes are converted to text containing only
//!    characters which can be shown as is. Invalid UTF-8, control characters
//!    and invisible formatting characters are replaced with a visible
//!    representation, called "special" text, which is always shown using
//!    [`SPECIAL_STYLE`]. Tabs are kept for now.
//! 2. Stripping: the parts of the text matched by the scheme strip patterns
//!    are removed, except for special text, which can't be removed.
//! 3. Colouring: each character gets the style of the first rule of the
//!    scheme matching it, if any.
//! 4. Tabs are expanded to spaces, with the style of the tab.

use std::ops::Range;

use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthChar;

use crate::config::{Rule, Scheme};
use crate::pattern::Pattern;

/// Style used for the representation of characters which can't be shown.
pub const SPECIAL_STYLE: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Tab stops are at every multiple of this number of columns.
const TAB_WIDTH: usize = 8;

/// A part of [`Line`] text with its style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Range of bytes of the line text.
    pub range: Range<usize>,
    pub style: Style,
}

/// A processed line, ready to be displayed.
///
/// The text contains only printable characters (no control characters and no
/// tabs) and is fully covered by spans: they are sorted, non-empty, contiguous
/// and on character boundaries. Adjacent spans have different styles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    text: String,
    spans: Vec<Span>,
}

impl Line {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Iterates over the parts of the text with their styles.
    pub fn segments(&self) -> impl Iterator<Item = (&str, Style)> {
        self.spans
            .iter()
            .map(|span| (self.text.get(span.range.clone()).unwrap_or(""), span.style))
    }

    /// Appends text with the given style, merging it with the last span if
    /// it has the same style.
    fn push_str(&mut self, s: &str, style: Style) {
        let start = self.text.len();
        self.text.push_str(s);
        let end = self.text.len();
        if start == end {
            return;
        }

        match self.spans.last_mut() {
            Some(last) if last.style == style => last.range.end = end,
            _ => self.spans.push(Span {
                range: start..end,
                style,
            }),
        }
    }
}

/// Processes a raw line using the given scheme.
pub fn process(raw: &[u8], scheme: &Scheme) -> Line {
    let text = strip(sanitise(raw), &scheme.strip);
    let styles = colour(&text.text, &scheme.rules);
    build(&text, &styles)
}

/// Text with a flag for each of its bytes indicating whether it is special.
#[derive(Debug, Default)]
struct Marked {
    text: String,
    special: Vec<bool>,
}

impl Marked {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            special: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, c: char, special: bool) {
        self.text.push(c);
        self.special.resize(self.text.len(), special);
    }

    fn push_special(&mut self, s: &str) {
        self.text.push_str(s);
        self.special.resize(self.text.len(), true);
    }

    fn is_special(&self, index: usize) -> bool {
        self.special.get(index).copied().unwrap_or(false)
    }
}

/// Converts raw bytes to text in which all characters can be shown as is.
fn sanitise(raw: &[u8]) -> Marked {
    let mut out = Marked::with_capacity(raw.len());
    for chunk in raw.utf8_chunks() {
        for c in chunk.valid().chars() {
            match escape(c) {
                Some(escaped) => out.push_special(&escaped),
                None => out.push(c, false),
            }
        }
        for b in chunk.invalid() {
            out.push_special(&format!("\\x{b:02X}"));
        }
    }
    out
}

/// Returns the representation to use for a character which can't be shown as
/// is, or `None` if it can be.
fn escape(c: char) -> Option<String> {
    match c {
        // Tabs are expanded at the very end.
        '\t' => None,
        // C0 controls are shown in caret notation, e.g. ESC as "^[".
        '\0'..='\x1f' => Some(format!("^{}", char::from(b'@' + c as u8))),
        '\x7f' => Some("^?".to_owned()),
        // Remaining control (i.e. C1) and format characters.
        _ if c.is_control() || is_format(c) => Some(format!("\\u{{{:X}}}", u32::from(c))),
        _ => None,
    }
}

/// Returns true for invisible characters which may affect how the text around
/// them is shown, i.e. the characters of the Unicode general category Cf
/// (format), which includes bidirectional text controls and zero-width
/// characters, and line and paragraph separators (Zl and Zp).
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
            | '\u{2028}'..='\u{202E}'
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

/// Removes the parts of the text matched by any of the patterns, which are
/// all applied to the original text. Special text is never removed.
fn strip(text: Marked, patterns: &[Pattern]) -> Marked {
    let mut remove = vec![false; text.text.len()];
    for pattern in patterns {
        pattern.for_each_range(&text.text, |range| {
            if let Some(bytes) = remove.get_mut(range) {
                bytes.fill(true);
            }
        });
    }

    if !remove.contains(&true) {
        return text;
    }

    let mut out = Marked::with_capacity(text.text.len());
    for (index, c) in text.text.char_indices() {
        let special = text.is_special(index);
        if special || !remove.get(index).copied().unwrap_or(false) {
            out.push(c, special);
        }
    }
    out
}

/// Returns the style of every byte of the text: the style of the first rule
/// matching it, if any.
fn colour<'a>(text: &str, rules: &'a [Rule]) -> Vec<Option<&'a Style>> {
    let mut styles = vec![None; text.len()];
    for rule in rules {
        rule.pattern.for_each_range(text, |range| {
            if let Some(bytes) = styles.get_mut(range) {
                for style in bytes.iter_mut().filter(|s| s.is_none()) {
                    *style = Some(&rule.style);
                }
            }
        });
    }
    styles
}

/// Builds the final line from the text and the styles of its bytes,
/// expanding tabs.
fn build(text: &Marked, styles: &[Option<&Style>]) -> Line {
    let mut line = Line {
        text: String::with_capacity(text.text.len()),
        spans: Vec::new(),
    };

    let mut column = 0;
    let mut buf = [0; 4];
    for (index, c) in text.text.char_indices() {
        let style = if text.is_special(index) {
            SPECIAL_STYLE
        } else {
            styles
                .get(index)
                .copied()
                .flatten()
                .copied()
                .unwrap_or_default()
        };

        if c == '\t' {
            let width = TAB_WIDTH - column % TAB_WIDTH;
            line.push_str(&" ".repeat(width), style);
            column += width;
        } else {
            line.push_str(c.encode_utf8(&mut buf), style);
            column += c.width().unwrap_or(0);
        }
    }

    line
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ratatui::style::Color;

    use super::*;
    use crate::test_util::{raw_line, scheme};

    fn segments(line: &Line) -> Vec<(&str, Style)> {
        line.segments().collect()
    }

    /// Processes the line without stripping nor colouring.
    fn plain(raw: &[u8]) -> Line {
        process(raw, &Scheme::default())
    }

    const NONE: Style = Style::new();
    const RED: Style = Style::new().fg(Color::Red);
    const BLUE: Style = Style::new().fg(Color::Blue);

    #[test]
    fn empty() {
        let line = plain(b"");
        assert_eq!(line.text(), "");
        assert!(line.spans().is_empty());
    }

    #[test]
    fn printable_text_unchanged() {
        for s in ["hello, world", "héllo 日本語 e\u{301}", "\\x41 ^[ \\u{85}"] {
            let line = plain(s.as_bytes());
            assert_eq!(segments(&line), [(s, NONE)]);
        }
    }

    #[test]
    fn invalid_utf8() {
        let line = plain(b"a\xffb");
        assert_eq!(
            segments(&line),
            [("a", NONE), ("\\xFF", SPECIAL_STYLE), ("b", NONE)]
        );

        // Incomplete sequence: each byte is shown separately.
        assert_eq!(plain(b"\xe6\x97(").text(), "\\xE6\\x97(");
        // Overlong encoding of '/' and an encoded surrogate.
        assert_eq!(plain(b"\xc0\xaf").text(), "\\xC0\\xAF");
        assert_eq!(plain(b"\xed\xa0\x80").text(), "\\xED\\xA0\\x80");
        // Valid sequence after an invalid one.
        assert_eq!(plain("\u{80}é".as_bytes()[1..].as_ref()).text(), "\\x80é");
    }

    #[test]
    fn control_characters() {
        let line = plain(b"\x1b[31mred\x1b[0m");
        assert_eq!(
            segments(&line),
            [
                ("^[", SPECIAL_STYLE),
                ("[31mred", NONE),
                ("^[", SPECIAL_STYLE),
                ("[0m", NONE)
            ]
        );

        assert_eq!(plain(b"\0").text(), "^@");
        assert_eq!(plain(b"\x07").text(), "^G");
        assert_eq!(plain(b"\x08").text(), "^H");
        assert_eq!(plain(b"\x0b\x0c").text(), "^K^L");
        assert_eq!(plain(b"\n").text(), "^J");
        assert_eq!(plain(b"crlf\r").text(), "crlf^M");
        assert_eq!(plain(b"\x1f\x7f").text(), "^_^?");

        // C1 controls, including NEL and CSI.
        assert_eq!(plain("\u{85}\u{9b}".as_bytes()).text(), "\\u{85}\\u{9B}");
    }

    #[test]
    fn format_characters() {
        let line = plain("abc\u{202E}def".as_bytes());
        assert_eq!(
            segments(&line),
            [("abc", NONE), ("\\u{202E}", SPECIAL_STYLE), ("def", NONE)]
        );

        assert_eq!(plain("a\u{200B}b".as_bytes()).text(), "a\\u{200B}b");
        assert_eq!(plain("\u{FEFF}bom".as_bytes()).text(), "\\u{FEFF}bom");
        assert_eq!(
            plain("\u{2066}\u{2069}".as_bytes()).text(),
            "\\u{2066}\\u{2069}"
        );
        assert_eq!(plain("\u{2028}".as_bytes()).text(), "\\u{2028}");
        assert_eq!(plain("\u{E0041}".as_bytes()).text(), "\\u{E0041}");

        // Combining characters are not format characters.
        assert_eq!(plain("e\u{301}".as_bytes()).text(), "e\u{301}");
    }

    #[test]
    fn tabs() {
        assert_eq!(plain(b"\t").text(), " ".repeat(8));
        assert_eq!(plain(b"a\tb").text(), format!("a{}b", " ".repeat(7)));
        assert_eq!(plain(b"1234567\tb").text(), "1234567 b");
        assert_eq!(
            plain(b"12345678\tb").text(),
            format!("12345678{}b", " ".repeat(8))
        );
        // Wide characters take two columns.
        assert_eq!(plain("日本\tb".as_bytes()).text(), "日本    b");
        // Special text takes as many columns as characters.
        assert_eq!(plain(b"\xff\tb").text(), "\\xFF    b");
    }

    #[test]
    fn strip_whole_matches() {
        let s = scheme(&[(r"^\S+ ", false)], &[]);
        assert_eq!(
            process(b"2026-10-04 02:11:37 msg", &s).text(),
            "02:11:37 msg"
        );

        let s = scheme(&[("b", false)], &[]);
        assert_eq!(process(b"abcbd", &s).text(), "acd");
    }

    #[test]
    fn strip_groups() {
        // The example from multitail configuration, replacing "-kr 0 7 -kr 15 22".
        let s = scheme(&[(r"^(\S+ +\d+ )\S+( \S+)", true)], &[]);
        assert_eq!(
            process(b"Oct  4 02:11:37 host vmunix: segfault", &s).text(),
            "02:11:37 vmunix: segfault"
        );
        assert_eq!(
            process(b"Oct 14 02:11:37 otherhost CRON[1]: x", &s).text(),
            "02:11:37 CRON[1]: x"
        );
        // Lines not matching are unchanged.
        assert_eq!(process(b"short", &s).text(), "short");
    }

    #[test]
    fn strip_all_patterns_use_original_text() {
        // The second pattern wouldn't match if it were applied after the first.
        let s = scheme(&[("^a", false), ("^ab", false), ("d", false)], &[]);
        assert_eq!(process(b"abcd", &s).text(), "c");

        // Overlapping matches.
        let s = scheme(&[("abc", false), ("bcd", false)], &[]);
        assert_eq!(process(b"abcde", &s).text(), "e");
    }

    #[test]
    fn strip_everything() {
        let s = scheme(&[(".*", false)], &[]);
        let line = process(b"anything", &s);
        assert_eq!(line.text(), "");
        assert!(line.spans().is_empty());
    }

    #[test]
    fn strip_empty_matches() {
        let s = scheme(&[("x*", false), ("^", false), ("$", false)], &[]);
        assert_eq!(process(b"abc", &s).text(), "abc");
    }

    #[test]
    fn strip_keeps_special_text() {
        // The pattern overlaps the start of "\xFF" only.
        let s = scheme(&[(r"^a\\", false)], &[]);
        let line = process(b"a\xffb", &s);
        assert_eq!(segments(&line), [("\\xFF", SPECIAL_STYLE), ("b", NONE)]);

        // The pattern covers special text entirely.
        let s = scheme(&[("^.*$", false)], &[]);
        let line = process(b"a\x1bb\x07c", &s);
        assert_eq!(segments(&line), [("^[^G", SPECIAL_STYLE)]);
    }

    #[test]
    fn strip_then_expand_tabs() {
        let s = scheme(&[("^abc", false)], &[]);
        assert_eq!(process(b"abc\tx", &s).text(), format!("{}x", " ".repeat(8)));
    }

    #[test]
    fn colour_whole_matches() {
        let s = scheme(&[], &[("b+", false, "red")]);
        let line = process(b"abbcb", &s);
        assert_eq!(
            segments(&line),
            [("a", NONE), ("bb", RED), ("c", NONE), ("b", RED)]
        );
    }

    #[test]
    fn colour_groups() {
        let s = scheme(&[], &[("(a)b(c)", true, "red")]);
        let line = process(b"xabcx", &s);
        assert_eq!(
            segments(&line),
            [
                ("x", NONE),
                ("a", RED),
                ("b", NONE),
                ("c", RED),
                ("x", NONE)
            ]
        );

        // Groups not participating in the match are skipped.
        let s = scheme(&[], &[("(x)?(y)", true, "red")]);
        assert_eq!(segments(&process(b"ay", &s)), [("a", NONE), ("y", RED)]);
    }

    #[test]
    fn colour_first_rule_wins() {
        let s = scheme(&[], &[("b", false, "red"), ("abc", false, "blue")]);
        let line = process(b"abcd", &s);
        assert_eq!(
            segments(&line),
            [("a", BLUE), ("b", RED), ("c", BLUE), ("d", NONE)]
        );

        // Later rules can't change anything already coloured.
        let s = scheme(&[], &[("abc", false, "blue"), ("b", false, "red")]);
        assert_eq!(
            segments(&process(b"abcd", &s)),
            [("abc", BLUE), ("d", NONE)]
        );
    }

    #[test]
    fn colour_empty_style_claims_text() {
        let s = scheme(
            &[],
            &[("^.*rspamd.*$", false, ""), ("U=(\\S+)", true, "red")],
        );
        assert_eq!(segments(&process(b"U=_rspamd", &s)), [("U=_rspamd", NONE)]);
        assert_eq!(
            segments(&process(b"U=root", &s)),
            [("U=", NONE), ("root", RED)]
        );
    }

    #[test]
    fn colour_merges_adjacent_spans() {
        let s = scheme(&[], &[("a", false, "red"), ("b", false, "red")]);
        assert_eq!(segments(&process(b"abc", &s)), [("ab", RED), ("c", NONE)]);
    }

    #[test]
    fn colour_after_strip() {
        let s = scheme(&[(r"^\S+ ", false)], &[(r"^(\S+)", true, "red")]);
        assert_eq!(
            segments(&process(b"date time msg", &s)),
            [("time", RED), (" msg", NONE)]
        );
    }

    #[test]
    fn colour_special_text_overrides() {
        let s = scheme(&[], &[(".*", false, "red")]);
        let line = process(b"a\x1bb", &s);
        assert_eq!(
            segments(&line),
            [("a", RED), ("^[", SPECIAL_STYLE), ("b", RED)]
        );
    }

    #[test]
    fn colour_tabs() {
        let s = scheme(&[], &[(r"\t", false, "red")]);
        let line = process(b"a\tb", &s);
        assert_eq!(
            segments(&line),
            [("a", NONE), ("       ", RED), ("b", NONE)]
        );
    }

    #[test]
    fn colour_multibyte() {
        let s = scheme(&[], &[("(é)", true, "red")]);
        assert_eq!(
            segments(&process("aéb".as_bytes(), &s)),
            [("a", NONE), ("é", RED), ("b", NONE)]
        );
    }

    /// Schemes used by the property tests. None of them uses SPECIAL_STYLE,
    /// to be able to count the special text in the result.
    fn test_schemes() -> Vec<Scheme> {
        vec![
            Scheme::default(),
            scheme(&[(".", false)], &[]),
            scheme(
                &[(r"^.{0,5}", false), (r"(\S+)\s", true), (r"\\", false)],
                &[(".", false, "red")],
            ),
            scheme(
                &[],
                &[
                    ("x*", false, "blue"),
                    ("(a)|(b)", true, "bold red"),
                    (r"\\x", false, "green"),
                    ("^", false, "yellow"),
                    (r"\s", false, "on white"),
                    ("", false, "underline"),
                    (".*", false, "color100"),
                ],
            ),
        ]
    }

    proptest! {
        #[test]
        fn process_invariants(raw in raw_line(), n in 0..4usize) {
            let schemes = test_schemes();
            let line = process(&raw, &schemes[n]);
            let text = line.text();

            for c in text.chars() {
                prop_assert!(!c.is_control() && !is_format(c), "{c:?} in {text:?}");
            }

            let mut pos = 0;
            let mut prev_style = None;
            for span in line.spans() {
                prop_assert_eq!(span.range.start, pos);
                prop_assert!(span.range.end > span.range.start);
                prop_assert!(text.is_char_boundary(span.range.end));
                prop_assert_ne!(Some(span.style), prev_style);
                pos = span.range.end;
                prev_style = Some(span.style);
            }
            prop_assert_eq!(pos, text.len());

            let special_in = sanitise(&raw).special.iter().filter(|&&s| s).count();
            let special_out: usize = line
                .spans()
                .iter()
                .filter(|s| s.style == SPECIAL_STYLE)
                .map(|s| s.range.len())
                .sum();
            prop_assert_eq!(special_in, special_out);
        }
    }
}
