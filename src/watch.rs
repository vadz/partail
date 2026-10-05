//! Being notified when the followed files change, using inotify.
//!
//! The directories containing the files are watched rather than the files
//! themselves, as this allows to detect the files being created, deleted or
//! renamed, as happens when they're rotated, in addition to being modified.
//!
//! Note that inotify doesn't report changes done by other machines to files
//! on network file systems, so the files still need to be polled from time
//! to time even when they're watched.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::Path;
use std::thread;

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

/// What happened to the watched files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notification {
    /// One of the files may have changed.
    Changed,
    /// Watching the files stopped working, e.g. because one of the watched
    /// directories was removed, so the files must be polled frequently. No
    /// more notifications are sent after this one.
    Failed,
}

/// Watched directory and the names of the files in it we're interested in.
struct Watched {
    wd: WatchDescriptor,
    names: Vec<OsString>,
}

/// Starts watching the given files in a background thread, which calls
/// `notify` whenever any of them may have changed.
///
/// Returns true if all of them are watched and false if some couldn't be,
/// e.g. because the directory containing them doesn't exist, in which case
/// they need to be polled frequently.
pub fn watch<P: AsRef<Path>>(
    files: &[P],
    notify: impl FnMut(Notification) + Send + 'static,
) -> io::Result<bool> {
    let inotify = Inotify::init()?;
    let mask = WatchMask::MODIFY
        | WatchMask::ATTRIB
        | WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
        | WatchMask::ONLYDIR;

    let mut watched: Vec<Watched> = Vec::new();
    let mut all = true;
    let mut add = |path: &Path| {
        let name = path.file_name().map(OsStr::to_owned);
        let dir = match path.parent() {
            Some(dir) if dir.as_os_str().is_empty() => Path::new("."),
            Some(dir) => dir,
            None => return false,
        };
        let (Some(name), Ok(wd)) = (name, inotify.watches().add(dir, mask)) else {
            return false;
        };
        match watched.iter_mut().find(|w| w.wd == wd) {
            Some(w) => w.names.push(name),
            None => watched.push(Watched {
                wd,
                names: vec![name],
            }),
        }
        true
    };

    for file in files {
        let file = file.as_ref();
        all &= add(file);

        // If the file is a symlink, the file it points to is modified rather
        // than the symlink itself, so watch the directory containing it too.
        if let Ok(target) = fs::canonicalize(file)
            && target != file
        {
            add(&target);
        }
    }

    thread::spawn(move || run(inotify, &watched, notify));

    Ok(all)
}

/// Waits for the events and calls `notify` for those concerning the files.
fn run(mut inotify: Inotify, watched: &[Watched], mut notify: impl FnMut(Notification)) {
    let is_watched = |wd: &WatchDescriptor, name: &OsStr| {
        watched
            .iter()
            .any(|w| w.wd == *wd && w.names.iter().any(|n| n == name))
    };

    let mut buf = vec![0; 4096];
    loop {
        let events = match inotify.read_events_blocking(&mut buf) {
            Ok(events) => events,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                notify(Notification::Failed);
                return;
            }
        };

        let mut changed = false;
        for event in events {
            if event.mask.contains(EventMask::Q_OVERFLOW) {
                // Some events were lost, so anything could have happened.
                changed = true;
            } else if event
                .mask
                .intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF | EventMask::IGNORED)
            {
                // The directory itself is gone, so we can't watch it any more.
                notify(Notification::Failed);
                return;
            } else if let Some(name) = event.name
                && is_watched(&event.wd, name)
            {
                changed = true;
            }
        }

        if changed {
            notify(Notification::Changed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);
    const SHORT_WAIT: Duration = Duration::from_millis(200);

    fn start(files: &[&Path]) -> (mpsc::Receiver<Notification>, bool) {
        let (tx, rx) = mpsc::channel();
        let all = watch(files, move |n| {
            let _ = tx.send(n);
        })
        .unwrap();
        (rx, all)
    }

    /// Waits for a notification, ignoring any duplicates of it.
    fn expect(rx: &mpsc::Receiver<Notification>, expected: Notification) {
        assert_eq!(rx.recv_timeout(WAIT), Ok(expected));
        while rx.recv_timeout(SHORT_WAIT).is_ok() {}
    }

    fn expect_nothing(rx: &mpsc::Receiver<Notification>) {
        assert!(rx.recv_timeout(SHORT_WAIT).is_err());
    }

    #[test]
    fn changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        fs::write(&path, "").unwrap();

        let (rx, all) = start(&[&path]);
        assert!(all);
        expect_nothing(&rx);

        // Appending.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"line\n").unwrap();
        expect(&rx, Notification::Changed);

        // Rotation.
        fs::rename(&path, dir.path().join("log.1")).unwrap();
        expect(&rx, Notification::Changed);
        fs::write(&path, "new\n").unwrap();
        expect(&rx, Notification::Changed);

        // Deletion.
        fs::remove_file(&path).unwrap();
        expect(&rx, Notification::Changed);
    }

    #[test]
    fn other_files_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let (rx, _) = start(&[&path]);

        fs::write(dir.path().join("other"), "data").unwrap();
        expect_nothing(&rx);

        // But the file being created is noticed.
        fs::write(&path, "data").unwrap();
        expect(&rx, Notification::Changed);
    }

    #[test]
    fn several_files_in_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        let (rx, all) = start(&[&one, &two]);
        assert!(all);

        fs::write(&two, "data").unwrap();
        expect(&rx, Notification::Changed);
        fs::write(&one, "data").unwrap();
        expect(&rx, Notification::Changed);
    }

    #[test]
    fn symlink_target_watched() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let target = real_dir.join("log");
        fs::write(&target, "").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (rx, _) = start(&[&link]);
        let mut f = fs::OpenOptions::new().append(true).open(&target).unwrap();
        f.write_all(b"line\n").unwrap();
        expect(&rx, Notification::Changed);
    }

    #[test]
    fn missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (_rx, all) = start(&[&dir.path().join("nonexistent/log")]);
        assert!(!all);
    }

    #[test]
    fn directory_removed() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let (rx, _) = start(&[&sub.join("log")]);

        fs::remove_dir(&sub).unwrap();
        expect(&rx, Notification::Failed);
    }
}
