//! Production in-memory index: arena-backed records with ranked substring
//! search.
//!
//! Records live in a flat `Vec<FileRecord>`; basenames and full paths live in
//! two byte arenas so a query scans contiguous memory. Directories are records
//! too, so the tree can be reconstructed and subtree updates can splice.
//!
//! Deletions are tombstones (`FLAG_DELETED`): the arenas are append-only, so a
//! removed record's bytes are not reclaimed until a future compaction pass. A
//! [`PathIndex`] maps full paths to record indices for O(1)
//! insert/remove/rename lookups; it keys on a 64-bit hash of the path and
//! confirms matches against the path arena, so it does **not** duplicate the
//! path bytes (the previous `HashMap<Box<str>, u32>` did, and dominated the
//! index's memory use).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

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
    pub name_len: u32,
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

/// A per-query timing breakdown, for profiling.
///
/// Produced by [`Index::search_profiled`]. The stages are disjoint and sum to
/// roughly [`SearchTiming::total`] (the small remainder is bookkeeping):
///
/// - `parse`: splitting the query into terms and precompiling them.
/// - `scan`: the linear scan over every record, including the top-N heap
///   pushes. This dominates for large indexes.
/// - `sort`: ordering the collected top-N candidates.
/// - `materialize`: allocating the `String`s for the returned page of hits.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchTiming {
    pub parse: Duration,
    pub scan: Duration,
    pub sort: Duration,
    pub materialize: Duration,
    pub total: Duration,
    /// Number of records examined (live and tombstoned).
    pub scanned: usize,
    /// Number of records that matched the query.
    pub matched: usize,
    /// Whether the parallel (rayon) scan path was taken.
    pub parallel: bool,
}

impl SearchTiming {
    /// A one-line human-readable summary, e.g. for daemon logs.
    pub fn summary(&self) -> String {
        format!(
            "total={:.2}ms parse={:.2}ms scan={:.2}ms sort={:.2}ms materialize={:.2}ms \
             scanned={} matched={} {}",
            self.total.as_secs_f64() * 1e3,
            self.parse.as_secs_f64() * 1e3,
            self.scan.as_secs_f64() * 1e3,
            self.sort.as_secs_f64() * 1e3,
            self.materialize.as_secs_f64() * 1e3,
            self.scanned,
            self.matched,
            if self.parallel {
                "parallel"
            } else {
                "sequential"
            },
        )
    }
}

/// Maps full paths to record indices without duplicating the path bytes.
///
/// The primary map is keyed by a 64-bit hash of the path; the path arena is
/// consulted to confirm a match. A hash collision (astronomically rare) is kept
/// in `collisions`, keyed by the full path, so lookups stay correct. This
/// replaces the previous `HashMap<Box<str>, u32>`, which stored a second copy of
/// every path and dominated the index's memory use.
#[derive(Debug, Default, Clone)]
struct PathIndex {
    main: HashMap<u64, u32>,
    collisions: HashMap<Box<str>, u32>,
}

