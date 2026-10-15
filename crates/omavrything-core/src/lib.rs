//! Core library for `omavrything`: the in-memory index, search engine,
//! filesystem scanner, and IPC protocol.

pub mod candidate;
pub mod glob;
pub mod idle;
pub mod index;
pub mod live;
pub mod mounts;
pub mod private_file;
pub mod protocol;
pub mod query;
pub mod scan;
pub mod snapshot;
pub mod watch;

/// The crate version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Bound the rayon global pool used by the scanner and the parallel search.
///
/// The scan is I/O-bound and scales only ~1.4x from one to sixteen cores (F8),
/// so a small pool keeps the throughput while cutting the resident thread
/// stacks (rc6 RSS spike). `threads` is clamped to at least 1. Returns `false`
/// when the global pool was already built (e.g. by a test harness or the
/// profiler), in which case the request is ignored rather than fatal.
pub fn tune_scan_pool(threads: usize) -> bool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|i| format!("omavry-scan-{i}"))
        .build_global()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }
}
