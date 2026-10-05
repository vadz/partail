//! Format of the status lines.
//!
//! The status line of each window consists of 3 parts, aligned to the left,
//! centred and aligned to the right, each defined by a template containing
//! placeholders such as `{file}` or `{size}` which are replaced with the
//! information about the followed file. Literal braces are written as `{{`
//! and `}}`.

use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::follow::Status;

/// Time format used by `{time}` if none is given explicitly.
const DEFAULT_TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Text used for the values which are unknown, e.g. the size of a missing
/// file.
const UNKNOWN: &str = "-";

/// The templates of the 3 parts of the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusFormat {
    pub left: Template,
    pub center: Template,
    pub right: Template,
}

impl StatusFormat {
    pub const DEFAULT_LEFT: &str = "{file}";
    pub const DEFAULT_CENTER: &str = "{status} {search}";
    pub const DEFAULT_RIGHT: &str = "{scroll} {lines}L {size:compact} {time}";
}

impl Default for StatusFormat {
    fn default() -> Self {
        let parse = |s| Template::parse(s).expect("default status format must be valid");
        Self {
            left: parse(Self::DEFAULT_LEFT),
            center: parse(Self::DEFAULT_CENTER),
            right: parse(Self::DEFAULT_RIGHT),
        }
    }
}

/// Error returned by [`Template::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError(String);

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TemplateError {}

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    File,
    Name,
    Lines,
    /// Number of lines below the view, if scrolled.
    Scroll,
    /// The current search pattern, if any.
    Search,
    /// Size in human-readable units, compact or not.
    Size {
        compact: bool,
    },
    Bytes,
    /// Last modification time with the given format.
    Time(String),
    Status,
}

/// The information which can be shown in the status line.
#[derive(Debug, Clone, Copy)]
pub struct Values<'a> {
    pub path: &'a Path,
    /// Number of lines read so far.
    pub lines: u64,
    /// Number of lines below the view, 0 if following the end of the file.
    pub scroll: usize,
    /// The current search pattern, if any.
    pub search: Option<&'a str>,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub status: &'a Status,
}

impl Template {
    /// Parses a template, checking that all its placeholders are valid.
    pub fn parse(s: &str) -> Result<Self, TemplateError> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    let rest = chars.as_str();
                    if let Some(after) = rest.strip_prefix('{') {
                        text.push('{');
                        chars = after.chars();
                        continue;
                    }
                    let Some((placeholder, after)) = rest.split_once('}') else {
                        return Err(TemplateError(format!(
                            "unterminated placeholder \"{{{rest}\""
                        )));
                    };
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(parse_placeholder(placeholder)?);
                    chars = after.chars();
                }
                '}' => {
                    let rest = chars.as_str();
                    let Some(after) = rest.strip_prefix('}') else {
                        return Err(TemplateError(
                            "unexpected \"}\", use \"}}\" for a literal brace".to_owned(),
                        ));
                    };
                    text.push('}');
                    chars = after.chars();
                }
                _ => text.push(c),
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Self { parts })
    }

    /// Returns the text of the template with the given values.
    ///
    /// The result is raw bytes, as the file name can be anything, and must be
    /// sanitised before being shown.
    pub fn render(&self, values: &Values) -> Vec<u8> {
        let mut out = Vec::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => out.extend_from_slice(text.as_bytes()),
                Part::File => out.extend_from_slice(values.path.as_os_str().as_bytes()),
                Part::Name => {
                    let name = values.path.file_name().unwrap_or(values.path.as_os_str());
                    out.extend_from_slice(name.as_bytes());
                }
                Part::Lines => out.extend_from_slice(values.lines.to_string().as_bytes()),
                Part::Search => {
                    if let Some(pattern) = values.search {
                        out.push(b'/');
                        out.extend_from_slice(pattern.as_bytes());
                    }
                }
                Part::Scroll => {
                    if values.scroll > 0 {
                        out.extend_from_slice(format!("\u{2191}{}", values.scroll).as_bytes());
                    }
                }
                Part::Size { compact } => {
                    let size = values
                        .size
                        .map_or_else(|| UNKNOWN.to_owned(), |n| human_size(n, *compact));
                    out.extend_from_slice(size.as_bytes());
                }
                Part::Bytes => {
                    let bytes = values
                        .size
                        .map_or_else(|| UNKNOWN.to_owned(), |n| n.to_string());
                    out.extend_from_slice(bytes.as_bytes());
                }
                Part::Time(format) => {
                    let time = values
                        .modified
                        .and_then(|t| format_time(t, format, &TimeZone::system()))
                        .unwrap_or_else(|| UNKNOWN.to_owned());
                    out.extend_from_slice(time.as_bytes());
                }
                Part::Status => match values.status {
                    Status::Following => {}
                    Status::Missing => out.extend_from_slice(b"missing"),
                    Status::Error(e) => {
                        out.extend_from_slice(b"error: ");
                        out.extend_from_slice(e.as_bytes());
                    }
                },
            }
        }
        out
    }
}

