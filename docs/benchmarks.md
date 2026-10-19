# Benchmarks

Baseline search-latency, indexing-throughput, and memory numbers, measured with
[`scripts/benchmark.sh`](../scripts/benchmark.sh). The script drives a real
daemon over the Unix socket, so the numbers include the socket round-trip and
the daemon's own scan — not just the in-memory index. It also samples the
daemon's resident set size (RSS) and its peak (the kernel's `VmHWM` high-water
mark), and reports the per-record memory cost.

Reproduce with:

```sh
scripts/benchmark.sh            # fast default (~10 s)
scripts/benchmark.sh --full     # larger sizes + the rescan measurement
scripts/benchmark.sh --json     # machine-readable summary on stdout
```

## 0.1.7 baseline — 2026-10-13

**Environment:** 16 logical cores, Linux 7.2, `omavrythingd 0.1.7` (release
build), live index of **736,460 entries**. Wall-clock round-trip per query,
measured over a persistent socket connection. This is the performance baseline
before the v0.2.0 indexing/search work.

### Live daemon (736 k entries)

| Query | p50 | p95 | max | daemon `took_ms` p50 |
| --- | --- | --- | --- | --- |
| `a` (broad) | 91.3 ms | 92.6 ms | 92.9 ms | 90 |
| `config` (selective) | 113.4 ms | 163.2 ms | 164.7 ms | 113 |
| `zzz-no-such-file-zzzz` (no match) | 116.2 ms | 165.8 ms | 166.6 ms | 116 |

