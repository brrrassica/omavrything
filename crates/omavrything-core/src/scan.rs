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
    roots.par_iter().for_each(|(root, _)| {
        let root_dev = fs::metadata(root).map(|m| m.dev()).ok();
        walk(root, root_dev, &excludes, &collected);
    });
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

/// Resolve the roots to scan: explicit roots, or non-pseudo mounts.
fn resolve_roots(config: &ScanConfig) -> Vec<(PathBuf, FsKind)> {
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
fn walk(dir: &Path, root_dev: Option<u64>, excludes: &GlobSet, out: &Mutex<Vec<WalkEntry>>) {
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
    }

    if !batch.is_empty() {
        out.lock().unwrap().extend(batch);
    }

    subdirs
        .into_par_iter()
        .for_each(|d| walk(&d, root_dev, excludes, out));
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
}
