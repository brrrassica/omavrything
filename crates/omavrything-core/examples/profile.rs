//! Standalone query profiler for the in-memory index.
//!
//! Builds a synthetic index of `N` records (default 1,000,000) and measures the
//! real wall-clock cost of a query, broken down by stage (parse / scan / sort /
//! materialize). This is the tool to reach for when a search feels slow: it
//! isolates the index from the daemon, the socket, and the UI.
//!
//! Usage:
//!   cargo run --release -p omavrything-core --example profile -- [N] [QUERY]
//!
//! Examples:
//!   # Simulate typing "file-0001234" one letter at a time over 1M records.
//!   cargo run --release -p omavrything-core --example profile
//!
//!   # Time a single query over 3M records.
//!   cargo run --release -p omavrything-core --example profile -- 3000000 report
//!
//! IMPORTANT: always pass `--release`. A debug build is 10-50x slower and will
//! make the numbers meaningless (the example warns when it detects one).

use std::path::Path;
use std::time::Instant;

use omavrything_core::index::{FsKind, Index};

/// Build a synthetic index of `n` files spread across 1000 directories, plus a
/// directory record for each distinct directory (so the tree is realistic).
fn build_index(n: usize) -> Index {
    let mut index = Index::new();
    index.add_root(Path::new("/"), FsKind::Other);
    for i in 0..n {
        let dir = format!("/home/user/dir{:04}/sub{:02}", i % 1000, i % 50);
        let path = format!("{dir}/file-{i:07}.txt");
        index.insert(
            Path::new(&path),
            false,
            (i as u64) * 10,
            1_700_000_000 + i as i64,
        );
    }
    // Settle the index the way the daemon does after its initial scan: merge the
    // candidate index's pending tokens and release the doubling slack.
    index.shrink();
    index
}

/// Run one query and print its timing breakdown.
fn profile_query(index: &Index, query: &str, limit: usize) {
    // Warm the caches with one untimed run so the first-touch page faults do not
    // pollute the measurement.
    let _ = index.search(query, limit, 0);

    let (outcome, timing) = index.search_profiled(query, limit, 0);
    println!(
        "  {:<16} {:>8.2} ms  (scan {:>7.2}  sort {:>6.2}  mat {:>6.2})  \
         hits={:<4} total={:<8} scanned={:<8} {}",
        format!("{query:?}"),
        timing.total.as_secs_f64() * 1e3,
        timing.scan.as_secs_f64() * 1e3,
        timing.sort.as_secs_f64() * 1e3,
        timing.materialize.as_secs_f64() * 1e3,
        outcome.hits.len(),
        outcome.total,
        timing.scanned,
        if timing.parallel {
            "parallel"
        } else {
            "sequential"
        },
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let query: Option<String> = args.next();

    if cfg!(debug_assertions) {
        eprintln!(
            "warning: this is a DEBUG build; timings are 10-50x slower than release.\n\
             \x20        re-run with: cargo run --release -p omavrything-core --example profile"
        );
    }

    println!("building index of {n} records...");
    let build_started = Instant::now();
    let index = build_index(n);
    println!(
        "index built: {} live records in {:.2} s\n",
        index.len(),
        build_started.elapsed().as_secs_f64()
    );

    match query {
        Some(q) => {
            println!("query {q:?} (limit 100):");
            profile_query(&index, &q, 100);
        }
        None => {
            // Simulate typing a query one character at a time, which is exactly
            // the interactive workload that feels slow.
            let typed = "file-0001234";
            println!("simulating typing {typed:?} (limit 100):");
            for end in 1..=typed.len() {
                profile_query(&index, &typed[..end], 100);
            }
        }
    }
}
