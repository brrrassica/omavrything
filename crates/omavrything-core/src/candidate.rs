//! Candidate-narrowing index: a two-level trigram index, a lowercased path
//! arena, and a sorted token table.
//!
//! The linear scan in [`crate::index`] examines every record. This module builds
//! a derived index that lets a query narrow the scan to a small candidate set
//! first, then verify each candidate with the exact same matcher, so the results
//! are **identical** to the linear scan.
//!
//! Three structures cooperate:
//!
//! - **Two-level trigram postings (E).** The naive design records the trigrams of
//!   *every* path component of *every* record, so a directory's trigrams are
//!   repeated in each of its descendants — the postings dominate the index's
//!   memory. The two-level design instead records:
//!   - **Level 1** — the trigrams of each record's **basename**, once per record.
//!   - **Level 2** — the trigrams of each **directory's name**, once per
//!     directory. A directory's trigrams apply to all of its descendants, which
//!     are a **contiguous range** of record indices because records are inserted
//!     sorted by path. The range is stored in `subtree_end` and expanded at
//!     lookup time.
//!
//!   A query term that is ASCII, at least three bytes long, and free of `/`
//!   cannot span a separator, so intersecting the posting lists of its trigrams
//!   yields a superset of the records that contain it. Terms that are shorter,
//!   non-ASCII, or contain `/` contribute no constraint (the scan falls back to
//!   every record for that term). The postings are stored in a **CSR / flat
//!   layout** (F14): one varint-encoded, delta-encoded posting arena plus a
//!   per-trigram byte offset, instead of a `HashMap<u32, Vec<u32>>` whose per-list
//!   `Vec` headers dominate the memory. Delta + varint encoding (F15) shrinks the
//!   arena further while keeping the intersection speedup.
//! - **Lowercased path arena (F2).** Each record's full path is stored
//!   ASCII-lowercased and back-to-back in `lower`, with a per-record offset and
//!   length. The hot matching loop reads this contiguous arena instead of chasing
//!   the record vector and the parent link, so it touches far less memory.
//! - **Sorted token table (F3, F13).** Each record's **basename** is recorded as
//!   a `(basename, record)` entry, kept sorted by basename bytes. A single-token
//!   prefix query binary-searches the range of basenames that start with the
//!   term. This is an *extra* candidate source unioned with the trigram set; the
//!   trigram set already guarantees completeness, so the token table never
//!   removes a true match. Storing only the basename (not every path component)
//!   keeps the table at one entry per record instead of one per component, and
//!   the entry is packed to 8 bytes (F13) by deriving the basename length from
//!   the record's stored path length rather than storing it per entry.
//!
//! The index is **append-only**, mirroring the record tombstones in
//! [`crate::index`]: inserts and renames append new postings and tokens, and a
//! removed or renamed record's stale entries are filtered out at match time
//! (a deleted record never matches; a renamed record's current path is what is
//! tested). [`crate::index::Index::compact`] rebuilds the whole structure, which
//! reclaims the stale entries.
//!
//! ## Bulk build vs. live updates
//!
//! A cold scan appends every record with [`CandidateIndex::insert_append`] and
//! then calls [`CandidateIndex::finish_bulk`], which finalizes the accumulated
//! postings into the flat CSR arenas and closes the subtree ranges. Live inserts
//! and renames after that append to a small **delta** map holding the record's
//! **full-path** component trigrams, which is unioned with the CSR at lookup
//! time. A live record is therefore covered for both basename and
//! ancestor-directory matches without touching the bulk subtree ranges. The
//! delta is reclaimed on the next full rebuild ([`crate::index::Index::compact`]
//! or a snapshot load).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::query::{Query, Term};

/// A fast, non-cryptographic hasher for the trigram maps (R8). The keys are
/// packed trigrams (`u32`), so a single multiply is enough; the default SipHash
/// is overkill and dominates the cold-scan insert cost.
#[derive(Default)]
struct TrigramHasher(u64);

impl std::hash::Hasher for TrigramHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // Not used by the trigram maps (their keys are `u32`), but implemented
        // so the hasher is a valid `Hasher`.
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }

    fn write_u32(&mut self, n: u32) {
        // Fibonacci hashing: multiply by a large odd constant; the high bits are
        // well distributed.
        self.0 = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// The `BuildHasher` for the trigram maps (R8).
#[derive(Default, Clone)]
struct TrigramBuildHasher;

impl std::hash::BuildHasher for TrigramBuildHasher {
    type Hasher = TrigramHasher;
    fn build_hasher(&self) -> TrigramHasher {
        TrigramHasher(0)
    }
}

/// A trigram -> sorted record list map, the staging form of the postings.
type TrigramMap = HashMap<u32, Vec<u32>, TrigramBuildHasher>;

/// Pack a three-byte trigram into a `u32` (little-endian byte order).
fn pack_trigram(a: u8, b: u8, c: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16)
}

/// A finalized, flat trigram posting store (F14, F15).
///
/// `keys` is the sorted list of packed trigrams; `offsets[slot]..offsets[slot +
/// 1]` is that trigram's byte range in `data`, which holds the record indices
/// delta-encoded and varint-encoded (F15). The record indices within a slot are
/// strictly increasing, so the deltas are positive and small. A sorted key array
/// plus binary search (F) replaces the previous `HashMap<u32, u32>`, whose
/// control bytes and bucket slack cost more than the keys themselves.
#[derive(Debug, Default, Clone)]
struct CsrPostings {
    /// Sorted packed trigrams; `keys[i]` is slot `i`.
    keys: Vec<u32>,
    /// Byte offsets into `data`; `offsets[i]..offsets[i + 1]` is slot `i`.
    offsets: Vec<u32>,
    /// Varint delta-encoded record indices, one contiguous range per slot.
    data: Vec<u8>,
}

