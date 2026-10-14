//! Filesystem scanner: mount discovery, parallel traversal, and index
//! population.
//!
//! Roots are either explicit (`ScanConfig::roots`) or discovered from the mount
//! table. Each root is walked in parallel with rayon; directories on a different
//! device (mount points) and directories matching an exclude glob are pruned.
//! Collected entries are held in a compact arena ([`WalkBuffer`]) and sorted by
//! path so parents are inserted before children.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use crate::glob::GlobSet;
use crate::index::{BatchEntry, FsKind, Index};
use crate::mounts::{self, Mount};

/// Scanner configuration.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Explicit roots. Empty means "discover from the mount table".
    pub roots: Vec<PathBuf>,
    /// Exclude globs applied to entries during traversal.
    pub excludes: Vec<String>,
    /// Skip pseudo-filesystems when discovering roots.
    pub skip_pseudo: bool,
    /// Collect each entry's size and mtime. When `false`, the walk uses only
    /// `readdir`'s `d_type` and skips the per-file `stat`, which is markedly
    /// faster but leaves `size`/`mtime` at zero. Directories are still stat'd so
    /// nested mounts are not crossed.
    pub metadata: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            excludes: Vec::new(),
            skip_pseudo: false,
            metadata: true,
        }
    }
}

impl ScanConfig {
    /// Scan the given roots.
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            excludes: Vec::new(),
            skip_pseudo: true,
            metadata: true,
        }
    }

    /// Discover roots from the mount table.
    pub fn auto() -> Self {
        Self {
            roots: Vec::new(),
            excludes: Vec::new(),
            skip_pseudo: true,
            metadata: true,
        }
    }
}

/// One entry discovered by the walker, stored compactly.
///
/// The path is not owned: it lives in the shared [`WalkBuffer`] arena, and this
/// record holds only its offset and length. This avoids one heap allocation per
/// path (a `PathBuf` per entry), which dominated the scanner's transient memory.
#[derive(Debug, Clone, Copy)]
struct CompactEntry {
    /// Offset of the path bytes into the buffer arena.
    path_off: u32,
    /// Byte length of the path.
    path_len: u32,
    size: u64,
    mtime: i64,
    is_dir: bool,
    is_symlink: bool,
}

/// A compact, arena-backed collection of walked entries.
///
/// Paths are stored back-to-back in `arena`; each [`CompactEntry`] records its
/// offset and length. Sorting compares arena slices, so the entries stay
/// contiguous and allocation-free. The arena is capped at 4 GiB by the `u32`
/// offsets, matching the index's own path arena.
#[derive(Debug, Default)]
struct WalkBuffer {
    arena: Vec<u8>,
    entries: Vec<CompactEntry>,
}

impl WalkBuffer {
    /// Append one entry, copying its path bytes into the arena.
    fn push(&mut self, path: &Path, is_dir: bool, is_symlink: bool, size: u64, mtime: i64) {
        let bytes = path.as_os_str().as_bytes();
        debug_assert!(
            self.arena.len() <= u32::MAX as usize,
            "walk arena exceeds 4 GiB"
        );
        let path_off = self.arena.len() as u32;
        self.arena.extend_from_slice(bytes);
        self.entries.push(CompactEntry {
            path_off,
            path_len: bytes.len() as u32,
            size,
            mtime,
            is_dir,
            is_symlink,
        });
    }

    /// The path bytes of an entry, borrowed from the arena.
    fn path_bytes(&self, e: &CompactEntry) -> &[u8] {
        let start = e.path_off as usize;
        &self.arena[start..start + e.path_len as usize]
    }

    /// The path of an entry, borrowed from the arena.
    fn path(&self, e: &CompactEntry) -> &Path {
        Path::new(OsStr::from_bytes(self.path_bytes(e)))
    }

    /// Sort entries by path, so parents precede their children.
    fn sort(&mut self) {
        let Self { arena, entries } = self;
        entries.sort_by(|a, b| {
            let pa = &arena[a.path_off as usize..(a.path_off + a.path_len) as usize];
            let pb = &arena[b.path_off as usize..(b.path_off + b.path_len) as usize];
            pa.cmp(pb)
        });
    }

    /// Append another buffer's entries, rebasing their path offsets.
    fn merge(&mut self, other: WalkBuffer) {
        let base = self.arena.len() as u32;
        self.arena.extend_from_slice(&other.arena);
        self.entries.extend(other.entries.into_iter().map(|mut e| {
            e.path_off += base;
            e
        }));
    }
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

    // Walk every root in parallel, each subtree producing one buffer that is
    // merged exactly once (F5), then insert the whole batch at once.
    let errors = AtomicUsize::new(0);
    let mut buffer = roots
        .par_iter()
        .map(|(root, _)| {
            let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
            walk(root, root_dev, &excludes, config.metadata, &errors)
        })
        .reduce(WalkBuffer::default, |mut a, b| {
            a.merge(b);
            a
        });
    report.errors = errors.load(Ordering::Relaxed);

    // A parent path is always a strict prefix of its children, so sorting by
    // path guarantees parents are inserted before children.
    buffer.sort();

    index.reserve(buffer.entries.len(), buffer.arena.len());
    let total = buffer.entries.len();
    let inserted = index.insert_batch(buffer.entries.iter().map(|e| BatchEntry {
        path: buffer.path_bytes(e),
        is_dir: e.is_dir,
        is_symlink: e.is_symlink,
        size: e.size,
        mtime: e.mtime,
    }));
    report.entries += inserted;
    report.skipped += total - inserted;

    report
}

