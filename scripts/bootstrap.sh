#!/usr/bin/env bash
# Bootstrap the omavrything system install from the plugin, as a regular user.
#
# The shell plugin runs this on first start. It ensures the daemon binary is
# available, then escalates to root with pkexec to run scripts/install.sh
# --from-plugin. The user authenticates once in the Omarchy polkit dialog
# (the omarchy.polkit shell plugin); no terminal and no manual command is
# required.
#
# Usage: scripts/bootstrap.sh [--force]
#   --force   Prompt even if a previous attempt was dismissed.
#
# Exit codes:
#   0    the system daemon is installed (or the user previously declined)
#   1    the install did not complete
#   126  pkexec: authentication dismissed or not authorized
#   127  pkexec: the installer could not be executed
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

SOCKET="/run/omavrything/omavrything.sock"
DAEMON="/usr/bin/omavrythingd"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/omavrything"
DISMISSED_MARKER="$CACHE_DIR/bootstrap-dismissed"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m==>\033[0m %s\n' "$*" >&2; exit 1; }

force=0
for arg in "$@"; do
  case "$arg" in
    "") ;; # tolerate an empty argument from the QML caller
    --force) force=1 ;;
    -h | --help)
      echo "usage: scripts/bootstrap.sh [--force]"
      exit 0
      ;;
    *) die "unknown option: $arg" ;;
  esac
done

# Already installed? The binary is the install marker; the socket is the search
# path. Either is enough to consider the system install present.
if [[ -x "$DAEMON" ]] || [[ -S "$SOCKET" ]]; then
  log "omavrythingd is already installed"
  exit 0
fi

# A previous dismissal: do not nag on every shell restart. The user can retry
# with `omarchy-shell tech.alatcerdas.omavrything setup` (which passes --force).
if [[ "$force" == 0 && -e "$DISMISSED_MARKER" ]]; then
  log "system setup was previously dismissed; run 'omarchy-shell tech.alatcerdas.omavrything setup' to retry"
  exit 0
fi

# Locate the daemon binary. Prefer a local build, then fetch the release asset.
# Two local locations are checked, in order:
#   shell/omavrything/bin/  the plugin tree (scripts/build.sh stages here too),
#                           so a plugin installed from a source checkout finds
#                           the binaries next to the QML;
#   bin/                    the canonical release layout (a source checkout
#                           after scripts/build.sh, or the release archive).
# A bare clone has neither, so the release asset is fetched and verified.
bindir=""
local_build=0
for candidate in "$here/shell/omavrything/bin" "$here/bin"; do
  if [[ -x "$candidate/omavrythingd" && -x "$candidate/omavry" ]]; then
    bindir="$candidate"
    local_build=1
    break
  fi
done
if [[ -n "$bindir" ]]; then
  log "using the local build in $bindir"
else
  bindir="$(mktemp -d "${TMPDIR:-/tmp}/omavrything-bootstrap.XXXXXX")"
  trap 'rm -rf "$bindir"' EXIT
  "$here/scripts/fetch-daemon.sh" "$bindir"
fi

# Escalate. pkexec runs the installer as root; the Omarchy polkit agent
# (omarchy.polkit) shows the authentication dialog. PKEXEC_UID is set by pkexec
# so the installer can resolve the invoking user. pkexec runs the program in
# root's home, so the installer must not rely on the current directory (it
# derives its own path from $BASH_SOURCE).
#
# A local build is not the pinned release binary, so the installer must skip the
# pinned-digest check for it (--no-verify). A fetched build is verified by the
# installer against the digests pinned in the reviewed source.
install_args=(--from-plugin --bindir "$bindir")
if [[ "$local_build" == 1 ]]; then
  install_args+=(--no-verify)
fi
log "requesting administrator authentication to install the system daemon"
set +e
pkexec "$here/scripts/install.sh" "${install_args[@]}"
rc=$?
set -e
case "$rc" in
  0)
    rm -f "$DISMISSED_MARKER"
    log "system daemon installed"
    ;;
  126)
    mkdir -p "$CACHE_DIR"
    : >"$DISMISSED_MARKER"
    warn "authentication was dismissed; run 'omarchy-shell tech.alatcerdas.omavrything setup' to retry"
    ;;
  127)
    warn "could not run the installer (pkexec 127)"
    ;;
  *)
    warn "the installer exited with status $rc"
    ;;
esac
exit "$rc"
