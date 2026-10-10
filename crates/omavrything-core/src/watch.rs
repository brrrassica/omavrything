//! inotify-based live filesystem watching.
//!
//! Watches directories recursively and translates inotify events into
//! [`FsEvent`]s. `IN_MOVED_FROM`/`IN_MOVED_TO` pairs are matched by cookie so a
//! rename becomes a single [`FsEvent::Renamed`]. A `MOVED_FROM` with no matching
//! `MOVED_TO` (a move *out* of the watched tree) is emitted as a removal once the
//! pairing window elapses, so moved-out entries do not linger in the index.
//! Watch-limit exhaustion (`ENOSPC`) is recorded rather than fatal, so the
//! periodic rescan can act as the safety net.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::live::FsEvent;

/// How long to wait for a `MOVED_TO` to pair with a `MOVED_FROM` before treating
/// the move as a removal. In-tree moves pair within the same read batch, so this
/// only affects moves out of the watched tree.
const MOVE_PAIR_TIMEOUT: Duration = Duration::from_secs(1);

/// The events the watcher subscribes to.
fn watch_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::CLOSE_WRITE
        | WatchMask::ATTRIB
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
}

/// A recursive inotify watcher.
pub struct Watcher {
    inotify: Inotify,
    dirs: HashMap<WatchDescriptor, PathBuf>,
    pending_moves: HashMap<u32, (PathBuf, Instant)>,
    failed: Vec<PathBuf>,
}

