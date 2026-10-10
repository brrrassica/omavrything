#!/usr/bin/env bash
# Build and run the omavrythingd daemon for local development.
#
# Usage: scripts/run-daemon.sh [--root <dir>] [--socket <path>]
# Defaults: root=$HOME, socket=$XDG_RUNTIME_DIR/omavrything.sock
#
# The daemon is built in release mode: search is a linear scan over millions of
# records, and a debug build is 10-50x slower (seconds per keystroke instead of
# milliseconds). Set OMAVRYTHING_PROFILE=1 to log a per-query timing breakdown.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

cargo build --release -p omavrythingd
exec ./target/release/omavrythingd "$@"
