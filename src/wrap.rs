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

    // Lines are almost always ASCII and wrapping them is much simpler.
    if line.text().is_ascii() {
        return wrap_ascii(line, width);
    }
    wrap_graphemes(line, width)
}

/// Implementation of [`wrap`] for ASCII lines: as the line text contains
/// only printable characters, each of them is a grapheme cluster of width 1.
fn wrap_ascii(line: &Line, width: usize) -> Vec<Row<'_>> {
    let text = line.text();
    if text.is_empty() {
        return vec![Row::new(text)];
    }

    let mut rows = Vec::with_capacity(text.len().div_ceil(width));
    let mut spans = line.spans().iter().peekable();
    for start in (0..text.len()).step_by(width) {
        let end = (start + width).min(text.len());
        let mut row = Row::new(text);
        while let Some(span) = spans.peek() {
            let range = span.range.start.max(start)..span.range.end.min(end);
            if !range.is_empty() {
                row.segments.push((range, span.style));
            }
            if span.range.end > end {
                break;
            }
            spans.next();
        }
        row.width = end - start;
        rows.push(row);
    }
    rows
}

/// Implementation of [`wrap`] for any lines.
fn wrap_graphemes(line: &Line, width: usize) -> Vec<Row<'_>> {
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

    let text = line.text();
    if text.is_ascii() {
        return text.len().div_ceil(width).max(1);
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

/// Returns the width of the line in columns.
pub fn width(line: &Line) -> usize {
    let text = line.text();
    if text.is_ascii() {
        return text.len();
    }
    text.graphemes(true)
        .map(|g| usize::from(g.cell_width()))
        .sum()
}

/// Returns the column at which the text at the given byte offset in the line
/// starts.
pub fn column_at(line: &Line, offset: usize) -> usize {
    let text = line.text();
    if text.is_ascii() {
        return offset.min(text.len());
    }
    text.grapheme_indices(true)
        .take_while(|(start, _)| *start < offset)
        .map(|(_, g)| usize::from(g.cell_width()))
        .sum()
}

/// Returns the part of the line shown in the columns from `first` to
/// `first + width`, without wrapping it.
///
/// The returned row is to be drawn after the returned number of blank
/// columns, which is non-zero only if a wide character is partially before
/// the first column, as such characters are omitted. Similarly, a wide
/// character partially after the last column is omitted too.
pub fn clip(line: &Line, first: usize, width: u16) -> (u16, Row<'_>) {
    let text = line.text();
    let end = first + usize::from(width);
    let mut row = Row::new(text);

    if text.is_ascii() {
        let start = first.min(text.len());
        let stop = end.min(text.len());
        for span in line.spans() {
            let range = span.range.start.max(start)..span.range.end.min(stop);
            if !range.is_empty() {
                row.segments.push((range, span.style));
            }
        }
        row.width = stop - start;
        return (0, row);
    }

    let mut indent = None;
    let mut column = 0;
    for (range, style, w) in graphemes(line) {
        let next = column + w;
        if next > end && w > 0 {
            break;
        }
        if column >= first {
            indent.get_or_insert(column - first);
            row.push(range, style, w);
        }
        column = next;
    }

    let indent = indent.unwrap_or(0).try_into().unwrap_or(width);
    (indent, row)
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

    /// Returns the indent and the text of the clipped line.
    fn clipped(line: &Line, first: usize, width: u16) -> (u16, String) {
        let (indent, row) = clip(line, first, width);
        (indent, row.segments().map(|(s, _)| s).collect())
    }

    #[test]
    fn widths_and_columns() {
        let line = plain("ab日本e\u{301}c");
        assert_eq!(width(&line), 8);
        assert_eq!(column_at(&line, 0), 0);
        assert_eq!(column_at(&line, 2), 2);
        // After "日".
        assert_eq!(column_at(&line, 5), 4);
        // After "e" with its combining accent.
        assert_eq!(column_at(&line, 11), 7);
        assert_eq!(column_at(&line, line.text().len()), 8);

        assert_eq!(width(&plain("abc")), 3);
        assert_eq!(column_at(&plain("abc"), 2), 2);
        assert_eq!(column_at(&plain("abc"), 10), 3);
        assert_eq!(width(&plain("")), 0);
    }

    #[test]
    fn clip_ascii() {
        let s = scheme(&[], &[("c+", false, "red")]);
        let line = process(b"abcccdef", &s);
        assert_eq!(clipped(&line, 0, 3), (0, "abc".to_owned()));
        assert_eq!(clipped(&line, 3, 3), (0, "ccd".to_owned()));
        assert_eq!(clipped(&line, 6, 10), (0, "ef".to_owned()));
        assert_eq!(clipped(&line, 8, 10), (0, String::new()));
        assert_eq!(clipped(&line, 100, 10), (0, String::new()));

        let (_, row) = clip(&line, 1, 4);
        let segments: Vec<_> = row.segments().collect();
        assert_eq!(segments, [("b", NONE), ("ccc", RED)]);
        assert_eq!(row.width(), 4);
    }

    #[test]
    fn clip_wide_characters() {
        let line = plain("a日本b");
        assert_eq!(clipped(&line, 0, 3), (0, "a日".to_owned()));
        // The wide character partially after the end is omitted.
        assert_eq!(clipped(&line, 0, 2), (0, "a".to_owned()));
        // The wide character partially before the start is omitted too.
        assert_eq!(clipped(&line, 2, 4), (1, "本b".to_owned()));
        assert_eq!(clipped(&line, 3, 3), (0, "本b".to_owned()));
        assert_eq!(clip(&line, 2, 4).1.width(), 3);
    }

    proptest! {
        #[test]
        fn clip_invariants(raw in raw_line(), first in 0..30usize, width in 1..20u16) {
            let line = process(&raw, &Scheme::default());
            let (indent, row) = clip(&line, first, width);
            prop_assert!(usize::from(indent) + row.width() <= usize::from(width));

            // Drawing the row with ratatui takes exactly its width.
            let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
            let mut x = indent;
            for (s, style) in row.segments() {
                (x, _) = buf.set_stringn(x, 0, s, usize::from(width - x), style);
            }
            prop_assert_eq!(usize::from(x - indent), row.width());

            // Clipping the whole line gives all of it.
            let (indent, row) = clip(&line, 0, u16::MAX);
            prop_assert_eq!(indent, 0);
            prop_assert_eq!(row.width(), super::width(&line));
            let all: String = row.segments().map(|(s, _)| s).collect();
            prop_assert_eq!(all.as_str(), line.text());
        }

        #[test]
        fn ascii_wrap_as_general(
            text in "[ -~]{0,100}",
            width in 1..20usize,
        ) {
            let s = scheme(&[], &[("[aeiou]+", false, "red"), ("x.", false, "bold")]);
            let line = process(text.as_bytes(), &s);
            prop_assert_eq!(wrap_ascii(&line, width), wrap_graphemes(&line, width));
        }

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
