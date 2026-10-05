//! The terminal UI: windows showing the followed files and the event loop.

use std::collections::VecDeque;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};

use partail::config::{Config, Scheme};
use partail::follow::{Event, Follower};
use partail::line::{self, Line};
use partail::status::{StatusFormat, Template, Values};
use partail::wrap::{self, Row};

/// How often the files are checked for changes.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Style of the status lines.
const STATUS_STYLE: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Marker appended to the parts of a too long line.
const LONG_LINE_MARKER: &str = "\\";

/// The whole application state.
pub struct App {
    panes: Vec<Pane>,
    status: StatusFormat,
}

/// A window showing one file.
struct Pane {
    follower: Follower,
    scheme: Arc<Scheme>,
    height: Option<NonZeroU16>,
    /// Lines read from the file, the most recent last.
    lines: VecDeque<Line>,
    /// Maximal number of lines to keep.
    scrollback: usize,
    /// Number of lines read since the start.
    lines_read: u64,
}

impl App {
    /// Creates the application showing the windows from the configuration.
    ///
    /// Only the last `initial_lines` lines of the existing files are shown.
    pub fn new(config: Config, initial_lines: usize) -> Self {
        let panes = config
            .windows
            .into_iter()
            .map(|window| Pane {
                follower: Follower::new(window.file, initial_lines),
                scheme: window.scheme,
                height: window.height,
                lines: VecDeque::new(),
                scrollback: config.scrollback.get(),
                lines_read: 0,
            })
            .collect();
        Self {
            panes,
            status: config.status,
        }
    }

    /// Reads new data from all files.
    fn poll(&mut self) -> PollResult {
        let mut result = PollResult::default();
        for pane in &mut self.panes {
            result.changed |= pane.poll();
            result.has_more |= pane.follower.has_more();
        }
        result
    }

    fn draw(&self, frame: &mut Frame) {
        let constraints = self.panes.iter().map(|pane| match pane.height {
            // Account for the status line.
            Some(height) => Constraint::Length(height.get().saturating_add(1)),
            None => Constraint::Fill(1),
        });
        let areas = Layout::vertical(constraints).split(frame.area());
        for (pane, area) in self.panes.iter().zip(areas.iter()) {
            pane.draw(*area, frame.buffer_mut(), &self.status);
        }
    }
}

/// The result of [`App::poll`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PollResult {
    /// True if anything shown on screen may have changed.
    changed: bool,
    /// True if there is more data to read immediately.
    has_more: bool,
}

impl Pane {
    /// Reads new data from the file and returns true if anything shown in
    /// this window, including its status line, may have changed.
    fn poll(&mut self) -> bool {
        let state = |f: &Follower| {
            (
                f.status().clone(),
                f.size(),
                f.modified(),
                f.partial().len(),
            )
        };

        let before = state(&self.follower);
        let events = self.follower.poll();
        let changed = !events.is_empty() || state(&self.follower) != before;
        for event in events {
            self.handle(event);
        }
        changed
    }

    fn handle(&mut self, event: Event) {
        let line = match event {
            Event::Line(bytes) => {
                self.lines_read += 1;
                line::process(&bytes, &self.scheme)
            }
            Event::LongLinePart(bytes) => {
                let mut line = line::process(&bytes, &self.scheme);
                line.push_marker(LONG_LINE_MARKER);
                line
            }
            Event::Replaced => Line::marker("--- file replaced ---"),
            Event::Truncated => Line::marker("--- file truncated ---"),
        };

        self.lines.push_back(line);
        while self.lines.len() > self.scrollback {
            self.lines.pop_front();
        }
    }

