//! Production in-memory index: arena-backed records with ranked substring
//! search.
//!
//! Records live in a flat `Vec<FileRecord>`; path bytes live in a single arena
//! so a query scans contiguous memory. To avoid duplicating a directory's path
//! in every descendant, **directories store their full path** while **files
//! store only their basename** and reference their parent; a file's full path is
//! `parent_path + "/" + basename`, reconstructed on demand. Directories are
//! records too, so the tree can be reconstructed and subtree updates can splice.
//!
//! Deletions are tombstones (`FLAG_DELETED`): the arena is append-only, so a
//! removed record's bytes are not reclaimed until a future compaction pass. A
//! [`PathIndex`] maps full paths to record indices for O(1)
//! insert/remove/rename lookups; it keys on a 32-bit hash of the path and
//! confirms matches against the reconstructed path, so it does **not** duplicate
//! the path bytes (the previous `HashMap<Box<str>, u32>` did, and dominated the
//! index's memory use).

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::candidate::{CandidateIndex, CandidateMemory};
use crate::query::{Query, Rank};

/// Above this many records, the scan is parallelized with rayon. Below it, the
/// thread-pool overhead outweighs the gain.
const PARALLEL_THRESHOLD: usize = 50_000;

/// The number of records each rayon task processes sequentially (F8). Chunking
/// amortizes rayon's per-item scheduling overhead over a contiguous run, which
/// keeps the hot matching loop cache-friendly.
const PARALLEL_CHUNK: usize = 4096;

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
///
/// The basename is not stored: it is derived from the full path (the bytes after
/// the last `/`), so the record carries only the path arena offset. Fields are
/// sized to pack the struct into 20 bytes: `size` saturates at 4 GiB, `mtime` is
/// Unix seconds (valid to 2106), `path_len` covers paths under 64 KiB, and
/// `depth` saturates at 255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRecord {
    /// Size in bytes, saturating at `u32::MAX` (4 GiB). Zero for directories.
    pub size: u32,
    /// Last-modified time, Unix seconds. Zero when unavailable.
    pub mtime: u32,
    /// Index of the parent directory record, or `u32::MAX` for a root.
    pub parent: u32,
    /// Offset of the full path into the path arena.
    pub path_off: u32,
    /// Byte length of the full path (paths are under 64 KiB).
    pub path_len: u16,
    /// Path depth (number of `/` separators), saturating at 255.
    pub depth: u8,
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
#[derive(Debug, Default, Clone)]
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    pub total: usize,
}

/// One entry for a bulk insert (F5). The path is borrowed from the caller's
/// walk arena, so a batch insert allocates nothing per entry.
#[derive(Debug, Clone, Copy)]
pub struct BatchEntry<'a> {
    /// The full path bytes.
    pub path: &'a [u8],
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mtime: i64,
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
/// The primary map is keyed by a 32-bit hash of the path; the path arena is
/// consulted to confirm a match. A 32-bit key halves the map's per-entry cost
/// (a `(u32, u32)` tuple is 8 bytes, versus 16 for `(u64, u32)`). A hash
/// collision (two distinct paths with the same hash) is kept in `collisions`,
/// keyed by the full path, so lookups stay correct. This replaces the previous
/// `HashMap<Box<str>, u32>`, which stored a second copy of every path and
/// dominated the index's memory use.
#[derive(Debug, Default, Clone)]
struct PathIndex {
    main: HashMap<u32, u32>,
    collisions: HashMap<Box<str>, u32>,
}

impl PathIndex {
    fn with_capacity(n: usize) -> Self {
        Self {
            main: HashMap::with_capacity(n),
            collisions: HashMap::new(),
        }
    }

