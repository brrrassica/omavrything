# Development

Build, test, and profiling workflow for contributors.

## Continuous integration

CI runs on GitHub Actions but **inside an official Arch container**
(`archlinux:base-devel`), not on the Ubuntu host. The target platform is
Omarchy Quattro / Arch Linux, so building and testing against Arch's toolchain,
glibc, and package availability catches problems that an Ubuntu build would miss.
The release archive is built the same way, so the shipped binaries match the
target platform.

**Runner placement is a security boundary.** Untrusted code must never run on a
runner that later performs a write-privileged release:

- `pull_request` and push-to-`main` jobs run on **GitHub-hosted**
  `ubuntu-latest`, still inside the same `archlinux:base-devel` container.
- Push-to-`develop` jobs run on the **on-premise** runner
  (`[self-hosted, opencode-sam]`).
- The docs-site workflow ([`.github/workflows/pages.yml`](../.github/workflows/pages.yml))
  also runs on GitHub-hosted `ubuntu-latest`: it executes repo code
  (`site/build.py`, `pip install`) and must not share a host with the release
  job.
- The release workflow runs only on the on-premise runner and uses its own
  `/home/github-runner/release-cache` Cargo/target volume that CI can never
  write, with `persist-credentials: false` on checkout.

Both paths pin the same toolchain (`1.99.1`, `nightly-2026-10-22`) and use the
same image, so a build is identical wherever it runs. This prevents a PR job
from leaving a `$CARGO_HOME/config.toml` `rustc-wrapper` that a later release
build would execute.

The workflow ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) has seven
jobs:

- **build** — `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`
  (debug and release), and `cargo build`.
- **qml** — `scripts/lint-qml.sh` runs `qmllint` over the shell plugin's QML
  (`qt6-declarative`). The omarchy shell modules are absent in the container, so
  the `qs.*` imports are reported as warnings; the job still catches QML syntax
  and semantic errors.
- **fuzz** — a short `cargo-fuzz` smoke session of the snapshot and protocol
  targets (nightly toolchain).
- **audit** — `cargo audit` for dependency advisories. This job runs on the
  runner host rather than in a container, because advisories are
  platform-independent (they only read `Cargo.lock`).
- **smoke** — `scripts/ci-smoke.sh` exercises install → enable → search →
  uninstall inside the Arch container.
- **bench** — `scripts/benchmark.sh --quick --hermetic --budgets` checks the
  hermetic p50/p95 and cold-scan throughput against the performance budgets.
- **package** — builds the release archive with `scripts/package-release.sh` and
  uploads it as a build artifact, so a broken release is caught before a tag.
  The archive is reproducible: `package-release.sh` normalises entry timestamps,
  modes, ownership, and ordering (and self-checks by zipping the staging tree
  twice), so two builds of the same tree have the same SHA-256. The binaries are
  reproducible too: with the pinned Rust 1.99.1 toolchain and the committed
  `Cargo.lock`, two clean builds in different target directories produce
  byte-identical `omavrythingd` and `omavry` binaries. This is what makes a
  digest pinned in `scripts/daemon-checksums.txt` meaningful for the published
  asset instead of only for the machine that happened to build it.

## Release pinning: the gate that runs before the tag

Releases are digest-pinned, and the pin has historically been the weakest part of
the process: the 0.2.3, 0.2.4 and 0.2.5 releases each needed a second attempt
because a bad pin was only discovered *after* the tag existed, which is
irreversible. The cause in every case was the same: the pin was produced on a
different machine (or before an environment fix landed) than the one that
verified it.

Three things now prevent that:

**1. A pre-tag gate —
[`.github/workflows/prepare-release.yml`](../.github/workflows/prepare-release.yml).**
Run it on the release branch before tagging:

```sh
gh workflow run prepare-release.yml --ref release/0.2.6
```

