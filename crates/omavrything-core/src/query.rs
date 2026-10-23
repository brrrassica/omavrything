//! Query parsing, term matching, and result ranking.
//!
//! A query is split on whitespace into AND terms. Each term is precompiled for
//! case-insensitive substring matching: ASCII terms use a byte-wise search with
//! `memchr` (no allocation per record); non-ASCII terms fall back to a
//! Unicode-aware lowercase comparison (risk R12).

use std::cmp::Ordering;

/// A parsed query: a list of whitespace-separated AND terms.
#[derive(Debug, Clone, Default)]
pub struct Query {
    terms: Vec<Term>,
}

impl Query {
    /// Parse `input` into AND terms. An empty or whitespace-only input yields an
    /// empty query, which matches nothing.
    pub fn parse(input: &str) -> Self {
        Self {
            terms: input.split_whitespace().map(Term::new).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn terms(&self) -> &[Term] {
        &self.terms
    }
}

/// A single query term, precompiled for case-insensitive matching.
#[derive(Debug, Clone)]
pub struct Term {
    ascii: Option<Vec<u8>>,
    unicode: Option<String>,
}

impl Term {
    fn new(raw: &str) -> Self {
        if raw.is_ascii() {
            Self {
                ascii: Some(raw.to_ascii_lowercase().into_bytes()),
                unicode: None,
            }
        } else {
            Self {
                ascii: None,
                unicode: Some(raw.to_lowercase()),
            }
        }
    }

    /// The lowercased ASCII needle, or `None` for a non-ASCII term.
    ///
    /// Used by the candidate index to derive trigrams and token prefixes.
    pub fn ascii(&self) -> Option<&[u8]> {
        self.ascii.as_deref()
    }

    /// The earliest byte position where this term matches `hay`, if any.
    pub fn find(&self, hay: &[u8]) -> Option<usize> {
        if let Some(needle) = &self.ascii {
            find_ascii_ci(hay, needle)
        } else if let Some(needle) = &self.unicode {
            String::from_utf8_lossy(hay)
                .to_lowercase()
                .find(needle.as_str())
        } else {
            None
        }
    }

    /// The earliest byte position where this term matches an already
    /// ASCII-lowercased haystack, if any.
    ///
    /// The candidate index stores each path ASCII-lowercased, so an ASCII term
    /// is a plain byte search (no per-byte case folding) and a non-ASCII term
    /// falls back to the Unicode-aware comparison. Positions are identical to
    /// [`Term::find`] because ASCII-lowercasing is byte-preserving.
    pub fn find_lower(&self, lower: &[u8]) -> Option<usize> {
        if let Some(needle) = &self.ascii {
            find_bytes(lower, needle)
        } else if let Some(needle) = &self.unicode {
            String::from_utf8_lossy(lower)
                .to_lowercase()
                .find(needle.as_str())
        } else {
            None
        }
    }

    /// The earliest position where this term matches the full path formed by
    /// `parent` + `/` + `name`, if any.
    ///
    /// The index stores a file's basename separately from its parent directory's
    /// path, so a term is matched against the two slices independently, plus the
    /// separator between them (a term containing `/` can span the boundary).
    pub fn find_in_file_path(&self, parent: &[u8], name: &[u8], sep: &[u8]) -> Option<usize> {
        let mut best: Option<usize> = None;
        if let Some(p) = self.find(parent) {
            best = Some(p);
        }
        if let Some(p) = self.find(name) {
            let pos = parent.len() + sep.len() + p;
            best = Some(best.map_or(pos, |b| b.min(pos)));
        }
        if let Some(pos) = self.find_boundary(parent, name, sep) {
            best = Some(best.map_or(pos, |b| b.min(pos)));
        }
        best
    }

    /// The earliest position where this term matches `parent` + `/` + `name`,
    /// and whether it matched within `name` (for ranking), for an
    /// already-lowercased `parent` and `name` (the candidate index's deduped
    /// arena, A). Positions are identical to [`Term::find_in_file_path`] because
    /// ASCII-lowercasing is byte-preserving.
    pub fn find_in_file_path_lower(
        &self,
        parent: &[u8],
        name: &[u8],
        sep: &[u8],
    ) -> Option<(usize, bool)> {
        let parent_pos = self.find_lower(parent);
        let name_pos = self.find_lower(name);
        let name_match = name_pos.is_some();
        // The parent precedes the name, so a parent match is the earliest; the
        // boundary (only reachable for a term containing `/`) precedes the name.
        let pos = match (parent_pos, name_pos) {
            (Some(p), _) => p,
            (None, Some(p)) => parent.len() + sep.len() + p,
            (None, None) => self.find_boundary(parent, name, sep)?,
        };
        Some((pos, name_match))
    }

    /// The position where this term spans the `parent`/`name` separator, if any.
    ///
    /// `sep` is `/` for a normal parent, or empty for the filesystem root `/`
    /// (which already ends with a separator). Only ASCII terms are checked.
    fn find_boundary(&self, parent: &[u8], name: &[u8], sep: &[u8]) -> Option<usize> {
        let needle = self.ascii.as_ref()?;
        for i in 0..=needle.len() {
            if i + sep.len() > needle.len() {
                break;
            }
            if &needle[i..i + sep.len()] != sep {
                continue;
            }
            let left = &needle[..i];
            let right = &needle[i + sep.len()..];
            if parent.len() >= left.len()
                && parent[parent.len() - left.len()..].eq_ignore_ascii_case(left)
                && name.len() >= right.len()
                && name[..right.len()].eq_ignore_ascii_case(right)
            {
                return Some(parent.len() - left.len());
            }
        }
        None
    }
}

/// Case-insensitive ASCII substring search over raw bytes.
///
/// Uses `memchr`/`memchr2` to skip to candidate positions, then verifies with
/// `eq_ignore_ascii_case`. Allocates nothing.
pub fn find_ascii_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }

    let lo = needle[0].to_ascii_lowercase();
    let up = needle[0].to_ascii_uppercase();

    let mut start = 0;
    while start + needle.len() <= hay.len() {
        let window = &hay[start..];
        let found = if lo == up {
            memchr::memchr(lo, window)
        } else {
            memchr::memchr2(lo, up, window)
        };
        let pos = found?;
        let idx = start + pos;
        // `idx` can be past `start`, so the candidate may not fit the needle.
        // No later candidate can fit either, so stop.
        if idx + needle.len() > hay.len() {
            return None;
        }
        if hay[idx..idx + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(idx);
        }
        start = idx + 1;
    }
    None
}

/// Plain byte-substring search over an already-lowercased haystack.
///
/// Uses `memchr::memmem`, whose substring search is SIMD-accelerated (F8), so
/// the hot matching loop over the lowercased path arena runs at memory speed.
/// Allocates nothing.
pub fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    // A single-byte needle is a plain `memchr`, which avoids `memmem`'s
    // substring-search setup. This is the common case for a one-character query,
    // which is also the full-scan case the candidate index cannot narrow.
    if needle.len() == 1 {
        return memchr::memchr(needle[0], hay);
    }
    memchr::memmem::find(hay, needle)
}

/// A match score. `Ord` orders **better** matches as **greater**, so a
/// `BinaryHeap` of `Reverse<Rank>` keeps the worst of the current top-N at the
/// root.
///
/// Ordering (best first): name match, earlier match position, shorter path,
/// shallower depth, more recent mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rank {
    /// True when every term matched within the basename.
    pub name_match: bool,
    /// Earliest match position across terms (smaller is better).
    pub first_pos: u32,
    /// Full-path length in bytes (shorter is better).
    pub path_len: u32,
    /// Path depth (shallower is better).
    pub depth: u16,
    /// Last-modified time, Unix seconds (more recent is better).
    pub mtime: i64,
}

impl Ord for Rank {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name_match
            .cmp(&other.name_match)
            .then_with(|| other.first_pos.cmp(&self.first_pos))
            .then_with(|| other.path_len.cmp(&self.path_len))
            .then_with(|| other.depth.cmp(&self.depth))
            .then_with(|| self.mtime.cmp(&other.mtime))
    }
}

impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Rank {
    /// Pack the rank into a single `u64` sort key (larger is better) so the
    /// top-N heap compares one integer instead of a multi-field struct. The
    /// fields are saturated to fit: `first_pos` and `path_len` to 12 bits
    /// (4095), `depth` to 6 bits (63), and `mtime` to 33 bits. The saturation
    /// only affects paths longer than 4 KiB, which are rare.
    pub fn key(&self) -> u64 {
        let name = (self.name_match as u64) << 63;
        let pos = 4095u64 - self.first_pos.min(4095) as u64;
        let len = 4095u64 - self.path_len.min(4095) as u64;
        let dep = 63u64 - self.depth.min(63) as u64;
        let mt = self.mtime.max(0) as u64 & 0x1_FFFF_FFFF;
        name | (pos << 51) | (len << 39) | (dep << 33) | mt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_has_no_terms() {
        assert!(Query::parse("").is_empty());
        assert!(Query::parse("   \t ").is_empty());
    }

    #[test]
    fn terms_split_on_whitespace() {
        let q = Query::parse("report 2024");
        assert_eq!(q.terms().len(), 2);
    }

    #[test]
    fn ascii_find_is_case_insensitive() {
        assert_eq!(find_ascii_ci(b"/Home/Sam/Report.PDF", b"report"), Some(10));
        assert_eq!(find_ascii_ci(b"abc", b"ABC"), Some(0));
        assert_eq!(find_ascii_ci(b"abc", b"xyz"), None);
    }

    #[test]
    fn ascii_find_handles_tail_candidates() {
        // The only 'a' is at the very end, where the needle cannot fit.
        assert_eq!(find_ascii_ci(b"xxxxa", b"ab"), None);
        assert_eq!(find_ascii_ci(b"xxxxab", b"ab"), Some(4));
    }

    #[test]
    fn find_bytes_locates_substrings() {
        // F8: the SIMD (memmem) substring search must be correct.
        assert_eq!(find_bytes(b"/home/sam/report.pdf", b"report"), Some(10));
        assert_eq!(find_bytes(b"abc", b"abc"), Some(0));
        assert_eq!(find_bytes(b"abc", b"abcd"), None);
        assert_eq!(find_bytes(b"abc", b""), Some(0));
        assert_eq!(find_bytes(b"abcabc", b"cab"), Some(2));
        assert_eq!(find_bytes(b"abc", b"zzz"), None);
    }

    #[test]
    fn unicode_term_matches_case_insensitively() {
        let t = Term::new("ÜNÏCODE");
        assert_eq!(t.find("/tmp/ünïcode.txt".as_bytes()), Some(5));
    }

    #[test]
    fn file_path_match_covers_parent_name_and_boundary() {
        let parent = b"/home/sam/Documents";
        let name = b"report.txt";

        // Within the parent path.
        assert_eq!(
            Term::new("documents").find_in_file_path(parent, name, b"/"),
            Some(10)
        );
        // Within the basename (position is offset past the separator).
        assert_eq!(
            Term::new("report").find_in_file_path(parent, name, b"/"),
            Some(parent.len() + 1)
        );
        // Spanning the separator.
        assert_eq!(
            Term::new("documents/report").find_in_file_path(parent, name, b"/"),
            Some(10)
        );
        // No match.
        assert_eq!(Term::new("zzz").find_in_file_path(parent, name, b"/"), None);
    }

    #[test]
    fn rank_orders_name_match_first() {
        let name = Rank {
            name_match: true,
            first_pos: 10,
            path_len: 50,
            depth: 3,
            mtime: 0,
        };
        let path_only = Rank {
            name_match: false,
            first_pos: 0,
            path_len: 10,
            depth: 1,
            mtime: 999,
        };
        assert!(name > path_only);
    }

    #[test]
    fn rank_breaks_ties_by_position_then_length_then_depth_then_mtime() {
        let base = Rank {
            name_match: true,
            first_pos: 5,
            path_len: 20,
            depth: 2,
            mtime: 100,
        };
        let earlier = Rank {
            first_pos: 1,
            ..base
        };
        assert!(earlier > base);

        let shorter = Rank {
            path_len: 10,
            ..base
        };
        assert!(shorter > base);

        let shallower = Rank { depth: 1, ..base };
        assert!(shallower > base);

        let newer = Rank { mtime: 200, ..base };
        assert!(newer > base);
    }
}