fn parse_placeholder(placeholder: &str) -> Result<Part, TemplateError> {
    let (name, arg) = match placeholder.split_once(':') {
        Some((name, arg)) => (name, Some(arg)),
        None => (placeholder, None),
    };

    let part = match name {
        "file" => Part::File,
        "name" => Part::Name,
        "lines" => Part::Lines,
        "scroll" => Part::Scroll,
        "search" => Part::Search,
        "size" => {
            let compact = match arg {
                None => false,
                Some("compact") => true,
                Some(arg) => {
                    return Err(TemplateError(format!(
                        "invalid size format \"{arg}\", only \"compact\" is supported"
                    )));
                }
            };
            return Ok(Part::Size { compact });
        }
        "bytes" => Part::Bytes,
        "status" => Part::Status,
        "time" => {
            let format = arg.unwrap_or(DEFAULT_TIME_FORMAT);
            // Check the format now, to avoid failing to format the time later.
            if let Err(e) =
                jiff::fmt::strtime::format(format, &Timestamp::UNIX_EPOCH.to_zoned(TimeZone::UTC))
            {
                return Err(TemplateError(format!(
                    "invalid time format \"{format}\": {e}"
                )));
            }
            return Ok(Part::Time(format.to_owned()));
        }
        _ => {
            return Err(TemplateError(format!(
                "unknown placeholder \"{{{placeholder}}}\", expected one of {{file}}, {{name}}, \
                 {{lines}}, {{scroll}}, {{search}}, {{size}}, {{bytes}}, {{time}} or \
                 {{status}}"
            )));
        }
    };

    if arg.is_some() {
        return Err(TemplateError(format!(
            "placeholder \"{{{name}}}\" doesn't take an argument"
        )));
    }
    Ok(part)
}

/// Formats the time in the given time zone, returning `None` if this fails.
fn format_time(time: SystemTime, format: &str, tz: &TimeZone) -> Option<String> {
    let zoned = Timestamp::try_from(time).ok()?.to_zoned(tz.clone());
    jiff::fmt::strtime::format(format, &zoned).ok()
}

