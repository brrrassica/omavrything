//! The newline-delimited JSON protocol spoken over the Unix domain socket.
//!
//! One request per line, one response per line. The result set is bounded by
//! `limit`, so JSON serialization is not the bottleneck.

use serde::{Deserialize, Serialize};

/// The request method. Only `Search` exists in v0.1; the discriminator is
/// present now so control methods can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    #[default]
    Search,
}

/// A search request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// The request method. Defaults to `search` when omitted.
    #[serde(default)]
    pub method: Method,
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

/// The daemon's index readiness, surfaced to clients so a UI can show a
/// "reloading index" state while an evicted image is being mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    /// The index is resident and queries are served directly.
    Ready,
    /// The index is being loaded; the query returned no results.
    Loading,
}

/// A search response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub results: Vec<ResultItem>,
    pub total: usize,
    pub took_ms: u64,
    /// Set when the request could not be served (e.g. malformed JSON). Absent
    /// on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The index readiness. Absent (treated as `ready`) on older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<IndexState>,
}

impl Response {
    /// An error response carrying no results.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            results: Vec::new(),
            total: 0,
            took_ms: 0,
            error: Some(message.into()),
            state: None,
        }
    }

    /// Whether the daemon is still loading the index. The query returned no
    /// results and should be retried once the load completes.
    pub fn is_loading(&self) -> bool {
        self.state == Some(IndexState::Loading)
    }
}

/// The default result cap when a request omits `limit`.
pub const DEFAULT_LIMIT: usize = 100;

/// The hard result cap, regardless of what a request asks for.
pub const MAX_LIMIT: usize = 1000;

/// The maximum length of a single request line, in bytes. A longer line is
/// rejected before it is parsed, so a client cannot force unbounded buffering.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Parse one request line (without the trailing newline).
///
/// This is the single entry point the daemon uses for untrusted socket input, so
/// it is also the target of the protocol fuzzer. It enforces [`MAX_LINE_BYTES`]
/// and rejects invalid UTF-8 and malformed JSON with a human-readable error.
pub fn parse_request(line: &[u8]) -> Result<Request, String> {
    if line.len() > MAX_LINE_BYTES {
        return Err(format!("request line exceeds {MAX_LINE_BYTES} bytes"));
    }
    let text = std::str::from_utf8(line).map_err(|e| format!("invalid UTF-8: {e}"))?;
    serde_json::from_str(text).map_err(|e| e.to_string())
}

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
    fn method_defaults_to_search_and_parses_explicitly() {
        let req: Request = serde_json::from_str(r#"{"q":"x"}"#).unwrap();
        assert_eq!(req.method, Method::Search);

        let req: Request = serde_json::from_str(r#"{"method":"search","q":"x"}"#).unwrap();
        assert_eq!(req.method, Method::Search);

        assert!(serde_json::from_str::<Request>(r#"{"method":"bogus","q":"x"}"#).is_err());
    }

    #[test]
    fn error_response_omits_error_on_success() {
        let ok = Response {
            results: Vec::new(),
            total: 0,
            took_ms: 0,
            error: None,
            state: None,
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert!(!json.contains("error"));

        let err = Response::error("bad request");
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("bad request"));
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(back.error.as_deref(), Some("bad request"));
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
    fn parse_request_accepts_a_valid_line() {
        let req = parse_request(br#"{"q":"report","limit":8}"#).unwrap();
        assert_eq!(req.q, "report");
        assert_eq!(req.effective_limit(), 8);
    }

    #[test]
    fn parse_request_rejects_malformed_and_oversized_input() {
        assert!(parse_request(b"not json").is_err());
        assert!(parse_request(&[0xff, 0xfe]).is_err());
        let oversized = vec![b'a'; MAX_LINE_BYTES + 1];
        assert!(parse_request(&oversized).is_err());
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
            error: None,
            state: Some(IndexState::Loading),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total, 1);
        assert_eq!(back.results[0].name, "a");
        assert_eq!(back.results[0].size, 10);
        assert_eq!(back.results[0].mtime, 42);
        assert_eq!(back.state, Some(IndexState::Loading));
    }

    #[test]
    fn state_is_omitted_when_absent() {
        let resp = Response {
            results: Vec::new(),
            total: 0,
            took_ms: 0,
            error: None,
            state: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("state"));
        // An older daemon's response (no `state`) deserializes to `None`.
        let back: Response =
            serde_json::from_str(r#"{"results":[],"total":0,"took_ms":0}"#).unwrap();
        assert_eq!(back.state, None);
    }
}