    /// Preallocate the primary map for `n` entries (F5).
    fn reserve(&mut self, n: usize) {
        self.main.reserve(n);
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

    /// An estimate of the heap bytes held by the map, based on its capacity.
    fn capacity_bytes(&self) -> usize {
        // hashbrown stores `(K, V)` tuples plus one control byte per bucket, and
        // keeps the bucket count at a power of two with a 7/8 load factor.
        let per_bucket = std::mem::size_of::<u32>() + std::mem::size_of::<u32>() + 1;
        let buckets = self.main.capacity() * 8 / 7;
        buckets * per_bucket
    }

    /// The record index for `key`, or `None`. `records`/`paths` are the index's
    /// arenas, used to confirm the hash match.
    fn get(&self, key: &str, records: &[FileRecord], paths: &[u8]) -> Option<u32> {
        self.get_bytes(key.as_bytes(), records, paths)
    }

    /// Like [`PathIndex::get`], but takes the key as raw bytes so a bulk insert
    /// that already holds the path in an arena avoids allocating a `String` per
    /// lookup (F5).
    fn get_bytes(&self, key: &[u8], records: &[FileRecord], paths: &[u8]) -> Option<u32> {
        let h = fnv1a32(key);
        if let Some(&idx) = self.main.get(&h) {
            if record_path_eq(&records[idx as usize], records, paths, key) {
                return Some(idx);
            }
        }
        // Collisions are keyed by the full path; a byte key is valid UTF-8 in
        // practice (paths come from `OsStr` bytes), so a lossy conversion is
        // only reached for the rare colliding path.
        std::str::from_utf8(key)
            .ok()
            .and_then(|s| self.collisions.get(s).copied())
    }

    /// Record `key` -> `idx`, keeping a colliding path in the fallback map.
    fn insert(&mut self, key: &str, idx: u32, records: &[FileRecord], paths: &[u8]) {
        let h = fnv1a32(key.as_bytes());
        let collides = self.main.get(&h).copied().is_some_and(|existing| {
            !record_path_eq(&records[existing as usize], records, paths, key.as_bytes())
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
        let h = fnv1a32(key);
        let collides = self.main.get(&h).copied().is_some_and(|existing| {
            !record_path_eq(&records[existing as usize], records, paths, key)
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
        let h = fnv1a32(key.as_bytes());
        if let Some(&idx) = self.main.get(&h) {
            if record_path_eq(&records[idx as usize], records, paths, key.as_bytes()) {
                self.main.remove(&h);
                return;
            }
        }
        self.collisions.remove(key);
    }
}

/// FNV-1a 32-bit hash, used to key the path index without storing the path.
///
/// A 32-bit hash halves the map's per-entry cost. Collisions (two distinct paths
/// with the same hash) are resolved by confirming against the path arena and
/// falling back to `collisions`, so correctness is unaffected.
fn fnv1a32(data: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in data {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Saturating conversion of a byte size to the record's `u32` field.
fn size_to_u32(size: u64) -> u32 {
    size.min(u32::MAX as u64) as u32
}

/// Clamp a Unix-seconds timestamp to the record's `u32` field.
fn mtime_to_u32(mtime: i64) -> u32 {
    mtime.clamp(0, u32::MAX as i64) as u32
}

/// The parent path bytes of `path`, or `None` for the filesystem root. Mirrors
/// [`std::path::Path::parent`] for the absolute paths the scanner produces.
fn parent_bytes(path: &[u8]) -> Option<&[u8]> {
    // Ignore a trailing separator (except for the root "/").
    let trimmed = if path.len() > 1 && path.ends_with(b"/") {
        &path[..path.len() - 1]
    } else {
        path
    };
    if trimmed.len() <= 1 {
        return None;
    }
    let pos = trimmed.iter().rposition(|&b| b == b'/')?;
    Some(&trimmed[..pos.max(1)])
}

/// Whether a record's stored bytes are its full path (directories and
/// parentless records) rather than a basename (files with a parent).
fn stores_full_path(rec: &FileRecord) -> bool {
    rec.is_dir() || rec.parent == u32::MAX
}

/// The stored bytes of a record: the full path for directories and parentless
/// records, the basename for files with a parent.
fn stored_bytes<'a>(rec: &FileRecord, paths: &'a [u8]) -> &'a [u8] {
    let start = rec.path_off as usize;
    &paths[start..start + rec.path_len as usize]
}

/// Write a record's full path into `out`.
fn write_full_path_bytes(
    rec: &FileRecord,
    records: &[FileRecord],
    paths: &[u8],
    out: &mut Vec<u8>,
) {
    if stores_full_path(rec) {
        out.extend_from_slice(stored_bytes(rec, paths));
    } else {
        let parent = &records[rec.parent as usize];
        let p = stored_bytes(parent, paths);
        out.extend_from_slice(p);
        // The filesystem root "/" already ends with a separator.
        if !p.ends_with(b"/") {
            out.push(b'/');
        }
        out.extend_from_slice(stored_bytes(rec, paths));
    }
}

/// Whether `rec`'s full path equals `key`.
fn record_path_eq(rec: &FileRecord, records: &[FileRecord], paths: &[u8], key: &[u8]) -> bool {
    if stores_full_path(rec) {
        stored_bytes(rec, paths) == key
    } else {
        let parent = &records[rec.parent as usize];
        let p = stored_bytes(parent, paths);
        let n = stored_bytes(rec, paths);
        let sep = if p.ends_with(b"/") { 0 } else { 1 };
        key.len() == p.len() + sep + n.len()
            && key.starts_with(p)
            && (sep == 0 || key[p.len()] == b'/')
            && &key[p.len() + sep..] == n
    }
}

/// The maximum number of memoized query results (F7).
const QUERY_CACHE_CAPACITY: usize = 64;

/// A bounded, insertion-ordered memoization cache for repeated queries (F7).
///
/// Typing a query one character at a time re-issues every prefix, and a UI that
/// re-renders re-issues the same query; both hit this cache. It is cleared on
/// **any** index mutation, so a cached result can never outlive the records it
/// was computed from.
#[derive(Debug, Default)]
struct QueryCache {
    entries: HashMap<(String, usize, usize), SearchOutcome>,
    order: VecDeque<(String, usize, usize)>,
    /// The index generation these entries were computed from. A mismatch means
    /// the index was mutated and the entries are stale.
    generation: u64,
}

impl QueryCache {
    fn get(&self, key: &(String, usize, usize)) -> Option<SearchOutcome> {
        self.entries.get(key).cloned()
    }

    fn put(&mut self, key: (String, usize, usize), value: SearchOutcome) {
        if self.entries.contains_key(&key) {
            return;
        }
        if self.entries.len() >= QUERY_CACHE_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, value);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

/// The in-memory index.
#[derive(Debug, Default)]
pub struct Index {
    records: Vec<FileRecord>,
    paths: Vec<u8>,
    roots: Vec<Root>,
    by_path: PathIndex,
    live: usize,
    /// Derived candidate-narrowing index (trigram postings, lowercased path
    /// arena, and token table). Rebuilt on compaction and snapshot load; kept
    /// current on insert and rename.
    candidate: CandidateIndex,
    /// Memoized query results (F7). Interior mutability so `search` can populate
    /// it through `&self`.
    cache: Mutex<QueryCache>,
    /// Bumped on every mutation. A cache entry is valid only while this matches
    /// the generation it was computed at, so invalidation is a single atomic
    /// store rather than a lock-and-clear on the hot insert path.
    cache_generation: AtomicU64,
}

impl Clone for Index {
    /// Clone the index data but **not** the query cache: a clone is a distinct
    /// index, so a cached result computed from the original must not be reused.
    fn clone(&self) -> Self {
        Self {
            records: self.records.clone(),
            paths: self.paths.clone(),
            roots: self.roots.clone(),
            by_path: self.by_path.clone(),
            live: self.live,
            candidate: self.candidate.clone(),
            cache: Mutex::new(QueryCache::default()),
            cache_generation: AtomicU64::new(0),
        }
    }
}

/// A per-component breakdown of the index's memory, for diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryBreakdown {
    /// Live record count.
    pub records_len: usize,
    /// Record vector capacity.
    pub records_cap: usize,
    /// Path arena bytes used.
    pub paths_len: usize,
    /// Path arena capacity.
    pub paths_cap: usize,
    /// Path map entries.
    pub map_len: usize,
    /// Path map estimated bytes.
    pub map_cap_bytes: usize,
    /// Candidate-index estimated bytes (trigram postings, lowercased arena, and
    /// token table).
    pub candidate_bytes: u64,
    /// Per-component breakdown of the candidate index (F11).
    pub candidate: CandidateMemory,
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
        self.invalidate_cache();
    }

    /// The raw records, for snapshot serialization.
    pub(crate) fn records(&self) -> &[FileRecord] {
        &self.records
    }

    /// An estimate of the resident heap memory held by the index, in bytes.
    ///
    /// Counts the **used** bytes of the record vector, the path arena, and the
    /// path map. A `Vec`'s spare capacity is not resident, so this tracks RSS
    /// more closely than a capacity-based figure; the full breakdown (including
    /// capacity) is available from [`Index::memory_breakdown`]. Used for
    /// `GetStatus`.
    pub fn memory_bytes(&self) -> u64 {
        let records = self.records.len() * std::mem::size_of::<FileRecord>();
        let arenas = self.paths.len();
        let map =
            self.by_path.len() * (std::mem::size_of::<u32>() + std::mem::size_of::<u32>() + 1);
        (records + arenas + map) as u64 + self.candidate.memory_bytes()
    }

    /// A per-component breakdown of the index's memory, for diagnostics.
    pub fn memory_breakdown(&self) -> MemoryBreakdown {
        MemoryBreakdown {
            records_len: self.records.len(),
            records_cap: self.records.capacity(),
            paths_len: self.paths.len(),
            paths_cap: self.paths.capacity(),
            map_len: self.by_path.len(),
            map_cap_bytes: self.by_path.capacity_bytes(),
            candidate_bytes: self.candidate.memory_bytes(),
            candidate: self.candidate.memory_breakdown(),
        }
    }

    /// Rebuild the arenas and record vector, dropping tombstones and reclaiming
    /// their bytes. Record indices change, so `parent` links and the path map are
    /// remapped. A no-op when there are no tombstones.
    pub fn compact(&mut self) {
        if !self.records.iter().any(|r| r.is_deleted()) {
            return;
        }
        self.invalidate_cache();

        let mut remap = vec![u32::MAX; self.records.len()];
        let mut records = Vec::with_capacity(self.live);
        let mut paths = Vec::with_capacity(self.paths.len());

        for (i, rec) in self.records.iter().enumerate() {
            if rec.is_deleted() {
                continue;
            }
            remap[i] = records.len() as u32;

            let path_off = paths.len() as u32;
            paths.extend_from_slice(stored_bytes(rec, &self.paths));

            let parent = if rec.parent == u32::MAX {
                u32::MAX
            } else {
                remap[rec.parent as usize]
            };
            records.push(FileRecord {
                size: rec.size,
                mtime: rec.mtime,
                parent,
                path_off,
                path_len: rec.path_len,
                depth: rec.depth,
                flags: rec.flags,
            });
        }

        self.records = records;
        self.paths = paths;
        self.by_path.clear();
        let mut buf: Vec<u8> = Vec::new();
        for (i, rec) in self.records.iter().enumerate() {
            buf.clear();
            write_full_path_bytes(rec, &self.records, &self.paths, &mut buf);
            self.by_path
                .insert_bytes(&buf, i as u32, &self.records, &self.paths);
        }
        // Record indices changed, so the derived candidate index must be rebuilt
        // from scratch; this also reclaims the stale postings and tokens left by
        // removed and renamed records.
        self.rebuild_candidate();
        // The arena was pre-sized to its pre-compaction length, which still
        // includes the tombstones just dropped; release the excess capacity.
        self.records.shrink_to_fit();
        self.paths.shrink_to_fit();
        self.by_path.shrink_to_fit();
    }

    /// Rebuild the candidate index from the current records, dropping stale
    /// postings and tokens. Used after compaction and snapshot load.
    fn rebuild_candidate(&mut self) {
        let mut candidate = CandidateIndex::new();
        let mut buf: Vec<u8> = Vec::new();
        for (i, rec) in self.records.iter().enumerate() {
            if rec.is_deleted() {
                continue;
            }
            buf.clear();
            write_full_path_bytes(rec, &self.records, &self.paths, &mut buf);
            buf.make_ascii_lowercase();
            candidate.insert(i as u32, &buf);
        }
        candidate.finish_bulk();
        self.candidate = candidate;
    }

    /// Release excess capacity in the record vector, path arena, and path map.
    ///
    /// Unlike [`Index::compact`], this does not require tombstones: it reclaims
    /// the doubling slack left by a fresh scan or a snapshot load. Call it once
    /// the index has settled (after the initial scan or a warm start).
    pub fn shrink(&mut self) {
        self.invalidate_cache();
        self.records.shrink_to_fit();
        self.paths.shrink_to_fit();
        self.by_path.shrink_to_fit();
        // Merge any pending tokens before releasing capacity, so the token table
        // is fully sorted and the pending buffer does not hold a second copy.
        self.candidate.finish_bulk();
        self.candidate.shrink();
    }

    /// The path arena, for snapshot serialization.
    pub(crate) fn paths(&self) -> &[u8] {
        &self.paths
    }

    /// Rebuild an index from raw parts, reconstructing the derived path map and
    /// live count. Used by the snapshot loader.
    pub(crate) fn from_parts(records: Vec<FileRecord>, paths: Vec<u8>, roots: Vec<Root>) -> Self {
        let mut by_path = PathIndex::with_capacity(records.len());
        let mut candidate = CandidateIndex::new();
        let mut live = 0usize;
        let mut buf: Vec<u8> = Vec::new();
        for (i, rec) in records.iter().enumerate() {
            if rec.is_deleted() {
                continue;
            }
            live += 1;
            buf.clear();
            write_full_path_bytes(rec, &records, &paths, &mut buf);
            by_path.insert_bytes(&buf, i as u32, &records, &paths);
            buf.make_ascii_lowercase();
            candidate.insert(i as u32, &buf);
        }
        candidate.finish_bulk();
        Self {
            records,
            paths,
            roots,
            by_path,
            live,
            candidate,
            cache: Mutex::new(QueryCache::default()),
            cache_generation: AtomicU64::new(0),
        }
    }

    /// Preallocate the record vector, path arena, path map, and candidate index
    /// for a bulk insert of `records` records totalling `path_bytes` of path
    /// bytes (F5). A no-op hint: correctness does not depend on it.
    pub fn reserve(&mut self, records: usize, path_bytes: usize) {
        self.records.reserve(records);
        self.paths.reserve(path_bytes);
        self.by_path.reserve(records);
        self.candidate.reserve(records, path_bytes);
    }

    /// Insert many records at once, reusing scratch buffers and avoiding the
    /// per-entry `String` allocations of [`Index::insert_with_flags`] (F5).
    ///
    /// `entries` must be sorted by path so a parent precedes its children (the
    /// scanner's walk buffer already is). Entries whose path is already indexed
    /// are skipped. Returns the number of records inserted.
    pub fn insert_batch<'a, I>(&mut self, entries: I) -> usize
    where
        I: IntoIterator<Item = BatchEntry<'a>>,
    {
        let mut inserted = 0usize;
        let mut lower: Vec<u8> = Vec::new();
        for e in entries {
            if self.find_bytes(e.path).is_some() {
                continue;
            }
            let parent = parent_bytes(e.path)
                .and_then(|p| self.find_bytes(p))
                .unwrap_or(u32::MAX);
            let depth = e
                .path
                .iter()
                .filter(|&&b| b == b'/')
                .count()
                .min(u8::MAX as usize) as u8;

            // Directories (and parentless records) store their full path; files
            // with a parent store only their basename.
            let stored: &[u8] = if e.is_dir || parent == u32::MAX {
                e.path
            } else {
                let start = e.path.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1);
                &e.path[start..]
            };

            let path_off = self.paths.len() as u32;
            self.paths.extend_from_slice(stored);
            let path_len = stored.len().min(u16::MAX as usize) as u16;

            let mut flags = 0u8;
            if e.is_dir {
                flags |= FLAG_DIR;
            }
            if e.is_symlink {
                flags |= FLAG_SYMLINK;
            }

            let idx = self.records.len() as u32;
            self.records.push(FileRecord {
                size: size_to_u32(e.size),
                mtime: mtime_to_u32(e.mtime),
                parent,
                path_off,
                path_len,
                depth,
                flags,
            });
            self.by_path
                .insert_bytes(e.path, idx, &self.records, &self.paths);

            // Index the full path, ASCII-lowercased, reusing one buffer.
            lower.clear();
            lower.extend_from_slice(e.path);
            lower.make_ascii_lowercase();
            // Records are appended in increasing index order, so the posting
            // lists can be pushed to instead of binary-searched.
            self.candidate.insert_append(idx, &lower);
            self.live += 1;
            inserted += 1;
        }
        if inserted > 0 {
            self.invalidate_cache();
        }
        inserted
    }

    /// The record index for a path given as raw bytes, if present.
    pub(crate) fn find_bytes(&self, key: &[u8]) -> Option<u32> {
        self.by_path.get_bytes(key, &self.records, &self.paths)
    }

    /// Invalidate every memoized query result. Called on any mutation (F7); a
    /// single atomic increment, so it does not slow the insert path.
    fn invalidate_cache(&self) {
        self.cache_generation.fetch_add(1, AtomicOrdering::Relaxed);
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
        self.invalidate_cache();
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
        let parent = path
            .parent()
            .and_then(|p| self.find_str(p.to_string_lossy().as_ref()))
            .unwrap_or(u32::MAX);
        let depth = path_str
            .bytes()
            .filter(|&b| b == b'/')
            .count()
            .min(u8::MAX as usize) as u8;

        // Directories (and parentless records) store their full path; files with
        // a parent store only their basename, so the parent's path is not
        // duplicated in every child.
        let bytes = path_str.as_bytes();
        let stored: &[u8] = if is_dir || parent == u32::MAX {
            bytes
        } else {
            let start = bytes.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1);
            &bytes[start..]
        };

        let path_off = self.paths.len() as u32;
        self.paths.extend_from_slice(stored);
        let path_len = stored.len().min(u16::MAX as usize) as u16;

        let mut flags = 0u8;
        if is_dir {
            flags |= FLAG_DIR;
        }
        if is_symlink {
            flags |= FLAG_SYMLINK;
        }

        let idx = self.records.len() as u32;
        self.records.push(FileRecord {
            size: size_to_u32(size),
            mtime: mtime_to_u32(mtime),
            parent,
            path_off,
            path_len,
            depth,
            flags,
        });
        self.map_insert(path_str.as_ref(), idx);
        // Index the full path (not the stored basename) so a term can match the
        // parent components too. The candidate index stores it ASCII-lowercased.
        let mut lower = path_str.as_bytes().to_vec();
        lower.make_ascii_lowercase();
        self.candidate.insert(idx, &lower);
        self.live += 1;
        self.invalidate_cache();
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
        rec.size = size_to_u32(size);
        rec.mtime = mtime_to_u32(mtime);
        self.invalidate_cache();
        true
    }

    pub fn record(&self, idx: u32) -> &FileRecord {
        &self.records[idx as usize]
    }

    /// The basename of a record, derived from its full path.
    pub fn name(&self, idx: u32) -> &str {
        let rec = &self.records[idx as usize];
        std::str::from_utf8(self.name_bytes(rec)).unwrap_or("")
    }

    /// The full path of a record.
    ///
    /// Borrowed for directories and parentless records; reconstructed (and thus
    /// owned) for files, whose stored bytes are only the basename.
    pub fn path(&self, idx: u32) -> Cow<'_, str> {
        let rec = &self.records[idx as usize];
        if stores_full_path(rec) {
            Cow::Borrowed(std::str::from_utf8(stored_bytes(rec, &self.paths)).unwrap_or(""))
        } else {
            let mut s = String::new();
            self.write_path(idx, &mut s);
            Cow::Owned(s)
        }
    }

    /// Write the full path of a record into `out`, reusing its capacity.
    pub fn write_path(&self, idx: u32, out: &mut String) {
        let rec = &self.records[idx as usize];
        if stores_full_path(rec) {
            out.push_str(std::str::from_utf8(stored_bytes(rec, &self.paths)).unwrap_or(""));
        } else {
            let parent = &self.records[rec.parent as usize];
            let p = std::str::from_utf8(stored_bytes(parent, &self.paths)).unwrap_or("");
            out.push_str(p);
            // The filesystem root "/" already ends with a separator.
            if !p.ends_with('/') {
                out.push('/');
            }
            out.push_str(std::str::from_utf8(stored_bytes(rec, &self.paths)).unwrap_or(""));
        }
    }

    /// The basename bytes of a record.
    fn name_bytes(&self, rec: &FileRecord) -> &[u8] {
        if stores_full_path(rec) {
            let path = stored_bytes(rec, &self.paths);
            let start = path.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1);
            &path[start..]
        } else {
            stored_bytes(rec, &self.paths)
        }
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
        let mut buf: Vec<u8> = Vec::new();
        for (i, rec) in self.records.iter().enumerate() {
            if rec.is_deleted() {
                continue;
            }
            buf.clear();
            write_full_path_bytes(rec, &self.records, &self.paths, &mut buf);
            if buf == key_bytes || buf.starts_with(prefix_bytes) {
                to_delete.push(i as u32);
            }
        }
        if to_delete.is_empty() {
            return false;
        }

        for &i in &to_delete {
            self.records[i as usize].flags |= FLAG_DELETED;
            buf.clear();
            write_full_path_bytes(
                &self.records[i as usize],
                &self.records,
                &self.paths,
                &mut buf,
            );
            self.map_remove(String::from_utf8_lossy(&buf).as_ref());
        }
        self.live -= to_delete.len();
        self.invalidate_cache();
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

        let mut buf: Vec<u8> = Vec::new();
        let affected: Vec<u32> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, rec)| {
                if rec.is_deleted() {
                    return false;
                }
                buf.clear();
                write_full_path_bytes(rec, &self.records, &self.paths, &mut buf);
                buf == old_bytes || buf.starts_with(old_prefix_bytes)
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
            buf.clear();
            write_full_path_bytes(&rec, &self.records, &self.paths, &mut buf);
            let old_path = buf.clone();
            let new_path: String = if old_path == old_bytes {
                new_key.to_string()
            } else {
                let suffix = &old_path[old_prefix_bytes.len()..];
                format!("{new_prefix}{}", String::from_utf8_lossy(suffix))
            };

            let is_exact = exact == Some(i);

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

            // Directories (and parentless records) store their full path, which
            // changes; files store only their basename, which does not.
            if stores_full_path(&rec) {
                let path_off = self.paths.len() as u32;
                self.paths.extend_from_slice(new_path.as_bytes());
                let path_len = new_path.len().min(u16::MAX as usize) as u16;
                let depth = new_path
                    .bytes()
                    .filter(|&b| b == b'/')
                    .count()
                    .min(u8::MAX as usize) as u8;
                let rec = &mut self.records[i as usize];
                rec.path_off = path_off;
                rec.path_len = path_len;
                rec.depth = depth;
            }
            if is_exact {
                self.records[i as usize].parent = new_parent;
            }

            // The candidate index stores the full path, which changed for every
            // affected record (including files, whose basename is unchanged but
            // whose parent path moved). Append the new lowercased path; the old
            // postings are filtered out at match time and reclaimed on compaction.
            let mut lower = new_path.as_bytes().to_vec();
            lower.make_ascii_lowercase();
            self.candidate.insert(i, &lower);
        }
        self.invalidate_cache();
        true
    }

    /// Search the index, returning the ranked page `[offset, offset + limit)`
    /// and the total number of matches.
    ///
    /// An empty query matches nothing. Repeated queries are served from a small
    /// memoization cache (F7) that is invalidated by any index mutation.
    pub fn search(&self, query: &str, limit: usize, offset: usize) -> SearchOutcome {
        let key = (query.to_string(), limit, offset);
        let generation = self.cache_generation.load(AtomicOrdering::Relaxed);
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.generation != generation {
                cache.clear();
                cache.generation = generation;
            }
            if let Some(hit) = cache.get(&key) {
                return hit;
            }
        }
        let (outcome, _) = self.search_uncached(query, limit, offset);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        // Only memoize if the index was not mutated while we searched.
        if cache.generation == generation {
            cache.put(key, outcome.clone());
        }
        outcome
    }

    /// Like [`Index::search`], but also returns a [`SearchTiming`] breakdown of
    /// where the time went. Used by the profiling example and the daemon's
    /// opt-in query profiler. Bypasses the memoization cache so the timing is
    /// always a real measurement.
    pub fn search_profiled(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> (SearchOutcome, SearchTiming) {
        self.search_uncached(query, limit, offset)
    }

    /// The uncached search implementation.
    fn search_uncached(
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

        let scan_started = Instant::now();
        // Narrow the scan to a candidate set when the query allows it. The
        // candidate set is always a superset of the matches, so verifying each
        // candidate with `match_record` yields the same results as a full scan.
        let candidates = self.candidate.candidates(&q);
        let (total, top, scanned, parallel) = match candidates {
            Some(cands) => {
                let parallel = cands.len() >= PARALLEL_THRESHOLD;
                let (total, top) = if parallel {
                    self.scan_candidates_parallel(&q, &cands, want)
                } else {
                    self.scan_candidates(&q, &cands, want)
                };
                (total, top, cands.len(), parallel)
            }
            None => {
                let parallel = self.records.len() >= PARALLEL_THRESHOLD;
                let (total, top) = if parallel {
                    self.scan_parallel(&q, want)
                } else {
                    self.scan_sequential(&q, want)
                };
                (total, top, self.records.len(), parallel)
            }
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
                scanned,
                matched: total,
                parallel,
            },
        )
    }

    /// Score one record against the query, or `None` if it does not match.
    ///
    /// Matching runs over the candidate index's contiguous, ASCII-lowercased
    /// path arena (F2), so the hot loop touches far less memory than chasing the
    /// record vector and the parent link. The result is identical to matching
    /// the original-case path: ASCII-lowercasing is byte-preserving, and a
    /// non-ASCII term falls back to the Unicode-aware comparison.
    fn match_record(&self, idx: u32, rec: &FileRecord, q: &Query) -> Option<Rank> {
        if rec.is_deleted() {
            return None;
        }
        let lower = self.candidate.lower_path(idx);
        let mut first_pos = u32::MAX;
        for term in q.terms() {
            let pos = term.find_lower(lower)?;
            first_pos = first_pos.min(pos as u32);
        }
        let name = self.candidate.lower_name(idx);
        let name_match = q.terms().iter().all(|t| t.find_lower(name).is_some());
        Some(Rank {
            name_match,
            first_pos,
            path_len: lower.len() as u32,
            depth: rec.depth as u16,
            mtime: rec.mtime as i64,
        })
    }

    fn scan_sequential(&self, q: &Query, want: usize) -> (usize, TopN) {
        let mut top = TopN::new(want);
        let mut total = 0usize;
        for (i, rec) in self.records.iter().enumerate() {
            if let Some(rank) = self.match_record(i as u32, rec, q) {
                total += 1;
                top.push(i as u32, rank);
            }
        }
        (total, top)
    }

    fn scan_parallel(&self, q: &Query, want: usize) -> (usize, TopN) {
        self.records
            .par_chunks(PARALLEL_CHUNK)
            .enumerate()
            .fold(
                || (0usize, TopN::new(want)),
                |(count, mut top), (chunk_i, chunk)| {
                    let base = chunk_i * PARALLEL_CHUNK;
                    let mut count = count;
                    for (j, rec) in chunk.iter().enumerate() {
                        let i = (base + j) as u32;
                        if let Some(rank) = self.match_record(i, rec, q) {
                            top.push(i, rank);
                            count += 1;
                        }
                    }
                    (count, top)
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

    /// Scan only the candidate records, verifying each with the exact matcher.
    fn scan_candidates(&self, q: &Query, cands: &[u32], want: usize) -> (usize, TopN) {
        let mut top = TopN::new(want);
        let mut total = 0usize;
        for &i in cands {
            if let Some(rank) = self.match_record(i, &self.records[i as usize], q) {
                total += 1;
                top.push(i, rank);
            }
        }
        (total, top)
    }

    /// Parallel variant of [`Index::scan_candidates`].
    fn scan_candidates_parallel(&self, q: &Query, cands: &[u32], want: usize) -> (usize, TopN) {
        cands
            .par_chunks(PARALLEL_CHUNK)
            .fold(
                || (0usize, TopN::new(want)),
                |(count, mut top), chunk| {
                    let mut count = count;
                    for &i in chunk {
                        if let Some(rank) = self.match_record(i, &self.records[i as usize], q) {
                            top.push(i, rank);
                            count += 1;
                        }
                    }
                    (count, top)
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
            path: self.path(idx).into_owned(),
            name: self.name(idx).to_string(),
            size: rec.size as u64,
            mtime: rec.mtime as i64,
            is_dir: rec.is_dir(),
        }
    }

    /// The pre-M23 matcher, kept as a test-only reference so the candidate index
    /// (F1) and the lowercased arena (F2) can be proven to preserve results
    /// exactly. It matches the original-case path via the parent/name split.
    #[cfg(test)]
    fn match_record_original(&self, rec: &FileRecord, q: &Query) -> Option<Rank> {
        if rec.is_deleted() {
            return None;
        }
        let mut first_pos = u32::MAX;
        let path_len = if stores_full_path(rec) {
            let path = stored_bytes(rec, &self.paths);
            for term in q.terms() {
                let pos = term.find(path)?;
                first_pos = first_pos.min(pos as u32);
            }
            rec.path_len as u32
        } else {
            let parent = &self.records[rec.parent as usize];
            let parent_path = stored_bytes(parent, &self.paths);
            let name = stored_bytes(rec, &self.paths);
            let sep: &[u8] = if parent_path.ends_with(b"/") {
                b""
            } else {
                b"/"
            };
            for term in q.terms() {
                let pos = term.find_in_file_path(parent_path, name, sep)?;
                first_pos = first_pos.min(pos as u32);
            }
            parent.path_len as u32 + sep.len() as u32 + rec.path_len as u32
        };
        let name = self.name_bytes(rec);
        let name_match = q.terms().iter().all(|t| t.find(name).is_some());
        Some(Rank {
            name_match,
            first_pos,
            path_len,
            depth: rec.depth as u16,
            mtime: rec.mtime as i64,
        })
    }

    /// A full linear scan using the pre-M23 matcher, for equivalence tests.
    #[cfg(test)]
    fn search_linear(&self, query: &str, limit: usize, offset: usize) -> SearchOutcome {
        let q = Query::parse(query);
        if q.is_empty() || limit == 0 {
            return SearchOutcome::default();
        }
        let want = offset.saturating_add(limit);
        let mut top = TopN::new(want);
        let mut total = 0usize;
        for (i, rec) in self.records.iter().enumerate() {
            if let Some(rank) = self.match_record_original(rec, &q) {
                total += 1;
                top.push(i as u32, rank);
            }
        }
        let hits = top
            .into_sorted()
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(i, _)| self.hit(i))
            .collect();
        SearchOutcome { hits, total }
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
    ///
    /// When the union fits within the limit the heaps are simply concatenated
    /// (an O(n) heapify); otherwise the best `limit` of the union are selected
    /// in linear time, which is cheaper than pushing every element of `other`
    /// (F8).
    fn merge(&mut self, other: TopN) {
        if other.heap.is_empty() {
            return;
        }
        if self.heap.is_empty() {
            *self = other;
            return;
        }
        if self.heap.len() + other.heap.len() <= self.limit {
            self.heap.extend(other.heap);
            return;
        }
        let mut all: Vec<Reverse<(Rank, Reverse<u32>)>> =
            self.heap.drain().chain(other.heap).collect();
        if all.len() > self.limit {
            // The smallest keys are the best (the heap is a min-heap of
            // `Reverse`), so partition the best `limit` to the front.
            all.select_nth_unstable(self.limit - 1);
            all.truncate(self.limit);
        }
        self.heap = BinaryHeap::from(all);
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
    fn file_record_is_compact() {
        // The record packs into 20 bytes: size/mtime (8) + parent/path_off (8) +
        // path_len (2) + depth (1) + flags (1). The basename is derived from the
        // path, so no name offset/length is stored.
        assert_eq!(std::mem::size_of::<FileRecord>(), 20);
    }

    #[test]
    fn size_and_mtime_saturate() {
        let mut index = Index::new();
        let big = index.insert(Path::new("/big.bin"), false, u64::MAX, i64::MAX);
        assert_eq!(index.record(big).size, u32::MAX);
        assert_eq!(index.record(big).mtime, u32::MAX);
        // A negative timestamp clamps to zero rather than wrapping.
        let neg = index.insert(Path::new("/neg.bin"), false, 0, -5);
        assert_eq!(index.record(neg).mtime, 0);
    }

    #[test]
    fn shrink_keeps_the_index_intact() {
        let mut index = Index::new();
        for i in 0..1000 {
            index.insert(Path::new(&format!("/home/file-{i}.txt")), false, 1, 0);
        }
        let before = index.memory_bytes();
        index.shrink();
        let after = index.memory_bytes();
        assert!(after <= before, "shrink must not grow the index");
        assert_eq!(index.len(), 1000);
        assert_eq!(index.search("file-500", 10, 0).hits.len(), 1);
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

    #[test]
    fn candidate_narrowing_matches_the_linear_scan() {
        // F1/F2 acceptance: the candidate index and the lowercased arena must
        // return exactly the same results as the pre-M23 linear scan.
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        for p in [
            "/home/sam/Documents/report-2024.pdf",
            "/home/sam/Documents/notes.md",
            "/home/sam/Pictures/holiday.jpg",
            "/etc/config.toml",
            "/var/log/syslog",
            "/usr/share/doc/README",
            "/home/bob/report.txt",
            "/tmp/Report.TXT",
            "/opt/app/config.yaml",
            "/home/sam/.config/app/settings.json",
            "/tmp/Ünïcode.txt",
        ] {
            index.insert(Path::new(p), false, 1, 0);
        }

        // Selective, broad, short, slash-containing, multi-term, non-ASCII, and
        // no-match queries.
        for q in [
            "report",
            "config",
            "re",
            "sam/report",
            "documents report",
            "zzz-no-such-file",
            "REPORT",
            "readme",
            "app",
            "settings.json",
            "ünïcode",
            "documents pictures",
        ] {
            let narrowed = index.search(q, 100, 0);
            let linear = index.search_linear(q, 100, 0);
            assert_eq!(narrowed.total, linear.total, "total differs for {q:?}");
            assert_eq!(narrowed.hits, linear.hits, "hits differ for {q:?}");
        }
    }

    #[test]
    fn selective_query_scans_a_small_fraction() {
        // F1 acceptance: a selective query on a large index scans only a small
        // fraction of the records.
        let mut index = Index::new();
        let n = 20_000;
        for i in 0..n {
            index.insert(
                Path::new(&format!("/data/dir{:04}/file-{i:06}.txt", i % 500)),
                false,
                1,
                0,
            );
        }
        let (out, timing) = index.search_profiled("file-000123", 100, 0);
        assert_eq!(out.total, 1);
        assert!(
            timing.scanned < n / 10,
            "selective query scanned {} of {n}",
            timing.scanned
        );
    }

    #[test]
    fn ngram_index_stays_consistent_after_live_updates() {
        // F4 acceptance: the candidate index tracks insert/remove/rename and
        // survives a compaction rebuild.
        let mut index = Index::new();
        index.insert(Path::new("/home/sam/alpha.txt"), false, 1, 0);
        index.insert(Path::new("/home/sam/beta.txt"), false, 1, 0);

        assert!(index.rename(
            Path::new("/home/sam/alpha.txt"),
            Path::new("/home/sam/gamma.txt")
        ));
        assert_eq!(index.search("alpha", 10, 0).hits.len(), 0);
        assert_eq!(index.search("gamma", 10, 0).hits.len(), 1);

        assert!(index.remove(Path::new("/home/sam/beta.txt")));
        assert_eq!(index.search("beta", 10, 0).hits.len(), 0);

        index.insert(Path::new("/home/sam/delta.txt"), false, 1, 0);
        assert_eq!(index.search("delta", 10, 0).hits.len(), 1);

        // Compaction rebuilds the candidate index; results must be unchanged.
        index.compact();
        assert_eq!(index.search("gamma", 10, 0).hits.len(), 1);
        assert_eq!(index.search("delta", 10, 0).hits.len(), 1);
        assert_eq!(index.search("beta", 10, 0).hits.len(), 0);
        assert_eq!(index.search("alpha", 10, 0).hits.len(), 0);
    }

    #[test]
    fn token_prefix_query_is_narrowed() {
        // F3: a single-token prefix query is answered from the candidate index
        // without scanning every record.
        let mut index = Index::new();
        for i in 0..1000 {
            index.insert(Path::new(&format!("/data/other-{i}.txt")), false, 1, 0);
        }
        index.insert(Path::new("/data/configuration.toml"), false, 1, 0);
        let (out, timing) = index.search_profiled("conf", 10, 0);
        assert_eq!(out.total, 1);
        assert!(timing.scanned < 1000, "scanned {}", timing.scanned);
    }

    #[test]
    fn candidate_index_survives_a_snapshot_round_trip() {
        // The candidate index is derived, so a snapshot load must rebuild it and
        // return the same results.
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        index.insert(Path::new("/home/sam/report.pdf"), false, 1, 0);
        index.insert(Path::new("/home/sam/notes.md"), false, 1, 0);

        let bytes = crate::snapshot::encode(&index, &[]);
        let (back, _) = crate::snapshot::decode(&bytes).unwrap();
        assert_eq!(back.search("report", 10, 0).hits.len(), 1);
        assert_eq!(back.search("notes", 10, 0).hits.len(), 1);
        assert_eq!(back.search("zzz", 10, 0).hits.len(), 0);
    }

    fn rank_with_pos(first_pos: u32) -> Rank {
        Rank {
            name_match: true,
            first_pos,
            path_len: 10,
            depth: 1,
            mtime: 0,
        }
    }

    #[test]
    fn insert_batch_matches_individual_inserts() {
        // F5: a batch insert must produce exactly the same index as inserting
        // each record one at a time.
        let paths: Vec<String> = (0..500)
            .map(|i| format!("/data/dir{:03}/file-{i:04}.txt", i % 20))
            .collect();

        let mut batched = Index::new();
        batched.add_root(Path::new("/"), FsKind::Other);
        batched.reserve(paths.len(), paths.iter().map(|p| p.len()).sum());
        let entries: Vec<BatchEntry> = paths
            .iter()
            .map(|p| BatchEntry {
                path: p.as_bytes(),
                is_dir: false,
                is_symlink: false,
                size: 1,
                mtime: 0,
            })
            .collect();
        assert_eq!(batched.insert_batch(entries), paths.len());

        let mut one_by_one = Index::new();
        one_by_one.add_root(Path::new("/"), FsKind::Other);
        for p in &paths {
            one_by_one.insert(Path::new(p), false, 1, 0);
        }

        assert_eq!(batched.len(), one_by_one.len());
        for q in ["file-0001", "dir003", "file-0499", "zzz"] {
            assert_eq!(
                batched.search(q, 100, 0).hits,
                one_by_one.search(q, 100, 0).hits,
                "results differ for {q:?}"
            );
        }
    }

    #[test]
    fn insert_batch_skips_existing_paths() {
        let mut index = Index::new();
        index.insert(Path::new("/a.txt"), false, 1, 0);
        let entries = [
            BatchEntry {
                path: b"/a.txt",
                is_dir: false,
                is_symlink: false,
                size: 9,
                mtime: 0,
            },
            BatchEntry {
                path: b"/b.txt",
                is_dir: false,
                is_symlink: false,
                size: 2,
                mtime: 0,
            },
        ];
        assert_eq!(index.insert_batch(entries), 1);
        assert_eq!(index.len(), 2);
        // The pre-existing record is untouched.
        assert_eq!(index.search("a.txt", 10, 0).hits[0].size, 1);
    }

    #[test]
    fn insert_batch_links_parents_from_sorted_paths() {
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        let entries = [
            BatchEntry {
                path: b"/home",
                is_dir: true,
                is_symlink: false,
                size: 0,
                mtime: 0,
            },
            BatchEntry {
                path: b"/home/sam",
                is_dir: true,
                is_symlink: false,
                size: 0,
                mtime: 0,
            },
            BatchEntry {
                path: b"/home/sam/a.txt",
                is_dir: false,
                is_symlink: false,
                size: 1,
                mtime: 0,
            },
        ];
        assert_eq!(index.insert_batch(entries), 3);
        let root = index.find(Path::new("/")).unwrap();
        let home = index.find(Path::new("/home")).unwrap();
        let sam = index.find(Path::new("/home/sam")).unwrap();
        let a = index.find(Path::new("/home/sam/a.txt")).unwrap();
        assert_eq!(index.record(home).parent, root);
        assert_eq!(index.record(sam).parent, home);
        assert_eq!(index.record(a).parent, sam);
        assert_eq!(index.path(a), "/home/sam/a.txt");
    }

    #[test]
    fn query_cache_returns_identical_results_and_invalidates_on_mutation() {
        // F7: a repeated query is served from the cache, and any mutation
        // invalidates it.
        let mut index = Index::new();
        index.insert(Path::new("/home/report-1.txt"), false, 1, 0);
        let first = index.search("report", 10, 0);
        let second = index.search("report", 10, 0);
        assert_eq!(first.hits, second.hits);
        assert_eq!(first.total, second.total);

        // A new matching record must appear after the mutation clears the cache.
        index.insert(Path::new("/home/report-2.txt"), false, 1, 0);
        assert_eq!(index.search("report", 10, 0).total, 2);

        // A removal is reflected too.
        assert!(index.remove(Path::new("/home/report-1.txt")));
        assert_eq!(index.search("report", 10, 0).total, 1);
    }

    #[test]
    fn query_cache_is_bounded() {
        let mut index = Index::new();
        for i in 0..200 {
            index.insert(Path::new(&format!("/f{i}.txt")), false, 1, 0);
        }
        for i in 0..200 {
            let _ = index.search(&format!("f{i}"), 10, 0);
        }
        let cache = index.cache.lock().unwrap();
        assert!(cache.entries.len() <= QUERY_CACHE_CAPACITY);
    }

    #[test]
    fn chunked_parallel_scan_matches_sequential() {
        // F8: the chunked rayon scan must return the same page as the
        // sequential scan.
        let mut index = Index::new();
        let n = PARALLEL_THRESHOLD + 5_000;
        for i in 0..n {
            index.insert(
                Path::new(&format!("/data/dir{:03}/file-{i:06}.txt", i % 100)),
                false,
                i as u64,
                0,
            );
        }
        let q = Query::parse("file-000123");
        let want = 100;
        let (seq_total, seq_top) = index.scan_sequential(&q, want);
        let (par_total, par_top) = index.scan_parallel(&q, want);
        assert_eq!(seq_total, par_total);
        assert_eq!(seq_top.into_sorted(), par_top.into_sorted());
    }

    #[test]
    fn topn_merge_keeps_the_best() {
        let mut a = TopN::new(3);
        let mut b = TopN::new(3);
        for (i, pos) in [(0u32, 5u32), (1, 3), (2, 9)] {
            a.push(i, rank_with_pos(pos));
        }
        for (i, pos) in [(3u32, 1u32), (4, 7), (5, 2)] {
            b.push(i, rank_with_pos(pos));
        }
        a.merge(b);
        let sorted = a.into_sorted();
        assert_eq!(sorted.len(), 3);
        // Best three by earliest position: pos 1 (idx 3), 2 (idx 5), 3 (idx 1).
        let idxs: Vec<u32> = sorted.iter().map(|(i, _)| *i).collect();
        assert_eq!(idxs, vec![3, 5, 1]);
    }
}
