//! Configuration file loading and validation.
//!
//! The configuration is a TOML file defining the windows to show and the
//! schemes used to strip and colour their lines. It is fully checked when it
//! is loaded, so that nothing can fail later because of it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::num::{NonZeroU16, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use ratatui::style::Style;

use crate::pattern::Pattern;
use crate::style;

/// Number of lines kept in each window if not specified in the configuration.
pub const DEFAULT_SCROLLBACK: NonZeroUsize = NonZeroUsize::new(100_000).expect("non-zero");

/// Validated configuration.
#[derive(Debug)]
pub struct Config {
    /// Maximal number of lines kept in each window.
    pub scrollback: NonZeroUsize,
    pub windows: Vec<Window>,
}

/// A window showing one file.
#[derive(Debug)]
pub struct Window {
    pub file: PathBuf,
    /// Height in lines, not counting the status line, or `None` for sharing
    /// the space left by the windows with a fixed height.
    pub height: Option<NonZeroU16>,
    /// Scheme to use, which is empty if the window doesn't specify any.
    pub scheme: Arc<Scheme>,
}

/// How to process the lines of a window: first remove the parts matched by
/// any `strip` pattern, then style the remaining text using `rules`.
#[derive(Debug, Default)]
pub struct Scheme {
    pub strip: Vec<Pattern>,
    /// The first rule styling a character takes precedence over later ones.
    pub rules: Vec<Rule>,
}

/// A colour rule.
#[derive(Debug)]
pub struct Rule {
    pub pattern: Pattern,
    pub style: Style,
}