impl CsrPostings {
    /// Build the flat layout from a trigram -> sorted record list map.
    fn build(map: &TrigramMap) -> Self {
        let mut keys: Vec<u32> = map.keys().copied().collect();
        keys.sort_unstable();
        let mut offsets = Vec::with_capacity(keys.len() + 1);
        let mut data = Vec::new();
        offsets.push(0u32);
        for &tg in &keys {
            encode_delta_varint(&map[&tg], &mut data);
            offsets.push(data.len() as u32);
        }
        Self {
            keys,
            offsets,
            data,
        }
    }

    /// The slot for a trigram, or `None` when absent.
    fn slot_of(&self, tg: u32) -> Option<u32> {
        self.keys.binary_search(&tg).ok().map(|i| i as u32)
    }

    /// Decode the posting list for `slot` into sorted record indices.
    fn decode(&self, slot: u32) -> Vec<u32> {
        let s = slot as usize;
        let start = self.offsets[s] as usize;
        let end = self.offsets[s + 1] as usize;
        decode_delta_varint(&self.data[start..end])
    }
}

/// The candidate-narrowing index. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct CandidateIndex {
    /// ASCII-lowercased full paths, back-to-back.
    lower: Vec<u8>,
    /// Per-record offset into `lower` (`u32::MAX` when absent).
    off: Vec<u32>,
    /// Per-record length in `lower`.
    len: Vec<u16>,
    /// Per-record parent directory index (`u32::MAX` for a root), used to
    /// reconstruct a file's full path from its parent's stored path (A).
    parent: Vec<u32>,
    /// Level 1: finalized basename-trigram postings (F14, F15). Empty until
    /// `finish_bulk`.
    csr: CsrPostings,
    /// Level 2: finalized directory-name-trigram postings (E). Empty until
    /// `finish_bulk`.
    dir_csr: CsrPostings,
    /// Sorted `(directory record, exclusive end of its descendant range)` pairs
    /// (E). Only directories appear; the range is `[dir, end)`.
    subtree_end: Vec<(u32, u32)>,
    /// Bulk-build staging for level 1 (basename trigrams), then the live overlay
    /// (full-path component trigrams) after `finish_bulk`.
    delta: TrigramMap,
    /// Bulk-build staging for level 2 (directory-name trigrams).
    dir_delta: TrigramMap,
    /// Bulk-build stack of open directories `(record, path_off, path_len)`, used
    /// to close subtree ranges. Empty after `finish_bulk`.
    stack: Vec<(u32, u32, u32)>,
    /// Whether the bulk build has been finalized. Before it, `candidates`
    /// declines to narrow (the subtree ranges are incomplete).
    bulk_finalized: bool,
    /// Stop-listed trigrams (F12): too common to be worth a posting list, so
    /// their list is dropped and they contribute no constraint. A query term
    /// whose trigrams are all stop-listed cannot narrow the scan.
    stop: HashSet<u32>,
}

/// A per-component breakdown of the candidate index's heap memory, for
/// diagnostics (F11).
#[derive(Debug, Clone, Copy, Default)]
pub struct CandidateMemory {
    /// Lowercased path arena bytes used.
    pub lower: u64,
    /// Per-record offset array bytes.
    pub off: u64,
    /// Per-record length array bytes.
    pub len: u64,
    /// Per-record parent array bytes (A).
    pub parent: u64,
    /// Trigram posting bytes (the CSR arenas plus any delta postings).
    pub postings: u64,
    /// Trigram map overhead (keys, slot values, offsets, control bytes).
    pub map: u64,
    /// Subtree-range array bytes (E).
    pub subtree: u64,
    /// Stop-list set bytes.
    pub stop: u64,
    /// Lowercased path arena **capacity** (rc7). The gap to `lower` is
    /// reclaimable slack that RSS still pays for.
    pub lower_cap: u64,
    /// Per-record offset array capacity.
    pub off_cap: u64,
    /// Per-record length array capacity.
    pub len_cap: u64,
    /// Per-record parent array capacity.
    pub parent_cap: u64,
    /// Trigram posting capacity (the CSR arenas plus any delta postings).
    pub postings_cap: u64,
    /// Trigram map capacity (keys, slot values, offsets, control bytes).
    pub map_cap: u64,
    /// Subtree-range array capacity.
    pub subtree_cap: u64,
    /// Stop-list set capacity.
    pub stop_cap: u64,
    /// Bulk-build stack capacity (empty after `finish_bulk`, but its capacity is
    /// retained until `shrink`).
    pub stack_cap: u64,
}

/// A trigram whose posting list covers more than `1 / STOP_LIST_DIVISOR` of the
/// records is stop-listed (F12): its list is dropped and it contributes no
/// constraint, so the candidate set stays a superset. This bounds the postings,
/// which dominate the candidate index's memory.
///
/// Tuned to 10 (R10): the memory curve is flat between 10 and 20 (306.9 vs
/// 302.8 B/record on the 52 k hermetic tree), but the broad-query p50 is ~2x
/// better at 10 (0.293 vs 0.62 ms) because the 5–10 % trigrams that 20 drops
/// are exactly the ones a broad directory-component query needs. Beyond 20 the
/// memory saving is marginal and the broad query stays slow.
const STOP_LIST_DIVISOR: usize = 10;

