//! Filesystem scanner: mount discovery, parallel traversal, and index
//! population.
//!
//! Roots are either explicit (`ScanConfig::roots`) or discovered from the mount
//! table. Each root is walked in parallel with rayon; directories on a different
//! device (mount points) and directories matching an exclude glob are pruned.
//! Collected entries are sorted by path so parents are inserted before children.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use crate::glob::GlobSet;
use crate::index::{FsKind, Index};
use crate::mounts::{self, Mount};

/// Scanner configuration.
#[derive(Debug, Clone, Default)]
pub struct ScanConfig {
    /// Explicit roots. Empty means "discover from the mount table".
    pub roots: Vec<PathBuf>,
    /// Exclude globs applied to entries during traversal.
    pub excludes: Vec<String>,
    /// Skip pseudo-filesystems when discovering roots.
    pub skip_pseudo: bool,
}

impl ScanConfig {
    /// Scan the given roots.
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            excludes: Vec::new(),
            skip_pseudo: true,
        }
    }

    /// Discover roots from the mount table.
    pub fn auto() -> Self {
        Self {
            roots: Vec::new(),
            excludes: Vec::new(),
            skip_pseudo: true,
        }
    }
}

/// One entry discovered by the walker.
#[derive(Debug, Clone)]
struct WalkEntry {
    path: PathBuf,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    mtime: i64,
}

/// A summary of a scan.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScanReport {
    pub roots: usize,
    pub entries: usize,
    pub skipped: usize,
    /// Directories that could not be read during traversal.
    pub errors: usize,
}

/// Build a fresh index by scanning the configured roots.
pub fn scan(config: &ScanConfig) -> (Index, ScanReport) {
    let mut index = Index::new();
    let report = scan_into_index(&mut index, config);
    (index, report)
}

/// Scan the configured roots into an existing index.
pub fn scan_into_index(index: &mut Index, config: &ScanConfig) -> ScanReport {
    let excludes = GlobSet::new(&config.excludes);
    let roots = resolve_roots(config);
    let mut report = ScanReport::default();

    for (root, fs_kind) in &roots {
        index.add_root(root, *fs_kind);
        report.roots += 1;
    }

    // Walk every root in parallel, collecting entries into one buffer.
    let collected: Mutex<Vec<WalkEntry>> = Mutex::new(Vec::new());
    let errors = AtomicUsize::new(0);
    roots.par_iter().for_each(|(root, _)| {
        let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
        walk(root, root_dev, &excludes, &collected, &errors);
    });
    report.errors = errors.load(Ordering::Relaxed);
    let mut entries = collected.into_inner().unwrap_or_default();

    // A parent path is always a strict prefix of its children, so sorting by
    // path guarantees parents are inserted before children.
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    for e in entries {
        if index.find(&e.path).is_some() {
            report.skipped += 1;
            continue;
        }
        index.insert_with_flags(&e.path, e.is_dir, e.is_symlink, e.size, e.mtime);
        report.entries += 1;
    }

    report
}

/// Walk `root` and insert any entries not already indexed. Used for live
/// updates when a new directory appears.
pub fn scan_subtree(index: &mut Index, root: &Path, config: &ScanConfig) {
    let excludes = GlobSet::new(&config.excludes);
    let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
    let collected: Mutex<Vec<WalkEntry>> = Mutex::new(Vec::new());
    let errors = AtomicUsize::new(0);
    walk(root, root_dev, &excludes, &collected, &errors);

    let mut entries = collected.into_inner().unwrap_or_default();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    for e in entries {
        if index.find(&e.path).is_some() {
            continue;
        }
        index.insert_with_flags(&e.path, e.is_dir, e.is_symlink, e.size, e.mtime);
    }
}

/// A prepared rescan: the entries discovered by walking the roots, the roots
/// themselves, and the traversal error count.
///
/// Produced by [`plan_rescan`], which performs the (I/O-bound) filesystem
/// traversal, and consumed by [`apply_rescan`], which reconciles the index. The
/// split lets callers walk outside the index lock and hold the lock only for the
/// in-memory reconciliation, so a rescan no longer needs to clone the whole
/// index (which doubled peak memory).
pub struct RescanPlan {
    entries: Vec<WalkEntry>,
    roots: Vec<(PathBuf, FsKind)>,
    errors: usize,
}

