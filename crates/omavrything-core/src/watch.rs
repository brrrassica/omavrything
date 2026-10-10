//! inotify-based live filesystem watching.
//!
//! Watches directories recursively and translates inotify events into
//! [`FsEvent`]s. `IN_MOVED_FROM`/`IN_MOVED_TO` pairs are matched by cookie so a
//! rename becomes a single [`FsEvent::Renamed`]. Watch-limit exhaustion
//! (`ENOSPC`) is recorded rather than fatal, so the periodic rescan can act as
//! the safety net.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::live::FsEvent;

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
    pending_moves: HashMap<u32, PathBuf>,
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
    pub fn watch_dir(&mut self, dir: &Path) -> io::Result<()> {
        match self.inotify.watches().add(dir, watch_mask()) {
            Ok(wd) => {
                self.dirs.insert(wd, dir.to_path_buf());
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                // max_user_watches exhausted: record and keep going.
                self.failed.push(dir.to_path_buf());
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        if let Ok(rd) = fs::read_dir(dir) {
            for entry in rd.flatten() {
                if entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                    self.watch_dir(&entry.path())?;
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

    /// Block until events are available, then translate them.
    pub fn read_events(&mut self, buffer: &mut [u8]) -> io::Result<Vec<FsEvent>> {
        let events = self.inotify.read_events_blocking(buffer)?;
        let mut out = Vec::new();

        for event in events {
            let Some(dir) = self.dirs.get(&event.wd) else {
                continue;
            };
            let path = match event.name {
                Some(name) => dir.join(name),
                None => dir.clone(),
            };

            if event.mask.contains(EventMask::MOVED_FROM) {
                if event.cookie != 0 {
                    self.pending_moves.insert(event.cookie, path);
                }
            } else if event.mask.contains(EventMask::MOVED_TO) {
                if event.cookie != 0 {
                    if let Some(from) = self.pending_moves.remove(&event.cookie) {
                        out.push(FsEvent::Renamed { from, to: path });
                        continue;
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

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// Poll the watcher until an event matching `pred` arrives or the timeout
    /// elapses.
    fn wait_for<F: Fn(&FsEvent) -> bool>(watcher: &mut Watcher, pred: F) -> Option<FsEvent> {
        let mut buffer = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let events = watcher.read_events(&mut buffer).ok()?;
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
}
