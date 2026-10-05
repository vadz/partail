//! The terminal UI: windows showing the followed files and the event loop.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::bail;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Clear;
use regex::{Regex, RegexBuilder};

use partail::config::{Config, Scheme};
use partail::follow::{Event, Follower};
use partail::line::{self, Line};
use partail::status::{StatusFormat, Template, Values};
use partail::watch::{self, Notification};
use partail::wrap::{self, Row};

/// How often the files are checked for changes if they can't be watched.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How often the watched files are still checked for changes, in case some
/// changes are not reported, as for the files on network file systems.
const WATCHED_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Style of the status lines.
const STATUS_STYLE: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Style of the status line of the focused window, if there are several.
const FOCUSED_STATUS_STYLE: Style = STATUS_STYLE.add_modifier(Modifier::BOLD);

/// Style used for highlighting the search matches.
const HIGHLIGHT_STYLE: Style = Style::new().fg(Color::Black).bg(Color::Yellow);

/// Marker appended to the parts of a too long line.
const LONG_LINE_MARKER: &str = "\\";

/// The whole application state.
pub struct App {
    panes: Vec<Pane>,
    status: StatusFormat,
    /// Index of the window the scrolling keys apply to.
    focus: usize,
    /// Text entered at the search prompt, if it's shown.
    prompt: Option<String>,
    /// The current search, whose matches are highlighted.
    search: Option<Search>,
    /// Message shown instead of the status line of the focused window until
    /// the next key press.
    message: Option<String>,
}

/// A search for a regex.
struct Search {
    pattern: String,
    regex: Regex,
}

/// What the event loop must do after handling an event.
#[derive(Debug, PartialEq, Eq)]
enum Response {
    Nothing,
    Redraw,
    /// Clear the screen and redraw everything.
    ClearScreen,
    Quit,
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
    /// Number of lines below the view, 0 if following the end of the file.
    ///
    /// The incomplete last line, if any, counts as a line here and in all
    /// the scrolling-related functions below.
    ///
    /// This may be greater than the maximal possible offset, see
    /// [`Pane::effective_offset`].
    offset: usize,
    /// Area used for showing the lines when the window was last drawn.
    content_area: Cell<Rect>,
    /// Index of the line containing the last search match, if any.
    last_match: Option<usize>,
}

