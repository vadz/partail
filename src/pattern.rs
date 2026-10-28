//! Regex-based patterns used by both strip and colour rules.

use std::ops::Range;

use regex::Regex;

/// A regex selecting either its whole matches or, if it has any capture
/// groups, only their parts matched by these groups.
#[derive(Debug, Clone)]
pub struct Pattern {
    regex: Regex,
    groups: bool,
}

impl Pattern {
    /// Compiles the regex.
    ///
    /// Capturing groups are only used for selecting the parts of the matches,
    /// as the regex syntax has no backreferences, so non-capturing groups,
    /// i.e. `(?:...)`, must be used when grouping is needed for other
    /// purposes, e.g. repetition or alternation.
    pub fn new(regex: &str) -> Result<Self, regex::Error> {
        let regex = Regex::new(regex)?;

        // The implicit group for the whole match is always counted.
        let groups = regex.captures_len() > 1;

        Ok(Self { regex, groups })
    }

    pub fn regex(&self) -> &Regex {
        &self.regex
    }

    /// Whether only capture groups, and not whole matches, are selected.
    pub fn groups(&self) -> bool {
        self.groups
    }

    /// Calls the given function with the byte range of every part of the
    /// text selected by this pattern.
    ///
    /// All non-overlapping matches are used. If only groups are selected, the
    /// groups which didn't participate in a match are skipped.
    pub fn for_each_range(&self, text: &str, mut f: impl FnMut(Range<usize>)) {
        if self.groups {
            for caps in self.regex.captures_iter(text) {
                caps.iter().skip(1).flatten().for_each(|m| f(m.range()));
            }
        } else {
            self.regex.find_iter(text).for_each(|m| f(m.range()));
        }
    }
}

#[cfg(test)]
// Arrays of ranges here are lists of byte ranges, not ranges of numbers.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn valid() {
        let p = Pattern::new("a(b)c").unwrap();
        assert!(p.groups());
        assert_eq!(p.regex().as_str(), "a(b)c");

        assert!(!Pattern::new("abc").unwrap().groups());
        // Non-capturing groups don't count.
        assert!(!Pattern::new("a(?:b)c").unwrap().groups());
    }

    fn ranges(regex: &str, text: &str) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        Pattern::new(regex)
            .unwrap()
            .for_each_range(text, |r| ranges.push(r));
        ranges
    }

    #[test]
    fn whole_matches() {
        assert_eq!(ranges("b+", "abbcb"), [1..3, 4..5]);
        assert!(ranges("x", "abc").is_empty());
        assert_eq!(ranges("^.", "abc"), [0..1]);
        // Non-capturing groups select the whole match.
        assert_eq!(ranges(r"(?:\d\.){2}\d", "v1.2.3"), [1..6]);
        // Empty matches are reported, but are harmless.
        assert_eq!(ranges("x*", "ab"), [0..0, 1..1, 2..2]);
    }

    #[test]
    fn group_matches() {
        assert_eq!(ranges("(a)b(c)", "abcabc"), [0..1, 2..3, 3..4, 5..6]);
        // Groups that don't participate are skipped, but later ones are not.
        assert_eq!(ranges("(x)?(y)", "y"), [0..1]);
        assert_eq!(ranges("(a)|(b)", "ab"), [0..1, 1..2]);
        // Nested groups are all reported.
        assert_eq!(ranges("((a)b)", "ab"), [0..2, 0..1]);
        // Named groups count too.
        assert_eq!(ranges("a(?<n>b)", "ab"), [1..2]);
        // Positions are in bytes.
        assert_eq!(ranges("é(.)", "aéb"), [3..4]);
    }

    #[test]
    fn invalid_regex() {
        let e = Pattern::new("a(b").unwrap_err();
        assert!(e.to_string().contains("unclosed group"), "{e}");
    }
}
