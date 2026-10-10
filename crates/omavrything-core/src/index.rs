//! Production in-memory index: arena-backed records with ranked substring
//! search.
//!
//! Records live in a flat `Vec<FileRecord>`; basenames and full paths live in
//! two byte arenas so a query scans contiguous memory. Directories are records
//! too, so the tree can be reconstructed and subtree updates can splice.
//!
//! Deletions are tombstones (`FLAG_DELETED`): the arenas are append-only, so a
//! removed record's bytes are not reclaimed until a future compaction pass. A
//! `HashMap<Box<str>, u32>` maps full paths to record indices for O(1)
//! insert/remove/rename lookups; it duplicates the path bytes, which is the
//! known memory tradeoff (risk R2).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;

use rayon::prelude::*;

use crate::query::{Query, Rank};

/// Above this many records, the scan is parallelized with rayon. Below it, the
/// thread-pool overhead outweighs the gain.
const PARALLEL_THRESHOLD: usize = 50_000;

/// `flags` bit: the record is a directory.
pub const FLAG_DIR: u8 = 1 << 0;
/// `flags` bit: the record is a symlink.
pub const FLAG_SYMLINK: u8 = 1 << 1;
/// `flags` bit: the record has been removed (tombstone).
pub const FLAG_DELETED: u8 = 1 << 2;

/// The filesystem kind of an indexed root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Btrfs,
    Ext4,
    Xfs,
    F2fs,
    Vfat,
    Ntfs,
    Other,
}

/// An indexed mount point.
#[derive(Debug, Clone)]
pub struct Root {
    pub path: String,
    pub fs_kind: FsKind,
}

/// One indexed filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRecord {
    /// Index of the parent directory record, or `u32::MAX` for a root.
    pub parent: u32,
    /// Offset of the basename into the name arena.
    pub name_off: u32,
    /// Byte length of the basename.
    pub name_len: u16,
    /// Offset of the full path into the path arena.
    pub path_off: u32,
    /// Byte length of the full path.
    pub path_len: u32,
    /// Size in bytes. Zero for directories.
    pub size: u64,
    /// Last-modified time, Unix seconds. Zero when unavailable.
    pub mtime: i64,
    /// Path depth (number of `/` separators).
    pub depth: u16,
    /// `FLAG_*` bits.
    pub flags: u8,
}

impl FileRecord {
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }

    pub fn is_symlink(&self) -> bool {
        self.flags & FLAG_SYMLINK != 0
    }

    pub fn is_deleted(&self) -> bool {
        self.flags & FLAG_DELETED != 0
    }
}

/// A resolved search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

/// The outcome of a query: the ranked page of hits plus the total match count.
#[derive(Debug, Default)]
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    pub total: usize,
}

/// The in-memory index.
#[derive(Debug, Default)]
pub struct Index {
    records: Vec<FileRecord>,
    names: Vec<u8>,
    paths: Vec<u8>,
    roots: Vec<Root>,
    by_path: HashMap<Box<str>, u32>,
    live: usize,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of live (non-deleted) records.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// Insert a root directory record and register the mount point.
    pub fn add_root(&mut self, path: &Path, fs_kind: FsKind) -> u32 {
        let idx = self.insert(path, true, 0, 0);
        self.roots.push(Root {
            path: path.to_string_lossy().into_owned(),
            fs_kind,
        });
        idx
    }

    /// Insert a file or directory record. Returns its record index.
    pub fn insert(&mut self, path: &Path, is_dir: bool, size: u64, mtime: i64) -> u32 {
        self.insert_with_flags(path, is_dir, false, size, mtime)
    }