    fn draw(&self, area: Rect, buf: &mut Buffer, format: &StatusFormat) {
        let [content, status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        self.draw_lines(content, buf);
        self.draw_status(status, buf, format);
    }

    /// Draws the most recent lines, the last one at the bottom.
    fn draw_lines(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        // The incomplete last line is shown too, as it may never be completed.
        let partial = self.follower.partial();
        let partial = (!partial.is_empty()).then(|| line::process(partial, &self.scheme));

        let mut y = area.bottom();
        for line in partial.iter().chain(self.lines.iter().rev()) {
            for row in wrap::wrap(line, area.width).iter().rev() {
                if y == area.top() {
                    return;
                }
                y -= 1;
                draw_row(row, area.x, y, area.width, buf, Style::new());
            }
        }
    }

    fn draw_status(&self, area: Rect, buf: &mut Buffer, format: &StatusFormat) {
        if area.is_empty() {
            return;
        }

        let values = Values {
            path: self.follower.path(),
            lines: self.lines_read,
            size: self.follower.size(),
            modified: self.follower.modified(),
            status: self.follower.status(),
        };

        buf.set_style(area, STATUS_STYLE);

        // The file name and the error message can contain anything, so
        // process them like the file contents.
        let process =
            |template: &Template| line::process(&template.render(&values), &Scheme::default());
        let (left, center, right) = (
            process(&format.left),
            process(&format.center),
            process(&format.right),
        );

        // The right part, which is typically short and contains the most
        // important information, is shown entirely if possible.
        let width = area.width;
        let right = first_row(&right, width);
        let right_x = width.saturating_sub(row_width(right.as_ref()));

        // The center part is centred if possible, but is moved to the left
        // if it would overlap the right part and truncated if it doesn't fit
        // before it at all.
        let center = first_row(&center, right_x);
        let center_width = row_width(center.as_ref());
        let left_width = row_width(first_row(&left, width).as_ref());
        let center_x = (width.saturating_sub(center_width) / 2)
            .max(left_width)
            .min(right_x.saturating_sub(center_width));

        // And the left part gets whatever remains.
        let left_end = if center_width > 0 { center_x } else { right_x };
        let left = first_row(&left, left_end);

        for (row, x) in [(left, 0), (center, center_x), (right, right_x)] {
            if let Some(row) = row {
                let x = area.x + x;
                draw_row(
                    &row,
                    x,
                    area.y,
                    area.right().saturating_sub(x),
                    buf,
                    STATUS_STYLE,
                );
            }
        }
    }
}

/// Returns the first row of the line wrapped at the given width, i.e. its
/// beginning which fits into this width, if anything fits.
fn first_row(line: &Line, width: u16) -> Option<Row<'_>> {
    wrap::wrap(line, width).into_iter().next()
}

/// Returns the width of the row, or 0 if there is none.
fn row_width(row: Option<&Row>) -> u16 {
    row.map_or(0, |row| row.width().try_into().unwrap_or(u16::MAX))
}

/// Draws the row at the given position, applying its styles on top of the
/// base style.
fn draw_row(row: &Row, x: u16, y: u16, width: u16, buf: &mut Buffer, base: Style) {
    let right = x.saturating_add(width);
    let mut x = x;
    for (text, style) in row.segments() {
        let max = usize::from(right.saturating_sub(x));
        (x, _) = buf.set_stringn(x, y, text, max, base.patch(style));
    }
}

/// Runs the event loop until the user quits.
pub fn run(terminal: &mut DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    // Only the non-blank cells are drawn initially, as the alternate screen
    // is supposed to be blank, but it isn't if the terminal doesn't support
    // it, as is the case of GNU screen by default, so clear it explicitly.
    clear(terminal)?;

    // Drawing is relatively expensive, so only do it when needed.
    let mut redraw = true;
    loop {
        let result = app.poll();
        if result.changed || redraw {
            terminal.draw(|frame| app.draw(frame))?;
            redraw = false;
        }

        let timeout = if result.has_more {
            Duration::ZERO
        } else {
            POLL_INTERVAL
        };
        if event::poll(timeout)? {
            match action(&event::read()?) {
                Some(Action::Quit) => {
                    // As for the initial clearing above, leaving the
                    // alternate screen doesn't erase our output if the
                    // terminal doesn't support it, so do it ourselves and,
                    // as clear(1) does, put the cursor at the top.
                    clear(terminal)?;
                    terminal.set_cursor_position((0, 0))?;
                    return Ok(());
                }
                Some(Action::Redraw) => {
                    clear(terminal)?;
                    redraw = true;
                }
                Some(Action::Resized) => redraw = true,
                None => {}
            }
        }
    }
}

/// Clears the screen and makes the next draw redraw everything.
///
/// This doesn't use `Terminal::clear()`, which queries the cursor position
/// and fails if the terminal doesn't answer, while we don't need it at all.
/// For a full screen terminal, resizing it, even to its current size, clears
/// it without doing this.
fn clear(terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
    let area = terminal.size()?.into();
    terminal.resize(area)?;
    Ok(())
}

