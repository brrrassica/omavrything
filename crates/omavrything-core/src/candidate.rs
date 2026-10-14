//! Candidate-narrowing index: trigram postings, a lowercased path arena, and a
//! sorted token table.
//!
//! The linear scan in [`crate::index`] examines every record. This module builds
//! a derived index that lets a query narrow the scan to a small candidate set
//! first, then verify each candidate with the exact same matcher, so the results
//! are **identical** to the linear scan.
//!
//! Three structures cooperate:
//!
//! - **Trigram postings (F1, F14, F15).** Every path component is split on `/`
//!   and its three-byte windows are recorded. A query term that is ASCII, at
//!   least three bytes long, and free of `/` cannot span a separator, so
//!   intersecting the posting lists of its trigrams yields a superset of the
//!   records that contain it. Terms that are shorter, non-ASCII, or contain `/`
//!   contribute no constraint (the scan falls back to every record for that
//!   term). The postings are stored in a **CSR / flat layout** (F14): one
//!   varint-encoded, delta-encoded posting arena plus a per-trigram byte offset,
//!   instead of a `HashMap<u32, Vec<u32>>` whose per-list `Vec` headers dominate
//!   the memory. Delta + varint encoding (F15) shrinks the arena further while
//!   keeping the intersection speedup.
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
//! postings into the flat CSR arena. Live inserts and renames after that append
//! to a small **delta** map, which is unioned with the CSR at lookup time and
//! merged into the CSR on the next `finish_bulk` (e.g. from
//! [`crate::index::Index::shrink`]). This keeps the steady-state memory in the
//! compact CSR while preserving the append-only, incremental update path.

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
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

/// Pack a three-byte trigram into a `u32` (little-endian byte order).
fn pack_trigram(a: u8, b: u8, c: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16)
}

/// One entry in the sorted token table: the offset of a record's basename into
/// [`CandidateIndex::lower`], and the record it belongs to (F13).
///
/// The basename length is **not** stored: it is derived from the record's stored
/// path length, since the basename is always the suffix of the record's full
/// path. This packs the entry from 12 to 8 bytes.
#[derive(Debug, Clone, Copy)]
struct TokenEntry {
    /// Offset of the basename into [`CandidateIndex::lower`].
    off: u32,
    /// The record the basename belongs to.
    record: u32,
}

/// A finalized, flat trigram posting store (F14, F15).
///
/// `slots` maps a packed trigram to a slot index; `offsets[slot]..offsets[slot +
/// 1]` is that trigram's byte range in `data`, which holds the record indices
/// delta-encoded and varint-encoded (F15). The record indices within a slot are
/// strictly increasing, so the deltas are positive and small.
#[derive(Debug, Default, Clone)]
struct CsrPostings {
    /// Trigram -> slot index.
    slots: HashMap<u32, u32, TrigramBuildHasher>,
    /// Byte offsets into `data`; `offsets[i]..offsets[i + 1]` is slot `i`.
    offsets: Vec<u32>,
    /// Varint delta-encoded record indices, one contiguous range per slot.
    data: Vec<u8>,
}

