//! Following a file by name, as `tail -F` does.
//!
//! The file is polled: each call to [`Follower::poll`] reads whatever was
//! appended to it since the previous call and then checks whether the file
//! was replaced by another one (e.g. rotated) or truncated, in which case it
//! starts reading the new contents from the beginning. A missing file is
//! waited for and I/O errors are reported, but neither stops the follower.
//!
//! Since polling is used, a file truncated and then grown beyond the previous
//! read position between two calls to `poll()` can't be detected as having
//! been truncated.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::mem;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Maximal length of a line in bytes: longer lines are split into parts.
pub const MAX_LINE: usize = 64 * 1024;

/// Maximal number of bytes read by a single call to [`Follower::poll`].
const MAX_READ: usize = 4 * 1024 * 1024;

/// Size of the blocks in which files are read.
const READ_CHUNK: usize = 64 * 1024;

/// Something happening to the followed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A complete line, without the trailing new line character.
    Line(Vec<u8>),
    /// A part of a line longer than [`MAX_LINE`], to be continued by more
    /// parts and ending with a complete line.
    LongLinePart(Vec<u8>),
    /// The file was replaced by another file with the same name, which is
    /// now read from its beginning.
    Replaced,
    /// The file was truncated and is now read from its beginning again.
    Truncated,
}

/// State of the followed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The file is being read normally.
    Following,
    /// No file with this name exists. If it existed before, it is still read
    /// in case something is still writing to it.
    Missing,
    /// An error occurred, which can be temporary.
    Error(String),
}

/// The currently open file.
#[derive(Debug)]
struct OpenFile {
    file: File,
    dev: u64,
    ino: u64,
    /// Number of bytes read from the file so far.
    offset: u64,
}

/// Follows the file with the given name.
#[derive(Debug)]
pub struct Follower {
    path: PathBuf,
    /// Number of lines at the end of the file to read when opening it with
    /// `tail_on_open` set.
    initial_lines: usize,
    /// True until the file is opened for the first time or is found to be
    /// missing: a file created after we started must be read entirely.
    tail_on_open: bool,
    file: Option<OpenFile>,
    /// Incomplete last line.
    partial: Vec<u8>,
    status: Status,
    /// True if not everything could be read during the last poll.
    has_more: bool,
    size: Option<u64>,
    modified: Option<SystemTime>,
    /// Total number of bytes read, only used for testing.
    bytes_read: u64,
    /// Buffer used for reading, kept to avoid allocating it every time.
    buf: Vec<u8>,
}

impl Follower {
    /// Creates a follower for the given file. Nothing is done until
    /// [`Follower::poll`] is called.
    ///
    /// If the file exists, only its last `initial_lines` lines are read
    /// initially.
    pub fn new(path: impl Into<PathBuf>, initial_lines: usize) -> Self {
        Self {
            path: path.into(),
            initial_lines,
            tail_on_open: true,
            file: None,
            partial: Vec::new(),
            status: Status::Following,
            has_more: false,
            size: None,
            modified: None,
            bytes_read: 0,
            buf: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Returns the incomplete last line read so far, which may be empty.
    pub fn partial(&self) -> &[u8] {
        &self.partial
    }

    /// Returns true if not everything available could be read during the
    /// last poll, which should then be repeated soon.
    pub fn has_more(&self) -> bool {
        self.has_more
    }

    /// Size of the file being read, if any.
    pub fn size(&self) -> Option<u64> {
        self.size
    }

    /// Last modification time of the file being read, if any.
    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    /// Reads the new contents of the file and returns what happened to it.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut budget = MAX_READ;

        // Don't check if the file was replaced or truncated before reading
        // everything from it.
        let mut done = self.read_available(&mut events, &mut budget);
        if done {
            done = self.check_path(&mut events, &mut budget);
        }
        self.has_more = !done;

        let metadata = self.file.as_ref().and_then(|f| f.file.metadata().ok());
        self.size = metadata.as_ref().map(fs::Metadata::len);
        self.modified = metadata.and_then(|m| m.modified().ok());

        events
    }

    /// Reads from the open file, if any, until its end or until the budget
    /// is exhausted. Returns false only in the latter case.
    fn read_available(&mut self, events: &mut Vec<Event>, budget: &mut usize) -> bool {
        let Some(file) = &mut self.file else {
            return true;
        };

        let buf = &mut self.buf;
        buf.resize(READ_CHUNK, 0);
        loop {
            if *budget == 0 {
                return false;
            }

            let max = (*budget).min(READ_CHUNK) as u64;
            match file.file.by_ref().take(max).read(buf) {
                Ok(0) => return true,
                Ok(n) => {
                    *budget = budget.saturating_sub(n);
                    file.offset += n as u64;
                    self.bytes_read += n as u64;
                    split_lines(&mut self.partial, buf.get(..n).unwrap_or_default(), events);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.status = Status::Error(format!("read error: {e}"));
                    return true;
                }
            }
        }
    }

    /// Checks whether the file was replaced or truncated and handles it if
    /// it was, also opening the file if it's not open yet. Returns false if
    /// the budget was exhausted before reading everything.
    fn check_path(&mut self, events: &mut Vec<Event>, budget: &mut usize) -> bool {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.status = Status::Missing;
                self.tail_on_open = false;
                return true;
            }
            Err(e) => {
                self.status = Status::Error(e.to_string());
                return true;
            }
        };