> **Note.** These are ~10–30× slower than the "~6–18 ms for 1 M records" figure
> in [`development.md`](development.md#profiling-query-performance), which comes
> from the standalone in-memory profiler. The live daemon scans the same number
> of records but with a much larger, cache-unfriendly working set, so the
> end-to-end cost is dominated by memory latency. Treat the standalone numbers
> as a lower bound and these as the user-visible cost.

### Memory

RSS is the daemon's resident set size; peak is the kernel's `VmHWM` high-water
mark, which captures the scan's transient peak even after the daemon trims its
heap. "Index" is the daemon's own accounting of the in-memory index
(`memory_bytes`) — the cache itself, excluding the binary, allocator, and thread
stacks.

| Mode | Entries | RSS | Peak RSS | RSS/record | Peak/record | Index/record |
| --- | --- | --- | --- | --- | --- | --- |
| Live daemon | 736,460 | 129 MiB | 198 MiB | 183.7 B | 283.1 B | 57.5 B |
| Hermetic, 50 k files | 52,000 | 12.3 MiB | 13.5 MiB | 247.1 B | 272.4 B | — |
| Hermetic, 5 k files | 7,000 | 7.5 MiB | 7.5 MiB | 1115.9 B | 1115.9 B | — |

The index itself costs **57.5 B/record** on the live daemon, close to the
~70 B/file model in [`memory-optimization.md`](memory-optimization.md). The
end-to-end RSS per record is higher because it includes the binary, the
allocator's retained arenas, and thread stacks; at the small hermetic sizes the
fixed overhead dominates, so the per-record figure is not meaningful there.

### Hermetic daemon (synthetic tree)

The hermetic phase starts a private daemon on a synthetic tree, so it is
reproducible and independent of the installed index.

| Tree | Cold scan | Selective p50 | Broad p50 | No-match p50 |
| --- | --- | --- | --- | --- |
| 5,000 files (7,000 entries) | 14 ms (500 k entries/s) | 0.51 ms | 0.39 ms | 0.47 ms |
| 50,000 files (52,000 entries) | 75 ms (693 k entries/s) | 3.04 ms | 3.14 ms | 2.92 ms |

### Search while indexing

Two measurements cover "search while the index is being updated":

- **Live updates (inotify burst)** — files are created while the search loop
  runs; the watcher applies the events under the index write lock. On the
  50,000-file tree the p50 rose from **3.18 ms** (idle) to **3.54 ms** (during
  the burst) — a ~11 % increase, with the p99 rising from 4.33 ms to 4.99 ms.
- **Periodic rescan** — the rescan is idle-gated (1-minute load, CPU idle, and
  the login1 idle hint), so an **active desktop session vetoes it** and the
  measurement reports zero windows. It runs in an idle session or in CI; use
  `--rescan` (or `--full`) to include it.

## 0.2.0 baseline — 2026-10-14

**Environment:** 16 logical cores, Linux 7.2, release build. The live daemon is
the 0.2.0-rc1 build on the real index (**735,133 entries, 8 roots**). This is the
first baseline with the M23 candidate-narrowing index (F1–F4) and the F12
stop-list.

### Live daemon (735 k entries)

| Query | p50 | p95 | p99 | max |
| --- | --- | --- | --- | --- |
| `a` (broad, ~735 k matches) | 82.5 ms | 85.6 ms | 95.7 ms | 95.7 ms |
| `config` (selective) | 2.2 ms | 3.6 ms | 51.1 ms | 51.1 ms |
| `zzz-no-such-file-zzzz` (no match) | 0.02 ms | 0.025 ms | 0.031 ms | 0.031 ms |

Selective and no-match queries are 1–2 orders of magnitude faster than the 0.1.7
baseline (113.4 ms and 116.2 ms p50). The broad 1-character query is a full scan
by design: a term shorter than three bytes cannot be narrowed by the trigram
index.

### Memory

| Mode | Entries | RSS | Peak RSS | RSS/record | Index/record |
| --- | --- | --- | --- | --- | --- |
| Live daemon | 735,133 | 469 MiB | 514 MiB | 670.3 B | 500.8 B |

The index cost rose from 57.5 B/record (0.1.7) to 500.8 B/record because of the
candidate index. Its per-component breakdown (F11) is reported by
`omavry status` and `scripts/benchmark.sh`:

| Component | Bytes | B/record |
| --- | --- | --- |
| Trigram postings | ~147 MB | ~200 |
| Lowercased path arena | ~44 MB | ~60 |
| Token table | ~44 MB | ~60 |
| Posting map overhead | ~9 MB | ~12 |
| Per-record offsets | ~4 MB | ~6 |

These figures predate the F12 stop-list, which drops the posting lists of the most
frequent trigrams and so bounds the postings (the dominant component). The
`candidate_index` config key can disable the index on very large indexes, at the
cost of a linear scan per query.

### Hermetic daemon (synthetic tree, 52,000 entries)

| Metric | 0.2.0-rc1 | 0.1.6 baseline |
| --- | --- | --- |
| Cold scan | 205 ms (253,658 entries/s) | 72 ms (722 k entries/s) |
| Selective `file-0001234` p50 | 0.07 ms | 3.07 ms |
| Broad `dir0001` p50 | 0.111 ms | 3.17 ms |
| No-match `zzz…` p50 | 0.019 ms | 2.96 ms |

### Performance budgets (F9)

`scripts/benchmark.sh --budgets` checks the hermetic p50/p95 and the cold-scan
throughput against per-size budgets and exits non-zero on a regression. The
budgets scale with the entry count (`p50 <= 2 ms + entries * per_record`, with
`per_record` 0.05 µs for selective/no-match and 0.1 µs for broad; the p95 budget
is 2x the p50 one) and the cold scan must stay above 50,000 entries/s. CI runs
`scripts/benchmark.sh --quick --hermetic --budgets` in the `bench` job.

## Interpreting the numbers

- **Cold scan** is the indexing throughput: entries walked and inserted per
  second. It is I/O- and CPU-bound and scales with the tree.
- **Idle search latency** is the interactive cost: what a keystroke pays.
  Selective queries are narrowed by the candidate index; a broad query that
  cannot be narrowed is a linear scan and scales with the index size.
- **During-indexing latency** is the same query while the index is being
  mutated. The write lock is held only while a batch is applied, so the
  increase is small; a large p99 here would indicate lock contention.
- **RSS / peak RSS** is the daemon's resident memory and its high-water mark.
  The per-record figures divide by the entry count; the index-only figure comes
  from the daemon's own accounting and is the true cache cost.
- The daemon's `took_ms` is integer milliseconds and rounds sub-millisecond
  queries to `0`; the wall-clock column is the finer-grained metric.
