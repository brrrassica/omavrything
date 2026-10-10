# Memory optimization feasibility analysis

Analysis of the five "Everything-style" optimizations proposed for `omavrything`,
measured against the current implementation in
[`crates/omavrything-core/src/index.rs`](../crates/omavrything-core/src/index.rs),
[`scan.rs`](../crates/omavrything-core/src/scan.rs), and
[`snapshot.rs`](../crates/omavrything-core/src/snapshot.rs).

## 1. Current memory model

The index is already arena-backed and index-based, which is why several of the
proposals are **already implemented**. Per live record, steady state:

| Component | Size / record | Notes |
|---|---|---|
| [`FileRecord`](../crates/omavrything-core/src/index.rs:57) | 20 B | u32 size/mtime/parent/path_off + u16 path_len + u8 depth/flags |
| `paths` arena | ~70 B | **full path bytes** (the dominant term) |
| [`PathIndex`](../crates/omavrything-core/src/index.rs:164) | ~9 B x load factor (~10 B) | `HashMap<u32, u32>` hash + index, no path copy |
| **Total** | **~100 B/file** | ~300 MB for 3M files |

The basename is derived from the path (the bytes after the last `/`), so there
is no separate `names` arena.

The gap to the observed ~700 MB RSS is explained by:

- **Transient scan buffer**: [`scan_into_index`](../crates/omavrything-core/src/scan.rs:81)
  collects every entry into a `Vec<WalkEntry>` of owned `PathBuf`s before
  inserting. Each `PathBuf` is 24 B + a heap string (~70 B) + allocator header,
  so ~100 B/file transiently — roughly **300 MB peak for 3M files**. This is the
  single largest contributor to peak RSS and is **not** in the proposal list.
- HashMap capacity slack and glibc arena retention (partly mitigated by
  [`trim_heap`](../crates/omavrythingd/src/main.rs:756)).

## 2. Feasibility of each proposal

### Proposal 1 — Flatten data structures (16-byte records)
**Status: ALREADY DONE (partially).** Records already live in a flat
`Vec<FileRecord>`; there are no per-record heap allocations. The remaining work
is shrinking the 48-byte struct toward 16-24 B by compressing fields
(`depth` u16->u8, `size` u64->u32 KB, `mtime` i64->u32). Feasible, but it forces
a snapshot format version bump and touches ranking and live updates.
**Complexity: Medium-low. Risk: Low-medium.**

### Proposal 2 — Monolithic string table / path pooling
**Status: PARTIALLY DONE.** Names and paths already live in two monolithic byte
arenas, and [`PathIndex`](../crates/omavrything-core/src/index.rs:164) already
avoids duplicating path bytes. Two distinct sub-ideas remain:

- **2a. Drop the redundant `names` arena.** The basename is always a suffix of
  the full path (after the last `/`), so `name_off`/`name_len` are derivable via
  `memrchr`. This removes ~15 B/file with no search slowdown.
  **Complexity: Low. Risk: Low.**
- **2b. Store only basenames + `parent` links, reconstruct full paths on
  demand.** This eliminates the entire `paths` arena (~70 B/file, the biggest
  win) but is **radical**: [`match_record`](../crates/omavrything-core/src/index.rs:786)
  currently scans contiguous path bytes; reconstructing paths per record means
  pointer-chasing the parent chain for every record on every keystroke, and
  `PathIndex`, `remove`, `rename`, and the snapshot format all key on full paths.
  Note Everything itself stores full paths for exactly this speed reason.
  **Complexity: High. Risk: High. Not recommended as a first step.**

### Proposal 3 — Bytecode / bitmap filter cache
**Status: NOT DONE.** The current search is a full linear scan per keystroke
using a bounded [`TopN`](../crates/omavrything-core/src/index.rs:860) heap, so
per-query memory is already tiny (the proposal's "avoid allocating result lists"
premise is largely moot). A bitmap of the previous match set (375 KB for 3M) is
primarily a **latency** optimization for incremental typing, not a memory one.
**Complexity: Medium. Risk: Medium (session state, correctness of incremental
refinement).**

### Proposal 4 — 32-bit pointers
**Status: ALREADY DONE.** All cross-references are `u32` indices
(`parent`, `name_off`, `path_off`, record indices). No 64-bit pointers are stored
in the index. **No work required.**

