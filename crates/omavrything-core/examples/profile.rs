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

use omavrything_core::index::{BatchEntry, FileRecord, FsKind, Index, SearchTiming};

/// Build a synthetic index of `n` files spread across 1000 directories, plus a
/// directory record for each distinct directory (so the tree is realistic and
/// the two-level candidate index (E) can index directory names once per
/// directory). The entries are inserted sorted by path via `insert_batch`,
/// exactly as the scanner does.
fn build_index(n: usize) -> Index {
    let mut index = Index::new();
    index.add_root(Path::new("/"), FsKind::Other);

    let mut entries: Vec<(String, bool, u64, i64)> = Vec::new();
    entries.push(("/home".into(), true, 0, 0));
    entries.push(("/home/user".into(), true, 0, 0));
    for d in 0..1000 {
        entries.push((format!("/home/user/dir{d:04}"), true, 0, 0));
        for s in 0..50 {
            entries.push((format!("/home/user/dir{d:04}/sub{s:02}"), true, 0, 0));
        }
    }
    for i in 0..n {
        let dir = format!("/home/user/dir{:04}/sub{:02}", i % 1000, i % 50);
        entries.push((
            format!("{dir}/file-{i:07}.txt"),
            false,
            (i as u64) * 10,
            1_700_000_000 + i as i64,
        ));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    index.reserve(entries.len(), entries.iter().map(|e| e.0.len()).sum());
    let batch: Vec<BatchEntry> = entries
        .iter()
        .map(|(p, is_dir, size, mtime)| BatchEntry {
            path: p.as_bytes(),
            is_dir: *is_dir,
            is_symlink: false,
            size: *size,
            mtime: *mtime,
        })
        .collect();
    index.insert_batch(batch);

    // Settle the index the way the daemon does after its initial scan: merge the
    // candidate index's pending tokens and release the doubling slack.
    index.shrink();
    index
}

/// The per-record cost of a scan, in nanoseconds. This is the figure to compare
/// against the 5-8 ms target: at 1 M records, 5-8 ms is 5-8 ns/record.
fn per_record_ns(timing: &SearchTiming) -> f64 {
    if timing.scanned == 0 {
        0.0
    } else {
        timing.total.as_nanos() as f64 / timing.scanned as f64
    }
}

/// Run one query and print its timing breakdown.
fn profile_query(index: &Index, query: &str, limit: usize) {
    // Warm the caches with one untimed run so the first-touch page faults do not
    // pollute the measurement.
    let _ = index.search(query, limit, 0);

    let (outcome, timing) = index.search_profiled(query, limit, 0);
    println!(
        "  {:<16} {:>8.2} ms  ({:>6.2} ns/rec)  (scan {:>7.2}  sort {:>6.2}  mat {:>6.2})  \
         hits={:<4} total={:<8} scanned={:<8} {}",
        format!("{query:?}"),
        timing.total.as_secs_f64() * 1e3,
        per_record_ns(&timing),
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

/// Measure a **full-scan** query: one the candidate index cannot narrow (a
/// single character, or a term shorter than three bytes). The candidate index
/// does not help here, so this isolates the raw scan and is the metric to
/// compare against the 5-8 ms target.
fn profile_full_scan(index: &Index, query: &str, limit: usize) {
    // Warm the caches with one untimed run so first-touch page faults do not
    // pollute the measurement.
    let _ = index.search(query, limit, 0);

    let (outcome, timing) = index.search_profiled(query, limit, 0);
    println!(
        "  full scan {:<8} {:>8.2} ms  ({:>6.2} ns/rec)  hits={:<4} total={:<8} scanned={:<8} {}",
        format!("{query:?}"),
        timing.total.as_secs_f64() * 1e3,
        per_record_ns(&timing),
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

/// Print the index's per-component memory breakdown, including the candidate
/// index's components (F11). The final `MEMORY ...` line is machine-readable for
/// the benchmark's budget check.
fn report_memory(index: &Index, requested: usize) {
    let b = index.memory_breakdown();
    let c = b.candidate;
    let live = index.len().max(1) as f64;
    let records_bytes = (b.records_len * std::mem::size_of::<FileRecord>()) as u64;
    let index_bytes = index.memory_bytes();
    let per = |bytes: u64| bytes as f64 / live;
    println!(
        "memory breakdown ({} live records, requested {requested}):",
        index.len()
    );
    println!(
        "  {:<10} {:>12} B  {:>7.1} B/record",
        "records",
        records_bytes,
        per(records_bytes)
    );
    println!(
        "  {:<10} {:>12} B  {:>7.1} B/record",
        "paths",
        b.paths_len,
        per(b.paths_len as u64)
    );
    println!(
        "  {:<10} {:>12} B  {:>7.1} B/record",
        "map",
        b.map_cap_bytes,
        per(b.map_cap_bytes as u64)
    );
    println!(
        "  {:<10} {:>12} B  {:>7.1} B/record",
        "candidate",
        b.candidate_bytes,
        per(b.candidate_bytes)
    );
    for (name, bytes) in [
        ("lower", c.lower),
        ("off", c.off),
        ("len", c.len),
        ("parent", c.parent),
        ("postings", c.postings),
        ("map", c.map),
        ("subtree", c.subtree),
        ("stop", c.stop),
    ] {
        println!(
            "    {:<8} {:>12} B  {:>7.1} B/record",
            name,
            bytes,
            per(bytes)
        );
    }
    println!(
        "  {:<10} {:>12} B  {:>7.1} B/record",
        "total",
        index_bytes,
        per(index_bytes)
    );
    println!(
        "MEMORY index_bytes={index_bytes} index_bytes_per_record={:.1} \
         candidate_bytes={} candidate_bytes_per_record={:.1} \
         lower={} off={} len={} parent={} postings={} map={} subtree={} stop={}",
        per(index_bytes),
        b.candidate_bytes,
        per(b.candidate_bytes),
        c.lower,
        c.off,
        c.len,
        c.parent,
        c.postings,
        c.map,
        c.subtree,
        c.stop,
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

    match query.as_deref() {
        Some("memory") => report_memory(&index, n),
        Some("fullscan") => {
            println!("full-scan latency (limit 100):");
            profile_full_scan(&index, "e", 100);
        }
        Some(q) => {
            println!("query {q:?} (limit 100):");
            profile_query(&index, q, 100);
        }
        None => {
            // Simulate typing a query one character at a time, which is exactly
            // the interactive workload that feels slow.
            let typed = "file-0001234";
            println!("simulating typing {typed:?} (limit 100):");
            for end in 1..=typed.len() {
                profile_query(&index, &typed[..end], 100);
            }
            // The full-scan path is the one the candidate index cannot narrow;
            // it is the metric to compare against the 5-8 ms target.
            println!("\nfull-scan latency (limit 100):");
            profile_full_scan(&index, "e", 100);
        }
    }
}
