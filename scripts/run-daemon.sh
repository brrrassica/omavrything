#!/usr/bin/env bash
# Build and run the omavrythingd daemon for local development.
#
# Usage: scripts/run-daemon.sh [--root <dir>] [--socket <path>]
# Defaults: root=$HOME, socket=$XDG_RUNTIME_DIR/omavrything.sock
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

cargo build -p omavrythingd
exec ./target/debug/omavrythingd "$@"