It builds the archive in the exact release container and toolchain, then fails
unless `scripts/daemon-checksums.txt` already pins the produced bytes. It also
prints the produced digests in the job summary, so the pin is taken from CI
rather than from a laptop. It publishes nothing and has no write permissions.

**2. One verification implementation, shared.**
[`scripts/verify-release-pin.sh`](../scripts/verify-release-pin.sh) is used by
both the gate and the release workflow, so a green gate predicts a green release.
A green gate that does not predict a green release is precisely the failure mode
that wasted the 0.2.4 and 0.2.5 releases.

**3. An append-only pin file.**
[`scripts/checksums-add.sh`](../scripts/checksums-add.sh) merges a freshly built
`dist/daemon-checksums.txt` into `scripts/daemon-checksums.txt` without ever
removing or overwriting an entry. Hand-editing that file is what dropped released
pins from `develop` at 0.2.3, 0.2.4 and 0.2.5. The `checksums-superset` CI job
additionally fails any candidate tree that is missing an entry `main` carries.

**4. Metadata and freeze checks.**
[`scripts/verify-release-metadata.sh`](../scripts/verify-release-metadata.sh)
proves every version reference agrees with the release version: `manifest.json`
(the download URL), `Cargo.toml`/`Cargo.lock` (what `--version` reports, which
`bootstrap.sh` compares to decide whether to upgrade), `PKGBUILD` `pkgver`, and
the user-facing docs and man page shipped inside the archive. A mismatch here
breaks the plugin bootstrap or the upgrade path even when the digests are
perfect.

[`scripts/archive-inputs.sh`](../scripts/archive-inputs.sh) fingerprints every
file that goes into the archive (mode + SHA-256 + path) and pins it in
`scripts/archive-inputs.txt`. Two things follow:

- editing a doc, the manifest, `data/**`, `shell/**`, or a staged script after
  pinning is **detected without rebuilding**, which is exactly the 0.2.5
  failure;
- the pre-tag gate (on the release branch) and the release job (on the tagged
  merge commit) both recompute it. If the two trees differ in any archive
  input, the release fails instead of publishing bytes nobody validated. This
  closes the "gate validated a different tree than the tag" gap.

The release flow is therefore:

```sh
rm -rf dist                           # never reuse a previous build's output
scripts/package-release.sh            # build dist/ + dist/daemon-checksums.txt
scripts/checksums-add.sh              # append-only pin + archive-inputs.txt
scripts/verify-release-metadata.sh    # versions agree everywhere
scripts/archive-inputs.sh --check     # nothing drifted since the pin
scripts/verify-release-pin.sh         # digests match
# push the release branch, run prepare-release.yml, confirm green
# merge to main, run prepare-release.yml on main again, confirm green
# only then: tag -s and let release.yml publish
```

The release job additionally refuses a tag that is not annotated, or that no
longer resolves to the commit it was created on — the 0.2.5 tag was re-pointed
after the fact, which no digest check can catch. After publishing it downloads
the tag tarball and reports the correct `PKGBUILD` `sha256sums` in the job
summary, so the AUR step is a copy rather than a remembered `updpkgsums`.

Pushing a `v*` tag runs
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds
the archive in the same Arch container, re-verifies the pin with the same script,
and only then creates a GitHub release with the zip attached.

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

To release, the version-bump pull request updates
[`CHANGELOG.md`](../CHANGELOG.md), bumps the version, and pins the release
archive and both binaries in
[`scripts/daemon-checksums.txt`](../scripts/daemon-checksums.txt). The pins are
produced by building the archive from the exact tree that will be tagged, with
the pinned Rust 1.99.1 toolchain and the committed `Cargo.lock`. The maintainer
merges that PR into `main` and tags the merge commit with `v<version>`.

The tag triggers
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which
rebuilds the archive from the tagged tree and verifies its SHA-256 against the
pin before it publishes anything. A missing pin or a digest mismatch fails the
release instead of silently re-pinning it, so the published asset is always the
one the reviewed source committed to. Releases are tag-driven and
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