        if !metadata.is_file() {
            self.status = Status::Error("not a regular file".to_owned());
            return true;
        }

        match &mut self.file {
            None => return self.open(events, budget),
            Some(file) if (metadata.dev(), metadata.ino()) != (file.dev, file.ino) => {
                // The data appended since our last read must not be lost.
                if !self.read_available(events, budget) {
                    return false;
                }
                self.flush_partial(events);
                events.push(Event::Replaced);
                self.file = None;
                return self.open(events, budget);
            }
            Some(file) if metadata.len() < file.offset => {
                if let Err(e) = file.file.seek(SeekFrom::Start(0)) {
                    self.status = Status::Error(format!("seek error: {e}"));
                    return true;
                }
                file.offset = 0;
                self.flush_partial(events);
                events.push(Event::Truncated);
            }
            Some(_) => {}
        }

        self.status = Status::Following;
        self.read_available(events, budget)
    }

    /// Opens the file and reads from it.
    fn open(&mut self, events: &mut Vec<Event>, budget: &mut usize) -> bool {
        match self.try_open() {
            Ok(file) => {
                self.file = Some(file);
                self.status = Status::Following;
                self.read_available(events, budget)
            }
            Err(e) => {
                self.status = Status::Error(e.to_string());
                true
            }
        }
    }

    fn try_open(&mut self) -> io::Result<OpenFile> {
        // Don't block if the file was replaced by a FIFO since we checked it.
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.path)?;

        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::other("not a regular file"));
        }

        let len = metadata.len();
        let start = if self.tail_on_open {
            find_tail_start(&file, len, self.initial_lines)?
        } else {
            0
        };
        file.seek(SeekFrom::Start(start))?;
        self.tail_on_open = false;

        Ok(OpenFile {
            file,
            dev: metadata.dev(),
            ino: metadata.ino(),
            offset: start,
        })
    }

    /// Returns the incomplete last line, if any, as a complete line.
    fn flush_partial(&mut self, events: &mut Vec<Event>) {
        if !self.partial.is_empty() {
            events.push(Event::Line(mem::take(&mut self.partial)));
        }
    }
}

/// Splits the data into lines, using `partial` for the incomplete last line.
fn split_lines(partial: &mut Vec<u8>, data: &[u8], events: &mut Vec<Event>) {
    for piece in data.split_inclusive(|&b| b == b'\n') {
        match piece.strip_suffix(b"\n") {
            Some(end) => {
                append_to_line(partial, end, events);
                events.push(Event::Line(mem::take(partial)));
            }
            None => append_to_line(partial, piece, events),
        }
    }
}

