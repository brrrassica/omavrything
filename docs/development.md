# Development

Build, test, and profiling workflow for contributors.

## Continuous integration

CI runs on GitHub Actions but **inside an official Arch container**
(`archlinux:base-devel`), not on the Ubuntu host. The target platform is
Omarchy Quattro / Arch Linux, so building and testing against Arch's toolchain,
glibc, and package availability catches problems that an Ubuntu build would miss.
The release archive is built the same way, so the shipped binaries match the
target platform.

The workflow ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) has five
jobs:

- **build** — `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`
  (debug and release), and `cargo build`.
- **qml** — `scripts/lint-qml.sh` runs `qmllint` over the shell plugin's QML
  (`qt6-declarative`). The omarchy shell modules are absent in the container, so
  the `qs.*` imports are reported as warnings; the job still catches QML syntax
  and semantic errors.
- **fuzz** — a short `cargo-fuzz` smoke session of the snapshot and protocol
  targets (nightly toolchain).
- **audit** — `cargo audit` for dependency advisories. This job stays on the
  default runner because advisories are platform-independent (they only read
  `Cargo.lock`).
- **package** — builds the release archive with `scripts/package-release.sh` and
  uploads it as a build artifact, so a broken release is caught before a tag.

Pushing a `v*` tag runs
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds
the archive in the same Arch container and creates a GitHub release with the zip
attached.

## Branching and releases

The project is published on the **Omarchy Marketplace**, which tracks `main`, so
`main` is protected and is never written to directly. All work lands on
`develop` first and reaches `main` only through a reviewed merge.

| Branch | Role |
| --- | --- |
| `main` | Protected marketplace/release branch. Always releasable. Updated only by merging `develop` via PR. |
| `develop` | Integration branch. The default target for all work. |
| `feat/*`, `fix/*`, `docs/*`, `chore/*` | Short-lived topic branches cut from `develop`. |

- Branch off `develop`, not `main`, and open pull requests into `develop`.
- Only a maintainer merges `develop` into `main` and tags a release.
- `main` has **branch protection enabled on the upstream repository** as a
  failsafe: direct pushes are rejected and changes must arrive through a pull
  request. A rejected push to `main` means the change should be re-targeted at
  `develop`; do not try to bypass the protection.

To release, a maintainer merges `develop` into `main` via PR, updates
[`CHANGELOG.md`](../CHANGELOG.md), then tags the merge commit on `main` with
`v<version>` and pushes the tag. The tag triggers
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds
the archive and creates the GitHub release. Releases are tag-driven and
branch-independent; the branch model only governs how the tagged commit is
produced.

CI ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) runs on every push
and pull request, so `develop` and topic branches are covered. The docs site
([`.github/workflows/pages.yml`](../.github/workflows/pages.yml)) publishes only
from `main`, so it reflects released content.

See [`docs/decisions.md`](decisions.md) (ADR-016) for the rationale.

## Build and test

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The daemon must be built in **release** mode for interactive use. A debug build
is roughly 10x slower, which turns a sub-millisecond query into hundreds of
milliseconds and makes typing feel unusable. `scripts/build.sh` compiles the
release binaries into `bin/`, and `scripts/run-daemon.sh` runs the daemon from
there.

## Workspace layout

| Crate | Artifact | Role |
| --- | --- | --- |
| `omavrything-core` | library | Index, search, scanner, IPC protocol, snapshot. |
| `omavrything-ntfs` | library | Pure-Rust NTFS MFT reader and partition detection. |
| `omavrything-client` | library | Shared Unix-socket and D-Bus clients. |
| `omavrythingd` | binary | System daemon (dedicated `omavrything` user). |
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
scripts/restart-all.sh --sudo                      # run the daemon as root for raw-device access in a dev checkout
```

See [`daemon.md`](daemon.md) for the full flag list.

## Profiling query performance

Search scans the index, narrowed by the candidate index when the query allows
it; a broad query that cannot be narrowed still scales with the index size.
Three tools measure it:

- **End-to-end benchmark** — [`scripts/benchmark.sh`](../scripts/benchmark.sh)
  drives a real daemon over the Unix socket and reports wall-clock latency
  distributions (p50/p95/p99/max) for selective, broad, and no-match queries,
  the cold-scan indexing throughput, and — the interesting case — search
  latency *while the index is being rebuilt* (a rescan) and *while it is being
  updated live* (an inotify burst). It also benchmarks the live installed
  daemon when one is reachable:

  ```sh
  scripts/benchmark.sh                 # fast default (~10 s): hermetic + live
  scripts/benchmark.sh --full          # larger sizes + the rescan measurement
  scripts/benchmark.sh --all           # --full plus the Criterion bench + profiler
  scripts/benchmark.sh --json          # machine-readable summary on stdout
  ```

  It needs `python3` and the `omavrythingd` binary (`bin/` or `/usr/bin`). The
  script ships in the release archive under `scripts/` and in the AUR package
  under `/usr/share/omavrything/scripts/`.

- **Standalone profiler** — builds a synthetic index and times a query (or a
  simulated burst of typing) with a stage-by-stage breakdown:

  ```sh
  cargo run --release -p omavrything-core --example profile            # 1M records, typing simulation
  cargo run --release -p omavrything-core --example profile -- 3000000 report
  ```

  Always pass `--release`: a debug build is 10-50x slower and the example warns
  when it detects one. On a 1M-record index a broad query is ~6-18 ms; selective
  queries are narrowed by the candidate index and are much faster.

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
optimization analysis, and [`profiling.md`](profiling.md) for CPU wakeups, idle
cost, and rescan-interval tuning.

## Fuzzing

The snapshot loader and the socket protocol parser are fuzzed with
[`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) (requires a nightly
toolchain):

```sh
cargo install cargo-fuzz
cd fuzz
cargo fuzz run snapshot_decode
cargo fuzz run protocol_parse
```

CI runs a short smoke session of each target on every push. A crash is written to
`fuzz/artifacts/<target>/`; reproduce it with
`cargo fuzz run <target> fuzz/artifacts/<target>/<crash>`.
