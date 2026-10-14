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
//! - **Trigram postings (F1).** Every path component is split on `/` and its
//!   three-byte windows are recorded in `ngrams: trigram -> sorted record
//!   indices`. A query term that is ASCII, at least three bytes long, and free of
//!   `/` cannot span a separator, so intersecting the posting lists of its
//!   trigrams yields a superset of the records that contain it. Terms that are
//!   shorter, non-ASCII, or contain `/` contribute no constraint (the scan falls
//!   back to every record for that term).
//! - **Lowercased path arena (F2).** Each record's full path is stored
//!   ASCII-lowercased and back-to-back in `lower`, with a per-record offset and
//!   length. The hot matching loop reads this contiguous arena instead of chasing
//!   the record vector and the parent link, so it touches far less memory.
//! - **Sorted token table (F3).** Each path component is also recorded as a
//!   `(component, record)` entry, kept sorted by component bytes. A single-token
//!   prefix query binary-searches the range of components that start with the
//!   term. This is an *extra* candidate source unioned with the trigram set; the
//!   trigram set already guarantees completeness, so the token table never
//!   removes a true match.
//!
//! The index is **append-only**, mirroring the record tombstones in
//! [`crate::index`]: inserts and renames append new postings and tokens, and a
//! removed or renamed record's stale entries are filtered out at match time
//! (a deleted record never matches; a renamed record's current path is what is
//! tested). [`crate::index::Index::compact`] rebuilds the whole structure, which
//! reclaims the stale entries.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::query::{Query, Term};

/// Pack a three-byte trigram into a `u32` (little-endian byte order).
fn pack_trigram(a: u8, b: u8, c: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16)
}

/// One entry in the sorted token table: a path component in the lowercased
/// arena, and the record it belongs to.
#[derive(Debug, Clone, Copy)]
struct TokenEntry {
    /// Offset of the component into [`CandidateIndex::lower`].
    off: u32,
    /// Byte length of the component.
    len: u16,
    /// The record the component belongs to.
    record: u32,
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
    /// Trigram -> sorted record indices.
    ngrams: HashMap<u32, Vec<u32>>,
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
    /// Trigram posting-list bytes (record indices).
    pub postings: u64,
    /// Trigram map overhead (keys, `Vec` headers, control bytes).
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
const STOP_LIST_DIVISOR: usize = 20;

/// Stop-listing is only applied once the index is at least this large, so a
/// small index keeps every posting list.
const STOP_LIST_MIN_RECORDS: usize = 1000;

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
                if self.stop.contains(&tg) {
                    continue;
                }
                let list = self.ngrams.entry(tg).or_default();
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

        // Token table: one entry per non-empty component.
        let mut start = 0usize;
        for (j, &b) in lower_path.iter().enumerate() {
            if b == b'/' {
                if j > start {
                    self.push_token(off + start as u32, (j - start) as u16, idx);
                }
                start = j + 1;
            }
        }
        if start < lower_path.len() {
            self.push_token(off + start as u32, (lower_path.len() - start) as u16, idx);
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
        self.ngrams.reserve(records.min(1 << 20));
    }

    fn push_token(&mut self, off: u32, len: u16, record: u32) {
        self.pending.push(TokenEntry { off, len, record });
    }

    /// Sort the pending tokens and merge them into the sorted table.
    fn merge_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let lower = &self.lower;
        self.pending.sort_unstable_by(|a, b| token_cmp(lower, a, b));

        let mut merged = Vec::with_capacity(self.tokens.len() + self.pending.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.tokens.len() && j < self.pending.len() {
            if token_cmp(lower, &self.tokens[i], &self.pending[j]) != Ordering::Greater {
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

    /// Finalize a bulk build: merge any pending tokens into the sorted table and
    /// compute the stop-list (F12).
    pub fn finish_bulk(&mut self) {
        self.merge_pending();
        self.compute_stop_list();
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
        let threshold = records / STOP_LIST_DIVISOR;
        let mut stop = HashSet::new();
        self.ngrams.retain(|&tg, list| {
            if list.len() > threshold {
                stop.insert(tg);
                false
            } else {
                true
            }
        });
        self.stop = stop;
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

        let mut lists: Vec<&[u32]> = Vec::with_capacity(needle.len() - 2);
        for w in needle.windows(3) {
            let tg = pack_trigram(w[0], w[1], w[2]);
            // A stop-listed trigram is too common to constrain the scan; skip it
            // (F12). The remaining trigrams still yield a superset.
            if self.stop.contains(&tg) {
                continue;
            }
            match self.ngrams.get(&tg) {
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
        let mut acc = lists[0].to_vec();
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

    /// Records with a path component that starts with `needle`, from the sorted
    /// token table and the pending additions.
    fn token_prefix_candidates(&self, needle: &[u8]) -> Vec<u32> {
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
        &self.lower[t.off as usize..t.off as usize + t.len as usize]
    }

    /// A per-component breakdown of the candidate index's heap memory (F11).
    pub fn memory_breakdown(&self) -> CandidateMemory {
        let lower = self.lower.len() as u64;
        let off = (self.off.len() * std::mem::size_of::<u32>()) as u64;
        let len = (self.len.len() * std::mem::size_of::<u16>()) as u64;
        let postings: u64 = self.ngrams.values().map(|v| (v.len() * 4) as u64).sum();
        // A rough per-entry cost for the hash map: key (4) + Vec header (24) +
        // one control byte.
        let map = (self.ngrams.len() * (4 + 24 + 1)) as u64;
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
        for list in self.ngrams.values_mut() {
            list.shrink_to_fit();
        }
        self.ngrams.shrink_to_fit();
        self.stop.shrink_to_fit();
    }
}

/// Compare two token entries by their component bytes, then by record index.
fn token_cmp(lower: &[u8], a: &TokenEntry, b: &TokenEntry) -> Ordering {
    let sa = &lower[a.off as usize..a.off as usize + a.len as usize];
    let sb = &lower[b.off as usize..b.off as usize + b.len as usize];
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
    fn token_prefix_lookup_finds_components() {
        let c = build(&["/home/sam/config.toml", "/etc/conf.d/app.conf"]);
        let cands = c.token_prefix_candidates(b"conf");
        assert_eq!(cands, vec![0, 1]);
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
        let cands = c.token_prefix_candidates(b"dir");
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
}
