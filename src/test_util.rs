//! Helpers shared by the tests of several modules.

use proptest::prelude::*;

use crate::config::{Rule, Scheme};
use crate::pattern::Pattern;
use crate::style;

/// Creates a scheme from (regex, groups) strip patterns and
/// (regex, groups, style) colour rules.
pub fn scheme(strip: &[(&str, bool)], rules: &[(&str, bool, &str)]) -> Scheme {
    Scheme {
        strip: strip
            .iter()
            .map(|&(regex, groups)| Pattern::new(regex, groups).unwrap())
            .collect(),
        rules: rules
            .iter()
            .map(|&(regex, groups, s)| Rule {
                pattern: Pattern::new(regex, groups).unwrap(),
                style: style::parse(s).unwrap(),
            })
            .collect(),
    }
}

/// Generates raw lines likely to contain problematic bytes and characters.
pub fn raw_line() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        any::<u8>().prop_map(|b| vec![b]),
        any::<char>().prop_map(|c| c.to_string().into_bytes()),
        prop::sample::select(vec![
            "\t",
            "\x1b[31m",
            "\r",
            "\u{85}",
            "\u{202E}",
            "\u{200B}",
            "\\x",
            " ",
            "ab",
            // Wide characters.
            "日本",
            // Combining characters, alone and after a base character.
            "\u{301}",
            "e\u{301}\u{302}",
            // Emoji presentation sequence, flag and ZWJ sequence.
            "\u{2764}\u{FE0F}",
            "\u{1F1EB}\u{1F1F7}",
            "\u{1F468}\u{200D}\u{1F469}",
            // Hangul syllable from jamo.
            "\u{1100}\u{1161}\u{11A8}",
            // Halfwidth katakana with a sound mark, special-cased by ratatui.
            "\u{FF76}\u{FF9E}",
        ])
        .prop_map(|s| s.as_bytes().to_vec()),
    ];
    prop::collection::vec(piece, 0..64).prop_map(|pieces| pieces.concat())
}
