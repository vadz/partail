//! Splitting of lines into rows of a given width.
//!
//! Lines are split between grapheme clusters, i.e. user-perceived characters,
//! never inside them, and not at word boundaries: as in multitail, rows are
//! filled completely. The width of each cluster is computed exactly as
//! ratatui computes it when drawing, so a row never takes more columns than
//! expected.

use std::mem;
use std::ops::Range;

use ratatui::buffer::CellWidth;
use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;

use crate::line::Line;

/// A row of a wrapped line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row<'a> {
    /// Text of the whole line.
    text: &'a str,
    /// Parts of the line text in this row, contiguous and in order.
    segments: Vec<(Range<usize>, Style)>,
    width: usize,
}

impl<'a> Row<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            segments: Vec::new(),
            width: 0,
        }
    }

    /// Width of the row in columns.
    ///
    /// This is never more than the width used for wrapping, except for a row
    /// containing a single grapheme cluster wider than it.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Iterates over the parts of the row with their styles.
    ///
    /// Each part consists of complete grapheme clusters, and consecutive
    /// parts have different styles.
    pub fn segments(&self) -> impl Iterator<Item = (&'a str, Style)> + '_ {
        let text = self.text;
        self.segments
            .iter()
            .map(move |(range, style)| (text.get(range.clone()).unwrap_or(""), *style))
    }

    fn push(&mut self, range: Range<usize>, style: Style, width: usize) {
        self.width += width;
        match self.segments.last_mut() {
            Some((last, last_style)) if *last_style == style => last.end = range.end,
            _ => self.segments.push((range, style)),
        }
    }
}

/// Splits the line into rows of at most the given width.
///
/// An empty line results in a single empty row, but nothing at all is
/// returned if the width is 0.
pub fn wrap(line: &Line, width: u16) -> Vec<Row<'_>> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let mut rows = Vec::new();
    let mut row = Row::new(line.text());
    for (range, style, w) in graphemes(line) {
        if starts_new_row(row.width, w, width) {
            rows.push(mem::replace(&mut row, Row::new(line.text())));
        }
        row.push(range, style, w);
    }
    rows.push(row);

    rows
}

/// Returns the number of rows [`wrap`] would return, more efficiently.
pub fn row_count(line: &Line, width: u16) -> usize {
    let width = usize::from(width);
    if width == 0 {
        return 0;
    }

    let mut rows = 1;
    let mut row_width = 0;
    for g in line.text().graphemes(true) {
        let w = usize::from(g.cell_width());
        if starts_new_row(row_width, w, width) {
            rows += 1;
            row_width = 0;
        }
        row_width += w;
    }

    rows
}

/// Returns true if a grapheme cluster of width `w` must be put on a new row
/// rather than on the current one, already containing `row_width` columns.
///
/// A cluster wider than `width` is put on a row of its own instead of being
/// dropped, as nothing better can be done with it.
fn starts_new_row(row_width: usize, w: usize, width: usize) -> bool {
    row_width > 0 && row_width + w > width
}

