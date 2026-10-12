#!/usr/bin/env bash
# Build the release binaries into bin/.
#
# This is the single build step for a source checkout. It compiles the workspace
# in release mode and copies the daemon and the CLI into bin/, the same layout
# the release archive ships. Every other script reads the binaries from bin/:
# install.sh installs them, run-daemon.sh and restart-all.sh run them, and
# package-release.sh stages them into the archive.
#
# Usage: scripts/build.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

# --locked keeps the build reproducible against the committed Cargo.lock.
cargo build --release --locked --workspace

mkdir -p bin
install -m755 target/release/omavrythingd bin/omavrythingd
install -m755 target/release/omavry bin/omavry

echo "Wrote bin/omavrythingd and bin/omavry"
