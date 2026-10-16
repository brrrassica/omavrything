//! Criterion benchmark: query latency over a synthetic 1M-file index.
//!
//! The index is built once, outside the timed loop. Two queries are measured:
//! a selective one (a single file) and a broad one (a directory prefix that
//! matches ~1000 files). Both scan every record, so the numbers reflect the
//! linear-scan cost the architecture targets at sub-20 ms for 1M files.

use std::path::Path;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use omavrything_core::index::{FsKind, Index};
use omavrything_core::snapshot::{self, IndexImage};

/// Build a synthetic index of `n` files spread across 1000 directories.
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
    // Settle the index the way the daemon does after its initial scan.
    index.shrink();
    index
}

fn bench_search(c: &mut Criterion) {
    let index = build_index(1_000_000);

    let mut group = c.benchmark_group("search_1m");
    group.bench_function("selective", |b| {
        b.iter(|| {
            let out = index.search(black_box("file-0001234"), 100, 0);
            black_box(out.total);
        });
    });
    group.bench_function("broad", |b| {
        b.iter(|| {
            let out = index.search(black_box("dir0001"), 100, 0);
            black_box(out.total);
        });
    });
    group.finish();
}

/// Benchmark the v5 image: parsing (the evicted-load cost) and the linear scan
/// over the borrowed image (the evicted-query cost).
fn bench_image(c: &mut Criterion) {
    let index = build_index(1_000_000);
    let bytes = snapshot::encode(&index, &[]);

    let mut group = c.benchmark_group("image_1m");
    group.bench_function("parse", |b| {
        b.iter(|| {
            let image = IndexImage::from_bytes(black_box(&bytes)).unwrap();
            black_box(image.len());
        });
    });
    group.bench_function("search_selective", |b| {
        let image = IndexImage::from_bytes(&bytes).unwrap();
        b.iter(|| {
            let out = image.search(black_box("file-0001234"), 100, 0);
            black_box(out.total);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_search, bench_image);
criterion_main!(benches);
