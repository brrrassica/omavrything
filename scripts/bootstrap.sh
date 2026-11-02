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

# The version this plugin ships. Read it from the manifest so the installed
# daemon can be compared against it.
manifest_version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$here/manifest.json" 2>/dev/null | head -n1)"
[[ -n "$manifest_version" ]] ||
  warn "could not read the version from manifest.json; treating any install as outdated"

# Already installed? The binary is the install marker; the socket is the search
# path. An install only counts as up to date when its daemon reports the same
# version as this plugin: otherwise a security fix in the reviewed source would
# never reach an existing daemon, because the old binary alone would satisfy the
# marker. An outdated install falls through to the fetch + install below, which
# is idempotent and restarts the daemon on the new binary.
#
# The version string is a coarse identity: two different builds of the same
# version report the same number, so a rebuilt security fix can go undetected.
# `--force` therefore bypasses this shortcut (not just the dismissal marker) and
# reinstalls unconditionally, which is the recovery path for a same-version
# rebuild. The plugin still bumps its manifest version for ordinary upgrades.
installed_version=""
if [[ -x "$DAEMON" ]]; then
  installed_version="$("$DAEMON" --version 2>/dev/null | awk '{print $2}' | head -n1 || true)"
fi
if [[ -x "$DAEMON" ]] || [[ -S "$SOCKET" ]]; then
  if [[ "$force" == 0 && -n "$manifest_version" && "$installed_version" == "$manifest_version" ]]; then
    log "omavrythingd $installed_version is already installed (use --force to reinstall)"
    exit 0
  fi
  log "omavrythingd ${installed_version:-unknown} differs from plugin ${manifest_version:-unknown}; updating"
fi

# A previous dismissal: do not nag on every shell restart. The marker records
# the version it dismissed, so a later plugin version (e.g. a security fix) can
# prompt once more while repeated restarts on the same version stay quiet. The
# user can always retry with `omarchy-shell tech.alatcerdas.omavrything setup`
# (which passes --force).
if [[ "$force" == 0 && "$manifest_version" != "" && -e "$DISMISSED_MARKER" ]] &&
  [[ "$(cat "$DISMISSED_MARKER" 2>/dev/null || true)" == "$manifest_version" ]]; then
  log "system setup for $manifest_version was previously dismissed; run 'omarchy-shell tech.alatcerdas.omavrything setup' to retry"
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
    # Record which version was dismissed so a later version can prompt again.
    printf '%s\n' "$manifest_version" >"$DISMISSED_MARKER"
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