/// Something the user asked for.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Quit,
    /// Redraw the entire screen, e.g. after something else wrote to it.
    Redraw,
    /// The terminal size changed, so everything must be drawn again.
    Resized,
}

fn action(event: &TermEvent) -> Option<Action> {
    let key = match event {
        TermEvent::Key(key) => key,
        TermEvent::Resize(..) => return Some(Action::Resized),
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('c') if ctrl => Some(Action::Quit),
        KeyCode::Char('l') if ctrl => Some(Action::Redraw),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use partail::config::Window;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// Creates an application with windows for the given files, with the
    /// given heights, and with the given scrollback.
    ///
    /// The status lines show only the file name, preceded by a space.
    fn make_app(windows: &[(&str, Option<u16>)], scrollback: usize) -> App {
        make_app_with_status(windows, scrollback, [" {file}", "", ""])
    }

    /// Creates an application with the given left, center and right parts of
    /// the status line.
    fn make_app_with_status(
        windows: &[(&str, Option<u16>)],
        scrollback: usize,
        [left, center, right]: [&str; 3],
    ) -> App {
        let config = Config {
            scrollback: scrollback.try_into().unwrap(),
            windows: windows
                .iter()
                .map(|&(file, height)| Window {
                    file: PathBuf::from(file),
                    height: height.map(|h| h.try_into().unwrap()),
                    scheme: Arc::default(),
                })
                .collect(),
            status: StatusFormat {
                left: Template::parse(left).unwrap(),
                center: Template::parse(center).unwrap(),
                right: Template::parse(right).unwrap(),
            },
        };
        App::new(config, 10)
    }

    fn add_lines(app: &mut App, pane: usize, lines: &[&str]) {
        for line in lines {
            app.panes[pane].handle(Event::Line(line.as_bytes().to_vec()));
        }
    }

    /// Draws the application and returns the text of the screen rows.
    fn render(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn layout() {
        let mut app = make_app(&[("one.log", Some(2)), ("two.log", None)], 100);
        add_lines(&mut app, 0, &["1a", "1b", "1c"]);
        add_lines(&mut app, 1, &["2a", "2b"]);

        assert_eq!(
            render(&app, 10, 8),
            [
                "1b        ",
                "1c        ",
                " one.log  ",
                "          ",
                "          ",
                "2a        ",
                "2b        ",
                " two.log  ",
            ]
        );
    }

    #[test]
    fn shared_space() {
        let mut app = make_app(&[("a", None), ("b", None)], 100);
        add_lines(&mut app, 0, &["x"]);
        add_lines(&mut app, 1, &["y"]);
        assert_eq!(
            render(&app, 3, 6),
            ["   ", "x  ", " a ", "   ", "y  ", " b "]
        );
    }

    #[test]
    fn wrapping() {
        let mut app = make_app(&[("f", None)], 100);
        add_lines(&mut app, 0, &["abcdefgh", "ij"]);
        assert_eq!(
            render(&app, 5, 5),
            ["     ", "abcde", "fgh  ", "ij   ", " f   "]
        );

        // Only the end of the wrapped line fits.
        assert_eq!(render(&app, 5, 3), ["fgh  ", "ij   ", " f   "]);
    }

    #[test]
    fn markers() {
        let mut app = make_app(&[("f", None)], 100);
        add_lines(&mut app, 0, &["old"]);
        app.panes[0].handle(Event::Truncated);
        app.panes[0].handle(Event::LongLinePart(b"long".to_vec()));
        add_lines(&mut app, 0, &["end"]);
        app.panes[0].handle(Event::Replaced);
        assert_eq!(
            render(&app, 22, 6),
            [
                "old                   ",
                "--- file truncated ---",
                "long\\                 ",
                "end                   ",
                "--- file replaced --- ",
                " f                    ",
            ]
        );
    }

    #[test]
    fn scrollback() {
        let mut app = make_app(&[("f", None)], 2);
        add_lines(&mut app, 0, &["1", "2", "3"]);
        assert_eq!(app.panes[0].lines.len(), 2);
        assert_eq!(render(&app, 2, 4), ["  ", "2 ", "3 ", " f"]);
    }

    #[test]
    fn unsafe_contents() {
        let mut app = make_app(&[("f", None)], 100);
        add_lines(&mut app, 0, &["\x1b[2J\x07"]);
        assert_eq!(render(&app, 10, 2), ["^[[2J^G   ", " f        "]);
    }

    #[test]
    fn partial_line_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "complete\nincomplete").unwrap();

        let format = [" {name}", "{status}", "{lines} lines, {bytes} bytes "];
        let mut app = make_app_with_status(&[(path.to_str().unwrap(), None)], 100, format);
        assert!(!app.poll().has_more);
        assert_eq!(
            render(&app, 30, 3),
            [
                "complete                      ",
                "incomplete                    ",
                " log        1 lines, 19 bytes ",
            ]
        );

        let mut app = make_app_with_status(&[("/nonexistent/log", None)], 100, format);
        app.poll();
        assert_eq!(render(&app, 30, 1), [" log  missing0 lines, - bytes "]);
    }

    #[test]
    fn changes_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "first\n").unwrap();

        let mut app = make_app(&[(path.to_str().unwrap(), None)], 100);
        assert!(app.poll().changed);
        assert!(!app.poll().changed);

        // A new line.
        let append = |data: &[u8]| {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(data).unwrap();
        };
        append(b"second\n");
        assert!(app.poll().changed);
        assert!(!app.poll().changed);

        // An incomplete line, then more of it.
        append(b"par");
        assert!(app.poll().changed);
        append(b"tial");
        assert!(app.poll().changed);
        assert!(!app.poll().changed);

        // The file disappearing changes its status.
        std::fs::remove_file(&path).unwrap();
        assert!(app.poll().changed);
        assert!(!app.poll().changed);
    }

    #[test]
    fn status_alignment() {
        let app = make_app_with_status(&[("f", None)], 100, ["L{name}", "C", "{name}R"]);
        assert_eq!(render(&app, 9, 1), ["Lf  C  fR"]);
        assert_eq!(render(&app, 10, 1), ["Lf  C   fR"]);

        // When there is not enough space, the parts don't overlap, but the
        // center part is moved to the left and the left part is truncated,
        // then the center one too.
        let app = make_app_with_status(&[("f", None)], 100, ["left part", "center", "right"]);
        assert_eq!(render(&app, 25, 1), ["left partcenter     right"]);
        assert_eq!(render(&app, 20, 1), ["left partcenterright"]);
        assert_eq!(render(&app, 15, 1), ["leftcenterright"]);
        assert_eq!(render(&app, 10, 1), ["centeright"]);
        assert_eq!(render(&app, 4, 1), ["righ"]);

        // Empty parts don't take any space.
        let app = make_app_with_status(&[("f", None)], 100, ["left part", "", "right"]);
        assert_eq!(render(&app, 12, 1), ["left paright"]);
    }

    #[test]
    fn status_sanitised() {
        let app = make_app_with_status(&[("bad\x1bname", None)], 100, ["{file}", "", ""]);
        assert_eq!(render(&app, 12, 1), ["bad^[name   "]);
    }

    #[test]
    fn tiny_terminal() {
        let mut app = make_app(&[("a", Some(5)), ("b", None)], 100);
        add_lines(&mut app, 0, &["xyz", "\u{65e5}\u{672c}"]);
        // Nothing sensible can be shown, but this must not panic.
        for (width, height) in [(0, 0), (1, 1), (2, 1), (1, 2), (1, 5), (3, 3)] {
            render(&app, width, height);
        }
    }

    #[test]
    fn keys() {
        use ratatui::crossterm::event::{KeyEvent, KeyEventState};

        let key = |code, modifiers| {
            TermEvent::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;

        assert_eq!(action(&key(KeyCode::Char('q'), none)), Some(Action::Quit));
        assert_eq!(action(&key(KeyCode::Char('c'), ctrl)), Some(Action::Quit));
        assert_eq!(action(&key(KeyCode::Char('l'), ctrl)), Some(Action::Redraw));
        assert_eq!(action(&key(KeyCode::Char('c'), none)), None);
        assert_eq!(action(&key(KeyCode::Char('l'), none)), None);

        let release = TermEvent::Key(KeyEvent {
            kind: KeyEventKind::Release,
            ..KeyEvent::new(KeyCode::Char('q'), none)
        });
        assert_eq!(action(&release), None);
        assert_eq!(action(&TermEvent::Resize(80, 24)), Some(Action::Resized));
        assert_eq!(action(&TermEvent::FocusGained), None);
    }
}
