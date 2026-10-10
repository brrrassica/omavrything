//! Applying filesystem change events to the index.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::index::Index;
use crate::scan::{ScanConfig, scan_subtree};

/// A filesystem change observed by the watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    /// A file or directory appeared.
    Created(PathBuf),
    /// A file or directory disappeared.
    Removed(PathBuf),
    /// A file or directory moved (rename or move).
    Renamed { from: PathBuf, to: PathBuf },
    /// A file's contents or metadata changed.
    Modified(PathBuf),
}

/// Apply one event to the index. Returns whether the index changed.
pub fn apply(index: &mut Index, event: &FsEvent, config: &ScanConfig) -> bool {
    match event {
        FsEvent::Created(path) => apply_created(index, path, config),
        FsEvent::Removed(path) => index.remove(path),
        FsEvent::Renamed { from, to } => {
            if index.rename(from, to) {
                true
            } else {
                // Moved in from outside the watched set: treat as a create.
                apply_created(index, to, config)
            }
        }
        FsEvent::Modified(path) => apply_modified(index, path),
    }
}

fn apply_created(index: &mut Index, path: &Path, config: &ScanConfig) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let is_dir = metadata.is_dir();
    let is_symlink = metadata.file_type().is_symlink();
    let size = if is_dir { 0 } else { metadata.len() };
    let mtime = mtime_of(&metadata);

    if index.find(path).is_none() {
        index.insert_with_flags(path, is_dir, is_symlink, size, mtime);
    }
    if is_dir {
        // A new directory may already contain files (e.g. a move-in).
        scan_subtree(index, path, config);
    }
    true
}

fn apply_modified(index: &mut Index, path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let size = if metadata.is_dir() { 0 } else { metadata.len() };
    index.update_metadata(path, size, mtime_of(&metadata))
}

fn mtime_of(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("omavrything-live-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn created_file_is_indexed() {
        let base = temp_dir("created");
        let file = base.join("a.txt");
        fs::write(&file, b"hello").unwrap();

        let mut index = Index::new();
        let config = ScanConfig::with_roots(vec![base.clone()]);
        assert!(apply(&mut index, &FsEvent::Created(file.clone()), &config));

        let hit = index.search("a.txt", 10, 0);
        assert_eq!(hit.hits.len(), 1);
        assert_eq!(hit.hits[0].size, 5);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn created_directory_is_scanned_recursively() {
        let base = temp_dir("created-dir");
        let dir = base.join("new");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/deep.txt"), b"x").unwrap();

        let mut index = Index::new();
        let config = ScanConfig::with_roots(vec![base.clone()]);
        apply(&mut index, &FsEvent::Created(dir.clone()), &config);

        assert_eq!(index.search("deep.txt", 10, 0).hits.len(), 1);
        assert!(index.find(&dir).is_some());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn removed_path_is_dropped() {
        let base = temp_dir("removed");
        let file = base.join("gone.txt");
        fs::write(&file, b"x").unwrap();

        let mut index = Index::new();
        let config = ScanConfig::with_roots(vec![base.clone()]);
        apply(&mut index, &FsEvent::Created(file.clone()), &config);
        assert_eq!(index.search("gone.txt", 10, 0).hits.len(), 1);

        fs::remove_file(&file).unwrap();
        assert!(apply(&mut index, &FsEvent::Removed(file), &config));
        assert_eq!(index.search("gone.txt", 10, 0).hits.len(), 0);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn renamed_path_updates_the_index() {
        let base = temp_dir("renamed");
        let from = base.join("old.txt");
        let to = base.join("new.txt");
        fs::write(&from, b"x").unwrap();

        let mut index = Index::new();
        let config = ScanConfig::with_roots(vec![base.clone()]);
        apply(&mut index, &FsEvent::Created(from.clone()), &config);

        fs::rename(&from, &to).unwrap();
        assert!(apply(
            &mut index,
            &FsEvent::Renamed {
                from: from.clone(),
                to: to.clone()
            },
            &config
        ));
        assert_eq!(index.search("old.txt", 10, 0).hits.len(), 0);
        assert_eq!(index.search("new.txt", 10, 0).hits.len(), 1);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn modified_updates_size() {
        let base = temp_dir("modified");
        let file = base.join("grow.txt");
        fs::write(&file, b"x").unwrap();

        let mut index = Index::new();
        let config = ScanConfig::with_roots(vec![base.clone()]);
        apply(&mut index, &FsEvent::Created(file.clone()), &config);
        assert_eq!(index.search("grow.txt", 10, 0).hits[0].size, 1);

        fs::write(&file, b"much longer contents").unwrap();
        assert!(apply(&mut index, &FsEvent::Modified(file), &config));
        assert_eq!(index.search("grow.txt", 10, 0).hits[0].size, 20);

        let _ = fs::remove_dir_all(&base);
    }
}