/// Walk the configured roots and collect the entries, without touching any
/// index. This is the expensive half of a rescan; run it outside the index lock.
pub fn plan_rescan(config: &ScanConfig) -> RescanPlan {
    let excludes = GlobSet::new(&config.excludes);
    let roots = resolve_roots(config);
    let collected: Mutex<Vec<WalkEntry>> = Mutex::new(Vec::new());
    let errors = AtomicUsize::new(0);
    roots.par_iter().for_each(|(root, _)| {
        let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
        walk(root, root_dev, &excludes, &collected, &errors);
    });
    let mut entries = collected.into_inner().unwrap_or_default();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    RescanPlan {
        entries,
        roots,
        errors: errors.load(Ordering::Relaxed),
    }
}

/// Reconcile a prepared [`RescanPlan`] into `index`: add new entries, refresh
/// metadata, and drop entries that no longer exist. Performs no filesystem I/O,
/// so the caller can hold the index write lock for the duration.
pub fn apply_rescan(index: &mut Index, plan: RescanPlan) -> ScanReport {
    let RescanPlan {
        entries,
        roots,
        errors,
    } = plan;
    let mut report = ScanReport {
        errors,
        ..ScanReport::default()
    };

    // Track which records the walk confirmed, by record index. A `Vec<bool>` is
    // far smaller than a `HashSet<PathBuf>` holding a copy of every path.
    let mut seen = vec![false; index.records().len()];
    for (root, _) in &roots {
        if let Some(idx) = index.find(root) {
            seen[idx as usize] = true;
        }
    }

    for e in &entries {
        match index.find(&e.path) {
            Some(idx) => {
                seen[idx as usize] = true;
                // Refresh metadata for entries that changed without an inotify
                // event.
                index.update_metadata(&e.path, e.size, e.mtime);
            }
            None => {
                let idx =
                    index.insert_with_flags(&e.path, e.is_dir, e.is_symlink, e.size, e.mtime);
                debug_assert_eq!(idx as usize, seen.len());
                seen.push(true);
                report.entries += 1;
            }
        }
    }

    // Records under a root that the walk did not confirm are stale.
    let mut stale: Vec<PathBuf> = Vec::new();
    for (i, rec) in index.records().iter().enumerate() {
        if rec.is_deleted() || seen[i] {
            continue;
        }
        let p = index.path(i as u32);
        if roots.iter().any(|(r, _)| Path::new(p).starts_with(r)) {
            stale.push(PathBuf::from(p));
        }
    }
    // Removing a directory tombstones its whole subtree, so drop descendants of
    // an already-removed path to avoid redundant O(n) scans.
    stale.sort();
    let mut removed: Vec<PathBuf> = Vec::new();
    for p in stale {
        if removed.iter().any(|r| p.starts_with(r)) {
            continue;
        }
        if index.remove(&p) {
            report.skipped += 1;
            removed.push(p);
        }
    }

    report
}

/// Re-walk the configured roots and reconcile the index: add new entries and
/// drop entries that no longer exist. This is the idle-gated safety net for
/// events missed by inotify.
pub fn rescan_into_index(index: &mut Index, config: &ScanConfig) -> ScanReport {
    apply_rescan(index, plan_rescan(config))
}

/// Resolve the roots to scan: explicit roots, or non-pseudo mounts.
pub fn resolve_roots(config: &ScanConfig) -> Vec<(PathBuf, FsKind)> {
    let mounts = mounts::read_mounts().unwrap_or_default();

    if !config.roots.is_empty() {
        return config
            .roots
            .iter()
            .map(|r| (r.clone(), fs_kind_for_path(&mounts, r)))
            .collect();
    }

    mounts
        .iter()
        .filter(|m| {
            !(config.skip_pseudo
                && (mounts::is_pseudo_fs(&m.fs_type)
                    || mounts::is_pseudo_mount_point(&m.mount_point)))
        })
        .map(|m| (m.mount_point.clone(), mounts::fs_kind(&m.fs_type)))
        .collect()
}

/// The filesystem kind of the mount that contains `path` (longest match).
fn fs_kind_for_path(mounts: &[Mount], path: &Path) -> FsKind {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point))
        .max_by_key(|m| m.mount_point.as_os_str().len())
        .map(|m| mounts::fs_kind(&m.fs_type))
        .unwrap_or(FsKind::Other)
}

