#!/usr/bin/env bash
# Restart the omavrything daemon and reload the omarchy-shell plugin.
#
# Use this after changing the daemon or the shell plugin: it restarts the
# release daemon, reinstalls the QML plugin, and asks the running shell to
# rescan plugins. It does NOT rebuild by default — pass --rebuild/-r to compile
# first. The daemon is always the release binary: a debug build is ~10x slower
# and makes typing feel unusable.
#
# Usage: scripts/restart-all.sh [--rebuild|-r] [--sudo] [-- <daemon args>...]
#
# Examples:
#   scripts/restart-all.sh                 # restart the existing binary, index $HOME
#   scripts/restart-all.sh --rebuild       # rebuild the release daemon, then restart
#   scripts/restart-all.sh -r              # same, short form
#   scripts/restart-all.sh --sudo          # run the daemon as root (to index NTFS)
#   scripts/restart-all.sh -- --auto       # pass --auto through to the daemon
#   scripts/restart-all.sh -- --root /data --no-watch
#
# --sudo makes sure sudo is usable up front (non-interactively when possible,
# otherwise with a visible foreground prompt). A backgrounded sudo cannot prompt
# (its stdin is /dev/null under nohup and it is not in the foreground process
# group), so without this the daemon silently comes up as the user and NTFS
# volumes are never scanned.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

build=0
sudo_daemon=0
daemon_args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --rebuild | -r)
      build=1
      shift
      ;;
    --sudo)
      sudo_daemon=1
      shift
      ;;
    --)
      shift
      daemon_args=("$@")
      break
      ;;
    *)
      daemon_args+=("$1")
      shift
      ;;
  esac
done

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() {
  printf '\033[1;31m==>\033[0m %s\n' "$*" >&2
  exit 1
}

# Make sure sudo is usable before any backgrounded use.
#
# `sudo` cannot prompt for a password when it is backgrounded: under `nohup` its
# stdin is /dev/null and it is not in the foreground process group, so it fails
# immediately when authentication is required. That is the "NTFS is not scanned"
# failure — the daemon comes up as the user and reading the raw NTFS device is
# denied. Check non-interactively first (NOPASSWD or a cached credential needs
# no prompt); only fall back to an interactive prompt when one is actually
# required, so a passwordless setup is never asked for a password.
sudo_ready=0
ensure_sudo() {
  [[ "$sudo_ready" == 1 ]] && return 0
  command -v sudo >/dev/null 2>&1 ||
    die "sudo is required to run the daemon as root but was not found"
  if sudo -n true 2>/dev/null; then
    sudo_ready=1
    return 0
  fi
  log "authenticating sudo (required to run the daemon as root)"
  sudo -v || die "sudo authentication failed; cannot run the daemon as root"
  sudo_ready=1
}

# 1. Build the release daemon (only with --rebuild/-r).
if [[ "$build" == 1 ]]; then
  log "building release daemon"
  cargo build --release -p omavrythingd
fi

# 2. Stop any running daemon. SIGTERM lets it write a snapshot and exit cleanly.
if pgrep -x omavrythingd >/dev/null 2>&1; then
  log "stopping running daemon"
  pkill -x omavrythingd 2>/dev/null || true
  for _ in $(seq 1 50); do
    pgrep -x omavrythingd >/dev/null 2>&1 || break
    sleep 0.1
  done
  # A daemon started as root (e.g. to index NTFS volumes) needs sudo to stop.
  if pgrep -x omavrythingd >/dev/null 2>&1; then
    log "daemon is root-owned; retrying with sudo"
    ensure_sudo
    sudo pkill -x omavrythingd || true
    for _ in $(seq 1 50); do
      pgrep -x omavrythingd >/dev/null 2>&1 || break
      sleep 0.1
    done
  fi
  if pgrep -x omavrythingd >/dev/null 2>&1; then
    die "could not stop the running daemon"
  fi
fi

# Remove a stale socket/lock left behind by a root-owned daemon: the lock file is
# opened for writing, so a root-owned one blocks a user-run daemon.
sock="${XDG_RUNTIME_DIR:-/tmp}/omavrything.sock"
for f in "$sock" "$sock.lock"; do
  if [[ -e "$f" && ! -w "$f" ]]; then
    log "removing root-owned $f (sudo)"
    ensure_sudo
    sudo rm -f "$f" || die "could not remove $f"
  fi
done

# 3. Start the daemon in the background.
log_file="${XDG_STATE_HOME:-$HOME/.local/state}/omavrything/daemon.log"
mkdir -p "$(dirname "$log_file")"
if [[ "$sudo_daemon" == 1 ]]; then
  # Run as root (needed to read NTFS devices) but index the user's home and
  # serve the user's socket, so the shell can still connect. Authenticate first
  # so the backgrounded sudo below does not need to prompt.
  ensure_sudo
  log "starting daemon as root (sudo)"
  daemon_args=(--root "$HOME" --socket "$sock" "${daemon_args[@]}")
  sudo nohup ./target/release/omavrythingd "${daemon_args[@]}" >"$log_file" 2>&1 &
else
  log "starting daemon"
  nohup ./target/release/omavrythingd "${daemon_args[@]}" >"$log_file" 2>&1 &
fi

# Wait for the daemon to appear: sudo authentication and startup can take a
# moment, so poll instead of a single fixed sleep.
daemon_pid=""
for _ in $(seq 1 50); do
  daemon_pid="$(pgrep -x omavrythingd | head -n1 || true)"
  [[ -n "$daemon_pid" ]] && break
  sleep 0.1
done
if [[ -z "$daemon_pid" ]]; then
  log "daemon failed to start; last log lines:"
  tail -n 20 "$log_file" >&2 || true
  exit 1
fi

# When root was requested, verify the daemon really is root. A backgrounded
# sudo that could not authenticate would otherwise leave a user-owned daemon
# that cannot read NTFS devices, and the failure would go unnoticed.
if [[ "$sudo_daemon" == 1 ]]; then
  daemon_uid="$(awk '/^Uid:/{print $2}' "/proc/$daemon_pid/status" 2>/dev/null || true)"
  if [[ "$daemon_uid" != "0" ]]; then
    die "daemon is running as uid ${daemon_uid:-?}, not root; NTFS will not be scanned"
  fi
fi
log "daemon running (pid $daemon_pid, log: $log_file)"

# 4. Reinstall the shell plugin and restart the shell. A plugin rescan keeps the
# previously compiled component, so the shell process must restart for changed
# QML to take effect.
log "reinstalling shell plugin"
plugin_dest="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/tech.alatcerdas.omavrything"
mkdir -p "$plugin_dest/shell/omavrything"
install -m644 manifest.json "$plugin_dest/manifest.json"
install -m644 shell/omavrything/*.qml "$plugin_dest/shell/omavrything/"
if command -v omarchy >/dev/null 2>&1; then
  omarchy restart shell >/dev/null 2>&1 ||
    log "could not restart the shell; run 'omarchy restart shell'"
else
  log "omarchy not found; restart the shell manually"
fi

log "done"
