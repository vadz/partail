//! Parsing of styles such as `"bold white on red"`.
//!
//! A style is a whitespace-separated list of words, in any order: attributes,
//! at most one foreground colour and at most one background colour, given as
//! `on` followed by a colour. An empty string is a valid style that doesn't
//! change anything.
//!
//! Colour names have their usual ANSI meaning, as in multitail: `white` is
//! ANSI colour 7, which ratatui calls [`Color::Gray`], and not ratatui's
//! [`Color::White`], which is ANSI colour 15, i.e. `bright-white` here.

use std::fmt;

use ratatui::style::{Color, Modifier, Style};

/// Error returned by [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StyleError {
    /// The word is neither an attribute, a colour nor `on`.
    UnknownWord(String),
    /// The word looks like a palette or RGB colour, but isn't a valid one.
    InvalidColor(String),
    /// `on` must be followed by a colour, but it was the last word.
    MissingBackground,
    /// `on` was followed by something which is not a colour.
    NotABackground(String),
    /// More than one foreground colour was given; this is the second one.
    DuplicateForeground(String),
    /// More than one background colour was given; this is the second one.
    DuplicateBackground(String),
}

impl fmt::Display for StyleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownWord(w) => write!(
                f,
                "unknown word \"{w}\", expected an attribute, a colour or \"on\""
            ),
            Self::InvalidColor(w) => write!(
                f,
                "invalid colour \"{w}\", expected \"color0\" to \"color255\" or \"#rrggbb\""
            ),
            Self::MissingBackground => write!(f, "\"on\" must be followed by a colour"),
            Self::NotABackground(w) => {
                write!(f, "\"on\" must be followed by a colour, not \"{w}\"")
            }
            Self::DuplicateForeground(w) => write!(f, "second foreground colour \"{w}\""),
            Self::DuplicateBackground(w) => write!(f, "second background colour \"{w}\""),
        }
    }
}

impl std::error::Error for StyleError {}

/// Parses a style string.
pub fn parse(s: &str) -> Result<Style, StyleError> {
    let mut style = Style::new();
    let mut fg_set = false;
    let mut bg_set = false;

    let mut words = s.split_whitespace();
    while let Some(word) = words.next() {
        if word == "on" {
            let bg = words.next().ok_or(StyleError::MissingBackground)?;
            let color = match parse_color(bg)? {
                Some(color) => color,
                None => return Err(StyleError::NotABackground(bg.to_owned())),
            };
            if bg_set {
                return Err(StyleError::DuplicateBackground(bg.to_owned()));
            }
            style = style.bg(color);
            bg_set = true;
        } else if let Some(modifier) = parse_attribute(word) {
            style = style.add_modifier(modifier);
        } else if let Some(color) = parse_color(word)? {
            if fg_set {
                return Err(StyleError::DuplicateForeground(word.to_owned()));
            }
            style = style.fg(color);
            fg_set = true;
        } else {
            return Err(StyleError::UnknownWord(word.to_owned()));
        }
    }

    Ok(style)
}

fn parse_attribute(word: &str) -> Option<Modifier> {
    Some(match word {
        "bold" => Modifier::BOLD,
        "dim" => Modifier::DIM,
        "italic" => Modifier::ITALIC,
        "underline" => Modifier::UNDERLINED,
        "reverse" => Modifier::REVERSED,
        "blink" => Modifier::SLOW_BLINK,
        _ => return None,
    })
}

/// Returns `Ok(None)` if the word isn't a colour at all and an error if it
/// looks like one but is invalid.
fn parse_color(word: &str) -> Result<Option<Color>, StyleError> {
    let color = match word {
        "default" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        "bright-black" => Color::DarkGray,
        "bright-red" => Color::LightRed,
        "bright-green" => Color::LightGreen,
        "bright-yellow" => Color::LightYellow,
        "bright-blue" => Color::LightBlue,
        "bright-magenta" => Color::LightMagenta,
        "bright-cyan" => Color::LightCyan,
        "bright-white" => Color::White,
        _ => {
            if let Some(index) = word.strip_prefix("color") {
                return parse_palette_index(index)
                    .map(|n| Some(Color::Indexed(n)))
                    .ok_or_else(|| StyleError::InvalidColor(word.to_owned()));
            }
            if let Some(hex) = word.strip_prefix('#') {
                return parse_rgb(hex)
                    .map(|(r, g, b)| Some(Color::Rgb(r, g, b)))
                    .ok_or_else(|| StyleError::InvalidColor(word.to_owned()));
            }
            return Ok(None);
        }
    };
    Ok(Some(color))
}