    /// Insert a record with an explicit symlink flag.
    pub fn insert_with_flags(
        &mut self,
        path: &Path,
        is_dir: bool,
        is_symlink: bool,
        size: u64,
        mtime: i64,
    ) -> u32 {
        let path_str = path.to_string_lossy();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parent = path
            .parent()
            .and_then(|p| self.by_path.get(p.to_string_lossy().as_ref()).copied())
            .unwrap_or(u32::MAX);
        let depth = path_str
            .bytes()
            .filter(|&b| b == b'/')
            .count()
            .min(u16::MAX as usize) as u16;

        let name_off = self.names.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        let name_len = name.len().min(u16::MAX as usize) as u16;

        let path_off = self.paths.len() as u32;
        self.paths.extend_from_slice(path_str.as_bytes());
        let path_len = path_str.len() as u32;

        let mut flags = 0u8;
        if is_dir {
            flags |= FLAG_DIR;
        }
        if is_symlink {
            flags |= FLAG_SYMLINK;
        }

        let idx = self.records.len() as u32;
        self.records.push(FileRecord {
            parent,
            name_off,
            name_len,
            path_off,
            path_len,
            size,
            mtime,
            depth,
            flags,
        });
        self.by_path
            .insert(path_str.into_owned().into_boxed_str(), idx);
        self.live += 1;
        idx
    }

    /// The record index for `path`, if present.
    pub fn find(&self, path: &Path) -> Option<u32> {
        self.by_path.get(path.to_string_lossy().as_ref()).copied()
    }

    pub fn record(&self, idx: u32) -> &FileRecord {
        &self.records[idx as usize]
    }

    /// The basename of a record.
    pub fn name(&self, idx: u32) -> &str {
        let rec = &self.records[idx as usize];
        let start = rec.name_off as usize;
        std::str::from_utf8(&self.names[start..start + rec.name_len as usize]).unwrap_or("")
    }

    /// The full path of a record.
    pub fn path(&self, idx: u32) -> &str {
        let rec = &self.records[idx as usize];
        let start = rec.path_off as usize;
        std::str::from_utf8(&self.paths[start..start + rec.path_len as usize]).unwrap_or("")
    }

    fn path_bytes(&self, rec: &FileRecord) -> &[u8] {
        let start = rec.path_off as usize;
        &self.paths[start..start + rec.path_len as usize]
    }

    fn name_bytes(&self, rec: &FileRecord) -> &[u8] {
        let start = rec.name_off as usize;
        &self.names[start..start + rec.name_len as usize]
    }

    /// Remove `path` and its subtree. Returns whether anything was removed.
    ///
    /// Records are tombstoned rather than physically removed, so the arenas stay
    /// append-only and record indices remain stable.
    pub fn remove(&mut self, path: &Path) -> bool {
        let key = path.to_string_lossy();
        let prefix = format!("{key}/");
        let key_bytes = key.as_bytes();
        let prefix_bytes = prefix.as_bytes();

        let mut to_delete: Vec<u32> = Vec::new();
        for (i, rec) in self.records.iter().enumerate() {
            if rec.is_deleted() {
                continue;
            }
            let p = self.path_bytes(rec);
            if p == key_bytes || p.starts_with(prefix_bytes) {
                to_delete.push(i as u32);
            }
        }
        if to_delete.is_empty() {
            return false;
        }

        for &i in &to_delete {
            self.records[i as usize].flags |= FLAG_DELETED;
            let p = self.path_bytes(&self.records[i as usize]).to_vec();
            self.by_path.remove(String::from_utf8_lossy(&p).as_ref());
        }
        self.live -= to_delete.len();
        true
    }

