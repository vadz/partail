//! Regex-based patterns used by both strip and colour rules.

use std::fmt;

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
}

#[cfg(test)]
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