/// The configuration as found in the file, before validation.
mod raw {
    use std::collections::BTreeMap;
    use std::num::{NonZeroU16, NonZeroUsize};
    use std::path::PathBuf;

    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Config {
        pub scrollback: Option<NonZeroUsize>,
        #[serde(default)]
        pub window: Vec<Window>,
        #[serde(default)]
        pub scheme: BTreeMap<String, Scheme>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Window {
        pub file: PathBuf,
        pub height: Option<NonZeroU16>,
        pub scheme: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Scheme {
        #[serde(default)]
        pub strip: Vec<Strip>,
        #[serde(default)]
        pub rule: Vec<Rule>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Strip {
        pub regex: String,
        #[serde(default)]
        pub groups: bool,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Rule {
        pub regex: String,
        #[serde(default)]
        pub groups: bool,
        pub style: String,
    }
}

impl Config {
    /// Loads the configuration from the given file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration file \"{}\"", path.display()))?;
        Self::parse(&text)
            .with_context(|| format!("invalid configuration file \"{}\"", path.display()))
    }

    /// Parses the configuration from the contents of a configuration file.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: raw::Config = toml::from_str(text)?;

        let mut schemes = BTreeMap::new();
        for (name, scheme) in raw.scheme {
            let scheme = compile_scheme(scheme).with_context(|| format!("scheme \"{name}\""))?;
            schemes.insert(name, Arc::new(scheme));
        }

        let no_scheme = Arc::new(Scheme::default());
        let windows = raw
            .window
            .into_iter()
            .enumerate()
            .map(|(n, w)| {
                compile_window(w, &schemes, &no_scheme).with_context(|| format!("window {}", n + 1))
            })
            .collect::<anyhow::Result<_>>()?;

        Ok(Self {
            scrollback: raw.scrollback.unwrap_or(DEFAULT_SCROLLBACK),
            windows,
        })
    }
}

fn compile_scheme(raw: raw::Scheme) -> anyhow::Result<Scheme> {
    let strip = raw
        .strip
        .into_iter()
        .enumerate()
        .map(|(n, s)| Pattern::new(&s.regex, s.groups).with_context(|| format!("strip {}", n + 1)))
        .collect::<anyhow::Result<_>>()?;

    let rules = raw
        .rule
        .into_iter()
        .enumerate()
        .map(|(n, r)| compile_rule(r).with_context(|| format!("rule {}", n + 1)))
        .collect::<anyhow::Result<_>>()?;

    Ok(Scheme { strip, rules })
}

fn compile_rule(raw: raw::Rule) -> anyhow::Result<Rule> {
    let pattern = Pattern::new(&raw.regex, raw.groups)?;
    let style =
        style::parse(&raw.style).with_context(|| format!("invalid style \"{}\"", raw.style))?;
    Ok(Rule { pattern, style })
}

fn compile_window(
    raw: raw::Window,
    schemes: &BTreeMap<String, Arc<Scheme>>,
    no_scheme: &Arc<Scheme>,
) -> anyhow::Result<Window> {
    if raw.file.as_os_str().is_empty() {
        bail!("file name must not be empty");
    }

    let scheme = match raw.scheme {
        None => Arc::clone(no_scheme),
        Some(name) => match schemes.get(&name) {
            Some(scheme) => Arc::clone(scheme),
            None => bail!("unknown scheme \"{name}\""),
        },
    };

    Ok(Window {
        file: raw.file,
        height: raw.height,
        scheme,
    })
}

/// Returns the path of the configuration file to use if none is given
/// explicitly, or `None` if it can't be determined.
pub fn default_path() -> Option<PathBuf> {
    default_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Implementation of [`default_path`] taking the values of the environment
/// variables `XDG_CONFIG_HOME` and `HOME`.
fn default_path_from(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    // The XDG specification says relative paths must be ignored.
    let config_dir = match xdg_config_home.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => {
            let home = home.filter(|h| !h.is_empty())?;
            PathBuf::from(home).join(".config")
        }
    };
    Some(config_dir.join("partail").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    fn parse_err(text: &str) -> String {
        match Config::parse(text) {
            Ok(config) => panic!("unexpectedly parsed: {config:?}"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[track_caller]
    fn assert_err_contains(text: &str, expected: &str) {
        let err = parse_err(text);
        assert!(
            err.contains(expected),
            "\"{err}\" doesn't contain \"{expected}\""
        );
    }

    #[test]
    fn empty() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.scrollback, DEFAULT_SCROLLBACK);
        assert!(config.windows.is_empty());
    }

    #[test]
    fn full() {
        let config = Config::parse(
            r#"
scrollback = 500

[[window]]
file = "/var/log/syslog"
height = 40
scheme = "syslog"

[[window]]
file = "/var/log/exim4/mainlog"
scheme = "exim"

[[window]]
file = "plain.log"

[scheme.syslog]
strip = [ { regex = '^(\S+ +\d+ )\S+( \S+)', groups = true } ]

[[scheme.syslog.rule]]
regex = 'vmunix: ([^:]*): segfault at'
groups = true
style = "red bold"

[[scheme.syslog.rule]]
regex = 'error'
style = "white"

[[scheme.syslog.rule]]
regex = 'boring'
style = ""

[scheme.exim]
strip = [ { regex = '^\S+ ' } ]

[scheme.unused]
"#,
        )
        .unwrap();

        assert_eq!(config.scrollback.get(), 500);
        assert_eq!(config.windows.len(), 3);

        let syslog = &config.windows[0];
        assert_eq!(syslog.file, Path::new("/var/log/syslog"));
        assert_eq!(syslog.height.map(NonZeroU16::get), Some(40));
        assert_eq!(syslog.scheme.strip.len(), 1);
        assert!(syslog.scheme.strip[0].groups());
        let rules = &syslog.scheme.rules;
        assert_eq!(rules.len(), 3);
        assert_eq!(
            rules[0].pattern.regex().as_str(),
            "vmunix: ([^:]*): segfault at"
        );
        assert!(rules[0].pattern.groups());
        assert_eq!(
            rules[0].style,
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        assert!(!rules[1].pattern.groups());
        assert_eq!(rules[1].style, Style::new().fg(Color::Gray));
        assert_eq!(rules[2].style, Style::new());

        let exim = &config.windows[1];
        assert_eq!(exim.height, None);
        assert_eq!(exim.scheme.strip.len(), 1);
        assert!(!exim.scheme.strip[0].groups());
        assert!(exim.scheme.rules.is_empty());

        let plain = &config.windows[2];
        assert!(plain.scheme.strip.is_empty());
        assert!(plain.scheme.rules.is_empty());
    }

    #[test]
    fn scheme_shared_between_windows() {
        let config = Config::parse(
            r#"
[[window]]
file = "a"
scheme = "s"

[[window]]
file = "b"
scheme = "s"

[scheme.s]
"#,
        )
        .unwrap();
        assert!(Arc::ptr_eq(
            &config.windows[0].scheme,
            &config.windows[1].scheme
        ));
    }

    #[test]
    fn unknown_keys() {
        assert_err_contains("colour = 1", "unknown field `colour`");
        assert_err_contains("[[window]]\nfile = 'a'\nwidth = 1", "unknown field `width`");
        assert_err_contains("[scheme.s]\nrules = []", "unknown field `rules`");
        assert_err_contains(
            "[[scheme.s.rule]]\nregex = 'a'\nstyle = ''\ncolor = 'red'",
            "unknown field `color`",
        );
        assert_err_contains(
            "[scheme.s]\nstrip = [ { regex = 'a', style = 'red' } ]",
            "unknown field `style`",
        );
    }

    #[test]
    fn missing_keys() {
        assert_err_contains("[[window]]\nheight = 1", "missing field `file`");
        assert_err_contains("[[scheme.s.rule]]\nregex = 'a'", "missing field `style`");
        assert_err_contains("[[scheme.s.rule]]\nstyle = 'red'", "missing field `regex`");
        assert_err_contains(
            "[scheme.s]\nstrip = [ { groups = true } ]",
            "missing field `regex`",
        );
    }

    #[test]
    fn invalid_values() {
        assert_err_contains("scrollback = 0", "nonzero");
        assert_err_contains("scrollback = -1", "scrollback");
        assert_err_contains("[[window]]\nfile = 'a'\nheight = 0", "nonzero");
        assert_err_contains("[[window]]\nfile = 'a'\nheight = 65536", "height");
        assert_err_contains(
            "[[window]]\nfile = ''",
            "window 1: file name must not be empty",
        );
    }

    #[test]
    fn unknown_scheme() {
        assert_err_contains(
            "[[window]]\nfile = 'a'\n[[window]]\nfile = 'b'\nscheme = 'nope'",
            "window 2: unknown scheme \"nope\"",
        );
    }

    #[test]
    fn invalid_rules() {
        let err = parse_err(
            "[[scheme.s.rule]]\nregex = 'a'\nstyle = ''\n[[scheme.s.rule]]\nregex = 'a('\nstyle = ''",
        );
        assert!(
            err.starts_with("scheme \"s\": rule 2: invalid regex: "),
            "{err}"
        );
        assert_eq!(err.matches("unclosed group").count(), 1, "{err}");

        assert_err_contains(
            "[[scheme.s.rule]]\nregex = 'a'\ngroups = true\nstyle = ''",
            "scheme \"s\": rule 1: \"groups = true\" requires a regex with capture groups",
        );
        assert_err_contains(
            "[[scheme.s.rule]]\nregex = 'a'\nstyle = 'purple bold'",
            "scheme \"s\": rule 1: invalid style \"purple bold\": unknown word \"purple\"",
        );
        assert_err_contains(
            "[scheme.s]\nstrip = [ { regex = 'a' }, { regex = '[' } ]",
            "scheme \"s\": strip 2: invalid regex: ",
        );
        assert_err_contains(
            "[scheme.s]\nstrip = [ { regex = 'a', groups = true } ]",
            "scheme \"s\": strip 1: \"groups = true\" requires",
        );
    }

    #[test]
    fn load_errors() {
        let e = Config::load(Path::new("/nonexistent/partail.toml")).unwrap_err();
        assert!(
            format!("{e:#}")
                .starts_with("failed to read configuration file \"/nonexistent/partail.toml\": "),
            "{e:#}"
        );
    }

    #[test]
    fn default_path() {
        let path = |xdg: Option<&str>, home: Option<&str>| {
            default_path_from(xdg.map(OsString::from), home.map(OsString::from))
        };
        let expected = |p: &str| Some(PathBuf::from(p));

        assert_eq!(
            path(Some("/xdg"), Some("/home/u")),
            expected("/xdg/partail/config.toml")
        );
        assert_eq!(
            path(None, Some("/home/u")),
            expected("/home/u/.config/partail/config.toml")
        );
        assert_eq!(
            path(Some(""), Some("/home/u")),
            expected("/home/u/.config/partail/config.toml")
        );
        assert_eq!(
            path(Some("relative"), Some("/home/u")),
            expected("/home/u/.config/partail/config.toml")
        );
        assert_eq!(
            path(Some("/xdg"), None),
            expected("/xdg/partail/config.toml")
        );
        assert_eq!(path(None, None), None);
        assert_eq!(path(None, Some("")), None);
    }
}