impl PathIndex {
    fn with_capacity(n: usize) -> Self {
        Self {
            main: HashMap::with_capacity(n),
            collisions: HashMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.main.len() + self.collisions.len()
    }

    fn clear(&mut self) {
        self.main.clear();
        self.collisions.clear();
    }

    fn shrink_to_fit(&mut self) {
        self.main.shrink_to_fit();
        self.collisions.shrink_to_fit();
    }

    /// The record index for `key`, or `None`. `records`/`paths` are the index's
    /// arenas, used to confirm the hash match.
    fn get(&self, key: &str, records: &[FileRecord], paths: &[u8]) -> Option<u32> {
        let h = fnv1a64(key.as_bytes());
        if let Some(&idx) = self.main.get(&h) {
            let rec = &records[idx as usize];
            let start = rec.path_off as usize;
            if &paths[start..start + rec.path_len as usize] == key.as_bytes() {
                return Some(idx);
            }
        }
        self.collisions.get(key).copied()
    }

    /// Record `key` -> `idx`, keeping a colliding path in the fallback map.
    fn insert(&mut self, key: &str, idx: u32, records: &[FileRecord], paths: &[u8]) {
        let h = fnv1a64(key.as_bytes());
        let collides = self.main.get(&h).copied().is_some_and(|existing| {
            let rec = &records[existing as usize];
            let start = rec.path_off as usize;
            &paths[start..start + rec.path_len as usize] != key.as_bytes()
        });
        if collides {
            self.collisions.insert(key.into(), idx);
        } else {
            self.main.insert(h, idx);
            self.collisions.remove(key);
        }
    }

    /// Like [`PathIndex::insert`], but takes the key as raw bytes so callers
    /// that already have the path in the arena (`compact`, `from_parts`) avoid
    /// allocating a `String` per record. The caller must ensure `key` is not
    /// currently in `collisions`, which holds when the map is built from
    /// scratch.
    fn insert_bytes(&mut self, key: &[u8], idx: u32, records: &[FileRecord], paths: &[u8]) {
        let h = fnv1a64(key);
        let collides = self.main.get(&h).copied().is_some_and(|existing| {
            let rec = &records[existing as usize];
            let start = rec.path_off as usize;
            &paths[start..start + rec.path_len as usize] != key
        });
        if collides {
            self.collisions.insert(
                String::from_utf8_lossy(key).into_owned().into_boxed_str(),
                idx,
            );
        } else {
            self.main.insert(h, idx);
        }
    }

    /// Remove `key` from the index.
    fn remove(&mut self, key: &str, records: &[FileRecord], paths: &[u8]) {
        let h = fnv1a64(key.as_bytes());
        if let Some(&idx) = self.main.get(&h) {
            let rec = &records[idx as usize];
            let start = rec.path_off as usize;
            if &paths[start..start + rec.path_len as usize] == key.as_bytes() {
                self.main.remove(&h);
                return;
            }
        }
        self.collisions.remove(key);
    }
}

/// FNV-1a 64-bit hash, used to key the path index without storing the path.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The in-memory index.
#[derive(Debug, Default, Clone)]
pub struct Index {
    records: Vec<FileRecord>,
    names: Vec<u8>,
    paths: Vec<u8>,
    roots: Vec<Root>,
    by_path: PathIndex,
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

    /// Keep only the roots whose path is in `keep`.
    pub(crate) fn retain_roots(&mut self, keep: &HashSet<String>) {
        self.roots.retain(|r| keep.contains(&r.path));
    }

    /// The raw records, for snapshot serialization.
    pub(crate) fn records(&self) -> &[FileRecord] {
        &self.records
    }

    /// An estimate of the heap memory held by the index, in bytes.
    ///
    /// Counts the record vector, both arenas, and a rough per-entry cost for the
    /// path map (a hash and a record index, not the path bytes). Used for
    /// `GetStatus`.
    pub fn memory_bytes(&self) -> u64 {
        let records = self.records.len() * std::mem::size_of::<FileRecord>();
        let arenas = self.names.len() + self.paths.len();
        // The path index stores a 64-bit hash and a 32-bit record index per
        // entry (plus a control byte), not a copy of the path bytes.
        let map =
            self.by_path.len() * (std::mem::size_of::<u64>() + std::mem::size_of::<u32>() + 1);
        (records + arenas + map) as u64
    }

    /// Rebuild the arenas and record vector, dropping tombstones and reclaiming
    /// their bytes. Record indices change, so `parent` links and the path map are
    /// remapped. A no-op when there are no tombstones.
    pub fn compact(&mut self) {
        if !self.records.iter().any(|r| r.is_deleted()) {
            return;
        }

        let mut remap = vec![u32::MAX; self.records.len()];
        let mut records = Vec::with_capacity(self.live);
        let mut names = Vec::with_capacity(self.names.len());
        let mut paths = Vec::with_capacity(self.paths.len());

        for (i, rec) in self.records.iter().enumerate() {
            if rec.is_deleted() {
                continue;
            }
            remap[i] = records.len() as u32;

            let name_off = names.len() as u32;
            names.extend_from_slice(self.name_bytes(rec));
            let path_off = paths.len() as u32;
            paths.extend_from_slice(self.path_bytes(rec));

            let parent = if rec.parent == u32::MAX {
                u32::MAX
            } else {
                remap[rec.parent as usize]
            };
            records.push(FileRecord {
                parent,
                name_off,
                name_len: rec.name_len,
                path_off,
                path_len: rec.path_len,
                size: rec.size,
                mtime: rec.mtime,
                depth: rec.depth,
                flags: rec.flags,
            });
        }

        self.records = records;
        self.names = names;
        self.paths = paths;
        self.by_path.clear();
        for (i, rec) in self.records.iter().enumerate() {
            let start = rec.path_off as usize;
            let end = start + rec.path_len as usize;
            self.by_path.insert_bytes(
                &self.paths[start..end],
                i as u32,
                &self.records,
                &self.paths,
            );
        }
        // The arenas were pre-sized to their pre-compaction length, which still
        // includes the tombstones just dropped; release the excess capacity.
        self.records.shrink_to_fit();
        self.names.shrink_to_fit();
        self.paths.shrink_to_fit();
        self.by_path.shrink_to_fit();
    }