/// Appends data to the incomplete line, emitting parts of it if it becomes
/// too long.
fn append_to_line(partial: &mut Vec<u8>, mut data: &[u8], events: &mut Vec<Event>) {
    while partial.len() + data.len() > MAX_LINE {
        let room = MAX_LINE.saturating_sub(partial.len());
        let Some((head, tail)) = data.split_at_checked(room) else {
            break;
        };
        partial.extend_from_slice(head);
        data = tail;

        let rest = partial.split_off(cut_position(partial));
        events.push(Event::LongLinePart(mem::replace(partial, rest)));
    }
    partial.extend_from_slice(data);
}

/// Returns the position at which to cut a too long line: at its end, unless
/// this would split a UTF-8 sequence, in which case it is cut before it.
fn cut_position(line: &[u8]) -> usize {
    let len = line.len();
    for back in 1..=len.min(3) {
        let pos = len - back;
        let Some(&b) = line.get(pos) else { break };
        if b & 0xC0 == 0x80 {
            // Continuation byte, look further back for the start.
            continue;
        }
        let char_len = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if char_len > back && pos > 0 { pos } else { len };
    }
    len
}

/// Returns the offset of the start of the last `lines` lines of the file of
/// the given length. An incomplete last line counts as a line.
///
/// To avoid reading too much of a file without new lines, the result is
/// never more than `lines * MAX_LINE` before its end.
fn find_tail_start(file: &File, len: u64, lines: usize) -> io::Result<u64> {
    if lines == 0 {
        return Ok(len);
    }

    let max_back = (lines as u64).saturating_mul(MAX_LINE as u64);
    let limit = len.saturating_sub(max_back);
    let mut buf = vec![0; READ_CHUNK];
    let mut found = 0;
    let mut end = len;
    while end > limit {
        let start = end.saturating_sub(READ_CHUNK as u64).max(limit);
        let chunk = buf
            .get_mut(..(end - start) as usize)
            .ok_or_else(|| io::Error::other("unexpected chunk size"))?;
        file.read_exact_at(chunk, start)?;

        for (index, &b) in chunk.iter().enumerate().rev() {
            let pos = start + index as u64;
            // The new line at the very end terminates the last line.
            if b == b'\n' && pos + 1 != len {
                found += 1;
                if found == lines {
                    return Ok(pos + 1);
                }
            }
        }
        end = start;
    }

    Ok(limit)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use proptest::prelude::*;

    use super::*;

    fn line(s: &str) -> Event {
        Event::Line(s.as_bytes().to_vec())
    }

    fn lines(range: impl IntoIterator<Item = usize>) -> Vec<Event> {
        range
            .into_iter()
            .map(|n| line(&format!("line {n}")))
            .collect()
    }

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    fn append(path: &Path, contents: &[u8]) {
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(contents).unwrap();
    }

    fn numbered_lines(count: usize) -> String {
        (1..=count).map(|n| format!("line {n}\n")).collect()
    }

    #[test]
    fn initial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, &numbered_lines(10));

        let mut f = Follower::new(&path, 3);
        assert_eq!(f.poll(), lines(8..=10));
        assert_eq!(f.status(), &Status::Following);
        assert_eq!(f.size(), Some(numbered_lines(10).len() as u64));
        assert!(f.modified().is_some());
        assert_eq!(f.poll(), []);

        // More lines than in the file.
        assert_eq!(Follower::new(&path, 20).poll(), lines(1..=10));
        // Exactly as many.
        assert_eq!(Follower::new(&path, 10).poll(), lines(1..=10));
        // No lines at all.
        assert_eq!(Follower::new(&path, 0).poll(), []);
    }

    #[test]
    fn initial_lines_with_incomplete_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "a\nb\nc");

        let mut f = Follower::new(&path, 2);
        assert_eq!(f.poll(), [line("b")]);
        assert_eq!(f.partial(), b"c");
    }

    #[test]
    fn initial_lines_with_empty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "a\n\n\nb\n");
        assert_eq!(
            Follower::new(&path, 3).poll(),
            [line(""), line(""), line("b")]
        );
    }

    #[test]
    fn initial_lines_of_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "");
        assert_eq!(Follower::new(&path, 3).poll(), []);
    }

    #[test]
    fn initial_lines_of_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let contents = numbered_lines(200_000);
        write(&path, &contents);

        let mut f = Follower::new(&path, 2);
        assert_eq!(f.poll(), lines(199_999..=200_000));
        assert!(f.bytes_read < 100, "read {} bytes", f.bytes_read);
    }

    #[test]
    fn initial_lines_without_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let contents = "x".repeat(10 * MAX_LINE);
        write(&path, &contents);

        // Only the last 2 * MAX_LINE bytes are read.
        let mut f = Follower::new(&path, 2);
        let events = f.poll();
        assert_eq!(f.bytes_read, 2 * MAX_LINE as u64);
        assert_eq!(events, [Event::LongLinePart(vec![b'x'; MAX_LINE])]);
        assert_eq!(f.partial().len(), MAX_LINE);
    }

    #[test]
    fn appended_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "old\n");

        let mut f = Follower::new(&path, 0);
        assert_eq!(f.poll(), []);

        append(&path, b"new 1\nnew 2\n");
        assert_eq!(f.poll(), [line("new 1"), line("new 2")]);
        assert_eq!(f.poll(), []);
    }

    #[test]
    fn partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "");

        let mut f = Follower::new(&path, 10);
        assert_eq!(f.poll(), []);

        append(&path, b"abc");
        assert_eq!(f.poll(), []);
        assert_eq!(f.partial(), b"abc");

        append(&path, b"def\nghi");
        assert_eq!(f.poll(), [line("abcdef")]);
        assert_eq!(f.partial(), b"ghi");

        append(&path, b"\n");
        assert_eq!(f.poll(), [line("ghi")]);
        assert_eq!(f.partial(), b"");
    }

    #[test]
    fn crlf_and_binary_data_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "");

        let mut f = Follower::new(&path, 10);
        f.poll();
        append(&path, b"dos\r\n\0\xff\x1b\n");
        assert_eq!(
            f.poll(),
            [
                Event::Line(b"dos\r".to_vec()),
                Event::Line(b"\0\xff\x1b".to_vec())
            ]
        );
    }

    #[test]
    fn rotation_by_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "1\n");

        let mut f = Follower::new(&path, 10);
        assert_eq!(f.poll(), [line("1")]);

        // Data appended just before the rotation is not lost, even if the
        // last line is incomplete.
        append(&path, b"2\n3");
        fs::rename(&path, dir.path().join("log.1")).unwrap();
        write(&path, "new\n");

        assert_eq!(
            f.poll(),
            [line("2"), line("3"), Event::Replaced, line("new")]
        );
        assert_eq!(f.status(), &Status::Following);
        assert_eq!(f.size(), Some(4));
    }

    #[test]
    fn rotation_by_copy_and_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "1\n2\n3\n");

        let mut f = Follower::new(&path, 10);
        assert_eq!(f.poll(), lines_of(&["1", "2", "3"]));

        fs::copy(&path, dir.path().join("log.1")).unwrap();
        write(&path, "");
        assert_eq!(f.poll(), [Event::Truncated]);

        append(&path, b"new\n");
        assert_eq!(f.poll(), [line("new")]);
    }

    #[test]
    fn truncated_and_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "some long line\n");

        let mut f = Follower::new(&path, 10);
        f.poll();

        // Shorter than before, so the truncation is detected.
        write(&path, "short\n");
        assert_eq!(f.poll(), [Event::Truncated, line("short")]);
    }

    #[test]
    fn missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");

        let mut f = Follower::new(&path, 1);
        assert_eq!(f.poll(), []);
        assert_eq!(f.status(), &Status::Missing);
        assert_eq!(f.size(), None);

        // The file created later is read entirely, even if only 1 line was
        // initially requested.
        write(&path, "a\nb\n");
        assert_eq!(f.poll(), lines_of(&["a", "b"]));
        assert_eq!(f.status(), &Status::Following);
    }

    #[test]
    fn deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "a\n");

        let mut f = Follower::new(&path, 10);
        assert_eq!(f.poll(), [line("a")]);

        // The file is still read after being deleted.
        let mut old = OpenOptions::new().append(true).open(&path).unwrap();
        fs::remove_file(&path).unwrap();
        old.write_all(b"b\n").unwrap();
        assert_eq!(f.poll(), [line("b")]);
        assert_eq!(f.status(), &Status::Missing);

        write(&path, "c\n");
        assert_eq!(f.poll(), [Event::Replaced, line("c")]);
        assert_eq!(f.status(), &Status::Following);
    }

    #[test]
    fn long_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "");

        let mut f = Follower::new(&path, 10);
        f.poll();

        let mut data = vec![b'x'; 2 * MAX_LINE + 10];
        data.push(b'\n');
        append(&path, &data);
        assert_eq!(
            f.poll(),
            [
                Event::LongLinePart(vec![b'x'; MAX_LINE]),
                Event::LongLinePart(vec![b'x'; MAX_LINE]),
                Event::Line(vec![b'x'; 10]),
            ]
        );
        assert_eq!(f.partial(), b"");

        // A line of exactly the maximal length is not split.
        let mut data = vec![b'y'; MAX_LINE];
        data.push(b'\n');
        append(&path, &data);
        assert_eq!(f.poll(), [Event::Line(vec![b'y'; MAX_LINE])]);
    }

    #[test]
    fn long_lines_are_not_cut_inside_characters() {
        let mut partial = Vec::new();
        let mut events = Vec::new();

        // "é" is 2 bytes, so with the leading "a" the limit is in its middle.
        let mut data = b"a".to_vec();
        data.extend("é".repeat(MAX_LINE / 2).as_bytes());
        split_lines(&mut partial, &data, &mut events);

        let [Event::LongLinePart(part)] = events.as_slice() else {
            panic!("unexpected events: {events:?}");
        };
        assert_eq!(part.len(), MAX_LINE - 1);
        assert!(std::str::from_utf8(part).is_ok());
        assert_eq!(partial, "é".as_bytes());
    }

    #[test]
    fn cut_positions() {
        assert_eq!(cut_position(b"abc"), 3);
        assert_eq!(cut_position("abé".as_bytes()), 4);
        assert_eq!(cut_position(&"abé".as_bytes()[..3]), 2);
        assert_eq!(cut_position(&"a日".as_bytes()[..2]), 1);
        assert_eq!(cut_position(&"a日".as_bytes()[..3]), 1);
        assert_eq!(cut_position("a日".as_bytes()), 4);
        assert_eq!(cut_position(&"a😀".as_bytes()[..4]), 1);
        // Invalid UTF-8 is cut anywhere.
        assert_eq!(cut_position(b"a\x80\x80\x80"), 4);
        assert_eq!(cut_position(b"\xff\xff"), 2);
        // Never cut at the very beginning.
        assert_eq!(cut_position(&"日".as_bytes()[..2]), 2);
    }

    #[test]
    fn read_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "");

        let mut f = Follower::new(&path, 10);
        f.poll();

        let count = 2 * MAX_READ / 8;
        let contents = "1234567\n".repeat(count);
        append(&path, contents.as_bytes());

        let events = f.poll();
        assert_eq!(events.len(), MAX_READ / 8);
        assert!(f.has_more());

        let events = f.poll();
        assert_eq!(events.len(), MAX_READ / 8);

        // There is no more data, but we didn't know it yet.
        assert_eq!(f.poll(), []);
        assert!(!f.has_more());
    }

    #[test]
    fn rotation_with_more_than_read_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        write(&path, "");

        let mut f = Follower::new(&path, 10);
        f.poll();

        append(&path, "1234567\n".repeat(MAX_READ / 8 + 1).as_bytes());
        fs::rename(&path, dir.path().join("log.1")).unwrap();
        write(&path, "new\n");

        // The old file must be read entirely before switching to the new one.
        let events = f.poll();
        assert_eq!(events.len(), MAX_READ / 8);
        assert!(f.has_more());
        assert_eq!(f.poll(), [line("1234567"), Event::Replaced, line("new")]);
    }

    #[test]
    fn not_regular_files() {
        let dir = tempfile::tempdir().unwrap();

        let mut f = Follower::new(dir.path(), 10);
        assert_eq!(f.poll(), []);
        assert_eq!(f.status(), &Status::Error("not a regular file".to_owned()));

        // Opening a FIFO would block, so it must not be done.
        let fifo = dir.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let mut f = Follower::new(&fifo, 10);
        assert_eq!(f.poll(), []);
        assert_eq!(f.status(), &Status::Error("not a regular file".to_owned()));

        // And the same for a device which never ends.
        let mut f = Follower::new("/dev/zero", 10);
        assert_eq!(f.poll(), []);
        assert_eq!(f.status(), &Status::Error("not a regular file".to_owned()));
    }

    #[test]
    fn error_then_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        fs::create_dir(&path).unwrap();

        let mut f = Follower::new(&path, 1);
        f.poll();
        assert!(matches!(f.status(), Status::Error(_)));

        // The file existed when we started, so only its end is read.
        fs::remove_dir(&path).unwrap();
        write(&path, "a\nb\n");
        assert_eq!(f.poll(), [line("b")]);
        assert_eq!(f.status(), &Status::Following);
    }

    fn lines_of(lines: &[&str]) -> Vec<Event> {
        lines.iter().map(|s| line(s)).collect()
    }

    /// Generates data with lines of very different lengths, including ones
    /// longer than MAX_LINE, containing multibyte characters.
    fn data_with_long_lines() -> impl Strategy<Value = Vec<u8>> {
        let piece = prop_oneof![
            Just(b"\n".to_vec()),
            any::<u8>().prop_map(|b| vec![b]),
            prop::sample::select(vec!["é", "日", "😀", "x"])
                .prop_flat_map(|s| (Just(s), 0..MAX_LINE / 2))
                .prop_map(|(s, n)| s.repeat(n).into_bytes()),
        ];
        prop::collection::vec(piece, 0..12).prop_map(|pieces| pieces.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn split_lines_invariants(
            data in data_with_long_lines(),
            chunk_size in 1..3 * MAX_LINE,
        ) {
            let mut partial = Vec::new();
            let mut events = Vec::new();
            for chunk in data.chunks(chunk_size) {
                split_lines(&mut partial, chunk, &mut events);
            }

            let mut rebuilt = Vec::new();
            for event in &events {
                match event {
                    Event::Line(line) => {
                        prop_assert!(line.len() <= MAX_LINE);
                        prop_assert!(!line.contains(&b'\n'));
                        rebuilt.extend_from_slice(line);
                        rebuilt.push(b'\n');
                    }
                    Event::LongLinePart(part) => {
                        prop_assert!(part.len() <= MAX_LINE);
                        prop_assert!(part.len() >= MAX_LINE - 3);
                        prop_assert!(!part.contains(&b'\n'));
                        // Valid UTF-8 is never cut in the middle of a
                        // character, i.e. there is no incomplete one at
                        // the end.
                        if let Err(e) = std::str::from_utf8(part) {
                            prop_assert!(e.error_len().is_some());
                        }
                        rebuilt.extend_from_slice(part);
                    }
                    other => prop_assert!(false, "unexpected event {other:?}"),
                }
            }
            prop_assert!(partial.len() <= MAX_LINE);
            rebuilt.extend_from_slice(&partial);
            prop_assert_eq!(rebuilt, data);
        }
    }
}