/// How to scroll a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scroll {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    /// Show the oldest lines.
    Top,
    /// Show the most recent lines and follow the file again.
    Bottom,
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
                offset: 0,
                content_area: Cell::default(),
                last_match: None,
            })
            .collect();
        Self {
            panes,
            status: config.status,
            focus: 0,
            prompt: None,
            search: None,
            message: None,
        }
    }

    /// Handles a terminal event.
    fn handle_event(&mut self, event: &TermEvent) -> Response {
        let TermEvent::Key(key) = event else {
            return match action(event) {
                Some(Action::Resized) => Response::Redraw,
                _ => Response::Nothing,
            };
        };
        if key.kind != KeyEventKind::Press {
            return Response::Nothing;
        }

        // Any key press makes the message disappear.
        let had_message = self.message.take().is_some();

        if self.prompt.is_some() {
            self.edit_prompt(key);
            return Response::Redraw;
        }

        match action(event) {
            Some(Action::Quit) => return Response::Quit,
            Some(Action::Redraw) => return Response::ClearScreen,
            Some(Action::Scroll(scroll)) => self.scroll(scroll),
            Some(Action::Focus { forward }) => self.move_focus(forward),
            Some(Action::StartSearch) => self.prompt = Some(String::new()),
            Some(Action::FindNext { older }) => self.find(older),
            Some(Action::Cancel) => self.search = None,
            Some(Action::Resized) => {}
            None if had_message => {}
            None => return Response::Nothing,
        }
        Response::Redraw
    }

    /// Handles a key press while the search prompt is shown.
    fn edit_prompt(&mut self, key: &KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Char('c') if ctrl => self.prompt = None,
            KeyCode::Enter => {
                let pattern = self.prompt.take().unwrap_or_default();
                self.start_search(pattern);
            }
            KeyCode::Backspace => {
                // As in vi, erasing everything cancels the search.
                if let Some(prompt) = &mut self.prompt
                    && prompt.pop().is_none()
                {
                    self.prompt = None;
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.push(c);
                }
            }
            _ => {}
        }
    }

    /// Starts searching for the given pattern, or repeats the last search if
    /// it's empty, from the bottom of the focused window towards the older
    /// lines.
    fn start_search(&mut self, pattern: String) {
        if !pattern.is_empty() {
            // Use "smart case": the search is case-insensitive unless the
            // pattern contains upper case letters.
            let case_insensitive = !pattern.chars().any(char::is_uppercase);
            match RegexBuilder::new(&pattern)
                .case_insensitive(case_insensitive)
                .build()
            {
                Ok(regex) => self.search = Some(Search { pattern, regex }),
                Err(e) => {
                    // The error messages span several lines, the last one
                    // explaining the problem.
                    let e = e.to_string();
                    let reason = e.lines().last().unwrap_or_default();
                    let reason = reason.strip_prefix("error: ").unwrap_or(reason);
                    self.message = Some(format!("invalid regex: {reason}"));
                    return;
                }
            }
        } else if self.search.is_none() {
            return;
        }

        if let Some(pane) = self.panes.get_mut(self.focus) {
            pane.last_match = None;
        }
        self.find(true);
    }

    /// Finds the next match of the current search in the focused window,
    /// looking at the older or the newer lines.
    fn find(&mut self, older: bool) {
        let Some(search) = &self.search else {
            self.message = Some("no search pattern".to_owned());
            return;
        };
        let Some(pane) = self.panes.get_mut(self.focus) else {
            return;
        };
        if !pane.find(&search.regex, older) {
            self.message = Some(format!("pattern not found: {}", search.pattern));
        }
    }

    /// Returns the paths of all the followed files.
    fn files(&self) -> impl Iterator<Item = &Path> {
        self.panes.iter().map(|pane| pane.follower.path())
    }

    /// Scrolls the focused window.
    fn scroll(&mut self, scroll: Scroll) {
        if let Some(pane) = self.panes.get_mut(self.focus) {
            pane.scroll(scroll);
        }
    }

    /// Focuses the next window, or the previous one if `forward` is false.
    fn move_focus(&mut self, forward: bool) {
        let count = self.panes.len();
        if count > 0 {
            self.focus = if forward {
                (self.focus + 1) % count
            } else {
                (self.focus + count - 1) % count
            };
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
        let several = self.panes.len() > 1;
        let regex = self.search.as_ref().map(|search| &search.regex);
        for (index, (pane, area)) in self.panes.iter().zip(areas.iter()).enumerate() {
            let focused = several && index == self.focus;
            pane.draw(*area, frame.buffer_mut(), &self.status, focused, regex);
        }

        // Show the prompt or the message in the status line of the focused
        // window, instead of the status itself.
        let text = match (&self.prompt, &self.message) {
            (Some(prompt), _) => format!("/{prompt}"),
            (None, Some(message)) => message.clone(),
            (None, None) => return,
        };
        let Some(area) = areas.get(self.focus) else {
            return;
        };
        if area.is_empty() {
            return;
        }
        let status = Rect {
            y: area.bottom() - 1,
            height: 1,
            ..*area
        };
        let style = if several {
            FOCUSED_STATUS_STYLE
        } else {
            STATUS_STYLE
        };
        // Erase the status line drawn by the window before.
        frame.render_widget(Clear, status);
        frame.buffer_mut().set_style(status, style);

        // Process the text as it can contain anything typed by the user.
        let line = line::process(text.as_bytes(), &Scheme::default());
        if let Some(row) = first_row(&line, status.width) {
            draw_row(
                &row,
                status.x,
                status.y,
                status.width,
                frame.buffer_mut(),
                style,
            );
            if self.prompt.is_some() {
                let x = status.x + row_width(Some(&row));
                frame.set_cursor_position((x.min(status.right() - 1), status.y));
            }
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
        let had_partial = self.has_partial();
        let events = self.follower.poll();
        let changed = !events.is_empty() || state(&self.follower) != before;
        for event in events {
            self.handle(event);
        }

        // Each new line has already been accounted for by handle(), but the
        // incomplete line may have appeared or disappeared, e.g. when it
        // became a complete line, which shouldn't change the view.
        if self.offset > 0 {
            match (had_partial, self.has_partial()) {
                (false, true) => self.offset += 1,
                (true, false) => self.offset -= 1,
                _ => {}
            }
        }
        self.offset = self.offset.min(self.line_count().saturating_sub(1));

        changed
    }

    fn has_partial(&self) -> bool {
        !self.follower.partial().is_empty()
    }

    /// Returns the number of lines, including the incomplete last line.
    fn line_count(&self) -> usize {
        self.lines.len() + usize::from(self.has_partial())
    }

    /// Returns the incomplete last line processed for showing it, if any.
    fn partial_line(&self) -> Option<Line> {
        self.has_partial()
            .then(|| line::process(self.follower.partial(), &self.scheme))
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

        // Keep showing the same lines if scrolled.
        if self.offset > 0 {
            self.offset += 1;
        }

        while self.lines.len() > self.scrollback {
            self.lines.pop_front();
            self.last_match = self.last_match.and_then(|index| index.checked_sub(1));
        }
        self.offset = self.offset.min(self.line_count().saturating_sub(1));
    }

    /// Finds the next line matching the regex, older or newer than the line
    /// with the last match or, if there is none, than the bottom line of the
    /// view, which is included in the search for the older lines.
    ///
    /// If a match is found, scrolls to show it at the bottom of the window,
    /// if possible, and returns true.
    fn find(&mut self, regex: &Regex, older: bool) -> bool {
        // The incomplete line is never searched.
        let Some(last) = self.lines.len().checked_sub(1) else {
            return false;
        };
        let bottom = self
            .line_count()
            .saturating_sub(1 + self.effective_offset(self.content_area.get()))
            .min(last);

        let is_match = |index: &usize| {
            self.lines
                .get(*index)
                .is_some_and(|line| regex.is_match(line.text()))
        };
        let found = match (older, self.last_match) {
            (true, Some(index)) => (0..index).rev().find(is_match),
            (true, None) => (0..=bottom).rev().find(is_match),
            (false, Some(index)) => (index + 1..=last).find(is_match),
            (false, None) => (bottom + 1..=last).find(is_match),
        };

        let Some(index) = found else {
            return false;
        };
        self.last_match = Some(index);
        self.offset = self.line_count() - 1 - index;
        true
    }

    /// Returns the number of rows taken by the line with the given index,
    /// which is the incomplete line if it's equal to the number of complete
    /// lines, when wrapped at the given width.
    fn rows(&self, index: usize, width: u16) -> usize {
        let rows = match self.lines.get(index) {
            Some(line) => wrap::row_count(line, width),
            None => self
                .partial_line()
                .map_or(1, |line| wrap::row_count(&line, width)),
        };
        rows.max(1)
    }

    /// Returns the maximal offset for the given area, i.e. the one for which
    /// the oldest line is at the top of it, or 0 if all lines fit into it.
    fn max_offset(&self, area: Rect) -> usize {
        let height = usize::from(area.height).max(1);
        let count = self.line_count();
        let mut rows = 0;
        for index in 0..count {
            rows += self.rows(index, area.width);
            if rows >= height {
                return count - 1 - index;
            }
        }
        0
    }

    /// Returns the offset to use for the given area, which may be less than
    /// the requested one to avoid leaving the top of the window empty.
    fn effective_offset(&self, area: Rect) -> usize {
        self.offset.min(self.max_offset(area))
    }

    /// Returns the number of lines, at least partially, visible in the area
    /// with the given offset.
    fn visible_lines(&self, offset: usize, area: Rect) -> usize {
        let height = usize::from(area.height).max(1);
        let Some(bottom) = self.line_count().checked_sub(offset + 1) else {
            return 0;
        };
        let mut rows = 0;
        let mut count = 0;
        for index in (0..=bottom).rev() {
            rows += self.rows(index, area.width);
            count += 1;
            if rows >= height {
                break;
            }
        }
        count
    }

    fn scroll(&mut self, scroll: Scroll) {
        // Search from the new position after scrolling.
        self.last_match = None;

        let area = self.content_area.get();
        let offset = self.effective_offset(area);

        // Keep one line visible when scrolling by pages, but always scroll.
        let page = self.visible_lines(offset, area).saturating_sub(1).max(1);
        let offset = match scroll {
            Scroll::LineUp => offset + 1,
            Scroll::LineDown => offset.saturating_sub(1),
            Scroll::PageUp => offset + page,
            Scroll::PageDown => offset.saturating_sub(page),
            Scroll::Top => usize::MAX,
            Scroll::Bottom => 0,
        };
        self.offset = offset.min(self.max_offset(area));
    }

    fn draw(
        &self,
        area: Rect,
        buf: &mut Buffer,
        format: &StatusFormat,
        focused: bool,
        search: Option<&Regex>,
    ) {
        let [content, status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        self.content_area.set(content);
        let offset = self.effective_offset(content);
        self.draw_lines(content, buf, offset, search);
        self.draw_status(status, buf, format, offset, focused);
    }

    /// Draws the lines with the given offset, the last one at the bottom,
    /// highlighting the matches of the search regex, if any.
    fn draw_lines(&self, area: Rect, buf: &mut Buffer, offset: usize, search: Option<&Regex>) {
        if area.is_empty() {
            return;
        }

        // The incomplete last line is shown too when following the file, as
        // it may never be completed.
        let partial = if offset == 0 {
            self.partial_line()
        } else {
            None
        };

        let end = self
            .line_count()
            .saturating_sub(offset)
            .min(self.lines.len());
        let mut y = area.bottom();
        for line in partial.iter().chain(self.lines.range(..end).rev()) {
            let highlighted;
            let line = match search {
                Some(regex) => {
                    let matches: Vec<_> = regex
                        .find_iter(line.text())
                        .map(|m| m.range())
                        .filter(|range| !range.is_empty())
                        .collect();
                    highlighted = line.highlighted(&matches, HIGHLIGHT_STYLE);
                    &highlighted
                }
                None => line,
            };
            for row in wrap::wrap(line, area.width).iter().rev() {
                if y == area.top() {
                    return;
                }
                y -= 1;
                draw_row(row, area.x, y, area.width, buf, Style::new());
            }
        }
    }

    fn draw_status(
        &self,
        area: Rect,
        buf: &mut Buffer,
        format: &StatusFormat,
        offset: usize,
        focused: bool,
    ) {
        if area.is_empty() {
            return;
        }

        let values = Values {
            path: self.follower.path(),
            lines: self.lines_read,
            scroll: offset,
            size: self.follower.size(),
            modified: self.follower.modified(),
            status: self.follower.status(),
        };

        let style = if focused {
            FOCUSED_STATUS_STYLE
        } else {
            STATUS_STYLE
        };
        buf.set_style(area, style);

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
                draw_row(&row, x, area.y, area.right().saturating_sub(x), buf, style);
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
        let style = base.patch(style);
        if text.is_ascii() {
            // This is much faster than set_stringn(), which splits the text
            // into grapheme clusters, and is equivalent to it for ASCII
            // text, which consists only of printable characters here.
            for c in text.chars() {
                if x >= right {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(c).set_style(style);
                }
                x += 1;
            }
        } else {
            let max = usize::from(right.saturating_sub(x));
            (x, _) = buf.set_stringn(x, y, text, max, style);
        }
    }
}

/// Messages sent to the event loop by the background threads.
enum Message {
    Terminal(io::Result<TermEvent>),
    Files(Notification),
}

/// Runs the event loop until the user quits.
pub fn run(terminal: &mut DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    // Only the non-blank cells are drawn initially, as the alternate screen
    // is supposed to be blank, but it isn't if the terminal doesn't support
    // it, as is the case of GNU screen by default, so clear it explicitly.
    clear(terminal)?;

    // Wait for both the terminal events and the file change notifications
    // by reading the former in a separate thread.
    let (tx, rx) = mpsc::channel();
    let terminal_tx = tx.clone();
    thread::spawn(move || {
        loop {
            let event = event::read();
            let failed = event.is_err();
            if terminal_tx.send(Message::Terminal(event)).is_err() || failed {
                break;
            }
        }
    });

    let files: Vec<PathBuf> = app.files().map(Path::to_path_buf).collect();
    let watched = watch::watch(&files, move |notification| {
        // This can only fail if we're exiting, so just ignore it.
        let _ = tx.send(Message::Files(notification));
    });
    let mut poll_interval = match watched {
        Ok(true) => WATCHED_POLL_INTERVAL,
        Ok(false) | Err(_) => POLL_INTERVAL,
    };

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
            poll_interval
        };
        let mut message = match rx.recv_timeout(timeout) {
            Ok(message) => Some(message),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => bail!("terminal events can't be read any more"),
        };

        // Handle all the pending messages before polling the files again.
        while let Some(current) = message {
            match current {
                // The files are polled at the next loop iteration anyhow.
                Message::Files(Notification::Changed) => {}
                Message::Files(Notification::Failed) => poll_interval = POLL_INTERVAL,
                Message::Terminal(event) => match app.handle_event(&event?) {
                    Response::Nothing => {}
                    Response::Redraw => redraw = true,
                    Response::ClearScreen => {
                        clear(terminal)?;
                        redraw = true;
                    }
                    Response::Quit => {
                        // As for the initial clearing above, leaving the
                        // alternate screen doesn't erase our output if the
                        // terminal doesn't support it, so do it ourselves
                        // and, as clear(1) does, put the cursor at the top.
                        clear(terminal)?;
                        terminal.set_cursor_position((0, 0))?;
                        return Ok(());
                    }
                },
            }
            message = rx.try_recv().ok();
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
    /// Scroll the focused window.
    Scroll(Scroll),
    /// Focus the next or previous window.
    Focus {
        forward: bool,
    },
    /// Show the search prompt.
    StartSearch,
    /// Find the next match of the current search.
    FindNext {
        older: bool,
    },
    /// Stop highlighting the search matches.
    Cancel,
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
        KeyCode::Up => Some(Action::Scroll(Scroll::LineUp)),
        KeyCode::Down => Some(Action::Scroll(Scroll::LineDown)),
        KeyCode::PageUp => Some(Action::Scroll(Scroll::PageUp)),
        KeyCode::PageDown => Some(Action::Scroll(Scroll::PageDown)),
        KeyCode::Home => Some(Action::Scroll(Scroll::Top)),
        KeyCode::End => Some(Action::Scroll(Scroll::Bottom)),
        KeyCode::Tab => Some(Action::Focus { forward: true }),
        KeyCode::BackTab => Some(Action::Focus { forward: false }),
        KeyCode::Char('/') => Some(Action::StartSearch),
        KeyCode::Char('n') => Some(Action::FindNext { older: true }),
        KeyCode::Char('N') => Some(Action::FindNext { older: false }),
        KeyCode::Esc => Some(Action::Cancel),
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

    /// Creates an application with a single window containing the given
    /// lines and showing the scroll position in its status line.
    fn scrollable_app(lines: &[&str], scrollback: usize) -> App {
        let mut app = make_app_with_status(&[("f", None)], scrollback, ["{scroll}", "", ""]);
        add_lines(&mut app, 0, lines);
        app
    }

    /// Scrolls the application and returns the screen contents.
    fn scroll(app: &mut App, scroll: Scroll, width: u16, height: u16) -> Vec<String> {
        app.scroll(scroll);
        render(app, width, height)
    }

    #[test]
    fn scrolling() {
        let lines: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut app = scrollable_app(&lines, 100);

        assert_eq!(render(&app, 3, 4), ["8  ", "9  ", "10 ", "   "]);
        assert_eq!(
            scroll(&mut app, Scroll::LineUp, 3, 4),
            ["7  ", "8  ", "9  ", "\u{2191}1 "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::LineDown, 3, 4),
            ["8  ", "9  ", "10 ", "   "]
        );
        // Can't scroll below the end.
        assert_eq!(
            scroll(&mut app, Scroll::LineDown, 3, 4),
            ["8  ", "9  ", "10 ", "   "]
        );

        // Pages keep one line visible.
        assert_eq!(
            scroll(&mut app, Scroll::PageUp, 3, 4),
            ["6  ", "7  ", "8  ", "\u{2191}2 "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::PageUp, 3, 4),
            ["4  ", "5  ", "6  ", "\u{2191}4 "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::PageDown, 3, 4),
            ["6  ", "7  ", "8  ", "\u{2191}2 "]
        );

        // The oldest line is shown at the top and we can't go further.
        assert_eq!(
            scroll(&mut app, Scroll::Top, 3, 4),
            ["1  ", "2  ", "3  ", "\u{2191}7 "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::LineUp, 3, 4),
            ["1  ", "2  ", "3  ", "\u{2191}7 "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::PageUp, 3, 4),
            ["1  ", "2  ", "3  ", "\u{2191}7 "]
        );

        assert_eq!(
            scroll(&mut app, Scroll::Bottom, 3, 4),
            ["8  ", "9  ", "10 ", "   "]
        );
    }

    #[test]
    fn scrolling_with_few_lines() {
        let mut app = scrollable_app(&["1", "2"], 100);
        assert_eq!(
            scroll(&mut app, Scroll::LineUp, 3, 4),
            ["   ", "1  ", "2  ", "   "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::Top, 3, 4),
            ["   ", "1  ", "2  ", "   "]
        );

        let mut app = scrollable_app(&[], 100);
        assert_eq!(scroll(&mut app, Scroll::PageUp, 3, 2), ["   ", "   "]);
    }

    #[test]
    fn scrolling_wrapped_lines() {
        let mut app = scrollable_app(&["111111", "2", "333333"], 100);
        assert_eq!(render(&app, 3, 4), ["2  ", "333", "333", "   "]);
        assert_eq!(
            scroll(&mut app, Scroll::LineUp, 3, 4),
            ["111", "111", "2  ", "\u{2191}1 "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::LineUp, 3, 4),
            ["111", "111", "2  ", "\u{2191}1 "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::PageDown, 3, 4),
            ["2  ", "333", "333", "   "]
        );
    }

    #[test]
    fn scrolled_view_is_kept() {
        let lines: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut app = scrollable_app(&lines, 10);
        render(&app, 3, 4);
        app.scroll(Scroll::PageUp);

        // New lines don't change what is shown.
        add_lines(&mut app, 0, &["11", "12"]);
        assert_eq!(render(&app, 3, 4), ["6  ", "7  ", "8  ", "\u{2191}4 "]);

        // Even when old lines are dropped, as long as the shown ones remain.
        add_lines(&mut app, 0, &["13", "14", "15"]);
        assert_eq!(render(&app, 3, 4), ["6  ", "7  ", "8  ", "\u{2191}7 "]);

        // When they don't, the oldest remaining ones are shown.
        add_lines(&mut app, 0, &["16"]);
        assert_eq!(render(&app, 3, 4), ["7  ", "8  ", "9  ", "\u{2191}7 "]);

        assert_eq!(
            scroll(&mut app, Scroll::Bottom, 3, 4),
            ["14 ", "15 ", "16 ", "   "]
        );
    }

    #[test]
    fn partial_line_only_shown_when_following() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "1\n2\n3\nincomplete").unwrap();

        let mut app =
            make_app_with_status(&[(path.to_str().unwrap(), None)], 100, ["{scroll}", "", ""]);
        app.poll();
        assert_eq!(
            render(&app, 10, 3),
            ["3         ", "incomplete", "          "]
        );
        assert_eq!(
            scroll(&mut app, Scroll::LineUp, 10, 3),
            ["2         ", "3         ", "\u{2191}1        "]
        );
    }

    #[test]
    fn scrolled_view_kept_when_partial_line_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "1\n2\n3\nincompl").unwrap();
        let append = |data: &[u8]| {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(data).unwrap();
        };

        let mut app =
            make_app_with_status(&[(path.to_str().unwrap(), None)], 100, ["{scroll}", "", ""]);
        app.poll();
        assert_eq!(
            scroll(&mut app, Scroll::LineUp, 10, 3),
            ["2         ", "3         ", "\u{2191}1        "]
        );

        // The incomplete line is completed and another line added.
        append(b"ete\n4\n");
        app.poll();
        assert_eq!(
            render(&app, 10, 3),
            ["2         ", "3         ", "\u{2191}2        "]
        );

        // A new incomplete line appears.
        append(b"5");
        app.poll();
        assert_eq!(
            render(&app, 10, 3),
            ["2         ", "3         ", "\u{2191}3        "]
        );

        assert_eq!(
            scroll(&mut app, Scroll::Bottom, 10, 3),
            ["4         ", "5         ", "          "]
        );
    }

    #[test]
    fn focus() {
        let mut app = make_app(&[("a", None), ("b", None), ("c", None)], 100);
        add_lines(&mut app, 0, &["a1", "a2"]);
        add_lines(&mut app, 1, &["b1", "b2"]);
        assert_eq!(app.focus, 0);

        app.move_focus(true);
        assert_eq!(app.focus, 1);
        app.move_focus(true);
        app.move_focus(true);
        assert_eq!(app.focus, 0);
        app.move_focus(false);
        assert_eq!(app.focus, 2);
        app.move_focus(false);
        assert_eq!(app.focus, 1);

        // Only the focused window is scrolled.
        assert_eq!(
            render(&app, 3, 6),
            ["a2 ", " a ", "b2 ", " b ", "   ", " c "]
        );
        app.scroll(Scroll::LineUp);
        assert_eq!(
            render(&app, 3, 6),
            ["a2 ", " a ", "b1 ", " b ", "   ", " c "]
        );

        // And its status line is in bold.
        let mut terminal = Terminal::new(TestBackend::new(3, 6)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer();
        let bold = |y: u16| buf[(0, y)].modifier.contains(Modifier::BOLD);
        assert_eq!([bold(1), bold(3), bold(5)], [false, true, false]);
    }

    #[test]
    fn single_window_status_not_bold() {
        let app = make_app(&[("a", None)], 100);
        let mut terminal = Terminal::new(TestBackend::new(3, 2)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(
            !terminal.backend().buffer()[(0, 1)]
                .modifier
                .contains(Modifier::BOLD)
        );
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
    fn ascii_drawn_as_by_ratatui() {
        let config = Config::parse(
            r#"
[[window]]
file = "f"
scheme = "s"

[[scheme.s.rule]]
regex = '[aeiou]+'
style = "red"

[[scheme.s.rule]]
regex = ','
style = "bold on blue"
"#,
        )
        .unwrap();
        let line = line::process(
            b"some text: with colours, and more",
            &config.windows[0].scheme,
        );
        let mut fast = Buffer::empty(Rect::new(0, 0, 12, 4));
        let mut slow = fast.clone();
        for (y, row) in wrap::wrap(&line, 10).iter().enumerate() {
            let y = y as u16;
            draw_row(row, 1, y, 10, &mut fast, STATUS_STYLE);
            let mut x = 1;
            for (text, style) in row.segments() {
                (x, _) =
                    slow.set_stringn(x, y, text, usize::from(11 - x), STATUS_STYLE.patch(style));
            }
        }
        assert_eq!(fast, slow);
    }

    fn press(app: &mut App, code: KeyCode) -> Response {
        app.handle_event(&TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn search(app: &mut App, pattern: &str) {
        press(app, KeyCode::Char('/'));
        type_text(app, pattern);
        press(app, KeyCode::Enter);
    }

    /// Lines used by the search tests: with 3 rows, the last 3 are shown.
    const SEARCH_LINES: [&str; 6] = ["foo", "bar", "foo bar", "baz", "qux", "Foo"];

    #[test]
    fn search_and_navigate() {
        let mut app = scrollable_app(&SEARCH_LINES, 100);
        assert_eq!(
            render(&app, 8, 4),
            ["baz     ", "qux     ", "Foo     ", "        "]
        );

        // The search is case-insensitive and includes the bottom line.
        search(&mut app, "foo");
        assert_eq!(
            render(&app, 8, 4),
            ["baz     ", "qux     ", "Foo     ", "        "]
        );

        // The next older match is shown at the bottom.
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(
            render(&app, 8, 4),
            ["foo     ", "bar     ", "foo bar ", "\u{2191}3      "]
        );

        // The oldest match can't be shown at the bottom, but is visible.
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(
            render(&app, 8, 4),
            ["foo     ", "bar     ", "foo bar ", "\u{2191}3      "]
        );
        assert_eq!(app.panes[0].last_match, Some(0));

        // And there are no more matches.
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(render(&app, 25, 4)[3], "pattern not found: foo   ");
        assert_eq!(app.panes[0].last_match, Some(0));

        // Going in the other direction.
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(app.panes[0].last_match, Some(2));
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(app.panes[0].last_match, Some(5));
        assert_eq!(
            render(&app, 8, 4),
            ["baz     ", "qux     ", "Foo     ", "        "]
        );
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(render(&app, 25, 4)[3], "pattern not found: foo   ");
    }

    #[test]
    fn search_smart_case() {
        let mut app = scrollable_app(&SEARCH_LINES, 100);
        render(&app, 8, 4);
        search(&mut app, "Foo");
        assert_eq!(app.panes[0].last_match, Some(5));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.panes[0].last_match, Some(5));
        assert_eq!(app.message.as_deref(), Some("pattern not found: Foo"));
    }

    #[test]
    fn search_from_scrolled_view() {
        let mut app = scrollable_app(&SEARCH_LINES, 100);
        render(&app, 8, 4);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        render(&app, 8, 4);

        // The search starts from the bottom of the view, i.e. "baz".
        search(&mut app, "ba");
        assert_eq!(app.panes[0].last_match, Some(3));

        // Scrolling makes the next search start from the view again.
        search(&mut app, "foo");
        assert_eq!(app.panes[0].last_match, Some(2));
        press(&mut app, KeyCode::End);
        render(&app, 8, 4);
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.panes[0].last_match, Some(5));
    }

    #[test]
    fn search_highlighting() {
        let mut app = scrollable_app(&SEARCH_LINES, 100);
        render(&app, 8, 4);
        search(&mut app, "a");

        let mut terminal = Terminal::new(TestBackend::new(8, 4)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer();
        let highlighted = |x: u16, y: u16| buf[(x, y)].bg == Color::Yellow;
        // "baz" is shown in the first row.
        assert_eq!(
            [highlighted(0, 0), highlighted(1, 0), highlighted(2, 0)],
            [false, true, false]
        );
        assert!(!highlighted(0, 1));

        // Esc stops highlighting.
        assert_eq!(press(&mut app, KeyCode::Esc), Response::Redraw);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_ne!(terminal.backend().buffer()[(1, 0)].bg, Color::Yellow);
    }

    #[test]
    fn search_prompt() {
        let mut app = scrollable_app(&SEARCH_LINES, 100);
        render(&app, 8, 4);

        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "bax");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(render(&app, 8, 4)[3], "/ba     ");

        // Keys normally doing something else are just typed.
        type_text(&mut app, "qn/");
        assert_eq!(app.prompt.as_deref(), Some("baqn/"));

        // Esc and Ctrl-C cancel the search, without quitting.
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.prompt, None);
        assert!(app.search.is_none());
        press(&mut app, KeyCode::Char('/'));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_event(&TermEvent::Key(ctrl_c)), Response::Redraw);
        assert_eq!(app.prompt, None);

        // Erasing everything cancels too.
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "x");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.prompt.as_deref(), Some(""));
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.prompt, None);
    }

    #[test]
    fn prompt_and_message_replace_status() {
        let mut app = make_app(&[("long_file_name", None)], 100);
        add_lines(&mut app, 0, &["foo"]);
        assert_eq!(render(&app, 12, 2)[1], " long_file_n");

        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "ba");
        assert_eq!(render(&app, 12, 2)[1], "/ba         ");

        press(&mut app, KeyCode::Enter);
        assert_eq!(render(&app, 22, 2)[1], "pattern not found: ba ");

        press(&mut app, KeyCode::Esc);
        assert_eq!(render(&app, 12, 2)[1], " long_file_n");
    }

    #[test]
    fn search_repeated_with_empty_pattern() {
        let mut app = scrollable_app(&SEARCH_LINES, 100);
        render(&app, 8, 4);

        // Nothing to repeat yet.
        search(&mut app, "");
        assert!(app.search.is_none());
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.message.as_deref(), Some("no search pattern"));

        search(&mut app, "bar");
        assert_eq!(app.panes[0].last_match, Some(2));
        press(&mut app, KeyCode::End);
        render(&app, 8, 4);
        search(&mut app, "");
        assert_eq!(app.panes[0].last_match, Some(2));
    }

    #[test]
    fn search_errors() {
        let mut app = scrollable_app(&SEARCH_LINES, 100);
        render(&app, 8, 4);
        search(&mut app, "(foo");
        assert_eq!(
            app.message.as_deref(),
            Some("invalid regex: unclosed group")
        );
        assert_eq!(render(&app, 32, 4)[3], "invalid regex: unclosed group   ");

        // Any key makes the message disappear.
        assert_eq!(press(&mut app, KeyCode::Char('x')), Response::Redraw);
        assert_eq!(app.message, None);
        assert_eq!(press(&mut app, KeyCode::Char('x')), Response::Nothing);
    }

    #[test]
    fn last_match_kept_when_lines_dropped() {
        let mut app = scrollable_app(&SEARCH_LINES, 6);
        render(&app, 8, 4);
        search(&mut app, "foo bar");
        assert_eq!(app.panes[0].last_match, Some(2));

        add_lines(&mut app, 0, &["new 1", "new 2"]);
        assert_eq!(app.panes[0].last_match, Some(0));
        add_lines(&mut app, 0, &["new 3"]);
        assert_eq!(app.panes[0].last_match, None);
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
        assert_eq!(
            action(&key(KeyCode::Up, none)),
            Some(Action::Scroll(Scroll::LineUp))
        );
        assert_eq!(
            action(&key(KeyCode::PageDown, none)),
            Some(Action::Scroll(Scroll::PageDown))
        );
        assert_eq!(
            action(&key(KeyCode::Home, none)),
            Some(Action::Scroll(Scroll::Top))
        );
        assert_eq!(
            action(&key(KeyCode::End, none)),
            Some(Action::Scroll(Scroll::Bottom))
        );
        assert_eq!(
            action(&key(KeyCode::Tab, none)),
            Some(Action::Focus { forward: true })
        );
        assert_eq!(
            action(&key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Action::Focus { forward: false })
        );
        assert_eq!(
            action(&key(KeyCode::Char('/'), none)),
            Some(Action::StartSearch)
        );
        assert_eq!(
            action(&key(KeyCode::Char('n'), none)),
            Some(Action::FindNext { older: true })
        );
        assert_eq!(
            action(&key(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            Some(Action::FindNext { older: false })
        );
        assert_eq!(action(&key(KeyCode::Esc, none)), Some(Action::Cancel));
        assert_eq!(action(&TermEvent::FocusGained), None);
    }
}