    /// The name arena, for snapshot serialization.
    pub(crate) fn names(&self) -> &[u8] {
        &self.names
    }

    /// The path arena, for snapshot serialization.
    pub(crate) fn paths(&self) -> &[u8] {
        &self.paths
    }

    /// Rebuild an index from raw parts, reconstructing the derived path map and
    /// live count. Used by the snapshot loader.
    pub(crate) fn from_parts(
        records: Vec<FileRecord>,
        names: Vec<u8>,
        paths: Vec<u8>,
        roots: Vec<Root>,
    ) -> Self {
        let mut by_path = PathIndex::with_capacity(records.len());
        let mut live = 0usize;
        for (i, rec) in records.iter().enumerate() {
            if rec.is_deleted() {
                continue;
            }
            live += 1;
            let start = rec.path_off as usize;
            let end = start + rec.path_len as usize;
            if let Some(bytes) = paths.get(start..end) {
                by_path.insert_bytes(bytes, i as u32, &records, &paths);
            }
        }
        Self {
            records,
            names,
            paths,
            roots,
            by_path,
            live,
        }
    }

    /// Iterate over live (non-deleted) records as `(record index, record)`.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &FileRecord)> {
        self.records
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.is_deleted())
            .map(|(i, r)| (i as u32, r))
    }

    /// Insert a root directory record and register the mount point.
    ///
    /// Idempotent: re-adding a root must not create a duplicate record, and the
    /// recorded filesystem kind is refreshed if it changed (e.g. a device was
    /// reformatted between runs).
    pub fn add_root(&mut self, path: &Path, fs_kind: FsKind) -> u32 {
        let idx = match self.find(path) {
            Some(idx) => idx,
            None => self.insert(path, true, 0, 0),
        };
        let path_str = path.to_string_lossy().into_owned();
        match self.roots.iter_mut().find(|r| r.path == path_str) {
            Some(root) => root.fs_kind = fs_kind,
            None => self.roots.push(Root {
                path: path_str,
                fs_kind,
            }),
        }
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
            .and_then(|p| self.find_str(p.to_string_lossy().as_ref()))
            .unwrap_or(u32::MAX);
        let depth = path_str
            .bytes()
            .filter(|&b| b == b'/')
            .count()
            .min(u16::MAX as usize) as u16;

        let name_off = self.names.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        let name_len = name.len() as u32;

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
        self.map_insert(path_str.as_ref(), idx);
        self.live += 1;
        idx
    }

    /// The record index for `path`, if present.
    pub fn find(&self, path: &Path) -> Option<u32> {
        self.find_str(path.to_string_lossy().as_ref())
    }

    /// The record index for a path string, if present.
    fn find_str(&self, key: &str) -> Option<u32> {
        self.by_path.get(key, &self.records, &self.paths)
    }

    /// Record `key` -> `idx` in the path index.
    fn map_insert(&mut self, key: &str, idx: u32) {
        self.by_path.insert(key, idx, &self.records, &self.paths);
    }

    /// Remove `key` from the path index.
    fn map_remove(&mut self, key: &str) {
        self.by_path.remove(key, &self.records, &self.paths);
    }

    /// Update the size and mtime of an existing record. Returns whether the
    /// record existed.
    pub fn update_metadata(&mut self, path: &Path, size: u64, mtime: i64) -> bool {
        let Some(idx) = self.find_str(path.to_string_lossy().as_ref()) else {
            return false;
        };
        let rec = &mut self.records[idx as usize];
        rec.size = size;
        rec.mtime = mtime;
        true
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
        // For the filesystem root "/", the subtree prefix is "/" itself; for any
        // other path it is "<key>/".
        let prefix = if key.ends_with('/') {
            key.to_string()
        } else {
            format!("{key}/")
        };
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
            self.map_remove(String::from_utf8_lossy(&p).as_ref());
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
        let exact = self.find_str(old_key.as_ref());
        let new_parent = new
            .parent()
            .and_then(|p| self.find_str(p.to_string_lossy().as_ref()))
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
                (off, name.len() as u32)
            } else {
                (rec.name_off, rec.name_len)
            };

            self.map_remove(String::from_utf8_lossy(&old_path).as_ref());
            // If the destination already exists (rename over an existing file or
            // empty directory), tombstone the displaced record so it does not
            // linger as a duplicate.
            let existing = self.find_str(new_path.as_str());
            if let Some(existing) = existing {
                if existing != i && !self.records[existing as usize].is_deleted() {
                    self.records[existing as usize].flags |= FLAG_DELETED;
                    self.live -= 1;
                }
            }
            self.map_insert(new_path.as_str(), i);

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
        self.search_profiled(query, limit, offset).0
    }

    /// Like [`Index::search`], but also returns a [`SearchTiming`] breakdown of
    /// where the time went. Used by the profiling example and the daemon's
    /// opt-in query profiler.
    pub fn search_profiled(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> (SearchOutcome, SearchTiming) {
        let started = Instant::now();

        let parse_started = Instant::now();
        let q = Query::parse(query);
        let parse = parse_started.elapsed();

        if q.is_empty() || limit == 0 {
            return (
                SearchOutcome::default(),
                SearchTiming {
                    parse,
                    total: started.elapsed(),
                    scanned: self.records.len(),
                    ..SearchTiming::default()
                },
            );
        }

        let want = offset.saturating_add(limit);
        let parallel = self.records.len() >= PARALLEL_THRESHOLD;

        let scan_started = Instant::now();
        let (total, top) = if parallel {
            self.scan_parallel(&q, want)
        } else {
            self.scan_sequential(&q, want)
        };
        let scan = scan_started.elapsed();

        let sort_started = Instant::now();
        let sorted = top.into_sorted();
        let sort = sort_started.elapsed();

        let materialize_started = Instant::now();
        let hits = sorted
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(i, _)| self.hit(i))
            .collect();
        let materialize = materialize_started.elapsed();

        (
            SearchOutcome { hits, total },
            SearchTiming {
                parse,
                scan,
                sort,
                materialize,
                total: started.elapsed(),
                scanned: self.records.len(),
                matched: total,
                parallel,
            },
        )
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
/// in a min-heap keyed by `(Rank, Reverse<record index>)`, so the root is always
/// the worst of the set. The record index is part of the key so that ties are
/// broken deterministically (lower index wins), which keeps result ordering
/// stable across the parallel scan.
struct TopN {
    limit: usize,
    heap: BinaryHeap<Reverse<(Rank, Reverse<u32>)>>,
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
        let key = (rank, Reverse(idx));
        if self.heap.len() < self.limit {
            self.heap.push(Reverse(key));
        } else if let Some(Reverse(worst)) = self.heap.peek() {
            if key > *worst {
                self.heap.pop();
                self.heap.push(Reverse(key));
            }
        }
    }

    /// Merge another collector's candidates into this one.
    fn merge(&mut self, other: TopN) {
        for Reverse((rank, Reverse(idx))) in other.heap {
            self.push(idx, rank);
        }
    }

    /// Consume the collector, returning `(record index, rank)` best-first.
    fn into_sorted(self) -> Vec<(u32, Rank)> {
        let mut v: Vec<(u32, Rank)> = self
            .heap
            .into_iter()
            .map(|Reverse((rank, Reverse(idx)))| (idx, rank))
            .collect();
        // Best first; ties broken by ascending record index for determinism.
        v.sort_by_key(|a| Reverse((a.1, Reverse(a.0))));
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

    #[test]
    fn add_root_is_idempotent() {
        let mut index = Index::new();
        let a = index.add_root(Path::new("/mnt/data"), FsKind::Ntfs);
        let b = index.add_root(Path::new("/mnt/data"), FsKind::Ntfs);
        assert_eq!(a, b);
        assert_eq!(index.len(), 1);
        assert_eq!(index.roots().len(), 1);
    }

    #[test]
    fn remove_root_slash_tombstones_the_whole_tree() {
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        index.insert(Path::new("/home/sam/a.txt"), false, 1, 0);
        index.insert(Path::new("/etc/hosts"), false, 1, 0);
        assert!(index.remove(Path::new("/")));
        assert_eq!(index.len(), 0);
        assert!(index.search("a.txt", 10, 0).hits.is_empty());
    }

    #[test]
    fn rename_over_existing_does_not_leave_a_duplicate() {
        let mut index = Index::new();
        index.insert(Path::new("/home/sam/old.txt"), false, 1, 0);
        index.insert(Path::new("/home/sam/new.txt"), false, 2, 0);

        assert!(index.rename(
            Path::new("/home/sam/old.txt"),
            Path::new("/home/sam/new.txt")
        ));
        assert_eq!(index.len(), 1);
        let out = index.search("new.txt", 10, 0);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].size, 1);
    }

    #[test]
    fn compact_reclaims_tombstones_and_remaps_parents() {
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        index.insert(Path::new("/home"), true, 0, 0);
        index.insert(Path::new("/home/a.txt"), false, 1, 0);
        index.insert(Path::new("/home/b.txt"), false, 2, 0);
        assert!(index.remove(Path::new("/home/a.txt")));
        assert_eq!(index.len(), 3); // root, home, b.txt

        index.compact();
        assert_eq!(index.len(), 3);
        assert_eq!(index.records().len(), 3);
        assert!(index.find(Path::new("/home/a.txt")).is_none());

        let home = index.find(Path::new("/home")).unwrap();
        let b = index.find(Path::new("/home/b.txt")).unwrap();
        assert_eq!(index.path(b), "/home/b.txt");
        // Parent links are remapped to the compacted record indices.
        assert_eq!(index.record(b).parent, home);
        assert_eq!(index.search("b.txt", 10, 0).hits.len(), 1);
    }

    #[test]
    fn compact_is_a_noop_without_tombstones() {
        let mut index = Index::new();
        index.insert(Path::new("/a.txt"), false, 1, 0);
        let before = index.records().len();
        index.compact();
        assert_eq!(index.records().len(), before);
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn parallel_scan_is_correct_and_bounded() {
        // Above PARALLEL_THRESHOLD so the rayon path is exercised.
        let mut index = Index::new();
        let n = PARALLEL_THRESHOLD + 1_000;
        for i in 0..n {
            index.insert(
                Path::new(&format!("/data/dir{:03}/file-{i:06}.txt", i % 100)),
                false,
                i as u64,
                0,
            );
        }
        let started = std::time::Instant::now();
        let out = index.search("file-000123", 100, 0);
        let elapsed = started.elapsed();
        assert_eq!(out.total, 1);
        assert_eq!(out.hits.len(), 1);
        // A coarse regression gate: a linear scan of ~51k records must be far
        // under this bound even in a debug build.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "search took {elapsed:?}"
        );
    }

    #[test]
    fn search_profiled_matches_search_and_reports_timing() {
        let mut index = Index::new();
        for i in 0..100 {
            index.insert(Path::new(&format!("/home/report-{i}.txt")), false, 1, 0);
        }

        let plain = index.search("report", 10, 0);
        let (profiled, timing) = index.search_profiled("report", 10, 0);

        // The profiled path must return exactly the same results.
        assert_eq!(profiled.total, plain.total);
        assert_eq!(profiled.hits, plain.hits);

        // The breakdown is populated and internally consistent.
        assert_eq!(timing.scanned, index.records().len());
        assert_eq!(timing.matched, plain.total);
        assert!(
            !timing.parallel,
            "100 records is below the parallel threshold"
        );
        assert!(timing.total >= timing.scan);
        assert!(timing.total >= timing.parse);
        assert!(timing.summary().contains("scanned=100"));
    }

    #[test]
    fn search_profiled_empty_query_is_zero_cost() {
        let mut index = Index::new();
        index.insert(Path::new("/a.txt"), false, 1, 0);
        let (outcome, timing) = index.search_profiled("", 10, 0);
        assert!(outcome.hits.is_empty());
        assert_eq!(outcome.total, 0);
        assert_eq!(timing.matched, 0);
    }
}