    /// Rename or move `old` to `new`, updating the record and its subtree.
    /// Returns whether anything was renamed.
    pub fn rename(&mut self, old: &Path, new: &Path) -> bool {
        let old_key = old.to_string_lossy();
        let new_key = new.to_string_lossy();
        let old_prefix = format!("{old_key}/");
        let new_prefix = format!("{new_key}/");
        let old_bytes = old_key.as_bytes();
        let old_prefix_bytes = old_prefix.as_bytes();

        let affected: Vec<u32> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, rec)| {
                if rec.is_deleted() {
                    return false;
                }
                let p = self.path_bytes(rec);
                p == old_bytes || p.starts_with(old_prefix_bytes)
            })
            .map(|(i, _)| i as u32)
            .collect();
        if affected.is_empty() {
            return false;
        }

        // The renamed record itself, if it is indexed (a directory may be
        // renamed before its own record exists).
        let exact = self.by_path.get(old_key.as_ref()).copied();
        let new_parent = new
            .parent()
            .and_then(|p| self.by_path.get(p.to_string_lossy().as_ref()).copied())
            .unwrap_or(u32::MAX);

        for i in affected {
            let rec = self.records[i as usize];
            let old_path = self.path_bytes(&rec).to_vec();
            let new_path: String = if old_path == old_bytes {
                new_key.to_string()
            } else {
                let suffix = &old_path[old_prefix_bytes.len()..];
                format!("{new_prefix}{}", String::from_utf8_lossy(suffix))
            };

            let path_off = self.paths.len() as u32;
            self.paths.extend_from_slice(new_path.as_bytes());
            let path_len = new_path.len() as u32;
            let depth = new_path
                .bytes()
                .filter(|&b| b == b'/')
                .count()
                .min(u16::MAX as usize) as u16;

            let is_exact = exact == Some(i);
            let (name_off, name_len) = if is_exact {
                let name = new
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let off = self.names.len() as u32;
                self.names.extend_from_slice(name.as_bytes());
                (off, name.len().min(u16::MAX as usize) as u16)
            } else {
                (rec.name_off, rec.name_len)
            };

            self.by_path
                .remove(String::from_utf8_lossy(&old_path).as_ref());
            self.by_path.insert(new_path.into_boxed_str(), i);

            let rec = &mut self.records[i as usize];
            rec.path_off = path_off;
            rec.path_len = path_len;
            rec.depth = depth;
            rec.name_off = name_off;
            rec.name_len = name_len;
            if is_exact {
                rec.parent = new_parent;
            }
        }
        true
    }

    /// Search the index, returning the ranked page `[offset, offset + limit)`
    /// and the total number of matches.
    ///
    /// An empty query matches nothing.
    pub fn search(&self, query: &str, limit: usize, offset: usize) -> SearchOutcome {
        let q = Query::parse(query);
        if q.is_empty() || limit == 0 {
            return SearchOutcome::default();
        }

        let want = offset.saturating_add(limit);
        let (total, top) = if self.records.len() >= PARALLEL_THRESHOLD {
            self.scan_parallel(&q, want)
        } else {
            self.scan_sequential(&q, want)
        };

        let hits = top
            .into_sorted()
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(i, _)| self.hit(i))
            .collect();

        SearchOutcome { hits, total }
    }

    /// Score one record against the query, or `None` if it does not match.
    fn match_record(&self, rec: &FileRecord, q: &Query) -> Option<Rank> {
        if rec.is_deleted() {
            return None;
        }
        let path = self.path_bytes(rec);
        let mut first_pos = u32::MAX;
        for term in q.terms() {
            let pos = term.find(path)?;
            first_pos = first_pos.min(pos as u32);
        }
        let name = self.name_bytes(rec);
        let name_match = q.terms().iter().all(|t| t.find(name).is_some());
        Some(Rank {
            name_match,
            first_pos,
            path_len: rec.path_len,
            depth: rec.depth,
            mtime: rec.mtime,
        })
    }

    fn scan_sequential(&self, q: &Query, want: usize) -> (usize, TopN) {
        let mut top = TopN::new(want);
        let mut total = 0usize;
        for (i, rec) in self.records.iter().enumerate() {
            if let Some(rank) = self.match_record(rec, q) {
                total += 1;
                top.push(i as u32, rank);
            }
        }
        (total, top)
    }

    fn scan_parallel(&self, q: &Query, want: usize) -> (usize, TopN) {
        self.records
            .par_iter()
            .enumerate()
            .fold(
                || (0usize, TopN::new(want)),
                |(count, mut top), (i, rec)| {
                    if let Some(rank) = self.match_record(rec, q) {
                        top.push(i as u32, rank);
                        (count + 1, top)
                    } else {
                        (count, top)
                    }
                },
            )
            .reduce(
                || (0usize, TopN::new(want)),
                |(c1, mut t1), (c2, t2)| {
                    t1.merge(t2);
                    (c1 + c2, t1)
                },
            )
    }

    fn hit(&self, idx: u32) -> Hit {
        let rec = &self.records[idx as usize];
        Hit {
            path: self.path(idx).to_string(),
            name: self.name(idx).to_string(),
            size: rec.size,
            mtime: rec.mtime,
            is_dir: rec.is_dir(),
        }
    }
}

