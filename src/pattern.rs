//! Regex-based patterns used by both strip and colour rules.

use std::fmt;
use std::ops::Range;

use regex::Regex;

/// A regex selecting either its whole matches or only their capture groups.
#[derive(Debug, Clone)]
pub struct Pattern {
    regex: Regex,
    groups: bool,
}

/// Error returned by [`Pattern::new`].
#[derive(Debug, Clone)]
pub enum PatternError {
    /// The regex couldn't be compiled.
    Regex(regex::Error),
    /// Only capture groups were requested, but the regex has none.
    NoGroups,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The details are available from source().
            Self::Regex(_) => write!(f, "invalid regex"),
            Self::NoGroups => write!(f, "\"groups = true\" requires a regex with capture groups"),
        }
    }
}

impl std::error::Error for PatternError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Regex(e) => Some(e),
            Self::NoGroups => None,
        }
    }
}

impl Pattern {
    /// Compiles the regex. If `groups` is true, only the parts of the matches
    /// corresponding to capture groups are selected, so there must be at
    /// least one of them.
    pub fn new(regex: &str, groups: bool) -> Result<Self, PatternError> {
        let regex = Regex::new(regex).map_err(PatternError::Regex)?;

        // The implicit group for the whole match is always counted.
        if groups && regex.captures_len() < 2 {
            return Err(PatternError::NoGroups);
        }

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
        let p = Pattern::new("a(b)c", true).unwrap();
        assert!(p.groups());
        assert_eq!(p.regex().as_str(), "a(b)c");

        assert!(!Pattern::new("abc", false).unwrap().groups());
        assert!(Pattern::new("a(b)c", false).is_ok());
    }

    fn ranges(regex: &str, groups: bool, text: &str) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        Pattern::new(regex, groups)
            .unwrap()
            .for_each_range(text, |r| ranges.push(r));
        ranges
    }

    #[test]
    fn whole_matches() {
        assert_eq!(ranges("b+", false, "abbcb"), [1..3, 4..5]);
        assert!(ranges("x", false, "abc").is_empty());
        assert_eq!(ranges("^.", false, "abc"), [0..1]);
        // Groups don't matter if only whole matches are selected.
        assert_eq!(ranges("a(b)", false, "abab"), [0..2, 2..4]);
        // Empty matches are reported, but are harmless.
        assert_eq!(ranges("x*", false, "ab"), [0..0, 1..1, 2..2]);
    }

    #[test]
    fn group_matches() {
        assert_eq!(ranges("(a)b(c)", true, "abcabc"), [0..1, 2..3, 3..4, 5..6]);
        // Groups that don't participate are skipped, but later ones are not.
        assert_eq!(ranges("(x)?(y)", true, "y"), [0..1]);
        assert_eq!(ranges("(a)|(b)", true, "ab"), [0..1, 1..2]);
        // Nested groups are all reported.
        assert_eq!(ranges("((a)b)", true, "ab"), [0..2, 0..1]);
        // Positions are in bytes.
        assert_eq!(ranges("é(.)", true, "aéb"), [3..4]);
    }

    #[test]
    fn invalid_regex() {
        let e = Pattern::new("a(b", false).unwrap_err();
        assert!(matches!(e, PatternError::Regex(_)));
        assert_eq!(e.to_string(), "invalid regex");
        let source = std::error::Error::source(&e).expect("regex error as source");
        assert!(source.to_string().contains("unclosed group"), "{source}");
    }

    #[test]
    fn groups_required() {
        assert!(matches!(
            Pattern::new("abc", true),
            Err(PatternError::NoGroups)
        ));
        // Non-capturing groups don't count.
        assert!(matches!(
            Pattern::new("a(?:b)c", true),
            Err(PatternError::NoGroups)
        ));
    }
}
