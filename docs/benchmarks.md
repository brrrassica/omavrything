# Benchmarks

Baseline search-latency and indexing-throughput numbers, measured with
[`scripts/benchmark.sh`](../scripts/benchmark.sh). The script drives a real
daemon over the Unix socket, so the numbers include the socket round-trip and
the daemon's own scan — not just the in-memory index.

Reproduce with:

```sh
scripts/benchmark.sh            # fast default (~10 s)
scripts/benchmark.sh --full     # larger sizes + the rescan measurement
scripts/benchmark.sh --json     # machine-readable summary on stdout
```

## 0.1.6 baseline — 2026-10-13

**Environment:** 16 logical cores, Linux 7.2, `omavrythingd 0.1.6` (release
build), live index of **1,372,106 entries**. Wall-clock round-trip per query,
measured over a persistent socket connection.

### Live daemon (1.37 M entries)

| Query | p50 | p95 | max | daemon `took_ms` p50 |
| --- | --- | --- | --- | --- |
| `a` (broad, ~1.35 M matches) | 181.2 ms | 182.4 ms | 182.4 ms | 180 |
| `config` (selective, ~44 k matches) | 279.8 ms | 283.1 ms | 283.1 ms | 279 |
| `zzz-no-such-file-zzzz` (no match) | 222.1 ms | 278.2 ms | 278.2 ms | 221 |

> **Note.** These are ~10–30× slower than the "~6–18 ms for 1 M records" figure
> in [`development.md`](development.md#profiling-query-performance), which comes
> from the standalone in-memory profiler. The live daemon scans the same number
> of records but with a much larger, cache-unfriendly working set, so the
> end-to-end cost is dominated by memory latency. Treat the standalone numbers
> as a lower bound and these as the user-visible cost.

### Hermetic daemon (synthetic tree)

The hermetic phase starts a private daemon on a synthetic tree, so it is
reproducible and independent of the installed index.

| Tree | Cold scan | Selective p50 | Broad p50 | No-match p50 |
| --- | --- | --- | --- | --- |
| 5,000 files (7,000 entries) | 14 ms (500 k entries/s) | 0.44 ms | 0.35 ms | 0.43 ms |
| 50,000 files (52,000 entries) | 72 ms (722 k entries/s) | 3.07 ms | 3.17 ms | 2.96 ms |

### Search while indexing

Two measurements cover "search while the index is being updated":

- **Live updates (inotify burst)** — files are created while the search loop
  runs; the watcher applies the events under the index write lock. On the
  50,000-file tree the p50 rose from **3.58 ms** (idle) to **3.85 ms** (during
  the burst) — a ~7 % increase, with the p99 rising from 4.86 ms to 5.17 ms.
- **Periodic rescan** — the rescan is idle-gated (1-minute load, CPU idle, and
  the login1 idle hint), so an **active desktop session vetoes it** and the
  measurement reports zero windows. It runs in an idle session or in CI; use
  `--rescan` (or `--full`) to include it.

## Interpreting the numbers

- **Cold scan** is the indexing throughput: entries walked and inserted per
  second. It is I/O- and CPU-bound and scales with the tree.
- **Idle search latency** is the interactive cost: what a keystroke pays. It is
  a linear scan, so it scales with the index size.
- **During-indexing latency** is the same query while the index is being
  mutated. The write lock is held only while a batch is applied, so the
  increase is small; a large p99 here would indicate lock contention.
- The daemon's `took_ms` is integer milliseconds and rounds sub-millisecond
  queries to `0`; the wall-clock column is the finer-grained metric.
