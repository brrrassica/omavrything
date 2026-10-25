#!/usr/bin/env bash
# Build the release binaries into bin/ and the plugin tree.
#
# This is the single build step for a source checkout. It compiles the workspace
# in release mode and copies the daemon and the CLI into two places:
#
#   bin/                       the canonical release layout. install.sh installs
#                              from here, run-daemon.sh and restart-all.sh run
#                              from here, and package-release.sh stages it into
#                              the archive.
#   shell/omavrything/bin/     the plugin tree, next to the QML. A plugin
#                              installed from a source checkout (or a release
#                              archive that carries it) finds the binaries here
#                              without a network fetch; scripts/bootstrap.sh
#                              prefers this location, then bin/, then fetches the
#                              release asset.
#
# Both locations are gitignored: the binaries are never committed. A bare clone
# has neither, so the plugin fetches the release asset on first run.
#
# Usage: scripts/build.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

# --locked keeps the build reproducible against the committed Cargo.lock.
cargo build --release --locked --workspace

mkdir -p bin shell/omavrything/bin
install -m755 target/release/omavrythingd bin/omavrythingd
install -m755 target/release/omavry bin/omavry
install -m755 target/release/omavrythingd shell/omavrything/bin/omavrythingd
install -m755 target/release/omavry shell/omavrything/bin/omavry

echo "Wrote bin/omavrythingd, bin/omavry, and shell/omavrything/bin/"