fn parse_palette_index(s: &str) -> Option<u8> {
    // Check for digits explicitly, as parse() would also accept "+1".
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn parse_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let component = |range| u8::from_str_radix(hex.get(range)?, 16).ok();
    Some((component(0..2)?, component(2..4)?, component(4..6)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Style {
        parse(s).unwrap_or_else(|e| panic!("failed to parse \"{s}\": {e}"))
    }

    fn err(s: &str) -> StyleError {
        parse(s).expect_err(s)
    }

    #[test]
    fn empty() {
        assert_eq!(ok(""), Style::new());
        assert_eq!(ok("  \t "), Style::new());
    }

    #[test]
    fn basic_colors_have_ansi_meaning() {
        assert_eq!(ok("black"), Style::new().fg(Color::Black));
        assert_eq!(ok("red"), Style::new().fg(Color::Red));
        assert_eq!(ok("green"), Style::new().fg(Color::Green));
        assert_eq!(ok("yellow"), Style::new().fg(Color::Yellow));
        assert_eq!(ok("blue"), Style::new().fg(Color::Blue));
        assert_eq!(ok("magenta"), Style::new().fg(Color::Magenta));
        assert_eq!(ok("cyan"), Style::new().fg(Color::Cyan));
        // ratatui calls ANSI colour 7 "gray" and 15 "white".
        assert_eq!(ok("white"), Style::new().fg(Color::Gray));
        assert_eq!(ok("default"), Style::new().fg(Color::Reset));
    }

    #[test]
    fn bright_colors() {
        assert_eq!(ok("bright-black"), Style::new().fg(Color::DarkGray));
        assert_eq!(ok("bright-red"), Style::new().fg(Color::LightRed));
        assert_eq!(ok("bright-green"), Style::new().fg(Color::LightGreen));
        assert_eq!(ok("bright-yellow"), Style::new().fg(Color::LightYellow));
        assert_eq!(ok("bright-blue"), Style::new().fg(Color::LightBlue));
        assert_eq!(ok("bright-magenta"), Style::new().fg(Color::LightMagenta));
        assert_eq!(ok("bright-cyan"), Style::new().fg(Color::LightCyan));
        assert_eq!(ok("bright-white"), Style::new().fg(Color::White));
    }

    #[test]
    fn palette_and_rgb() {
        assert_eq!(ok("color0"), Style::new().fg(Color::Indexed(0)));
        assert_eq!(ok("color255"), Style::new().fg(Color::Indexed(255)));
        assert_eq!(ok("color007"), Style::new().fg(Color::Indexed(7)));
        assert_eq!(ok("#ff8700"), Style::new().fg(Color::Rgb(0xff, 0x87, 0)));
        assert_eq!(ok("#FFaa01"), Style::new().fg(Color::Rgb(0xff, 0xaa, 1)));
    }

    #[test]
    fn attributes() {
        assert_eq!(ok("bold"), Style::new().add_modifier(Modifier::BOLD));
        assert_eq!(ok("dim"), Style::new().add_modifier(Modifier::DIM));
        assert_eq!(ok("italic"), Style::new().add_modifier(Modifier::ITALIC));
        assert_eq!(
            ok("underline"),
            Style::new().add_modifier(Modifier::UNDERLINED)
        );
        assert_eq!(ok("reverse"), Style::new().add_modifier(Modifier::REVERSED));
        assert_eq!(ok("blink"), Style::new().add_modifier(Modifier::SLOW_BLINK));
        assert_eq!(
            ok("bold underline bold"),
            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        );
    }

    #[test]
    fn combinations_in_any_order() {
        let expected = Style::new()
            .fg(Color::Gray)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD);
        assert_eq!(ok("bold white on red"), expected);
        assert_eq!(ok("white bold on red"), expected);
        assert_eq!(ok("on red white bold"), expected);
        assert_eq!(ok("  on\tred   bold white "), expected);

        assert_eq!(
            ok("color245 on color17"),
            Style::new().fg(Color::Indexed(245)).bg(Color::Indexed(17))
        );
        assert_eq!(ok("on #000000"), Style::new().bg(Color::Rgb(0, 0, 0)));
    }

    #[test]
    fn errors() {
        assert_eq!(err("purple"), StyleError::UnknownWord("purple".into()));
        assert_eq!(err("Red"), StyleError::UnknownWord("Red".into()));
        assert_eq!(err("bright"), StyleError::UnknownWord("bright".into()));
        assert_eq!(
            err("red,,bold"),
            StyleError::UnknownWord("red,,bold".into())
        );
        assert_eq!(err("color256"), StyleError::InvalidColor("color256".into()));
        assert_eq!(err("color"), StyleError::InvalidColor("color".into()));
        assert_eq!(err("color+1"), StyleError::InvalidColor("color+1".into()));
        assert_eq!(err("color-1"), StyleError::InvalidColor("color-1".into()));
        assert_eq!(err("#fff"), StyleError::InvalidColor("#fff".into()));
        assert_eq!(err("#ff87001"), StyleError::InvalidColor("#ff87001".into()));
        assert_eq!(err("#gg0000"), StyleError::InvalidColor("#gg0000".into()));
        assert_eq!(err("#+f0000"), StyleError::InvalidColor("#+f0000".into()));
        assert_eq!(err("#ff00é"), StyleError::InvalidColor("#ff00é".into()));
        assert_eq!(
            err("red blue"),
            StyleError::DuplicateForeground("blue".into())
        );
        assert_eq!(
            err("on red on blue"),
            StyleError::DuplicateBackground("blue".into())
        );
        assert_eq!(err("red on"), StyleError::MissingBackground);
        assert_eq!(err("on bold"), StyleError::NotABackground("bold".into()));
        assert_eq!(err("on on"), StyleError::NotABackground("on".into()));
        assert_eq!(
            err("on color300"),
            StyleError::InvalidColor("color300".into())
        );
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            err("purple").to_string(),
            "unknown word \"purple\", expected an attribute, a colour or \"on\""
        );
        assert_eq!(
            err("color256").to_string(),
            "invalid colour \"color256\", expected \"color0\" to \"color255\" or \"#rrggbb\""
        );
    }
}