### Proposal 5 — Ditch `fstat`, use `readdir` `d_type`
**Status: NOT DONE.** [`walk`](../crates/omavrything-core/src/scan.rs:281) calls
`entry.metadata()` (an `lstat`) for **every** entry. `DirEntry::file_type()` uses
the `d_type` field from `readdir` with no extra syscall. However, `size` and
`mtime` still require a stat, so the win is limited unless metadata is made
optional/lazy. A pragmatic version: use `file_type()` for the dir/symlink
decision and only stat non-directories (or only when size/mtime are needed).
**Complexity: Low. Risk: Low. Benefit: modest (fewer syscalls, not much RSS).**

## 3. Additional findings (not in the proposal list)

- **Transient `Vec<WalkEntry>` (highest-impact RSS item).** DONE: the walker now
  collects into a compact [`WalkBuffer`](../crates/omavrything-core/src/scan.rs)
  (a single byte arena plus `u32` offsets), eliminating the per-path `PathBuf`
  heap allocation. This cuts peak RSS by ~200 MB at 3M files.
- **`names` arena redundancy** (see 2a) — cheap, safe win.
- **`PathIndex` load factor** — a custom open-addressed table could shave the
  HashMap overhead, but this is a micro-optimization.

## 4. Recommended ordering (lowest complexity / least radical first)

| # | Change | Complexity | Risk | Primary benefit | Status |
|---|---|---|---|---|---|
| 0 | Proposal 4 (32-bit indices) | none | none | already done | done |
| 0 | Proposal 1 (flat records) | none | none | already done | done |
| 1 | Proposal 2a: drop redundant `names` arena | Low | Low | ~15 B/file | done |
| 2 | Proposal 5: `readdir` `d_type`, stat only when needed | Low | Low | fewer syscalls | done |
| 3 | Proposal 1 remainder: shrink `FileRecord` to 24-32 B | Medium-low | Low-med | ~16-24 B/file | done |
| 4 | Transient scan buffer -> compact arena / streaming | Medium | Medium | ~200 MB peak | done |
| 5 | Proposal 3: bitmap filter cache for incremental search | Medium | Medium | latency | pending |
| 6 | Proposal 2b: drop full-path storage, reconstruct | High | High | ~70 B/file | pending |

## 5. Resident memory follow-up

After the arena/record work the daemon still sat at ~597 MB RSS. The remaining
memory is the index itself, plus two allocation-slack bugs and allocator
retention:

- **Snapshot loader over-allocated `records`.** `Vec::with_capacity(count.min(1 << 20))`
  let the vector double up to 8M capacity for 4.5M records (~112 MB slack). It
  now pre-sizes exactly after validating the count against the remaining bytes.
- **`PathIndex` was oversized.** A `HashMap<u64, u32>` pads each `(u64, u32)`
  tuple to 16 B; switching the key to a 32-bit hash makes it `(u32, u32)` (8 B),
  halving the map.
- **`compact()` was a no-op without tombstones**, so a cold scan never shrank its
  arenas. A new `Index::shrink()` reclaims the doubling slack after the scan or a
  warm start, and `memory_bytes()` now reports capacity rather than length.
- **Allocator retention.** `malloc_trim(0)` does not reliably release per-thread
  glibc arenas, so the scan's transient buffers could stay resident. The daemon
  now calls `mallopt(M_ARENA_MAX, 1)` at startup so a single arena is trimmed.

`FileRecord` was also shrunk from 32 to 20 bytes (`size`/`mtime` to `u32`,
`path_len` to `u16`, `depth` to `u8`), bumping the snapshot format to v3.

## 6. Suggested execution plan

1. Add a memory-accounting test/benchmark that reports bytes-per-record for a
   synthetic 3M index, so each change is measured (extend
   [`profile.rs`](../crates/omavrything-core/examples/profile.rs)).
2. Implement 2a (derive basename from path; remove `names` arena) and bump the
   snapshot version.
3. Implement 5 (`file_type()` fast path in `walk`).
4. Shrink `FileRecord` fields; bump snapshot version; update ranking/live paths.
5. Replace the transient `WalkEntry` buffer with a compact arena.
6. Evaluate the bitmap cache (3) and, only if still needed, the radical path
   dedup (2b).