/// A bounded top-N collector. Keeps the `limit` best-ranked records seen so far
/// in a min-heap keyed by [`Rank`], so the root is always the worst of the set.
struct TopN {
    limit: usize,
    heap: BinaryHeap<Reverse<(Rank, u32)>>,
}

impl TopN {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::with_capacity(limit.min(4096)),
        }
    }

    fn push(&mut self, idx: u32, rank: Rank) {
        if self.limit == 0 {
            return;
        }
        if self.heap.len() < self.limit {
            self.heap.push(Reverse((rank, idx)));
        } else if let Some(Reverse((worst, _))) = self.heap.peek() {
            if rank > *worst {
                self.heap.pop();
                self.heap.push(Reverse((rank, idx)));
            }
        }
    }

    /// Merge another collector's candidates into this one.
    fn merge(&mut self, other: TopN) {
        for Reverse((rank, idx)) in other.heap {
            self.push(idx, rank);
        }
    }

    /// Consume the collector, returning `(record index, rank)` best-first.
    fn into_sorted(self) -> Vec<(u32, Rank)> {
        let mut v: Vec<(u32, Rank)> = self
            .heap
            .into_iter()
            .map(|Reverse((rank, idx))| (idx, rank))
            .collect();
        v.sort_by_key(|a| Reverse(a.1));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx_of(index: &Index, path: &str) -> u32 {
        index.find(Path::new(path)).expect("path not indexed")
    }

    #[test]
    fn insert_stores_name_path_and_metadata() {
        let mut index = Index::new();
        let i = index.insert(Path::new("/home/sam/report.pdf"), false, 1234, 42);
        assert_eq!(index.len(), 1);
        assert_eq!(index.name(i), "report.pdf");
        assert_eq!(index.path(i), "/home/sam/report.pdf");
        assert_eq!(index.record(i).size, 1234);
        assert_eq!(index.record(i).mtime, 42);
        assert!(!index.record(i).is_dir());
    }

    #[test]
    fn insert_links_parent_and_depth() {
        let mut index = Index::new();
        let root = index.add_root(Path::new("/"), FsKind::Other);
        let home = index.insert(Path::new("/home"), true, 0, 0);
        let file = index.insert(Path::new("/home/sam.txt"), false, 1, 0);
        assert_eq!(index.record(home).parent, root);
        assert_eq!(index.record(file).parent, home);
        assert_eq!(index.record(root).depth, 1);
        assert_eq!(index.record(file).depth, 2);
    }

    #[test]
    fn empty_query_matches_nothing() {
        let mut index = Index::new();
        index.insert(Path::new("/a.txt"), false, 1, 0);
        assert!(index.search("", 10, 0).hits.is_empty());
        assert!(index.search("   ", 10, 0).hits.is_empty());
    }

    #[test]
    fn search_is_case_insensitive_and_ands_terms() {
        let mut index = Index::new();
        index.insert(
            Path::new("/home/sam/Documents/report-2024.pdf"),
            false,
            1,
            0,
        );
        index.insert(Path::new("/home/sam/Documents/notes.md"), false, 1, 0);
        index.insert(Path::new("/home/sam/Pictures"), true, 0, 0);

        assert_eq!(index.search("REPORT", 10, 0).hits.len(), 1);
        assert_eq!(index.search("documents report", 10, 0).hits.len(), 1);
        assert_eq!(index.search("documents pictures", 10, 0).hits.len(), 0);
    }

    #[test]
    fn search_ranks_name_matches_over_path_only() {
        let mut index = Index::new();
        index.insert(Path::new("/docs/report/notes.txt"), false, 1, 0);
        index.insert(Path::new("/docs/notes/report.txt"), false, 1, 0);

        let out = index.search("report", 10, 0);
        assert_eq!(out.hits.len(), 2);
        assert_eq!(out.hits[0].name, "report.txt");
    }

    #[test]
    fn search_prefers_shorter_and_shallower_paths() {
        let mut index = Index::new();
        index.insert(Path::new("/deep/deep/deep/report.txt"), false, 1, 0);
        index.insert(Path::new("/a/b/report.txt"), false, 1, 0);
        index.insert(Path::new("/x/report.txt"), false, 1, 0);

        let out = index.search("report", 10, 0);
        let paths: Vec<&str> = out.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/x/report.txt",
                "/a/b/report.txt",
                "/deep/deep/deep/report.txt"
            ]
        );
    }

    #[test]
    fn search_total_counts_all_matches_and_offset_pages() {
        let mut index = Index::new();
        for i in 0..5 {
            index.insert(Path::new(&format!("/home/report-{i}.txt")), false, 1, 0);
        }
        let first = index.search("report", 2, 0);
        assert_eq!(first.hits.len(), 2);
        assert_eq!(first.total, 5);

        let second = index.search("report", 2, 2);
        assert_eq!(second.hits.len(), 2);
        assert_eq!(second.total, 5);

        let last = index.search("report", 2, 4);
        assert_eq!(last.hits.len(), 1);
        assert_eq!(last.total, 5);

        let past = index.search("report", 2, 99);
        assert!(past.hits.is_empty());
        assert_eq!(past.total, 5);
    }

    #[test]
    fn unicode_terms_match_case_insensitively() {
        let mut index = Index::new();
        index.insert(Path::new("/tmp/Ünïcode.txt"), false, 1, 0);
        assert_eq!(index.search("ünïcode", 10, 0).hits.len(), 1);
        assert_eq!(index.search("ÜNÏCODE", 10, 0).hits.len(), 1);
    }

    #[test]
    fn remove_tombstones_the_subtree() {
        let mut index = Index::new();
        index.insert(Path::new("/home/sam/a.txt"), false, 1, 0);
        index.insert(Path::new("/home/sam/sub/b.txt"), false, 1, 0);
        index.insert(Path::new("/home/other.txt"), false, 1, 0);
        assert_eq!(index.len(), 3);

        assert!(index.remove(Path::new("/home/sam")));
        assert_eq!(index.len(), 1);
        assert!(index.find(Path::new("/home/sam/a.txt")).is_none());
        assert!(index.find(Path::new("/home/sam/sub/b.txt")).is_none());
        assert!(index.find(Path::new("/home/other.txt")).is_some());

        // Removed records are not returned by search.
        assert_eq!(index.search("a.txt", 10, 0).hits.len(), 0);
        assert_eq!(index.search("other", 10, 0).hits.len(), 1);
    }

    #[test]
    fn remove_missing_path_is_a_noop() {
        let mut index = Index::new();
        index.insert(Path::new("/a.txt"), false, 1, 0);
        assert!(!index.remove(Path::new("/nope.txt")));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn rename_updates_the_record_and_subtree() {
        let mut index = Index::new();
        index.insert(Path::new("/home/sam/a.txt"), false, 1, 0);
        index.insert(Path::new("/home/sam/sub/b.txt"), false, 1, 0);

        assert!(index.rename(Path::new("/home/sam"), Path::new("/home/bob")));
        assert!(index.find(Path::new("/home/sam")).is_none());
        assert!(index.find(Path::new("/home/sam/sub/b.txt")).is_none());

        let a = idx_of(&index, "/home/bob/a.txt");
        assert_eq!(index.name(a), "a.txt");
        let b = idx_of(&index, "/home/bob/sub/b.txt");
        assert_eq!(index.name(b), "b.txt");
        assert_eq!(index.path(b), "/home/bob/sub/b.txt");

        // Search sees the new paths, not the old ones.
        assert_eq!(index.search("bob", 10, 0).hits.len(), 2);
        assert_eq!(index.search("sam", 10, 0).hits.len(), 0);
    }

    #[test]
    fn rename_missing_path_is_a_noop() {
        let mut index = Index::new();
        index.insert(Path::new("/a.txt"), false, 1, 0);
        assert!(!index.rename(Path::new("/nope.txt"), Path::new("/x.txt")));
    }

    #[test]
    fn roots_are_recorded() {
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Btrfs);
        index.add_root(Path::new("/mnt/data"), FsKind::Ntfs);
        assert_eq!(index.roots().len(), 2);
        assert_eq!(index.roots()[1].fs_kind, FsKind::Ntfs);
    }
}