/// Walk `root` and insert any entries not already indexed. Used for live
/// updates when a new directory appears.
pub fn scan_subtree(index: &mut Index, root: &Path, config: &ScanConfig) {
    let excludes = GlobSet::new(&config.excludes);
    let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
    let errors = AtomicUsize::new(0);
    let mut buffer = walk(root, root_dev, &excludes, config.metadata, &errors);

    buffer.sort();
    index.reserve(buffer.entries.len(), buffer.arena.len());
    index.insert_batch(buffer.entries.iter().map(|e| BatchEntry {
        path: buffer.path_bytes(e),
        is_dir: e.is_dir,
        is_symlink: e.is_symlink,
        size: e.size,
        mtime: e.mtime,
    }));
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
    entries: WalkBuffer,
    roots: Vec<(PathBuf, FsKind)>,
    errors: usize,
}

/// Walk the configured roots and collect the entries, without touching any
/// index. This is the expensive half of a rescan; run it outside the index lock.
pub fn plan_rescan(config: &ScanConfig) -> RescanPlan {
    let excludes = GlobSet::new(&config.excludes);
    let roots = resolve_roots(config);
    let errors = AtomicUsize::new(0);
    let mut entries = roots
        .par_iter()
        .map(|(root, _)| {
            let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
            walk(root, root_dev, &excludes, config.metadata, &errors)
        })
        .reduce(WalkBuffer::default, |mut a, b| {
            a.merge(b);
            a
        });
    entries.sort();
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

    for e in &entries.entries {
        let path = entries.path(e);
        match index.find(path) {
            Some(idx) => {
                seen[idx as usize] = true;
                // Refresh metadata for entries that changed without an inotify
                // event.
                index.update_metadata(path, e.size, e.mtime);
            }
            None => {
                let idx = index.insert_with_flags(path, e.is_dir, e.is_symlink, e.size, e.mtime);
                debug_assert_eq!(idx as usize, seen.len());
                seen.push(true);
                report.entries += 1;
            }
        }
    }

    // Records under a root that the walk did not confirm are stale.
    let mut stale: Vec<PathBuf> = Vec::new();
    let mut buf = String::new();
    for (i, rec) in index.records().iter().enumerate() {
        if rec.is_deleted() || seen[i] {
            continue;
        }
        buf.clear();
        index.write_path(i as u32, &mut buf);
        if roots.iter().any(|(r, _)| Path::new(&buf).starts_with(r)) {
            stale.push(PathBuf::from(&buf));
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
    collect_metadata: bool,
    errors: &AtomicUsize,
) -> WalkBuffer {
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut batch = WalkBuffer::default();

    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !excludes.is_empty() && excludes.matches(&path) {
                continue;
            }
            // `file_type()` reads the `d_type` field that `readdir` already
            // returned, so on ext4/xfs/btrfs it costs no extra syscall. Only
            // fall back to `metadata()` when size/mtime are actually needed.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let is_dir = file_type.is_dir();
            let is_symlink = file_type.is_symlink();

            // Directories need `dev` to avoid crossing a nested mount; files
            // need `size`/`mtime`. In names-only mode (`collect_metadata` false)
            // directories are still stat'd (a small fraction of entries) so
            // mount boundaries are respected, but files skip the stat entirely.
            let metadata = if collect_metadata || is_dir {
                entry.metadata().ok()
            } else {
                None
            };

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

            batch.push(&path, is_dir, is_symlink, size, mtime);
            if is_dir {
                subdirs.push(path);
            }
        }
    } else {
        errors.fetch_add(1, Ordering::Relaxed);
    }

    // Recurse in parallel and merge each subtree's buffer exactly once, so the
    // walk never contends on a shared lock (F5).
    let children = subdirs
        .into_par_iter()
        .map(|d| walk(&d, root_dev, excludes, collect_metadata, errors))
        .reduce(WalkBuffer::default, |mut a, b| {
            a.merge(b);
            a
        });
    batch.merge(children);
    batch
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
            metadata: true,
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

    #[test]
    fn names_only_scan_finds_paths_but_skips_metadata() {
        let base = temp_dir("names-only");
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("a.txt"), b"hello").unwrap();
        fs::write(base.join("sub/b.txt"), b"world!").unwrap();

        let config = ScanConfig {
            roots: vec![base.clone()],
            excludes: Vec::new(),
            skip_pseudo: true,
            metadata: false,
        };
        let (index, report) = scan(&config);

        // The tree is still fully discovered, with correct types.
        assert!(index.find(&base.join("a.txt")).is_some());
        assert!(index.find(&base.join("sub")).is_some());
        assert!(index.find(&base.join("sub/b.txt")).is_some());
        assert_eq!(report.entries, 3);

        // Size and mtime are not collected in names-only mode.
        let hit = index.search("a.txt", 10, 0);
        assert_eq!(hit.hits.len(), 1);
        assert_eq!(hit.hits[0].size, 0);
        assert_eq!(hit.hits[0].mtime, 0);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn walk_buffer_sorts_and_merges_by_path() {
        let mut a = WalkBuffer::default();
        a.push(Path::new("/b/two.txt"), false, false, 2, 20);
        a.push(Path::new("/a/one.txt"), false, false, 1, 10);

        let mut b = WalkBuffer::default();
        b.push(Path::new("/c/three.txt"), true, false, 0, 30);

        a.merge(b);
        a.sort();

        let paths: Vec<String> = a
            .entries
            .iter()
            .map(|e| a.path(e).to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/a/one.txt", "/b/two.txt", "/c/three.txt"]);

        // Metadata survives the merge and the offset rebasing.
        let three = a
            .entries
            .iter()
            .find(|e| a.path(e) == Path::new("/c/three.txt"))
            .unwrap();
        assert!(three.is_dir);
        assert_eq!(three.mtime, 30);
    }
}
