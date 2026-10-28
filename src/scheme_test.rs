//! Testing a scheme by outputting the lines processed by it to the terminal,
//! without using the full screen UI.

use std::io::{self, BufRead, Write};

use anyhow::Context;
use ratatui::backend::IntoCrossterm;
use ratatui::style::Style;

use partail::config::Scheme;
use partail::line::{self, Escape, Explanation};

/// Outputs the given lines, or the lines read from the standard input if
/// there are none, processed using the scheme. If `verbose` is true, also
/// outputs what each strip pattern and rule matched in them.
pub fn run(scheme: &Scheme, escape: Escape, verbose: bool, lines: &[&[u8]]) -> anyhow::Result<()> {
    let tester = Tester {
        scheme,
        escape,
        verbose,
    };
    let mut out = io::stdout().lock();

    let result = if lines.is_empty() {
        let mut result = Ok(());
        for raw in io::stdin().lock().split(b'\n') {
            let raw = raw.context("failed to read standard input")?;
            result = tester.write_line(&mut out, &raw);
            if result.is_err() {
                break;
            }
        }
        result
    } else {
        lines
            .iter()
            .try_for_each(|raw| tester.write_line(&mut out, raw))
    };

    match result.and_then(|()| out.flush()) {
        // Not an error when the output is piped to e.g. head.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result.context("failed to write to standard output"),
    }
}

struct Tester<'a> {
    scheme: &'a Scheme,
    escape: Escape,
    verbose: bool,
}

impl Tester<'_> {
    fn write_line(&self, out: &mut impl Write, raw: &[u8]) -> io::Result<()> {
        let line = line::process(raw, self.scheme, self.escape);
        for (text, style) in line.segments() {
            write_styled(out, text, style)?;
        }
        writeln!(out)?;

        if self.verbose {
            self.write_explanation(out, &line::explain(raw, self.scheme, self.escape))?;
        }
        Ok(())
    }

    fn write_explanation(&self, out: &mut impl Write, e: &Explanation) -> io::Result<()> {
        let mut matched = false;

        for (n, (pattern, ranges)) in self.scheme.strip.iter().zip(&e.strips).enumerate() {
            if ranges.is_empty() {
                continue;
            }
            matched = true;
            write!(out, "  strip {} '{}':", n + 1, pattern.regex().as_str())?;
            for range in ranges {
                write!(out, " \"{}\"", e.sanitised.get(range.clone()).unwrap_or(""))?;
            }
            writeln!(out)?;
        }

        for (n, (rule, matches)) in self.scheme.rules.iter().zip(&e.rules).enumerate() {
            if matches.is_empty() {
                continue;
            }
            matched = true;
            write!(out, "  rule {} '{}':", n + 1, rule.pattern.regex().as_str())?;
            for m in matches {
                write!(out, " \"")?;
                write_styled(
                    out,
                    e.stripped.get(m.range.clone()).unwrap_or(""),
                    rule.style,
                )?;
                write!(out, "\"")?;
                if m.claimed == 0 {
                    write!(out, " (overridden)")?;
                } else if m.claimed < m.range.len() {
                    write!(out, " (partly overridden)")?;
                }
            }
            writeln!(out)?;
        }

        if !matched {
            writeln!(out, "  no matches")?;
        }
        Ok(())
    }
}

/// Writes the text using ANSI escape sequences for its style.
fn write_styled(out: &mut impl Write, text: &str, style: Style) -> io::Result<()> {
    write!(out, "{}", style.into_crossterm().apply(text))
}

#[cfg(test)]
mod tests {
    use partail::config::Config;

    use super::*;

    /// Returns the output for the line with the given scheme from the
    /// configuration, with the escape sequences shown visibly.
    fn output(config: &str, verbose: bool, raw: &[u8]) -> String {
        let config = Config::parse(config).unwrap();
        let tester = Tester {
            scheme: &config.schemes["s"],
            escape: config.escape,
            verbose,
        };
        let mut out = Vec::new();
        tester.write_line(&mut out, raw).unwrap();
        String::from_utf8(out).unwrap().replace('\x1b', "ESC")
    }

    const CONFIG: &str = r#"
[scheme.s]
strip = [ { regex = '^\S+ ' } ]

[[scheme.s.rule]]
regex = 'b+'
style = "red"

[[scheme.s.rule]]
regex = 'ab'
style = "bold"

[[scheme.s.rule]]
regex = '(a)(x)?'
style = ""
"#;

    #[test]
    fn styled() {
        assert_eq!(
            output(CONFIG, false, b"date abbc\x01"),
            "ESC[1ma\
             ESC[0mESC[38;5;1mbbESC[39m\
             c\
             ESC[7m^AESC[0m\n"
        );
    }

    #[test]
    fn verbose() {
        assert_eq!(
            output(CONFIG, true, b"date abba"),
            "ESC[1maESC[0mESC[38;5;1mbbESC[39ma\n  \
             strip 1 '^\\S+ ': \"date \"\n  \
             rule 1 'b+': \"ESC[38;5;1mbbESC[39m\"\n  \
             rule 2 'ab': \"ESC[1mabESC[0m\" (partly overridden)\n  \
             rule 3 '(a)(x)?': \"a\" (overridden) \"a\"\n"
        );
        assert_eq!(
            output(CONFIG, true, b"date c"),
            "c\n  strip 1 '^\\S+ ': \"date \"\n"
        );
        assert_eq!(output(CONFIG, true, b"c"), "c\n  no matches\n");
    }
}