/// Recursively walk `dir`, appending entries to `out`. Directories on a
/// different device than `root_dev` are not crossed, and excluded paths are
/// pruned.
fn walk(
    dir: &Path,
    root_dev: Option<u64>,
    excludes: &GlobSet,
    out: &Mutex<Vec<WalkEntry>>,
    errors: &AtomicUsize,
) {
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut batch: Vec<WalkEntry> = Vec::new();

    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !excludes.is_empty() && excludes.matches(&path) {
                continue;
            }
            let metadata = entry.metadata().ok();
            let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let is_symlink = metadata
                .as_ref()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);

            // Do not cross into another filesystem (a nested mount).
            if is_dir {
                if let (Some(dev), Some(root_dev)) = (metadata.as_ref().map(|m| m.dev()), root_dev)
                {
                    if dev != root_dev {
                        continue;
                    }
                }
            }

            let size = if is_dir {
                0
            } else {
                metadata.as_ref().map(|m| m.len()).unwrap_or(0)
            };
            let mtime = metadata
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            batch.push(WalkEntry {
                path: path.clone(),
                is_dir,
                is_symlink,
                size,
                mtime,
            });
            if is_dir {
                subdirs.push(path);
            }
        }
    } else {
        errors.fetch_add(1, Ordering::Relaxed);
    }

    if !batch.is_empty() {
        out.lock().unwrap().extend(batch);
    }

    subdirs
        .into_par_iter()
        .for_each(|d| walk(&d, root_dev, excludes, out, errors));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("omavrything-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    /// A sequential reference walk, mirroring what the scanner should find.
    fn manual_walk(root: &Path) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if let Ok(rd) = fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    out.insert(p.to_string_lossy().into_owned());
                    if e.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                        stack.push(p);
                    }
                }
            }
        }
        out
    }

    fn indexed_paths(index: &Index) -> BTreeSet<String> {
        index
            .iter()
            .map(|(i, _)| index.path(i).to_string())
            .collect()
    }

    #[test]
    fn scan_matches_a_manual_walk() {
        let base = temp_dir("scan-match");
        fs::create_dir_all(base.join("sub/deep")).unwrap();
        fs::write(base.join("a.txt"), b"hello").unwrap();
        fs::write(base.join("sub/b.txt"), b"world!").unwrap();
        fs::write(base.join("sub/deep/c.txt"), b"!").unwrap();

        let config = ScanConfig::with_roots(vec![base.clone()]);
        let (index, report) = scan(&config);

        let mut expected = manual_walk(&base);
        expected.insert(base.to_string_lossy().into_owned());

        assert_eq!(indexed_paths(&index), expected);
        assert_eq!(report.roots, 1);
        assert_eq!(report.entries, expected.len() - 1); // minus the root record

        let a = index.search("a.txt", 10, 0);
        assert_eq!(a.hits.len(), 1);
        assert_eq!(a.hits[0].size, 5);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn excludes_prune_a_subtree() {
        let base = temp_dir("scan-exclude");
        fs::create_dir_all(base.join("keep")).unwrap();
        fs::create_dir_all(base.join(".git/objects")).unwrap();
        fs::write(base.join("keep/a.txt"), b"x").unwrap();
        fs::write(base.join(".git/objects/blob"), b"y").unwrap();

        let config = ScanConfig {
            roots: vec![base.clone()],
            excludes: vec!["**/.git/**".into()],
            skip_pseudo: true,
        };
        let (index, _) = scan(&config);

        let paths = indexed_paths(&index);
        assert!(paths.iter().any(|p| p.ends_with("/keep/a.txt")));
        assert!(!paths.iter().any(|p| p.contains("/.git")));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn auto_roots_skip_pseudo_filesystems() {
        // Uses the real mount table; just assert no pseudo mount is selected.
        let config = ScanConfig::auto();
        let roots = resolve_roots(&config);
        assert!(
            roots.iter().all(|(p, _)| !mounts::is_pseudo_mount_point(p)),
            "auto roots must not include pseudo mount points: {roots:?}"
        );
    }

    #[test]
    fn rescan_adds_updates_and_removes() {
        let base = temp_dir("rescan");
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("a.txt"), b"x").unwrap();
        fs::write(base.join("sub/b.txt"), b"y").unwrap();

        let config = ScanConfig::with_roots(vec![base.clone()]);
        let (mut index, _) = scan(&config);
        assert!(index.find(&base.join("a.txt")).is_some());
        assert!(index.find(&base.join("sub/b.txt")).is_some());

        // Add a file, delete another, and grow a third.
        fs::write(base.join("new.txt"), b"z").unwrap();
        fs::remove_file(base.join("sub/b.txt")).unwrap();
        fs::write(base.join("a.txt"), b"much longer").unwrap();

        let report = rescan_into_index(&mut index, &config);

        assert!(index.find(&base.join("new.txt")).is_some());
        assert!(index.find(&base.join("sub/b.txt")).is_none());
        assert_eq!(index.search("a.txt", 10, 0).hits[0].size, 11);
        // The root record must survive the reconcile.
        assert!(index.find(&base).is_some());
        assert_eq!(report.entries, 1); // new.txt
        assert_eq!(report.skipped, 1); // sub/b.txt

        let _ = fs::remove_dir_all(&base);
    }
}
