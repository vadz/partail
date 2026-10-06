# partail

`partail` ("parallel tail") follows several log files at once, each in its own
window of a terminal UI, colouring their lines using regular expressions.

It is inspired by [multitail](https://github.com/folkertvanheusden/multitail)
and can replace it for its most common use, but is written in safe Rust and
designed to cope with untrusted input: invalid UTF-8, terminal escape
sequences and other control characters in the log files are shown visibly
instead of being passed to the terminal.

**Status:** usable, but still under development: the configuration format may
change.


## Features

- Any number of files shown in windows stacked vertically, each with its own
  status line.
- Files are followed by name, like `tail -F` does: rotation (by renaming or
  by copying and truncating), deletion and creation of the file are handled.
- Parts of the lines can be removed, e.g. the date or host name in syslog
  lines, and the remaining text coloured using regular expressions, with the
  same precedence rules as multitail.
- Scrolling back through the lines kept in memory, searching them for a
  regular expression and showing them without wrapping, with horizontal
  scrolling.
- Configurable status lines.
- Low resource usage: the files are watched using inotify and the screen is
  redrawn only when something changes.


## Building

Requires Rust 1.88 or later and Linux (inotify is used for watching the
files).

```sh
cargo build --release
```

The binary is then `target/release/partail`.


## Usage

```
partail [-c CONFIG] [FILE...]
```

Without arguments, `partail` shows the windows defined in its configuration
file, which is `$XDG_CONFIG_HOME/partail/config.toml`, i.e. usually
`~/.config/partail/config.toml`, or the file given with `-c`.

Files given on the command line are shown instead of the windows from the
configuration file, without any colouring, e.g. `partail a.log b.log`.

When starting, only enough lines to fill the screen are read from the end of
the existing files. Files created later are read entirely.

### Keys

| Key | Action |
|---|---|
| `q`, `Ctrl-C` | Quit |
| `Tab`, `Shift-Tab` | Focus the next/previous window (its status line is shown in bold) |
| `Up`, `Down` | Scroll the focused window by one line |
| `PgUp`, `PgDn` | Scroll the focused window by a page |
| `Home` | Show the oldest lines kept |
| `End` | Show the most recent lines and follow the file again |
| `w` | Toggle wrapping long lines in the focused window |
| `Left`, `Right` | Scroll horizontally by half of the window width, when not wrapping |
| `/` | Search for a regular expression |
| `n`, `N` | Go to the next older/newer match |
| `Esc` | Stop highlighting the search matches |
| `Ctrl-L` | Redraw the screen |

Scrolling is done by whole lines: a wrapped line taller than the window can be
read entirely only after turning wrapping off. While scrolled, the view stays
on the same lines when new ones arrive.

### Searching

`/` shows a prompt in the status line of the focused window. `Enter`
searches for the entered regular expression, `Esc` cancels the search and
`Up` and `Down` recall the previously used patterns (the last 20 of them are
kept while `partail` is running). Entering an empty pattern repeats the last
search.

The search is case-insensitive unless the pattern contains upper case letters.
It starts from the end of the bottom line of the window and goes towards the
older lines, scrolling to show the match at the bottom of the window. `n` and
`N` then go to the previous and next matches. All matches are highlighted in
all windows and the current one, from which the next search starts, is also
shown in bold and underlined.

The search uses the text as shown, i.e. after removing the stripped parts and
replacing the special characters (see below). The syntax of the regular
expressions is that of the [regex crate](https://docs.rs/regex/latest/regex/#syntax).


## Configuration

The configuration file uses [TOML](https://toml.io/) format. Here is an
example showing all the available options:

```toml
# Number of lines kept in memory for each window (default: 100000).
scrollback = 100000

# Windows are shown in the order in which they're defined.
[[window]]
file = "/var/log/syslog"
# Height in lines, not counting the status line. Windows without height
# share the space left by those with a fixed one.
height = 40
# Scheme defined below, optional.
scheme = "syslog"
# Whether long lines are wrapped (default) or truncated initially.
wrap = true

[[window]]
file = "/var/log/exim4/mainlog"
scheme = "exim"

[scheme.syslog]
# Parts of the line removed before colouring it, here the date and the host
# name, keeping the time.
strip = [ { regex = '^(\S+ +\d+ )\S+( \S+)', groups = true } ]

# Colouring rules: the first rule colouring a character takes precedence over
# all the subsequent ones.
[[scheme.syslog.rule]]
regex = 'segfault at'
style = "bold red"

[[scheme.syslog.rule]]
regex = '^..:..:.. (CRON\[[0-9]+\]):'
groups = true
style = "bright-black"

[[scheme.syslog.rule]]
regex = 'error'
style = "red"

[scheme.exim]
strip = [ { regex = '^\S+ ' } ]

[[scheme.exim.rule]]
regex = '<= (\S+)'
groups = true
style = "cyan"

[status]
left = "{file}"
center = "{status} {search}"
right = "{scroll}{hscroll} {lines}L {size:compact} {time}"
```

Any error in the configuration file, such as an unknown option, an invalid
regular expression or style, is reported with its location when starting.

### Schemes

A scheme defines how the lines of the windows using it are processed:

1. All `strip` regexes are matched against the line and the text matched by
   any of them is removed. With `groups = true`, only the parts matched by
   the capture groups are removed and not the entire match.
2. The `rule` regexes are matched against the remaining text, in order, and
   each character gets the style of the first rule matching it. With
   `groups = true`, only the capture groups are styled, otherwise the whole
   match is. A rule with an empty style, `""`, doesn't change the appearance
   of the text, but prevents the subsequent rules from styling it.

All matches in the line are used, not just the first one. Both kinds of
regexes use the [regex crate syntax](https://docs.rs/regex/latest/regex/#syntax).

### Styles

A style is a list of space-separated words, in any order: attributes, at most
one foreground colour and `on` followed by a background colour, e.g.
`"bold red"`, `"white on blue"` or `"underline #ff8700"`.

The attributes are `bold`, `dim`, `italic`, `underline`, `reverse` and
`blink`. The colours can be specified as:

| Syntax | Meaning |
|---|---|
| `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white` | Standard ANSI colours 0 to 7 |
| `bright-black` ... `bright-white` | Bright ANSI colours 8 to 15 |
| `color0` ... `color255` | Colour from the 256-colour palette |
| `#rrggbb` | True colour |
| `default` | The default terminal colour |

The colours are used as is, so the terminal must support them: using the
palette or true colours with a terminal which doesn't may give unexpected
results.

### Status line

The status line of each window consists of the left, centre and right parts,
defined by the `left`, `center` and `right` options in the `[status]`
section. Each of them is optional and uses the default shown in the example
above if not specified. If the parts don't fit together, the left one is
truncated first, then the centre one.

The following placeholders can be used in them:

| Placeholder | Replaced with |
|---|---|
| `{file}` | Full path of the file |
| `{name}` | File name without the directory |
| `{lines}` | Number of lines read since starting |
| `{size}` | File size in human-readable form, e.g. `1.6 KB` |
| `{size:compact}` | File size in compact form, e.g. `1.6K` |
| `{bytes}` | File size in bytes |
| `{time}` | Last modification time of the file, as `2026-10-05 17:20:59` |
| `{time:FORMAT}` | Last modification time in the given [strftime format](https://docs.rs/jiff/latest/jiff/fmt/strtime/index.html), e.g. `{time:%H:%M}` |
| `{status}` | Empty normally, `missing` or `error: ...` otherwise |
| `{scroll}` | Empty normally, `↑N` when scrolled back, where N is the number of lines below the view |
| `{hscroll}` | Empty normally, `→N` when scrolled horizontally by N columns |
| `{search}` | Empty normally, `/pattern` when searching |

Use `{{` and `}}` for literal braces.


## Handling of untrusted input

Log files can contain anything, including data controlled by an attacker,
so `partail` never passes their contents to the terminal as is:

- Invalid UTF-8 bytes are shown as `\xNN`.
- Control characters are shown in caret notation, e.g. `^[` for ESC or `^M`
  for the carriage return at the end of the lines of files using DOS line
  endings, and C1 control characters as `\u{NN}`.
- Invisible characters which can change how the surrounding text is shown,
  such as bidirectional text controls or zero-width spaces, are shown as
  `\u{NNNN}`.

This special text is shown in reverse video, which takes precedence over the
colouring rules, and is never removed by the `strip` regexes.

Lines longer than 64 KiB are split into several parts, the last character of
all but the last part being a `\` shown in reverse video. Similarly,
`--- file replaced ---` and `--- file truncated ---` lines are shown when the
file is rotated.

Only regular files are followed: following a FIFO or a device is refused, as
reading from them could block or never end.


## Differences from multitail

- The configuration file uses a different format and options.
- Byte ranges can't be removed from the lines (multitail `-kr` option), but
  regexes can be used for the same purpose, more robustly.
- multitail stops colouring the capture groups of a rule after the first one
  which didn't participate in the match, while `partail` colours all those
  that did.
- Regular expressions use the [regex crate](https://docs.rs/regex/) syntax
  instead of POSIX extended regular expressions. Most expressions work the
  same, but the matches of alternatives can be different, as the leftmost
  alternative matching wins instead of the longest one.
- The colours are not limited to the standard 8 ones.
- Windows show the most recent lines at their bottom.
- Many multitail features are not supported, e.g. following the output of
  commands, merging several files in one window, filtering lines or
  side-by-side layout.


## Limitations

- Changes to the files on network file systems done by other machines may
  only be noticed after up to 5 seconds, as they're not reported by inotify.
- If a file is truncated and then grows beyond its previous size before
  `partail` notices it, the truncation is not detected.
- The terminal is not restored if `partail` is killed by a signal, use
  `reset` to fix it if this happens.


## License

`partail` is distributed under the [MIT license](LICENSE).