/// Iterates over the grapheme clusters of the line, with their byte range,
/// style and width. The style of a cluster is the style of its first
/// character, as a cluster is drawn in a single cell and so can't use
/// several styles.
fn graphemes(line: &Line) -> impl Iterator<Item = (Range<usize>, Style, usize)> + '_ {
    let mut spans = line.spans().iter().peekable();
    line.text().grapheme_indices(true).map(move |(start, g)| {
        while spans.next_if(|span| span.range.end <= start).is_some() {}
        let style = spans.peek().map_or_else(Style::default, |span| span.style);
        (start..start + g.len(), style, usize::from(g.cell_width()))
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    use super::*;
    use crate::config::Scheme;
    use crate::line::process;
    use crate::test_util::{raw_line, scheme};

    const NONE: Style = Style::new();
    const RED: Style = Style::new().fg(Color::Red);
    const BLUE: Style = Style::new().fg(Color::Blue);

    fn plain(s: &str) -> Line {
        process(s.as_bytes(), &Scheme::default())
    }

    /// Returns the text of all rows.
    fn texts(line: &Line, width: u16) -> Vec<String> {
        wrap(line, width)
            .iter()
            .map(|row| row.segments().map(|(s, _)| s).collect())
            .collect()
    }

    fn widths(line: &Line, width: u16) -> Vec<usize> {
        wrap(line, width).iter().map(Row::width).collect()
    }

    #[test]
    fn empty() {
        let line = plain("");
        let rows = wrap(&line, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].width(), 0);
        assert_eq!(rows[0].segments().count(), 0);
        assert_eq!(row_count(&line, 10), 1);
    }

    #[test]
    fn zero_width() {
        let line = plain("abc");
        assert!(wrap(&line, 0).is_empty());
        assert_eq!(row_count(&line, 0), 0);
    }

    #[test]
    fn ascii() {
        let line = plain("abcdefghij");
        assert_eq!(texts(&line, 20), ["abcdefghij"]);
        assert_eq!(texts(&line, 10), ["abcdefghij"]);
        assert_eq!(texts(&line, 9), ["abcdefghi", "j"]);
        assert_eq!(texts(&line, 4), ["abcd", "efgh", "ij"]);
        assert_eq!(texts(&line, 1).len(), 10);
        assert_eq!(widths(&line, 4), [4, 4, 2]);
    }

    #[test]
    fn not_at_word_boundaries() {
        assert_eq!(texts(&plain("ab cd ef"), 4), ["ab c", "d ef"]);
    }

    #[test]
    fn styles() {
        let s = scheme(&[], &[("B+", false, "red")]);
        let line = process(b"aaBBBBcc", &s);
        let rows = wrap(&line, 3);
        let segments: Vec<Vec<_>> = rows.iter().map(|r| r.segments().collect()).collect();
        assert_eq!(
            segments,
            [
                vec![("aa", NONE), ("B", RED)],
                vec![("BBB", RED)],
                vec![("cc", NONE)],
            ]
        );
    }

    #[test]
    fn wide_characters() {
        let line = plain("a日本");
        assert_eq!(texts(&line, 5), ["a日本"]);
        assert_eq!(texts(&line, 4), ["a日", "本"]);
        assert_eq!(texts(&line, 3), ["a日", "本"]);
        assert_eq!(widths(&line, 3), [3, 2]);
        // The wide character doesn't fit at the end of the row.
        assert_eq!(texts(&line, 2), ["a", "日", "本"]);
        assert_eq!(widths(&line, 2), [1, 2, 2]);
        assert_eq!(texts(&plain("日本"), 3), ["日", "本"]);
    }

    #[test]
    fn too_wide_characters() {
        let line = plain("a日b");
        assert_eq!(texts(&line, 1), ["a", "日", "b"]);
        assert_eq!(widths(&line, 1), [1, 2, 1]);
        assert_eq!(row_count(&line, 1), 3);
    }

    #[test]
    fn combining_characters() {
        let line = plain("e\u{301}e\u{301}\u{302}e\u{301}");
        assert_eq!(texts(&line, 2), ["e\u{301}e\u{301}\u{302}", "e\u{301}"]);
        assert_eq!(widths(&line, 2), [2, 1]);

        // Combining character without base character.
        let line = plain("\u{301}ab");
        assert_eq!(texts(&line, 2), ["\u{301}ab"]);
        assert_eq!(widths(&line, 2), [2]);
    }

    #[test]
    fn cluster_uses_style_of_first_character() {
        // The rule colours the combining characters differently.
        let s = scheme(&[], &[(r"\p{M}", false, "blue"), ("e", false, "red")]);
        let line = process("ae\u{301}b\u{301}".as_bytes(), &s);
        let rows = wrap(&line, 10);
        let segments: Vec<_> = rows[0].segments().collect();
        assert_eq!(
            segments,
            [("a", NONE), ("e\u{301}", RED), ("b\u{301}", NONE)]
        );

        // The style of the line itself is unchanged.
        assert!(line.spans().iter().any(|span| span.style == BLUE));
    }

    #[test]
    fn widths_as_in_ratatui() {
        // unicode-width considers the sound mark to be zero width, but
        // ratatui and terminals don't.
        let line = plain("\u{FF76}\u{FF9E}x");
        assert_eq!(widths(&line, 10), [3]);
        assert_eq!(texts(&line, 2), ["\u{FF76}\u{FF9E}", "x"]);

        // Emoji presentation sequence.
        assert_eq!(widths(&plain("\u{2764}\u{FE0F}"), 10), [2]);
    }

    proptest! {
        #[test]
        fn wrap_invariants(raw in raw_line(), width in 1..20u16) {
            let s = scheme(
                &[],
                &[
                    (r"\p{M}", false, "blue"),
                    ("[aeiou]", false, "red"),
                    ("日", false, "bold"),
                    (r"\\", false, "underline"),
                ],
            );
            let line = process(&raw, &s);
            let rows = wrap(&line, width);

            prop_assert_eq!(rows.len(), row_count(&line, width));

            // All the text is in the rows, in order.
            let all: String = rows.iter().flat_map(|r| r.segments().map(|(s, _)| s)).collect();
            prop_assert_eq!(all.as_str(), line.text());

            let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
            for (n, row) in rows.iter().enumerate() {
                let mut prev_style = None;
                for (s, style) in row.segments() {
                    prop_assert!(!s.is_empty());
                    prop_assert_ne!(Some(style), prev_style);
                    prev_style = Some(style);
                }

                let visible = row
                    .segments()
                    .flat_map(|(s, _)| s.graphemes(true))
                    .filter(|g| g.cell_width() > 0)
                    .count();
                if row.width() > usize::from(width) {
                    prop_assert_eq!(visible, 1);
                    continue;
                }

                // Drawing the row with ratatui takes exactly its width.
                buf.reset();
                let mut x = 0;
                for (s, style) in row.segments() {
                    (x, _) = buf.set_stringn(x, 0, s, usize::from(width - x), style);
                }
                prop_assert_eq!(usize::from(x), row.width());

                // Rows are filled as much as possible.
                if let Some(next) = rows.get(n + 1) {
                    let first = next.segments().next().and_then(|(s, _)| s.graphemes(true).next());
                    let first_width = first.map_or(0, |g| usize::from(g.cell_width()));
                    prop_assert!(row.width() + first_width > usize::from(width));
                }
            }
        }
    }
}
