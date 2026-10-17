//! Filesystem scanner: mount discovery, parallel traversal, and index
//! population.
//!
//! Roots are either explicit (`ScanConfig::roots`) or discovered from the mount
//! table. Each root is walked in parallel with rayon; directories on a different
//! device (mount points) and directories matching an exclude glob are pruned.
//! Collected entries are held in a compact arena ([`WalkBuffer`]) and sorted by
//! path so parents are inserted before children.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rayon::prelude::*;

use crate::glob::GlobSet;
use crate::index::{BatchEntry, DeltaIndex, FsKind, Index};
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

/// How many failing paths are kept as a sample (A2). The count is exact; only
/// the path list is bounded, so a tree with thousands of unreadable
/// directories cannot balloon the report.
const MAX_ERROR_PATHS: usize = 32;

/// Traversal failures recorded during a walk (A2): an exact count plus a
/// bounded sample of failing paths, so an unreadable or stuck subtree is
/// visible without unbounded memory.
pub struct ScanErrors {
    count: AtomicUsize,
    paths: Mutex<Vec<String>>,
}

impl ScanErrors {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            paths: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, path: &Path) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut paths) = self.paths.lock() {
            if paths.len() < MAX_ERROR_PATHS {
                paths.push(path.to_string_lossy().into_owned());
            }
        }
    }

    /// The failure count and a bounded sample of failing paths.
    pub fn report(&self) -> (usize, Vec<String>) {
        (
            self.count.load(Ordering::Relaxed),
            self.paths.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        )
    }
}

/// Cancellation for a scan (A3): the walk is abandoned when a newer scan
/// supersedes it (the generation moves past `token`) or when the daemon is
/// shutting down. Checked once per directory, so a cancellation stops the walk
/// within one directory's readdir rather than mid-recursion unbounded.
pub struct ScanCancel<'a> {
    generation: &'a AtomicU64,
    shutdown: &'a AtomicBool,
    token: u64,
}

impl<'a> ScanCancel<'a> {
    /// Take a token from the current generation. A later `fetch_add` on
    /// `generation` (a superseding scan) makes this cancel fire.
    pub fn new(generation: &'a AtomicU64, shutdown: &'a AtomicBool) -> Self {
        Self {
            generation,
            shutdown,
            token: generation.load(Ordering::Relaxed),
        }
    }

    pub fn cancelled(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
            || self.generation.load(Ordering::Relaxed) != self.token
    }
}

/// A6: how long a network-filesystem root may be traversed before the walk
/// gives up, so a stalled mount cannot block a scan indefinitely.
const NETWORK_TRAVERSAL_BUDGET: Duration = Duration::from_secs(30);

/// The traversal deadline for `root`, if any: a root on a network filesystem
/// is time-boxed (A6).
fn root_deadline(mounts: &[Mount], root: &Path) -> Option<Instant> {
    let network = mounts
        .iter()
        .any(|m| root.starts_with(&m.mount_point) && mounts::is_network_fs(&m.fs_type));
    network.then(|| Instant::now() + NETWORK_TRAVERSAL_BUDGET)
}