/// Stop-listing is only applied once the index is at least this large, so a
/// small index keeps every posting list.
const STOP_LIST_MIN_RECORDS: usize = 1000;

/// The stop-list divisor, overridable with `OMAVRYTHING_STOP_LIST_DIVISOR` for
/// measurement (R10). 0 disables the stop-list.
fn stop_list_divisor() -> usize {
    std::env::var("OMAVRYTHING_STOP_LIST_DIVISOR")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(STOP_LIST_DIVISOR)
}

/// Append `v` to `out` as an unsigned LEB128 varint (F15).
fn write_varint(mut v: u32, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Read an unsigned LEB128 varint from `data` at `*pos`, advancing `*pos`.
fn read_varint(data: &[u8], pos: &mut usize) -> u32 {
    let mut v = 0u32;
    let mut shift = 0u32;
    loop {
        let b = data[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    v
}

/// Delta + varint encode a sorted, deduplicated record list into `out` (F15).
fn encode_delta_varint(list: &[u32], out: &mut Vec<u8>) {
    let mut prev = 0u32;
    for &v in list {
        write_varint(v - prev, out);
        prev = v;
    }
}

/// Decode a delta + varint encoded record list (F15).
fn decode_delta_varint(data: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut prev = 0u32;
    while pos < data.len() {
        prev += read_varint(data, &mut pos);
        out.push(prev);
    }
    out
}

/// The basename of a path: the bytes after the last `/`.
fn basename_of(path: &[u8]) -> &[u8] {
    let start = path.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1);
    &path[start..]
}

/// Whether `child` is `parent` itself or a descendant of it.
fn is_descendant(child: &[u8], parent: &[u8]) -> bool {
    if !child.starts_with(parent) {
        return false;
    }
    if child.len() == parent.len() {
        return true;
    }
    parent.ends_with(b"/") || child[parent.len()] == b'/'
}

/// Index the trigrams of every `/`-separated component of `bytes` into `map`.
fn index_trigrams(map: &mut TrigramMap, stop: &HashSet<u32>, idx: u32, bytes: &[u8], append: bool) {
    for comp in bytes.split(|&b| b == b'/') {
        if comp.len() < 3 {
            continue;
        }
        for w in comp.windows(3) {
            let tg = pack_trigram(w[0], w[1], w[2]);
            // A stop-listed trigram has no posting list; skip it so a live
            // insert does not rebuild a list the bulk build dropped (F12).
            if !stop.is_empty() && stop.contains(&tg) {
                continue;
            }
            let list = map.entry(tg).or_insert_with(|| Vec::with_capacity(4));
            if append {
                // `idx` is the largest so far, so a push keeps the list sorted;
                // skip a repeat of the same trigram within this path.
                if list.last() != Some(&idx) {
                    list.push(idx);
                }
            } else if let Err(pos) = list.binary_search(&idx) {
                list.insert(pos, idx);
            }
        }
    }
}

impl CandidateIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a record's ASCII-lowercased full path and index its trigrams and
    /// tokens.
    ///
    /// `idx` is the record's index. For a freshly appended record it is
    /// `records.len()`, so the posting lists stay sorted; for a renamed record it
    /// is an existing index, and the new postings are inserted in sorted
    /// position. `is_dir` selects the level-2 (directory) postings during a bulk
    /// build.
    pub fn insert(&mut self, idx: u32, lower_path: &[u8], is_dir: bool, parent: u32) {
        self.insert_inner(idx, lower_path, is_dir, parent, false);
    }

    /// Append a record whose index is greater than every index already inserted
    /// (a fresh, monotonically increasing bulk build). Posting lists are pushed
    /// to rather than binary-searched, which is markedly faster for a cold scan
    /// (F5). The caller must guarantee `idx` is strictly increasing.
    pub fn insert_append(&mut self, idx: u32, lower_path: &[u8], is_dir: bool, parent: u32) {
        self.insert_inner(idx, lower_path, is_dir, parent, true);
    }

    fn insert_inner(
        &mut self,
        idx: u32,
        lower_path: &[u8],
        is_dir: bool,
        parent: u32,
        append: bool,
    ) {
        // Store the basename for files (with a parent), the full path for
        // directories and parentless records, mirroring the main arena (A).
        let stored: &[u8] = if is_dir || parent == u32::MAX {
            lower_path
        } else {
            basename_of(lower_path)
        };
        let off = self.lower.len() as u32;
        self.lower.extend_from_slice(stored);

        let i = idx as usize;
        if self.off.len() <= i {
            self.off.resize(i + 1, u32::MAX);
            self.len.resize(i + 1, 0);
            self.parent.resize(i + 1, u32::MAX);
        }
        self.off[i] = off;
        self.len[i] = stored.len().min(u16::MAX as usize) as u16;
        self.parent[i] = parent;

        if append {
            // Sorted bulk build: two-level. Level 1 = basename trigrams; level 2
            // = directory-name trigrams; maintain the subtree stack.
            let basename = basename_of(lower_path);
            index_trigrams(&mut self.delta, &self.stop, idx, basename, append);
            if is_dir {
                index_trigrams(&mut self.dir_delta, &self.stop, idx, basename, append);
            }
            self.push_subtree(idx, lower_path, is_dir);
        } else {
            // Unsorted/live: index every path component so the record is covered
            // for both basename and ancestor-directory matches.
            index_trigrams(&mut self.delta, &self.stop, idx, lower_path, append);
        }
    }

    /// Maintain the bulk-build subtree stack: close every open directory whose
    /// subtree does not include the new record, then open the new record if it is
    /// a directory.
    fn push_subtree(&mut self, idx: u32, full_path: &[u8], is_dir: bool) {
        while let Some(&(d, doff, dlen)) = self.stack.last() {
            let descendant = {
                let dp = &self.lower[doff as usize..(doff + dlen) as usize];
                is_descendant(full_path, dp)
            };
            if descendant {
                break;
            }
            self.set_dir_end(d, idx);
            self.stack.pop();
        }
        if is_dir {
            let i = idx as usize;
            self.stack.push((idx, self.off[i], self.len[i] as u32));
        }
    }

    /// Record the exclusive end of directory `d`'s descendant range.
    fn set_dir_end(&mut self, d: u32, end: u32) {
        self.subtree_end.push((d, end));
    }

    /// Replace the subtree-range array with one derived from the record parent
    /// links (E). The Index computes this so the ranges are correct even when
    /// records are not sorted by path (e.g. after live updates). The dense
    /// per-record array is compacted to the directories only.
    pub fn set_subtree_end(&mut self, ends: Vec<u32>) {
        self.subtree_end = ends
            .into_iter()
            .enumerate()
            .filter(|(_, e)| *e != 0)
            .map(|(i, e)| (i as u32, e))
            .collect();
    }

    /// Preallocate the arenas, posting maps, and token table for a bulk build of
    /// `records` records totalling `path_bytes` of lowercased path bytes (F5).
    pub fn reserve(&mut self, records: usize, path_bytes: usize) {
        self.lower.reserve(path_bytes);
        self.off.reserve(records);
        self.len.reserve(records);
        self.subtree_end.reserve(records);
        // A rough upper bound on the number of distinct trigrams; capped so a
        // huge index does not reserve the full 16 MiB trigram space up front.
        self.delta.reserve(records.min(1 << 20));
    }

    /// Finalize a bulk build: close the subtree ranges, compute the stop-list
    /// (F12), and flatten the accumulated postings into the CSR arenas (F14, F15).
    ///
    /// Idempotent: a second call (e.g. from [`crate::index::Index::shrink`]) only
    /// merges tokens, so the live overlay is never folded into the bulk arenas.
    pub fn finish_bulk(&mut self) {
        if self.bulk_finalized {
            return;
        }
        // Close any directories still open at the end of the bulk build.
        let n = self.off.len() as u32;
        while let Some((d, _, _)) = self.stack.pop() {
            self.set_dir_end(d, n);
        }
        self.subtree_end.sort_unstable();
        self.subtree_end.dedup();
        self.compute_stop_list();
        self.finalize_postings();
        self.bulk_finalized = true;
    }

    /// Drop the posting lists of the most frequent trigrams (F12). A dropped
    /// trigram contributes no constraint, so the candidate set stays a superset
    /// and results are unchanged; the matcher still verifies every candidate.
    ///
    /// Idempotent: once a stop-list has been computed it is not recomputed, so a
    /// later `finish_bulk` (e.g. from `shrink`) cannot clear it and turn the
    /// dropped trigrams into false "absent" lookups.
    fn compute_stop_list(&mut self) {
        if !self.stop.is_empty() {
            return;
        }
        let records = self.off.len();
        if records < STOP_LIST_MIN_RECORDS {
            return;
        }
        // A measurement knob (R10): the divisor can be overridden at runtime so
        // the stop-list's memory/latency trade-off can be measured. 0 disables
        // the stop-list entirely.
        let divisor = stop_list_divisor();
        if divisor == 0 {
            return;
        }
        let threshold = records / divisor;
        let mut stop = HashSet::new();
        self.delta.retain(|&tg, list| {
            if list.len() > threshold {
                stop.insert(tg);
                false
            } else {
                true
            }
        });
        // A directory trigram that appears in most directories would expand to
        // most of the index, so stop-list it too (E).
        self.dir_delta.retain(|&tg, list| {
            if list.len() > threshold {
                stop.insert(tg);
                false
            } else {
                true
            }
        });
        self.stop = stop;
    }

    /// Flatten the staged postings into the CSR arenas (F14, F15).
    fn finalize_postings(&mut self) {
        if !self.delta.is_empty() {
            self.csr = CsrPostings::build(&self.delta);
            self.delta.clear();
        }
        if !self.dir_delta.is_empty() {
            self.dir_csr = CsrPostings::build(&self.dir_delta);
            self.dir_delta.clear();
        }
    }

    /// The bytes stored for a record: the lowercased basename for a file (with a
    /// parent), the lowercased full path for a directory or parentless record
    /// (A). Empty when the record has no entry.
    pub fn lower_stored(&self, idx: u32) -> &[u8] {
        let i = idx as usize;
        if i >= self.off.len() || self.off[i] == u32::MAX {
            return &[];
        }
        let off = self.off[i] as usize;
        &self.lower[off..off + self.len[i] as usize]
    }

    /// The basename of a record's lowercased path.
    pub fn lower_name(&self, idx: u32) -> &[u8] {
        basename_of(self.lower_stored(idx))
    }

    /// The lowercased full path of a record's parent directory (A). The parent
    /// is always a directory, so it stores its full path.
    pub fn lower_parent(&self, idx: u32) -> &[u8] {
        let i = idx as usize;
        if i >= self.parent.len() || self.parent[i] == u32::MAX {
            return &[];
        }
        self.lower_stored(self.parent[i])
    }

    /// The candidate record indices for `q`, or `None` when no term can narrow
    /// the scan (the caller must then scan every record).
    ///
    /// The returned set is always a **superset** of the records that match, so
    /// verifying each candidate with the exact matcher yields the same results as
    /// a linear scan.
    pub fn candidates(&self, q: &Query) -> Option<Vec<u32>> {
        let mut result: Option<Vec<u32>> = None;
        for term in q.terms() {
            let Some(set) = self.term_candidates(term) else {
                continue;
            };
            result = Some(match result {
                None => set,
                Some(prev) => intersect_sorted(&prev, &set),
            });
            if result.as_ref().is_some_and(|r| r.is_empty()) {
                return Some(Vec::new());
            }
        }
        result
    }

    /// The candidate set for one term, or `None` when the term cannot narrow.
    fn term_candidates(&self, term: &Term) -> Option<Vec<u32>> {
        let needle = term.ascii()?;
        if needle.len() < 3 || needle.contains(&b'/') {
            return None;
        }

        let mut lists: Vec<Vec<u32>> = Vec::with_capacity(needle.len() - 2);
        for w in needle.windows(3) {
            let tg = pack_trigram(w[0], w[1], w[2]);
            // A stop-listed trigram is too common to constrain the scan; skip it
            // (F12). The remaining trigrams still yield a superset.
            if self.stop.contains(&tg) {
                continue;
            }
            match self.postings_for(tg) {
                Some(list) => lists.push(list),
                // A missing trigram means no record contains it, so no record
                // contains the term.
                None => return Some(Vec::new()),
            }
        }

        // Every trigram was stop-listed: the term cannot narrow the scan.
        if lists.is_empty() {
            return None;
        }

        // Intersect smallest-first to keep the working set small.
        lists.sort_by_key(|l| l.len());
        let mut acc = lists[0].clone();
        for list in &lists[1..] {
            acc = intersect_sorted(&acc, list);
            if acc.is_empty() {
                return Some(acc);
            }
        }

        Some(acc)
    }

    /// The posting list for a trigram: the union of the level-1 basename
    /// postings, the level-2 directory postings expanded to their descendant
    /// ranges, and the live delta. `None` when no record contains the trigram.
    fn postings_for(&self, tg: u32) -> Option<Vec<u32>> {
        let mut parts: Vec<Vec<u32>> = Vec::new();
        if let Some(slot) = self.csr.slot_of(tg) {
            parts.push(self.csr.decode(slot));
        }
        if let Some(slot) = self.dir_csr.slot_of(tg) {
            parts.push(self.expand_dirs(&self.dir_csr.decode(slot)));
        }
        if let Some(d) = self.delta.get(&tg) {
            parts.push(d.clone());
        }
        if let Some(d) = self.dir_delta.get(&tg) {
            parts.push(self.expand_dirs(d));
        }
        match parts.len() {
            0 => None,
            1 => Some(parts.pop().unwrap()),
            _ => {
                let mut acc = parts.pop().unwrap();
                for p in parts {
                    acc = union_sorted(&acc, &p);
                }
                Some(acc)
            }
        }
    }

    /// Expand a sorted list of directory records to the union of their
    /// descendant ranges `[dir, subtree_end[dir])`, merging overlapping ranges.
    fn expand_dirs(&self, dirs: &[u32]) -> Vec<u32> {
        // An unset range (0) means the directory is still open (the bulk build
        // has not closed it), so its subtree extends to the current end. This
        // keeps the expansion a superset even before `finish_bulk`.
        let n = self.off.len() as u32;
        let end_of = |d: u32| -> u32 {
            match self.subtree_end.binary_search_by_key(&d, |&(k, _)| k) {
                Ok(i) => self.subtree_end[i].1,
                Err(_) => n,
            }
        };
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < dirs.len() {
            let start = dirs[i];
            let mut end = end_of(start);
            i += 1;
            while i < dirs.len() && dirs[i] < end {
                end = end.max(end_of(dirs[i]));
                i += 1;
            }
            out.extend(start..end);
        }
        out
    }

    /// A per-component breakdown of the candidate index's heap memory (F11).
    ///
    /// The `*_cap` fields are the **capacity** (resident) bytes; the used fields
    /// are the bytes actually in use. The gap is reclaimable slack (rc7).
    pub fn memory_breakdown(&self) -> CandidateMemory {
        let lower = self.lower.len() as u64;
        let off = (self.off.len() * std::mem::size_of::<u32>()) as u64;
        let len = (self.len.len() * std::mem::size_of::<u16>()) as u64;
        let parent = (self.parent.len() * std::mem::size_of::<u32>()) as u64;
        let delta_postings: u64 = self.delta.values().map(|v| (v.len() * 4) as u64).sum();
        let dir_delta_postings: u64 = self.dir_delta.values().map(|v| (v.len() * 4) as u64).sum();
        let postings = self.csr.data.len() as u64
            + self.dir_csr.data.len() as u64
            + delta_postings
            + dir_delta_postings;
        // The CSR slot maps (key + slot value + control byte), their offset
        // arrays, and the delta maps (key + `Vec` header + control byte).
        let map = (self.csr.keys.len() + self.dir_csr.keys.len()) as u64
            * std::mem::size_of::<u32>() as u64
            + (self.csr.offsets.len() + self.dir_csr.offsets.len()) as u64
                * std::mem::size_of::<u32>() as u64
            + (self.delta.len() + self.dir_delta.len()) as u64 * (4 + 24 + 1);
        let subtree = (self.subtree_end.len() * std::mem::size_of::<(u32, u32)>()) as u64;
        // A stop-list entry is a `u32` key plus a control byte.
        let stop = (self.stop.len() * (4 + 1)) as u64;

        // Capacity (rc7): the resident bytes a `Vec`/`HashMap` actually holds,
        // which is what RSS pays for. The gap to the used figure is reclaimable
        // slack that `shrink` releases.
        let lower_cap = self.lower.capacity() as u64;
        let off_cap = (self.off.capacity() * std::mem::size_of::<u32>()) as u64;
        let len_cap = (self.len.capacity() * std::mem::size_of::<u16>()) as u64;
        let parent_cap = (self.parent.capacity() * std::mem::size_of::<u32>()) as u64;
        let delta_cap: u64 = self.delta.values().map(|v| (v.capacity() * 4) as u64).sum();
        let dir_delta_cap: u64 = self
            .dir_delta
            .values()
            .map(|v| (v.capacity() * 4) as u64)
            .sum();
        let postings_cap = self.csr.data.capacity() as u64
            + self.dir_csr.data.capacity() as u64
            + delta_cap
            + dir_delta_cap;
        let map_cap = (self.csr.keys.capacity() + self.dir_csr.keys.capacity()) as u64
            * std::mem::size_of::<u32>() as u64
            + (self.csr.offsets.capacity() + self.dir_csr.offsets.capacity()) as u64
                * std::mem::size_of::<u32>() as u64
            + (self.delta.capacity() + self.dir_delta.capacity()) as u64 * (4 + 24 + 1);
        let subtree_cap = (self.subtree_end.capacity() * std::mem::size_of::<(u32, u32)>()) as u64;
        let stop_cap = (self.stop.capacity() * (4 + 1)) as u64;
        let stack_cap = (self.stack.capacity() * std::mem::size_of::<(u32, u32, u32)>()) as u64;

        CandidateMemory {
            lower,
            off,
            len,
            parent,
            postings,
            map,
            subtree,
            stop,
            lower_cap,
            off_cap,
            len_cap,
            parent_cap,
            postings_cap,
            map_cap,
            subtree_cap,
            stop_cap,
            stack_cap,
        }
    }

    /// An estimate of the heap bytes held by the candidate index.
    pub fn memory_bytes(&self) -> u64 {
        let b = self.memory_breakdown();
        b.lower + b.off + b.len + b.parent + b.postings + b.map + b.subtree + b.stop
    }

    /// The resident (capacity) bytes held by the candidate index, including the
    /// reclaimable slack. This is what RSS pays for, versus [`Self::memory_bytes`]
    /// which counts used bytes (rc7).
    pub fn capacity_bytes(&self) -> u64 {
        let b = self.memory_breakdown();
        b.lower_cap
            + b.off_cap
            + b.len_cap
            + b.parent_cap
            + b.postings_cap
            + b.map_cap
            + b.subtree_cap
            + b.stop_cap
            + b.stack_cap
    }

    /// Release excess capacity in the arenas and posting lists.
    pub fn shrink(&mut self) {
        self.lower.shrink_to_fit();
        self.off.shrink_to_fit();
        self.len.shrink_to_fit();
        self.parent.shrink_to_fit();
        self.subtree_end.shrink_to_fit();
        self.csr.keys.shrink_to_fit();
        self.csr.offsets.shrink_to_fit();
        self.csr.data.shrink_to_fit();
        self.dir_csr.keys.shrink_to_fit();
        self.dir_csr.offsets.shrink_to_fit();
        self.dir_csr.data.shrink_to_fit();
        for list in self.delta.values_mut() {
            list.shrink_to_fit();
        }
        self.delta.shrink_to_fit();
        for list in self.dir_delta.values_mut() {
            list.shrink_to_fit();
        }
        self.dir_delta.shrink_to_fit();
        self.stop.shrink_to_fit();
    }

    /// Reclaim the doubling slack in the candidate index's **`Vec`** arenas
    /// (rc7). A `Vec::shrink_to_fit` is a memcpy, so this is cheap enough for the
    /// live-update path; the `HashMap`/`HashSet` fields (the `delta` overlay and
    /// the stop-list) are left to the full [`Self::shrink`], which rehashes.
    ///
    /// Returns whether any arena was shrunk, so the caller can trim the allocator.
    pub fn shrink_arenas_if_slack(&mut self) -> bool {
        let mut shrank = false;
        shrank |= shrink_vec_if_slack(&mut self.lower);
        shrank |= shrink_vec_if_slack(&mut self.off);
        shrank |= shrink_vec_if_slack(&mut self.len);
        shrank |= shrink_vec_if_slack(&mut self.parent);
        shrank |= shrink_vec_if_slack(&mut self.subtree_end);
        shrank |= shrink_vec_if_slack(&mut self.csr.keys);
        shrank |= shrink_vec_if_slack(&mut self.csr.offsets);
        shrank |= shrink_vec_if_slack(&mut self.csr.data);
        shrank |= shrink_vec_if_slack(&mut self.dir_csr.keys);
        shrank |= shrink_vec_if_slack(&mut self.dir_csr.offsets);
        shrank |= shrink_vec_if_slack(&mut self.dir_csr.data);
        shrank |= shrink_vec_if_slack(&mut self.stack);
        shrank
    }
}

/// Shrink a `Vec` when its capacity is at least double its length (slack >=
/// len), i.e. it holds as much free space as used (rc7, tightened in rc11).
///
/// A looser quarter-slack test oscillates with appends: `shrink_to_fit` leaves
/// `capacity == len`, the next push doubles the capacity, and the quarter-slack
/// test then passes again -- so a live-update batch would reallocate the whole
/// arena (`lower` is >100 MB on a real index) on every batch, under the index
/// write lock. Requiring `slack >= len` means a vec that is growing in use is
/// never shrink-churned; only genuine overshoot (a settled scan, or a vec that
/// shrank in length) is reclaimed, which still covers the 2x-capacity case the
/// rc6 RSS work targeted. A no-op when the slack is small, so a small
/// live-update batch does not trigger a reallocation on every event.
fn shrink_vec_if_slack<T>(v: &mut Vec<T>) -> bool {
    let slack = v.capacity().saturating_sub(v.len());
    if slack >= v.len().max(1) {
        v.shrink_to_fit();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod shrink_tests {
    use super::shrink_vec_if_slack;

    #[test]
    fn reclaims_overshoot_but_does_not_oscillate_with_appends() {
        // Capacity >= 2x length is genuine overshoot: reclaimed.
        let mut v: Vec<u8> = vec![0u8; 100];
        v.reserve(900);
        assert!(v.capacity() >= 200);
        assert!(shrink_vec_if_slack(&mut v), "overshoot should be reclaimed");
        assert_eq!(v.capacity(), v.len(), "shrink_to_fit leaves cap == len");

        // A single append doubles the capacity again, but the shrink must not
        // fire: the vec is now in use, not slack. This is what stops a live
        // update batch from reallocating a >100 MB arena on every event.
        v.push(1);
        assert!(v.capacity() > v.len());
        assert!(
            !shrink_vec_if_slack(&mut v),
            "one append must not re-trigger a shrink"
        );
    }
}

/// Intersect two sorted, deduplicated slices.
fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Union two sorted, deduplicated slices.
fn union_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(paths: &[&str]) -> CandidateIndex {
        let mut c = CandidateIndex::new();
        for (i, p) in paths.iter().enumerate() {
            let mut lower = p.as_bytes().to_vec();
            lower.make_ascii_lowercase();
            c.insert(i as u32, &lower, false, u32::MAX);
        }
        c.finish_bulk();
        c
    }

    #[test]
    fn trigram_candidates_are_a_superset() {
        let c = build(&[
            "/home/sam/report-2024.pdf",
            "/home/sam/notes.md",
            "/etc/config.toml",
        ]);
        let q = Query::parse("report");
        let cands = c.candidates(&q).expect("report must narrow");
        // Only the report record contains the trigrams of "report".
        assert_eq!(cands, vec![0]);
    }

    #[test]
    fn short_and_slash_terms_do_not_narrow() {
        let c = build(&["/home/sam/report.pdf"]);
        assert!(c.candidates(&Query::parse("re")).is_none());
        assert!(c.candidates(&Query::parse("sam/report")).is_none());
    }

    #[test]
    fn missing_trigram_yields_no_candidates() {
        let c = build(&["/home/sam/report.pdf"]);
        let cands = c.candidates(&Query::parse("zzzzz")).unwrap();
        assert!(cands.is_empty());
    }

    #[test]
    fn lower_path_and_name_are_lowercased() {
        let c = build(&["/Home/Sam/Report.PDF"]);
        assert_eq!(c.lower_stored(0), b"/home/sam/report.pdf");
        assert_eq!(c.lower_name(0), b"report.pdf");
    }

    #[test]
    fn insert_append_matches_insert() {
        // F5: the bulk-append path must build the same candidate index as the
        // general insert path. Directory records are included because the
        // two-level design indexes directory names per directory record (E).
        let paths: [(&str, bool); 8] = [
            ("/etc", true),
            ("/etc/config.toml", false),
            ("/home", true),
            ("/home/sam", true),
            ("/home/sam/aaaa.txt", false),
            ("/home/sam/notes.md", false),
            ("/home/sam/report-2024.pdf", false),
            ("/home/sam/report-2025.pdf", false),
        ];
        let mut appended = CandidateIndex::new();
        let mut inserted = CandidateIndex::new();
        for (i, (p, is_dir)) in paths.iter().enumerate() {
            let mut lower = p.as_bytes().to_vec();
            lower.make_ascii_lowercase();
            appended.insert_append(i as u32, &lower, *is_dir, u32::MAX);
            inserted.insert(i as u32, &lower, *is_dir, u32::MAX);
        }
        appended.finish_bulk();
        inserted.finish_bulk();
        for q in ["report", "config", "sam", "aaaa", "zzz"] {
            let query = Query::parse(q);
            assert_eq!(
                appended.candidates(&query),
                inserted.candidates(&query),
                "candidates differ for {q:?}"
            );
        }
    }

    #[test]
    fn stop_list_drops_frequent_trigrams_but_keeps_results() {
        // F12: a trigram present in most records is stop-listed, and a query
        // made only of stop-listed trigrams still returns every match (the scan
        // falls back to every record for that term).
        let mut c = CandidateIndex::new();
        let n = STOP_LIST_MIN_RECORDS * 2;
        for i in 0..n {
            let p = format!("/data/common-{i:06}.txt");
            c.insert_append(i as u32, p.as_bytes(), false, u32::MAX);
        }
        c.finish_bulk();
        // "common" appears in every path, so its trigrams are stop-listed.
        assert!(!c.stop.is_empty(), "expected a non-empty stop-list");
        // A term made only of stop-listed trigrams cannot narrow.
        assert!(c.candidates(&Query::parse("common")).is_none());
        // A selective term still narrows to a superset that contains the match.
        let cands = c.candidates(&Query::parse("common-000123")).unwrap();
        assert!(cands.contains(&123));
        assert!(cands.len() < n, "selective term must narrow");
    }

    #[test]
    fn stop_list_is_idempotent_across_finish_bulk() {
        // A second `finish_bulk` (as `Index::shrink` performs) must not clear the
        // stop-list, which would turn dropped trigrams into false "absent"
        // lookups and lose matches.
        let mut c = CandidateIndex::new();
        let n = STOP_LIST_MIN_RECORDS * 2;
        for i in 0..n {
            let p = format!("/data/common-{i:06}.txt");
            c.insert_append(i as u32, p.as_bytes(), false, u32::MAX);
        }
        c.finish_bulk();
        let stop_before = c.stop.clone();
        c.finish_bulk();
        assert_eq!(c.stop, stop_before);
        assert!(c.candidates(&Query::parse("common")).is_none());
    }

    #[test]
    fn memory_breakdown_sums_to_memory_bytes() {
        // F11: the per-component breakdown must account for every byte the
        // single-number estimate reports.
        let c = build(&["/home/sam/report.pdf", "/etc/config.toml"]);
        let b = c.memory_breakdown();
        assert_eq!(
            b.lower + b.off + b.len + b.parent + b.postings + b.map + b.subtree + b.stop,
            c.memory_bytes()
        );
    }

    #[test]
    fn varint_round_trips() {
        // F15: the varint codec must round-trip every value shape.
        for list in [
            vec![],
            vec![0],
            vec![0, 1, 2],
            vec![5, 100, 1000, 100_000],
            vec![u32::MAX],
            vec![0, u32::MAX],
        ] {
            let mut data = Vec::new();
            encode_delta_varint(&list, &mut data);
            assert_eq!(decode_delta_varint(&data), list, "round-trip {list:?}");
        }
    }

    #[test]
    fn csr_postings_are_sorted_and_deduped() {
        // F14/F15: every finalized slot decodes to a strictly increasing list.
        let c = build(&[
            "/home/sam/report-2024.pdf",
            "/home/sam/report-2025.pdf",
            "/home/sam/notes.md",
            "/etc/config.toml",
        ]);
        assert!(!c.csr.keys.is_empty(), "expected a finalized CSR");
        for slot in 0..c.csr.keys.len() as u32 {
            let decoded = c.csr.decode(slot);
            assert!(!decoded.is_empty());
            assert!(
                decoded.windows(2).all(|w| w[0] < w[1]),
                "slot must be sorted and deduped: {decoded:?}"
            );
        }
    }

    #[test]
    fn live_insert_after_finalize_is_found() {
        // F14: a live insert after finalization lands in the delta overlay and
        // must still be found by a query.
        let mut c = build(&["/home/sam/report.pdf"]);
        c.insert(1, b"/home/sam/quarterly-report.pdf", false, u32::MAX);
        let cands = c.candidates(&Query::parse("quarterly")).unwrap();
        assert!(cands.contains(&1), "delta insert must be a candidate");
    }

    #[test]
    fn rename_after_finalize_updates_candidates() {
        // F14: a rename after finalization appends to the delta overlay; the new
        // path must be a candidate.
        let mut c = build(&["/home/sam/old-name.txt"]);
        c.insert(0, b"/home/sam/new-name.txt", false, u32::MAX);
        let cands = c.candidates(&Query::parse("new-name")).unwrap();
        assert!(cands.contains(&0), "renamed record must be a candidate");
    }

    #[test]
    fn directory_trigrams_expand_to_descendants() {
        // E: a directory's name trigrams must match all of its descendants via
        // the contiguous subtree range, without a per-record posting.
        let mut c = CandidateIndex::new();
        // Sorted by path: /data, /data/alpha, /data/alpha/a.txt, /data/alpha/b.txt,
        // /data/beta, /data/beta/c.txt
        let paths: [(&str, bool); 6] = [
            ("/data", true),
            ("/data/alpha", true),
            ("/data/alpha/a.txt", false),
            ("/data/alpha/b.txt", false),
            ("/data/beta", true),
            ("/data/beta/c.txt", false),
        ];
        for (i, (p, is_dir)) in paths.iter().enumerate() {
            c.insert_append(i as u32, p.as_bytes(), *is_dir, u32::MAX);
        }
        c.finish_bulk();
        // "alpha" is a directory name; its descendants are records 1, 2, 3.
        let cands = c.candidates(&Query::parse("alpha")).unwrap();
        assert!(cands.contains(&1) && cands.contains(&2) && cands.contains(&3));
        assert!(!cands.contains(&5), "beta's child must not match alpha");
        // "beta" matches records 4 and 5.
        let cands = c.candidates(&Query::parse("beta")).unwrap();
        assert!(cands.contains(&4) && cands.contains(&5));
        assert!(!cands.contains(&2));
    }

    #[test]
    fn delta_is_not_folded_into_csr_on_second_finish_bulk() {
        // E: after finalization the delta holds live full-path components, which
        // must not be folded into the basename CSR by a later `finish_bulk`.
        let mut c = build(&["/home/sam/report.pdf"]);
        c.insert(1, b"/home/sam/quarterly-report.pdf", false, u32::MAX);
        assert!(!c.delta.is_empty(), "delta should hold the live insert");
        c.finish_bulk();
        assert!(
            !c.delta.is_empty(),
            "a second finish_bulk must not fold the live overlay into the CSR"
        );
        let cands = c.candidates(&Query::parse("quarterly")).unwrap();
        assert!(cands.contains(&1));
        let cands = c.candidates(&Query::parse("report")).unwrap();
        assert!(cands.contains(&0) && cands.contains(&1));
    }
}
