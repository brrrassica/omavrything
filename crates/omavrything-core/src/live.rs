//! Applying filesystem change events to the index.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::index::{DeltaIndex, FileRecord, Index, PathIndex, write_full_path_bytes};
use crate::scan::{ScanConfig, scan_subtree, scan_subtree_into_delta};

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

/// Apply one event to a [`DeltaIndex`] overlay while the base image is evicted.
///
/// `by_path` is the resident path -> base-index map, and `records`/`paths` are
/// the base image's arenas (used to confirm the map's hash matches). Returns
/// whether the delta changed.
pub fn apply_delta(
    delta: &mut DeltaIndex,
    by_path: &PathIndex,
    records: &[FileRecord],
    paths: &[u8],
    event: &FsEvent,
    config: &ScanConfig,
) -> bool {
    match event {
        FsEvent::Created(path) => delta_created(delta, by_path, records, paths, path, config),
        FsEvent::Removed(path) => delta_removed(delta, records, paths, path),
        FsEvent::Renamed { from, to } => {
            let changed = delta_removed(delta, records, paths, from);
            delta_created(delta, by_path, records, paths, to, config) || changed
        }
        FsEvent::Modified(path) => delta_modified(delta, by_path, records, paths, path),
    }
}

fn delta_created(
    delta: &mut DeltaIndex,
    by_path: &PathIndex,
    records: &[FileRecord],
    paths: &[u8],
    path: &Path,
    config: &ScanConfig,
) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let is_dir = metadata.is_dir();
    let is_symlink = metadata.file_type().is_symlink();
    let size = if is_dir { 0 } else { metadata.len() };
    // A create over an existing base record is an update: drop the base record.
    if let Some(idx) = by_path.get(&path.to_string_lossy(), records, paths) {
        delta.remove(idx);
    }
    delta.insert(path, is_dir, is_symlink, size, mtime_of(&metadata));
    if is_dir {
        scan_subtree_into_delta(delta, path, config);
    }
    true
}

fn delta_removed(
    delta: &mut DeltaIndex,
    records: &[FileRecord],
    paths: &[u8],
    path: &Path,
) -> bool {
    let mut changed = false;
    let key = path.to_string_lossy();
    // For the filesystem root "/", the subtree prefix is "/" itself; for any
    // other path it is "<key>/".
    let prefix = if key.ends_with('/') {
        key.to_string()
    } else {
        format!("{key}/")
    };
    let key_bytes = key.as_bytes();
    let prefix_bytes = prefix.as_bytes();
    let mut buf: Vec<u8> = Vec::new();
    // inotify reports a directory removal or rename as one event, without
    // per-child events, so drop the whole base subtree, not just the exact path.
    for (i, rec) in records.iter().enumerate() {
        if rec.is_deleted() {
            continue;
        }
        buf.clear();
        write_full_path_bytes(rec, records, paths, &mut buf);
        if buf == key_bytes || buf.starts_with(prefix_bytes) {
            delta.remove(i as u32);
            changed = true;
        }
    }
    // Pending inserts of the old subtree were created while evicted, so they
    // have no base record; tombstone them too.
    changed |= delta.remove_subtree(path);
    changed
}

fn delta_modified(
    delta: &mut DeltaIndex,
    by_path: &PathIndex,
    records: &[FileRecord],
    paths: &[u8],
    path: &Path,
) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let size = if metadata.is_dir() { 0 } else { metadata.len() };
    if let Some(idx) = by_path.get(&path.to_string_lossy(), records, paths) {
        delta.remove(idx);
    }
    delta.insert(
        path,
        metadata.is_dir(),
        metadata.file_type().is_symlink(),
        size,
        mtime_of(&metadata),
    );
    true
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
    fn renamed_child_file_updates_the_index() {
        // Regression: the parent directory is indexed, so the file stores only
        // its basename and the rename must rewrite it.
        let base = temp_dir("renamed-child");
        let dir = base.join("dir");
        fs::create_dir_all(&dir).unwrap();
        let from = dir.join("old.txt");
        let to = dir.join("new.txt");
        fs::write(&from, b"x").unwrap();

        let mut index = Index::new();
        let config = ScanConfig::with_roots(vec![base.clone()]);
        apply(&mut index, &FsEvent::Created(dir.clone()), &config);
        assert!(index.find(&from).is_some());

        fs::rename(&from, &to).unwrap();
        assert!(apply(
            &mut index,
            &FsEvent::Renamed {
                from: from.clone(),
                to: to.clone()
            },
            &config
        ));

        let idx = index.find(&to).expect("renamed path missing");
        assert_eq!(index.path(idx), to.to_string_lossy());
        assert_eq!(index.name(idx), "new.txt");
        assert!(index.find(&from).is_none());
        assert_eq!(index.search("old.txt", 10, 0).hits.len(), 0);
        let hits = index.search("new.txt", 10, 0).hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, to.to_string_lossy());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn renamed_directory_drops_base_descendants_from_delta() {
        // Regression: a directory rename arrives as one event, so the delta
        // must remove the old subtree from the base image instead of only the
        // directory record, or searches return both the old and new children.
        let base = temp_dir("delta-renamed-dir");
        let from = base.join("base");
        let to = base.join("moved");
        fs::create_dir_all(from.join("sub")).unwrap();
        let old_child = from.join("sub/child.txt");
        let new_child = to.join("sub/child.txt");
        fs::write(&old_child, b"x").unwrap();

        // Build the base image as an evicted index would hold it.
        let mut index = Index::new();
        let config = ScanConfig::with_roots(vec![base.clone()]);
        apply(&mut index, &FsEvent::Created(base.clone()), &config);
        assert!(index.find(&old_child).is_some());
        let by_path = PathIndex::build(index.records(), index.paths());

        // Rename while evicted, then fold the accumulated delta.
        fs::rename(&from, &to).unwrap();
        let mut delta = DeltaIndex::new();
        assert!(apply_delta(
            &mut delta,
            &by_path,
            index.records(),
            index.paths(),
            &FsEvent::Renamed {
                from: from.clone(),
                to: to.clone()
            },
            &config
        ));
        index.apply_delta(&delta);

        assert!(index.find(&old_child).is_none());
        assert!(index.find(&new_child).is_some());
        let hits = index.search("child.txt", 10, 0).hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, new_child.to_string_lossy());

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
