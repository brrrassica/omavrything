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
    fn unicode_term_matches_case_insensitively() {
        let t = Term::new("ÜNÏCODE");
        assert_eq!(t.find("/tmp/ünïcode.txt".as_bytes()), Some(5));
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