/// A summary of a scan.
#[derive(Debug, Default, Clone)]
pub struct ScanReport {
    pub roots: usize,
    pub entries: usize,
    pub skipped: usize,
    /// Directories that could not be read during traversal.
    pub errors: usize,
    /// A bounded sample of the failing paths (A2).
    pub error_paths: Vec<String>,
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
    let errors = ScanErrors::new();
    let boundaries = mount_boundaries();
    let mount_table = mounts::read_mounts().unwrap_or_default();
    let mut buffer = roots
        .par_iter()
        .map(|(root, _)| {
            let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
            let visited = Mutex::new(HashSet::new());
            let ctx = WalkCtx {
                root,
                root_dev,
                boundaries: &boundaries,
                visited: &visited,
                excludes: &excludes,
                collect_metadata: config.metadata,
                errors: &errors,
                cancel: None,
                deadline: root_deadline(&mount_table, root),
            };
            walk(root, &ctx)
        })
        .reduce(WalkBuffer::default, |mut a, b| {
            a.merge(b);
            a
        });
    let (error_count, error_paths) = errors.report();
    report.errors = error_count;
    report.error_paths = error_paths;

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
    let errors = ScanErrors::new();
    let boundaries = mount_boundaries();
    let visited = Mutex::new(HashSet::new());
    let mount_table = mounts::read_mounts().unwrap_or_default();
    let ctx = WalkCtx {
        root,
        root_dev,
        boundaries: &boundaries,
        visited: &visited,
        excludes: &excludes,
        collect_metadata: config.metadata,
        errors: &errors,
        cancel: None,
        deadline: root_deadline(&mount_table, root),
    };
    let mut buffer = walk(root, &ctx);

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

/// Walk `root` and record every entry into `delta`. Used for live updates when a
/// new directory appears while the base image is evicted.
pub fn scan_subtree_into_delta(delta: &mut DeltaIndex, root: &Path, config: &ScanConfig) {
    let excludes = GlobSet::new(&config.excludes);
    let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
    let errors = ScanErrors::new();
    let boundaries = mount_boundaries();
    let visited = Mutex::new(HashSet::new());
    let mount_table = mounts::read_mounts().unwrap_or_default();
    let ctx = WalkCtx {
        root,
        root_dev,
        boundaries: &boundaries,
        visited: &visited,
        excludes: &excludes,
        collect_metadata: config.metadata,
        errors: &errors,
        cancel: None,
        deadline: root_deadline(&mount_table, root),
    };
    let mut buffer = walk(root, &ctx);
    buffer.sort();
    for e in &buffer.entries {
        let path = Path::new(OsStr::from_bytes(buffer.path_bytes(e)));
        delta.insert(path, e.is_dir, e.is_symlink, e.size, e.mtime);
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
    entries: WalkBuffer,
    roots: Vec<(PathBuf, FsKind)>,
    errors: usize,
    /// A bounded sample of the failing paths (A2).
    pub error_paths: Vec<String>,
}

/// Walk the configured roots and collect the entries, without touching any
/// index. This is the expensive half of a rescan; run it outside the index lock.
pub fn plan_rescan(config: &ScanConfig) -> RescanPlan {
    plan_rescan_cancellable(config, None)
}

/// Like [`plan_rescan`], but the walk stops promptly when `cancel` fires (A3):
/// on shutdown or when a newer rescan supersedes this one.
pub fn plan_rescan_cancellable(config: &ScanConfig, cancel: Option<&ScanCancel>) -> RescanPlan {
    let excludes = GlobSet::new(&config.excludes);
    let roots = resolve_roots(config);
    let errors = ScanErrors::new();
    let boundaries = mount_boundaries();
    let mount_table = mounts::read_mounts().unwrap_or_default();
    let mut entries = roots
        .par_iter()
        .map(|(root, _)| {
            let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
            let visited = Mutex::new(HashSet::new());
            let ctx = WalkCtx {
                root,
                root_dev,
                boundaries: &boundaries,
                visited: &visited,
                excludes: &excludes,
                collect_metadata: config.metadata,
                errors: &errors,
                cancel,
                deadline: root_deadline(&mount_table, root),
            };
            walk(root, &ctx)
        })
        .reduce(WalkBuffer::default, |mut a, b| {
            a.merge(b);
            a
        });
    entries.sort();
    let (errors, error_paths) = errors.report();
    RescanPlan {
        entries,
        roots,
        errors,
        error_paths,
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
        error_paths,
    } = plan;
    let mut report = ScanReport {
        errors,
        error_paths,
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

/// Shared context for one root's walk.
struct WalkCtx<'a> {
    /// The top-level root this walk started from; the root itself is always
    /// entered even though it is usually a mount point.
    root: &'a Path,
    root_dev: Option<u64>,
    /// Mount points from `/proc/self/mountinfo`; never crossed (A1).
    boundaries: &'a HashSet<PathBuf>,
    /// Visited `(dev, inode)` directories, preventing traversal cycles (A1).
    visited: &'a Mutex<HashSet<(u64, u64)>>,
    excludes: &'a GlobSet,
    collect_metadata: bool,
    errors: &'a ScanErrors,
    /// Cancellation (A3): checked once per directory.
    cancel: Option<&'a ScanCancel<'a>>,
    /// Traversal deadline (A6): a network-filesystem root is time-boxed.
    deadline: Option<Instant>,
}

/// The set of mount points from `/proc/self/mountinfo`, used to prune nested
/// mounts even when they share the root's device (bind mounts, btrfs
/// subvolumes). Read once per scan: the mount table can change between scans
/// (hotplug), and one file read per scan is negligible next to the walk.
fn mount_boundaries() -> HashSet<PathBuf> {
    mounts::read_mountinfo()
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.mount_point)
        .collect()
}

/// Recursively walk `dir`, appending entries to `out`. Directories on a
/// different device than the root are not crossed, directories that are mount
/// points (other than the walk root itself) are not crossed even when they
/// share the device, excluded paths are pruned, and the visited `(dev, inode)`
/// set prevents traversal cycles.
fn walk(dir: &Path, ctx: &WalkCtx) -> WalkBuffer {
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut batch = WalkBuffer::default();

    // A3/A6: stop promptly on cancellation or when the traversal deadline for
    // a network root has passed. Checked once per directory, so the walk stops
    // within one readdir rather than recursing unboundedly.
    if let Some(cancel) = ctx.cancel {
        if cancel.cancelled() {
            return batch;
        }
    }
    if let Some(deadline) = ctx.deadline {
        if Instant::now() >= deadline {
            return batch;
        }
    }

    let WalkCtx {
        root,
        root_dev,
        boundaries,
        visited,
        excludes,
        collect_metadata,
        errors,
        ..
    } = *ctx;
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

            // Do not cross into another filesystem (a nested mount). A1: the
            // mountinfo boundary set also prunes nested mounts that share the
            // root's device (bind mounts, btrfs subvolumes), which the `dev`
            // check alone cannot see. The walk root itself is, of course,
            // entered.
            if is_dir {
                if boundaries.contains(&path) && path != root {
                    continue;
                }
                if let (Some(dev), Some(root_dev)) = (metadata.as_ref().map(|m| m.dev()), root_dev)
                {
                    if dev != root_dev {
                        continue;
                    }
                }
                // A1: track visited `(dev, inode)` so a traversal cycle (only
                // possible through mounts, which the boundary check above
                // already prunes) cannot loop the walk. Requires the metadata
                // this directory was stat'd for anyway.
                if let Some(m) = metadata.as_ref() {
                    let key = (m.dev(), m.ino());
                    if let Ok(mut seen) = visited.lock() {
                        if !seen.insert(key) {
                            continue;
                        }
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
        errors.record(dir);
    }

    // Recurse in parallel and merge each subtree's buffer exactly once, so the
    // walk never contends on a shared lock (F5).
    let children =
        subdirs
            .into_par_iter()
            .map(|d| walk(&d, ctx))
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
    use std::process::Command;

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
    fn unreadable_directories_are_counted_and_sampled() {
        // A mode-000 directory cannot be read by an unprivileged user; as root
        // every directory is readable, so the check cannot be exercised.
        if unsafe { libc::geteuid() } == 0 {
            println!("skip - unreadable-directory test (running as root)");
            return;
        }
        let base = temp_dir("scan-errors");
        fs::create_dir_all(base.join("locked")).unwrap();
        fs::write(base.join("ok.txt"), b"x").unwrap();
        let locked = base.join("locked");
        let mut perms = fs::metadata(&locked).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o000);
        fs::set_permissions(&locked, perms).unwrap();

        let config = ScanConfig::with_roots(vec![base.clone()]);
        let (_, report) = scan(&config);

        assert_eq!(report.errors, 1);
        assert_eq!(report.error_paths.len(), 1);
        assert_eq!(Path::new(&report.error_paths[0]), locked);

        // Restore so remove_dir_all succeeds.
        let mut perms = fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&locked, perms).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_cancelled_scan_returns_no_entries() {
        let base = temp_dir("scan-cancel");
        fs::create_dir_all(base.join("sub/deep")).unwrap();
        fs::write(base.join("a.txt"), b"x").unwrap();
        fs::write(base.join("sub/b.txt"), b"y").unwrap();
        fs::write(base.join("sub/deep/c.txt"), b"z").unwrap();

        // A3: a stale generation token (a superseding scan bumped it after
        // this scan took its token) makes the walk return immediately.
        let generation = std::sync::atomic::AtomicU64::new(0);
        let shutdown = std::sync::atomic::AtomicBool::new(false);
        let cancel = ScanCancel::new(&generation, &shutdown);
        generation.fetch_add(1, Ordering::Relaxed);
        let config = ScanConfig::with_roots(vec![base.clone()]);
        let plan = plan_rescan_cancellable(&config, Some(&cancel));
        assert_eq!(plan.entries.entries.len(), 0);

        // A3: the shutdown flag also cancels the walk.
        let generation = std::sync::atomic::AtomicU64::new(0);
        let shutdown = std::sync::atomic::AtomicBool::new(true);
        let cancel = ScanCancel::new(&generation, &shutdown);
        let plan = plan_rescan_cancellable(&config, Some(&cancel));
        assert_eq!(plan.entries.entries.len(), 0);

        // Without a cancel the same tree is fully walked (the root itself is
        // not part of the plan; its five descendants are).
        let plan = plan_rescan(&config);
        assert_eq!(plan.entries.entries.len(), 5);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn network_roots_are_time_boxed_and_local_roots_are_not() {
        let table = vec![
            Mount {
                source: "server:/share".into(),
                mount_point: PathBuf::from("/mnt/net"),
                fs_type: "nfs4".into(),
                options: String::new(),
            },
            Mount {
                source: "/dev/sda1".into(),
                mount_point: PathBuf::from("/mnt/local"),
                fs_type: "ext4".into(),
                options: String::new(),
            },
        ];
        assert!(root_deadline(&table, Path::new("/mnt/net/data")).is_some());
        assert!(root_deadline(&table, Path::new("/mnt/local/data")).is_none());
        assert!(root_deadline(&table, Path::new("/elsewhere")).is_none());
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

    /// A bind mount shares the root's device, so only the mountinfo boundary
    /// set can prune it. Mounting a directory onto its own descendant creates
    /// a traversal loop. Both need mount privileges; when they are unavailable
    /// (an unprivileged test run) the check is skipped rather than failed.
    #[test]
    fn a_bind_mount_and_a_synthetic_loop_are_not_crossed() {
        let base = temp_dir("scan-bind");
        fs::create_dir_all(base.join("sub/deeper")).unwrap();
        fs::write(base.join("sub/a.txt"), b"x").unwrap();
        fs::write(base.join("sub/deeper/b.txt"), b"y").unwrap();

        // Bind `sub` onto `sub_bind` (same device, a real mount point) and, if
        // the kernel allows it, onto `sub/deeper` (a loop).
        let mounted_bind = mount_bind(&base.join("sub"), &base.join("sub_bind"));
        let mounted_loop = mount_bind(&base.join("sub"), &base.join("sub/deeper"));

        if !mounted_bind && !mounted_loop {
            let _ = fs::remove_dir_all(&base);
            println!("skip - bind-mount boundary test (no mount privileges)");
            return;
        }

        let config = ScanConfig::with_roots(vec![base.clone()]);
        let (index, _) = scan(&config);
        let paths = indexed_paths(&index);

        // The real subtree is indexed once.
        assert!(paths.iter().any(|p| p.ends_with("/sub/a.txt")));
        assert!(paths.iter().any(|p| p.ends_with("/sub/deeper/b.txt")));
        // The bind-mount copy is never crossed, despite sharing the device.
        assert!(!paths.iter().any(|p| p.contains("sub_bind")));
        // The loop target is never entered, so the walk terminates and
        // `deeper/b.txt` appears exactly once.
        assert_eq!(
            paths
                .iter()
                .filter(|p| p.ends_with("/deeper/b.txt"))
                .count(),
            1
        );

        if mounted_bind {
            let _ = Command::new("umount").arg(base.join("sub_bind")).status();
        }
        if mounted_loop {
            let _ = Command::new("umount").arg(base.join("sub/deeper")).status();
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// Bind-mount `src` onto `target`, returning whether it succeeded.
    fn mount_bind(src: &Path, target: &Path) -> bool {
        let _ = fs::create_dir_all(target);
        Command::new("mount")
            .args(["--bind"])
            .arg(src)
            .arg(target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
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