/// Formats the size using the most appropriate unit, with one decimal digit
/// for the values less than 10, e.g. "512 B", "1.6 KB" or "23 MB", or, in
/// compact form, as `ls -h` does, e.g. "512B", "1.6K" or "23M".
fn human_size(size: u64, compact: bool) -> String {
    const UNITS: [char; 5] = ['K', 'M', 'G', 'T', 'P'];

    let mut value = size as f64;
    let mut unit = None;
    for u in UNITS {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = Some(u);
    }

    let number = match unit {
        None => size.to_string(),
        Some(_) if value < 10.0 => format!("{value:.1}"),
        Some(_) => format!("{value:.0}"),
    };
    match (unit, compact) {
        (None, false) => format!("{number} B"),
        (None, true) => format!("{number}B"),
        (Some(unit), false) => format!("{number} {unit}B"),
        (Some(unit), true) => format!("{number}{unit}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn render(template: &str, values: &Values) -> String {
        let bytes = Template::parse(template).unwrap().render(values);
        String::from_utf8(bytes).unwrap()
    }

    fn values(status: &Status) -> Values<'_> {
        Values {
            path: Path::new("/var/log/syslog"),
            lines: 42,
            scroll: 0,
            search: None,
            size: Some(1677),
            modified: None,
            status,
        }
    }

    fn parse_err(template: &str) -> String {
        Template::parse(template).unwrap_err().to_string()
    }

    #[test]
    fn placeholders() {
        let ok = Status::Following;
        let v = values(&ok);
        assert_eq!(render("", &v), "");
        assert_eq!(render("plain text", &v), "plain text");
        assert_eq!(render("{file}", &v), "/var/log/syslog");
        assert_eq!(render("{name}", &v), "syslog");
        assert_eq!(render("{lines} lines", &v), "42 lines");
        assert_eq!(render("{size}", &v), "1.6 KB");
        assert_eq!(render("{size:compact}", &v), "1.6K");
        assert_eq!(render("{bytes}", &v), "1677");
        assert_eq!(render("[{status}]", &v), "[]");
        assert_eq!(render("[{scroll}]", &v), "[]");
        let scrolled = Values { scroll: 123, ..v };
        assert_eq!(render("[{scroll}]", &scrolled), "[\u{2191}123]");
        assert_eq!(render("[{search}]", &v), "[]");
        let searching = Values {
            search: Some("fo+"),
            ..v
        };
        assert_eq!(render("[{search}]", &searching), "[/fo+]");
        assert_eq!(
            render(" {name}: {lines}/{size} ", &v),
            " syslog: 42/1.6 KB "
        );
    }

    #[test]
    fn unknown_values() {
        let ok = Status::Following;
        let v = Values {
            size: None,
            modified: None,
            ..values(&ok)
        };
        assert_eq!(render("{size} {bytes} {time} {time:%H}", &v), "- - - -");
    }

    #[test]
    fn status() {
        assert_eq!(render("{status}", &values(&Status::Missing)), "missing");
        let error = Status::Error("permission denied".to_owned());
        assert_eq!(
            render("{status}", &values(&error)),
            "error: permission denied"
        );
    }

    #[test]
    fn escaped_braces() {
        let ok = Status::Following;
        let v = values(&ok);
        assert_eq!(render("{{file}}", &v), "{file}");
        assert_eq!(render("{{{name}}}", &v), "{syslog}");
        assert_eq!(render("}}{{", &v), "}{");
    }

    #[test]
    fn non_utf8_file_name() {
        use std::ffi::OsStr;

        let ok = Status::Following;
        let v = Values {
            path: Path::new(OsStr::from_bytes(b"/tmp/\xff.log")),
            ..values(&ok)
        };
        assert_eq!(Template::parse("{name}").unwrap().render(&v), b"\xff.log");
    }

    #[test]
    fn errors() {
        assert_eq!(
            parse_err("{nope}"),
            "unknown placeholder \"{nope}\", expected one of {file}, {name}, {lines}, \
             {scroll}, {search}, {size}, {bytes}, {time} or {status}"
        );
        assert_eq!(parse_err("{file"), "unterminated placeholder \"{file\"");
        assert_eq!(
            parse_err("a } b"),
            "unexpected \"}\", use \"}}\" for a literal brace"
        );
        assert_eq!(
            parse_err("{size:x}"),
            "invalid size format \"x\", only \"compact\" is supported"
        );
        assert_eq!(
            parse_err("{lines:x}"),
            "placeholder \"{lines}\" doesn't take an argument"
        );
        assert!(parse_err("{time:%H %}").starts_with("invalid time format \"%H %\": "));
        assert!(parse_err("{}").starts_with("unknown placeholder \"{}\""));
    }

    #[test]
    fn time() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_214_331);
        let utc = TimeZone::UTC;
        assert_eq!(
            format_time(t, DEFAULT_TIME_FORMAT, &utc).as_deref(),
            Some("2026-10-05 15:32:11")
        );
        assert_eq!(format_time(t, "%H:%M", &utc).as_deref(), Some("15:32"));

        let paris = TimeZone::get("Europe/Paris").unwrap();
        assert_eq!(
            format_time(t, "%H:%M %Z", &paris).as_deref(),
            Some("17:32 CEST")
        );

        // With the local time zone, just check that something is produced.
        let ok = Status::Following;
        let v = Values {
            modified: Some(t),
            ..values(&ok)
        };
        assert_eq!(render("{time:%Y}", &v), "2026");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0, false), "0 B");
        assert_eq!(human_size(1023, false), "1023 B");
        assert_eq!(human_size(1024, false), "1.0 KB");
        assert_eq!(human_size(1677, false), "1.6 KB");
        assert_eq!(human_size(10 * 1024, false), "10 KB");
        assert_eq!(human_size(1024 * 1024 - 1, false), "1024 KB");
        assert_eq!(human_size(1024 * 1024, false), "1.0 MB");
        assert_eq!(human_size(23 * 1024 * 1024 + 500_000, false), "23 MB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024, false), "5.0 GB");
        assert_eq!(human_size(u64::MAX, false), "16384 PB");

        assert_eq!(human_size(0, true), "0B");
        assert_eq!(human_size(1023, true), "1023B");
        assert_eq!(human_size(1677, true), "1.6K");
        assert_eq!(human_size(23 * 1024 * 1024 + 500_000, true), "23M");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024, true), "5.0G");
    }

    #[test]
    fn default_format_is_valid() {
        let format = StatusFormat::default();
        let ok = Status::Following;
        let v = values(&ok);
        assert_eq!(format.left.render(&v), b"/var/log/syslog");
        assert_eq!(format.center.render(&v), b" ");
        assert_eq!(format.right.render(&v), b" 42L 1.6K -");
    }
}
