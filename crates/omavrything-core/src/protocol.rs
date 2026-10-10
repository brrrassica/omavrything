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
}

/// One result row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultItem {
    pub path: String,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req: Request = serde_json::from_str(r#"{"q":"report","limit":8}"#).unwrap();
        assert_eq!(req.q, "report");
        assert_eq!(req.effective_limit(), 8);
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
                path: "/tmp/a".into(),
                is_dir: false,
            }],
            total: 1,
            took_ms: 3,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total, 1);
        assert_eq!(back.results[0].path, "/tmp/a");
    }
}