impl CsrPostings {
    /// Build the flat layout from a trigram -> sorted record list map.
    fn build(map: &HashMap<u32, Vec<u32>, TrigramBuildHasher>) -> Self {
        let mut slots = HashMap::with_capacity_and_hasher(map.len(), TrigramBuildHasher);
        let mut offsets = Vec::with_capacity(map.len() + 1);
        let mut data = Vec::new();
        offsets.push(0u32);
        for (slot, (&tg, list)) in map.iter().enumerate() {
            slots.insert(tg, slot as u32);
            encode_delta_varint(list, &mut data);
            offsets.push(data.len() as u32);
        }
        Self {
            slots,
            offsets,
            data,
        }
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
    /// Finalized flat trigram postings (F14, F15). Empty until `finish_bulk`.
    csr: CsrPostings,
    /// Trigram -> sorted record indices added after finalization (live inserts
    /// and renames). Unioned with the CSR at lookup time and merged into it on
    /// the next `finish_bulk`. During a bulk build this holds every posting.
    delta: HashMap<u32, Vec<u32>, TrigramBuildHasher>,
    /// Sorted token table.
    tokens: Vec<TokenEntry>,
    /// Unsorted token additions, merged into `tokens` once they grow.
    pending: Vec<TokenEntry>,
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
    /// Trigram posting bytes (the CSR arena plus any delta postings).
    pub postings: u64,
    /// Trigram map overhead (keys, slot values, offsets, control bytes).
    pub map: u64,
    /// Token table bytes (sorted plus pending).
    pub tokens: u64,
    /// Stop-list set bytes.
    pub stop: u64,
}

/// The number of pending tokens below which a merge is not worth it.
const PENDING_MIN: usize = 4096;

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

/// Whether the token table is built, overridable with
/// `OMAVRYTHING_TOKEN_TABLE=0` for measurement (R10).
fn tokens_enabled() -> bool {
    std::env::var("OMAVRYTHING_TOKEN_TABLE")
        .map(|v| v != "0")
        .unwrap_or(true)
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

impl CandidateIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a record's ASCII-lowercased full path and index its trigrams and
    /// tokens.
    ///
    /// `idx` is the record's index. For a freshly appended record it is
    /// `records.len()`, so the trigram posting lists stay sorted; for a renamed
    /// record it is an existing index, and the new postings are inserted in
    /// sorted position.
    pub fn insert(&mut self, idx: u32, lower_path: &[u8]) {
        self.insert_inner(idx, lower_path, false);
    }

    /// Append a record whose index is greater than every index already inserted
    /// (a fresh, monotonically increasing bulk build). Posting lists are pushed
    /// to rather than binary-searched, which is markedly faster for a cold scan
    /// (F5). The caller must guarantee `idx` is strictly increasing.
    pub fn insert_append(&mut self, idx: u32, lower_path: &[u8]) {
        self.insert_inner(idx, lower_path, true);
    }

    fn insert_inner(&mut self, idx: u32, lower_path: &[u8], append: bool) {
        let off = self.lower.len() as u32;
        self.lower.extend_from_slice(lower_path);

        let i = idx as usize;
        if self.off.len() <= i {
            self.off.resize(i + 1, u32::MAX);
            self.len.resize(i + 1, 0);
        }
        self.off[i] = off;
        self.len[i] = lower_path.len().min(u16::MAX as usize) as u16;

        // Trigram postings over each path component. A term without `/` cannot
        // span a separator, so component trigrams are sufficient.
        for comp in lower_path.split(|&b| b == b'/') {
            if comp.len() < 3 {
                continue;
            }
            for w in comp.windows(3) {
                let tg = pack_trigram(w[0], w[1], w[2]);
                // A stop-listed trigram has no posting list; skip it so a live
                // insert does not rebuild a list the bulk build dropped (F12).
                // The stop-list is empty during the initial bulk build, so guard
                // the lookup (R8).
                if !self.stop.is_empty() && self.stop.contains(&tg) {
                    continue;
                }
                // A small initial capacity avoids the first reallocations for a
                // trigram that appears in many records (R8).
                let list = self
                    .delta
                    .entry(tg)
                    .or_insert_with(|| Vec::with_capacity(4));
                if append {
                    // `idx` is the largest so far, so a push keeps the list
                    // sorted; skip a repeat of the same trigram within this path.
                    if list.last() != Some(&idx) {
                        list.push(idx);
                    }
                } else if let Err(pos) = list.binary_search(&idx) {
                    list.insert(pos, idx);
                }
            }
        }

        // Token table (F13): only the basename is recorded, not every path
        // component. The trigram set already covers directory components, so the
        // token table is a basename-prefix shortcut; one entry per record instead
        // of one per component cuts its memory ~10x. `OMAVRYTHING_TOKEN_TABLE=0`
        // disables it entirely (a measurement knob for R10).
        if tokens_enabled() {
            let start = lower_path
                .iter()
                .rposition(|&b| b == b'/')
                .map_or(0, |i| i + 1);
            if start < lower_path.len() {
                self.push_token(off + start as u32, idx);
            }
        }

        if self.pending.len() >= self.tokens.len().max(PENDING_MIN) {
            self.merge_pending();
        }
    }

    /// Preallocate the arenas, posting map, and token table for a bulk build of
    /// `records` records totalling `path_bytes` of lowercased path bytes (F5).
    pub fn reserve(&mut self, records: usize, path_bytes: usize) {
        self.lower.reserve(path_bytes);
        self.off.reserve(records);
        self.len.reserve(records);
        self.pending.reserve(records);
        self.tokens.reserve(records);
        // A rough upper bound on the number of distinct trigrams; capped so a
        // huge index does not reserve the full 16 MiB trigram space up front.
        self.delta.reserve(records.min(1 << 20));
    }

    fn push_token(&mut self, off: u32, record: u32) {
        self.pending.push(TokenEntry { off, record });
    }

    /// Sort the pending tokens and merge them into the sorted table.
    fn merge_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let lower = &self.lower;
        let off = &self.off;
        let len = &self.len;
        self.pending
            .sort_unstable_by(|a, b| token_cmp(lower, off, len, a, b));

        let mut merged = Vec::with_capacity(self.tokens.len() + self.pending.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.tokens.len() && j < self.pending.len() {
            if token_cmp(lower, off, len, &self.tokens[i], &self.pending[j]) != Ordering::Greater {
                merged.push(self.tokens[i]);
                i += 1;
            } else {
                merged.push(self.pending[j]);
                j += 1;
            }
        }
        merged.extend_from_slice(&self.tokens[i..]);
        merged.extend_from_slice(&self.pending[j..]);
        self.tokens = merged;
        self.pending.clear();
    }

    /// Finalize a bulk build: merge any pending tokens into the sorted table,
    /// compute the stop-list (F12), and flatten the accumulated postings into
    /// the CSR arena (F14, F15).
    pub fn finish_bulk(&mut self) {
        self.merge_pending();
        self.compute_stop_list();
        self.finalize_postings();
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
        self.stop = stop;
    }

    /// Flatten the delta postings into the CSR arena (F14, F15). On the first
    /// finalization the CSR is built directly from the delta; on a later one the
    /// delta is merged into the existing CSR (decoded, unioned, re-encoded).
    fn finalize_postings(&mut self) {
        if self.delta.is_empty() {
            return;
        }
        if self.csr.slots.is_empty() {
            self.csr = CsrPostings::build(&self.delta);
        } else {
            let mut merged: HashMap<u32, Vec<u32>, TrigramBuildHasher> =
                HashMap::with_capacity_and_hasher(
                    self.csr.slots.len() + self.delta.len(),
                    TrigramBuildHasher,
                );
            for (&tg, &slot) in &self.csr.slots {
                merged.insert(tg, self.csr.decode(slot));
            }
            for (&tg, list) in &self.delta {
                match merged.entry(tg) {
                    Entry::Occupied(mut e) => {
                        let dst = e.get_mut();
                        let union = union_sorted(dst, list);
                        *dst = union;
                    }
                    Entry::Vacant(e) => {
                        e.insert(list.clone());
                    }
                }
            }
            self.csr = CsrPostings::build(&merged);
        }
        self.delta.clear();
    }

    /// The ASCII-lowercased full path of a record, or an empty slice when the
    /// record has no entry.
    pub fn lower_path(&self, idx: u32) -> &[u8] {
        let i = idx as usize;
        if i >= self.off.len() || self.off[i] == u32::MAX {
            return &[];
        }
        let off = self.off[i] as usize;
        &self.lower[off..off + self.len[i] as usize]
    }

    /// The basename of a record's lowercased full path.
    pub fn lower_name(&self, idx: u32) -> &[u8] {
        let p = self.lower_path(idx);
        let start = p.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
        &p[start..]
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

        // F3: union with the token-prefix candidates. The trigram set already
        // guarantees completeness, so this only ever adds records that the
        // matcher then confirms or rejects.
        let token_cands = self.token_prefix_candidates(needle);
        Some(union_sorted(&acc, &token_cands))
    }

    /// The posting list for a trigram: the decoded CSR slot unioned with any
    /// delta postings, or `None` when no record contains the trigram.
    fn postings_for(&self, tg: u32) -> Option<Vec<u32>> {
        let csr = self.csr.slots.get(&tg).map(|&slot| self.csr.decode(slot));
        let delta = self.delta.get(&tg);
        match (csr, delta) {
            (None, None) => None,
            (Some(c), None) => Some(c),
            (None, Some(d)) => Some(d.clone()),
            (Some(c), Some(d)) => Some(union_sorted(&c, d)),
        }
    }

    /// Records with a path component that starts with `needle`, from the sorted
    /// token table and the pending additions.
    fn token_prefix_candidates(&self, needle: &[u8]) -> Vec<u32> {
        if !tokens_enabled() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let start = self
            .tokens
            .partition_point(|t| self.token_slice(t) < needle);
        for t in &self.tokens[start..] {
            if !self.token_slice(t).starts_with(needle) {
                break;
            }
            out.push(t.record);
        }
        for t in &self.pending {
            if self.token_slice(t).starts_with(needle) {
                out.push(t.record);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    fn token_slice(&self, t: &TokenEntry) -> &[u8] {
        token_bytes(&self.lower, &self.off, &self.len, t)
    }

    /// A per-component breakdown of the candidate index's heap memory (F11).
    pub fn memory_breakdown(&self) -> CandidateMemory {
        let lower = self.lower.len() as u64;
        let off = (self.off.len() * std::mem::size_of::<u32>()) as u64;
        let len = (self.len.len() * std::mem::size_of::<u16>()) as u64;
        let delta_postings: u64 = self.delta.values().map(|v| (v.len() * 4) as u64).sum();
        let postings = self.csr.data.len() as u64 + delta_postings;
        // The CSR slot map (key + slot value + control byte), its offset array,
        // and the delta map (key + `Vec` header + control byte).
        let map = self.csr.slots.len() as u64 * (4 + 4 + 1)
            + self.csr.offsets.len() as u64 * std::mem::size_of::<u32>() as u64
            + self.delta.len() as u64 * (4 + 24 + 1);
        let tokens =
            ((self.tokens.len() + self.pending.len()) * std::mem::size_of::<TokenEntry>()) as u64;
        // A stop-list entry is a `u32` key plus a control byte.
        let stop = (self.stop.len() * (4 + 1)) as u64;
        CandidateMemory {
            lower,
            off,
            len,
            postings,
            map,
            tokens,
            stop,
        }
    }

    /// An estimate of the heap bytes held by the candidate index.
    pub fn memory_bytes(&self) -> u64 {
        let b = self.memory_breakdown();
        b.lower + b.off + b.len + b.postings + b.map + b.tokens + b.stop
    }

    /// Release excess capacity in the arenas, posting lists, and token table.
    pub fn shrink(&mut self) {
        self.lower.shrink_to_fit();
        self.off.shrink_to_fit();
        self.len.shrink_to_fit();
        self.tokens.shrink_to_fit();
        self.pending.shrink_to_fit();
        self.csr.slots.shrink_to_fit();
        self.csr.offsets.shrink_to_fit();
        self.csr.data.shrink_to_fit();
        for list in self.delta.values_mut() {
            list.shrink_to_fit();
        }
        self.delta.shrink_to_fit();
        self.stop.shrink_to_fit();
    }
}

/// The basename bytes of a token entry, deriving the length from the record's
/// stored path (F13).
fn token_bytes<'a>(lower: &'a [u8], off: &[u32], len: &[u16], t: &TokenEntry) -> &'a [u8] {
    let start = t.off as usize;
    let end = off[t.record as usize] as usize + len[t.record as usize] as usize;
    &lower[start..end]
}

/// Compare two token entries by their basename bytes, then by record index.
fn token_cmp(lower: &[u8], off: &[u32], len: &[u16], a: &TokenEntry, b: &TokenEntry) -> Ordering {
    let sa = token_bytes(lower, off, len, a);
    let sb = token_bytes(lower, off, len, b);
    sa.cmp(sb).then_with(|| a.record.cmp(&b.record))
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
            c.insert(i as u32, &lower);
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
    fn token_prefix_lookup_finds_basenames() {
        // F13: the token table stores basenames only, so a prefix matches a
        // basename, not a directory component.
        let c = build(&["/home/sam/config.toml", "/etc/conf.d/app.conf"]);
        let cands = c.token_prefix_candidates(b"conf");
        assert_eq!(cands, vec![0]);
        let none = c.token_prefix_candidates(b"zzz");
        assert!(none.is_empty());
    }

    #[test]
    fn lower_path_and_name_are_lowercased() {
        let c = build(&["/Home/Sam/Report.PDF"]);
        assert_eq!(c.lower_path(0), b"/home/sam/report.pdf");
        assert_eq!(c.lower_name(0), b"report.pdf");
    }

    #[test]
    fn pending_tokens_are_merged_and_queryable() {
        let mut c = CandidateIndex::new();
        for i in 0..10 {
            let p = format!("/dir{i}/file{i}.txt");
            c.insert(i, p.as_bytes());
        }
        // No explicit finish_bulk: the pending buffer must still be searched.
        // F13: the token table stores basenames, so the prefix is the basename.
        let cands = c.token_prefix_candidates(b"file");
        assert_eq!(cands.len(), 10);
    }

    #[test]
    fn insert_append_matches_insert() {
        // F5: the bulk-append path must build the same candidate index as the
        // general insert path.
        let paths = [
            "/home/sam/report-2024.pdf",
            "/home/sam/notes.md",
            "/etc/config.toml",
            "/home/sam/report-2025.pdf",
            "/home/sam/aaaa.txt",
        ];
        let mut appended = CandidateIndex::new();
        let mut inserted = CandidateIndex::new();
        for (i, p) in paths.iter().enumerate() {
            let mut lower = p.as_bytes().to_vec();
            lower.make_ascii_lowercase();
            appended.insert_append(i as u32, &lower);
            inserted.insert(i as u32, &lower);
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
            c.insert_append(i as u32, p.as_bytes());
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
            c.insert_append(i as u32, p.as_bytes());
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
            b.lower + b.off + b.len + b.postings + b.map + b.tokens + b.stop,
            c.memory_bytes()
        );
    }

    #[test]
    fn token_entry_is_eight_bytes() {
        // F13: the packed token entry must be 8 bytes, not 12.
        assert_eq!(std::mem::size_of::<TokenEntry>(), 8);
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
        assert!(!c.csr.slots.is_empty(), "expected a finalized CSR");
        for &slot in c.csr.slots.values() {
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
        c.insert(1, b"/home/sam/quarterly-report.pdf");
        let cands = c.candidates(&Query::parse("quarterly")).unwrap();
        assert!(cands.contains(&1), "delta insert must be a candidate");
    }

    #[test]
    fn rename_after_finalize_updates_candidates() {
        // F14: a rename after finalization appends to the delta overlay; the new
        // path must be a candidate.
        let mut c = build(&["/home/sam/old-name.txt"]);
        c.insert(0, b"/home/sam/new-name.txt");
        let cands = c.candidates(&Query::parse("new-name")).unwrap();
        assert!(cands.contains(&0), "renamed record must be a candidate");
    }

    #[test]
    fn delta_is_merged_into_csr_on_finish_bulk() {
        // F14: a second `finish_bulk` folds the delta into the CSR and clears it,
        // and the merged index still answers queries.
        let mut c = build(&["/home/sam/report.pdf"]);
        c.insert(1, b"/home/sam/quarterly-report.pdf");
        assert!(!c.delta.is_empty(), "delta should hold the live insert");
        c.finish_bulk();
        assert!(c.delta.is_empty(), "finish_bulk must clear the delta");
        let cands = c.candidates(&Query::parse("quarterly")).unwrap();
        assert!(cands.contains(&1));
        let cands = c.candidates(&Query::parse("report")).unwrap();
        assert!(cands.contains(&0) && cands.contains(&1));
    }
}
