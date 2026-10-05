//! The terminal UI: windows showing the followed files and the event loop.

use std::collections::VecDeque;
use std::num::NonZeroU16;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};

use partail::config::{Config, Scheme};
use partail::follow::{Event, Follower, Status};
use partail::line::{self, Line};
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
            })
            .collect();
        Self { panes }
    }

    /// Reads new data from all files. Returns true if there is more data to
    /// read immediately.
    fn poll(&mut self) -> bool {
        let mut has_more = false;
        for pane in &mut self.panes {
            for event in pane.follower.poll() {
                pane.handle(event);
            }
            has_more |= pane.follower.has_more();
        }
        has_more
    }

    fn draw(&self, frame: &mut Frame) {
        let constraints = self.panes.iter().map(|pane| match pane.height {
            // Account for the status line.
            Some(height) => Constraint::Length(height.get().saturating_add(1)),
            None => Constraint::Fill(1),
        });
        let areas = Layout::vertical(constraints).split(frame.area());
        for (pane, area) in self.panes.iter().zip(areas.iter()) {
            pane.draw(*area, frame.buffer_mut());
        }
    }
}

impl Pane {
    fn handle(&mut self, event: Event) {
        let line = match event {
            Event::Line(bytes) => line::process(&bytes, &self.scheme),
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

    fn draw(&self, area: Rect, buf: &mut Buffer) {
        let [content, status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        self.draw_lines(content, buf);
        self.draw_status(status, buf);
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

    fn draw_status(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        let mut text = b" ".to_vec();
        text.extend_from_slice(self.follower.path().as_os_str().as_bytes());
        if let Some(size) = self.follower.size() {
            text.extend_from_slice(format!(" - {size} bytes").as_bytes());
        }
        match self.follower.status() {
            Status::Following => {}
            Status::Missing => text.extend_from_slice(b" - missing"),
            Status::Error(e) => text.extend_from_slice(format!(" - error: {e}").as_bytes()),
        }

        // The file name and the error can contain anything, so process them
        // like the file contents.
        let line = line::process(&text, &Scheme::default());

        buf.set_style(area, STATUS_STYLE);
        if let Some(row) = wrap::wrap(&line, area.width).first() {
            draw_row(row, area.x, area.y, area.width, buf, STATUS_STYLE);
        }
    }
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
    loop {
        let has_more = app.poll();
        terminal.draw(|frame| app.draw(frame))?;

        let timeout = if has_more {
            Duration::ZERO
        } else {
            POLL_INTERVAL
        };
        if event::poll(timeout)? && is_quit(&event::read()?) {
            return Ok(());
        }
    }
}

fn is_quit(event: &TermEvent) -> bool {
    let TermEvent::Key(key) = event else {
        return false;
    };
    if key.kind != KeyEventKind::Press {
        return false;
    }
    match key.code {
        KeyCode::Char('q') => true,
        KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
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
    fn make_app(windows: &[(&str, Option<u16>)], scrollback: usize) -> App {
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

        let mut app = make_app(&[(path.to_str().unwrap(), None)], 100);
        assert!(!app.poll());
        let screen = render(&app, 80, 3);
        assert_eq!(screen[0].trim_end(), "complete");
        assert_eq!(screen[1].trim_end(), "incomplete");
        assert_eq!(
            screen[2].trim_end(),
            format!(" {} - 19 bytes", path.display())
        );

        let mut app = make_app(&[("/nonexistent/log", None)], 100);
        app.poll();
        assert_eq!(render(&app, 30, 1), [" /nonexistent/log - missing   "]);
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
}
