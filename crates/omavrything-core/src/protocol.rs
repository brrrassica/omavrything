//! The newline-delimited JSON protocol spoken over the Unix domain socket.
//!
//! One request per line, one response per line. The result set is bounded by
//! `limit`, so JSON serialization is not the bottleneck.

use serde::{Deserialize, Serialize};

/// A search request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// The query string. Space-separated terms are ANDed.
    pub q: String,
    /// Maximum number of results to return. Defaults to 100, capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Number of leading matches to skip, for paging. Defaults to 0.
    #[serde(default)]
    pub offset: Option<usize>,
}

/// One result row.
///
/// Carries the metadata the UI needs to disambiguate duplicates: the basename,
/// the full path, the size, and the last-modified time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultItem {
    /// The basename (final path component).
    pub name: String,
    /// The full path.
    pub path: String,
    /// Size in bytes. Zero for directories.
    pub size: u64,
    /// Last-modified time as Unix seconds. Zero when unavailable.
    pub mtime: i64,
    pub is_dir: bool,
}

/// A search response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub results: Vec<ResultItem>,
    pub total: usize,
    pub took_ms: u64,
}

/// The default result cap when a request omits `limit`.
pub const DEFAULT_LIMIT: usize = 100;

/// The hard result cap, regardless of what a request asks for.
pub const MAX_LIMIT: usize = 1000;

impl Request {
    /// The effective limit for this request, clamped to [`MAX_LIMIT`].
    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
    }

    /// The effective offset for this request. Defaults to 0.
    pub fn effective_offset(&self) -> usize {
        self.offset.unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req: Request = serde_json::from_str(r#"{"q":"report","limit":8,"offset":16}"#).unwrap();
        assert_eq!(req.q, "report");
        assert_eq!(req.effective_limit(), 8);
        assert_eq!(req.effective_offset(), 16);
    }

    #[test]
    fn offset_defaults_to_zero() {
        let req: Request = serde_json::from_str(r#"{"q":"x"}"#).unwrap();
        assert_eq!(req.effective_offset(), 0);
    }

    #[test]
    fn limit_defaults_and_clamps() {
        let req: Request = serde_json::from_str(r#"{"q":"x"}"#).unwrap();
        assert_eq!(req.effective_limit(), DEFAULT_LIMIT);

        let req: Request = serde_json::from_str(r#"{"q":"x","limit":99999}"#).unwrap();
        assert_eq!(req.effective_limit(), MAX_LIMIT);
    }

    #[test]
    fn response_round_trips() {
        let resp = Response {
            results: vec![ResultItem {
                name: "a".into(),
                path: "/tmp/a".into(),
                size: 10,
                mtime: 42,
                is_dir: false,
            }],
            total: 1,
            took_ms: 3,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total, 1);
        assert_eq!(back.results[0].name, "a");
        assert_eq!(back.results[0].size, 10);
        assert_eq!(back.results[0].mtime, 42);
    }
}
