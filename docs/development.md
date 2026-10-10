# Development

Build, test, and profiling workflow for contributors.

## Build and test

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The daemon must be built in **release** mode for interactive use. A debug build
is roughly 10x slower, which turns a sub-millisecond query into hundreds of
milliseconds and makes typing feel unusable. `scripts/run-daemon.sh` builds and
runs the release binary.

## Workspace layout

| Crate | Artifact | Role |
| --- | --- | --- |
| `omavrything-core` | library | Index, search, scanner, IPC protocol. |
| `omavrything-ntfs` | library | Pure-Rust NTFS MFT reader and partition detection. |
| `omavrythingd` | binary | Root system daemon. |
| `omavrything-cli` | binary (`omavry`) | CLI client. |
| `omavrything` shell plugin | QML | Search bar + results overlay. |

See [`architecture.md`](architecture.md) for the design and
[`decisions.md`](decisions.md) for the decision log.

## Running the daemon from source

```sh
scripts/run-daemon.sh                              # indexes $HOME
scripts/run-daemon.sh --root /home/sam/Documents   # indexes a subtree
scripts/restart-all.sh                             # restart the daemon + reload the shell plugin
scripts/restart-all.sh --rebuild                   # rebuild the release daemon first (-r)
scripts/restart-all.sh --sudo                      # run the daemon as root (to index NTFS)
```

See [`daemon.md`](daemon.md) for the full flag list.

## Profiling query performance

Search is a linear scan over every record, so its cost is proportional to the
index size. Two tools measure it:

- **Standalone profiler** — builds a synthetic index and times a query (or a
  simulated burst of typing) with a stage-by-stage breakdown:

  ```sh
  cargo run --release -p omavrything-core --example profile            # 1M records, typing simulation
  cargo run --release -p omavrything-core --example profile -- 3000000 report
  ```

  Always pass `--release`: a debug build is 10-50x slower and the example warns
  when it detects one. On a 1M-record index a query is ~6-18 ms; the cost scales
  linearly (1M ≈ 6 ms, 2M ≈ 12 ms, 3M ≈ 18 ms).

- **Daemon query profiler** — set `OMAVRYTHING_PROFILE=1` when starting the
  daemon and every search logs a breakdown to stderr:

  ```sh
  OMAVRYTHING_PROFILE=1 scripts/run-daemon.sh
  # search "report": total=0.87ms parse=0.00ms scan=0.85ms sort=0.01ms materialize=0.01ms scanned=50401 matched=50000 parallel
  ```

  The `scan` stage dominates; `parse`, `sort`, and `materialize` are negligible
  for a bounded page. If `total` is far above the standalone numbers, the daemon
  is likely a debug build or the index is much larger than expected.

See [`memory-optimization.md`](memory-optimization.md) for the memory model and
optimization analysis.