impl Watcher {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            inotify: Inotify::init()?,
            dirs: HashMap::new(),
            pending_moves: HashMap::new(),
            failed: Vec::new(),
        })
    }

    /// Recursively watch `dir` and all of its subdirectories.
    ///
    /// Iterative (not recursive) so a deep tree cannot overflow the stack.
    pub fn watch_dir(&mut self, dir: &Path) -> io::Result<()> {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            match self.inotify.watches().add(&current, watch_mask()) {
                Ok(wd) => {
                    self.dirs.insert(wd, current.clone());
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                    // max_user_watches exhausted: record and keep going.
                    self.failed.push(current);
                    continue;
                }
                Err(e) => return Err(e),
            }

            if let Ok(rd) = fs::read_dir(&current) {
                for entry in rd.flatten() {
                    if entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                        stack.push(entry.path());
                    }
                }
            }
        }
        Ok(())
    }

    /// Directories that could not be watched (watch-limit exhaustion).
    pub fn failed(&self) -> &[PathBuf] {
        &self.failed
    }

    /// The number of directories currently watched.
    pub fn watch_count(&self) -> usize {
        self.dirs.len()
    }

    /// Wait up to `timeout` for events, then translate them.
    ///
    /// Pending moves older than [`MOVE_PAIR_TIMEOUT`] are emitted as removals
    /// before returning, so a move out of the watched tree is reflected even if
    /// no further events arrive.
    pub fn read_events(
        &mut self,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> io::Result<Vec<FsEvent>> {
        let mut out = Vec::new();
        self.expire_pending(&mut out);

        if !self.poll_ready(timeout)? {
            return Ok(out);
        }

        match self.inotify.read_events(buffer) {
            Ok(events) => {
                for event in events {
                    self.translate(event, &mut out);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }

        Ok(out)
    }

    /// Poll the inotify fd for readability, up to `timeout`.
    fn poll_ready(&self, timeout: Duration) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.inotify.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `pfd` is a valid, single-element pollfd array.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rc > 0)
    }

    /// Emit removals for moves that never paired with a `MOVED_TO`.
    fn expire_pending(&mut self, out: &mut Vec<FsEvent>) {
        let now = Instant::now();
        let expired: Vec<u32> = self
            .pending_moves
            .iter()
            .filter(|(_, (_, at))| now.duration_since(*at) >= MOVE_PAIR_TIMEOUT)
            .map(|(cookie, _)| *cookie)
            .collect();
        for cookie in expired {
            if let Some((path, _)) = self.pending_moves.remove(&cookie) {
                out.push(FsEvent::Removed(path));
            }
        }
    }

    /// Translate one raw inotify event into zero or more [`FsEvent`]s.
    fn translate(&mut self, event: inotify::Event<&std::ffi::OsStr>, out: &mut Vec<FsEvent>) {
        // The watch was removed (e.g. the directory was deleted): forget it.
        if event.mask.contains(EventMask::IGNORED) {
            self.dirs.remove(&event.wd);
            return;
        }

        // The watched directory itself was deleted or moved away.
        if event.mask.contains(EventMask::DELETE_SELF) || event.mask.contains(EventMask::MOVE_SELF)
        {
            if let Some(dir) = self.dirs.remove(&event.wd) {
                out.push(FsEvent::Removed(dir));
            }
            return;
        }

        let Some(dir) = self.dirs.get(&event.wd) else {
            return;
        };
        let path = match event.name {
            Some(name) => dir.join(name),
            None => dir.clone(),
        };

        if event.mask.contains(EventMask::MOVED_FROM) {
            if event.cookie != 0 {
                self.pending_moves
                    .insert(event.cookie, (path, Instant::now()));
            } else {
                out.push(FsEvent::Removed(path));
            }
        } else if event.mask.contains(EventMask::MOVED_TO) {
            if event.cookie != 0 {
                if let Some((from, _)) = self.pending_moves.remove(&event.cookie) {
                    out.push(FsEvent::Renamed { from, to: path });
                    return;
                }
            }
            out.push(FsEvent::Created(path));
        } else if event.mask.contains(EventMask::CREATE) {
            out.push(FsEvent::Created(path));
        } else if event.mask.contains(EventMask::DELETE) {
            out.push(FsEvent::Removed(path));
        } else if event.mask.contains(EventMask::CLOSE_WRITE)
            || event.mask.contains(EventMask::ATTRIB)
        {
            out.push(FsEvent::Modified(path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    /// Poll the watcher until an event matching `pred` arrives or the timeout
    /// elapses.
    fn wait_for<F: Fn(&FsEvent) -> bool>(watcher: &mut Watcher, pred: F) -> Option<FsEvent> {
        let mut buffer = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let events = watcher
                .read_events(&mut buffer, Duration::from_millis(100))
                .ok()?;
            for e in events {
                if pred(&e) {
                    return Some(e);
                }
            }
        }
        None
    }

    #[test]
    fn reports_created_and_removed() {
        let base = std::env::temp_dir().join(format!("omavrything-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let mut watcher = Watcher::new().unwrap();
        watcher.watch_dir(&base).unwrap();

        let file = base.join("created.txt");
        fs::File::create(&file).unwrap().write_all(b"hi").unwrap();

        let created = wait_for(&mut watcher, |e| matches!(e, FsEvent::Created(_)));
        assert!(created.is_some(), "expected a Created event");

        fs::remove_file(&file).unwrap();
        let removed = wait_for(&mut watcher, |e| matches!(e, FsEvent::Removed(_)));
        assert!(removed.is_some(), "expected a Removed event");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn pairs_moves_into_a_rename() {
        let base =
            std::env::temp_dir().join(format!("omavrything-watch-mv-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let mut watcher = Watcher::new().unwrap();
        watcher.watch_dir(&base).unwrap();

        let from = base.join("from.txt");
        let to = base.join("to.txt");
        fs::write(&from, b"x").unwrap();
        // Drain the create event.
        let _ = wait_for(&mut watcher, |e| matches!(e, FsEvent::Created(_)));

        fs::rename(&from, &to).unwrap();
        let renamed = wait_for(&mut watcher, |e| matches!(e, FsEvent::Renamed { .. }));
        match renamed {
            Some(FsEvent::Renamed { from: f, to: t }) => {
                assert_eq!(f, from);
                assert_eq!(t, to);
            }
            other => panic!("expected a Renamed event, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn move_out_of_tree_becomes_a_removal() {
        let base =
            std::env::temp_dir().join(format!("omavrything-watch-out-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("omavrything-watch-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let mut watcher = Watcher::new().unwrap();
        watcher.watch_dir(&base).unwrap();

        let from = base.join("leaving.txt");
        fs::write(&from, b"x").unwrap();
        let _ = wait_for(&mut watcher, |e| matches!(e, FsEvent::Created(_)));

        // Move the file out of the watched tree: only MOVED_FROM is delivered.
        fs::rename(&from, outside.join("leaving.txt")).unwrap();
        let removed = wait_for(&mut watcher, |e| matches!(e, FsEvent::Removed(_)));
        assert!(removed.is_some(), "expected a Removed event for a move-out");

        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&outside);
    }
}
