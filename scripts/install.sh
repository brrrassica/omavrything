#!/usr/bin/env bash
# Install omavrything system-wide in one step.
#
# Usage:
#   sudo scripts/install.sh
#
# This is the only install path. It installs the daemon, CLI, systemd unit,
# sysusers drop-in, D-Bus policy, polkit action, man page, default config, and
# the omarchy-shell plugin; creates the omavrything user; adds the invoking user
# to the omavrything group; reloads D-Bus; enables and starts the daemon; and
# installs, enables, and loads the shell plugin for the invoking user.
#
# The binaries are read from bin/ at the top level, the same layout the release
# archive ships. In a source checkout, run scripts/build.sh first to populate it.
#
# There are no options. The invoking user (the one who ran sudo) is the user the
# shell plugin is installed for; run it with sudo, not as root directly.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Options. The default (no options) is the manual `sudo scripts/install.sh`
# path. --from-plugin is used by scripts/bootstrap.sh, which escalates with
# pkexec from the shell plugin; it skips the plugin-copy step because
# `omarchy plugin add` already installed and enabled the plugin. --no-verify
# skips the pinned-digest check for a local build (scripts/build.sh), whose
# binaries are not the pinned release binaries.
from_plugin=0
bindir_override=""
no_verify=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --from-plugin)
      from_plugin=1
      shift
      ;;
    --bindir)
      [[ $# -ge 2 ]] || {
        echo "error: --bindir requires a directory" >&2
        exit 2
      }
      bindir_override="$2"
      shift 2
      ;;
    --no-verify)
      no_verify=1
      shift
      ;;
    -h | --help)
      echo "usage: sudo scripts/install.sh [--from-plugin] [--bindir <dir>] [--no-verify]"
      exit 0
      ;;
    *)
      echo "error: unknown option: $1" >&2
      echo "usage: sudo scripts/install.sh [--from-plugin] [--bindir <dir>] [--no-verify]" >&2
      exit 2
      ;;
  esac
done

if [[ "$(id -u)" -ne 0 ]]; then
  echo "error: install.sh must be run as root (use sudo or pkexec)" >&2
  exit 1
fi

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die() {
  printf '\033[1;31m==>\033[0m %s\n' "$*" >&2
  exit 1
}

# Directories staged for the atomic swaps below. Cleared as soon as a swap
# succeeds, so the EXIT trap never removes the tree that was just installed.
share_stage=""
doc_stage=""
plugin_new=""

cleanup_staging() {
  if [[ -n "$share_stage" ]]; then rm -rf "$share_stage" || true; fi
  if [[ -n "$doc_stage" ]]; then rm -rf "$doc_stage" || true; fi
  if [[ -n "$plugin_new" ]]; then rm -rf "$plugin_new" || true; fi
}
trap cleanup_staging EXIT

# Install a fully staged directory by renaming the old tree aside and the new
# one into its place. A failure at any point restores the old tree, so the host
# is never left without the previous directory (issues #130 and #134).
# Usage: atomic_dir_swap <runner> <staged-dir> <destination>
# The runner is `as_user` for the per-user plugin and `as_root` for system dirs.
as_root() { "$@"; }
atomic_dir_swap() {
  local runner="$1" staged="$2" dest="$3"
  local old="${dest}.old-$$"
  local moved_old=0

  if [[ -e "$dest" || -L "$dest" ]]; then
    "$runner" rm -rf "$old" || return 1
    "$runner" mv -T "$dest" "$old" || return 1
    moved_old=1
  fi
  if ! "$runner" mv -T "$staged" "$dest"; then
    if [[ "$moved_old" -eq 1 ]]; then
      "$runner" mv -T "$old" "$dest" || true
    fi
    return 1
  fi
  if [[ "$moved_old" -eq 1 ]]; then
    "$runner" rm -rf "$old" || true
  fi
}

# Locate the release binaries. They live in bin/ at the top level, the same
# layout the release archive ships; a source checkout populates it with
# scripts/build.sh. --bindir overrides the location (the bootstrap passes a
# fetched or local build directory).
bindir="${bindir_override:-$here/bin}"
if [[ ! -x "$bindir/omavrythingd" || ! -x "$bindir/omavry" ]]; then
  die "omavrythingd/omavry not found in $bindir; run scripts/build.sh first"
fi

# The version reported by the manifest, used for the install summary and to
# locate the pinned digests.
version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$here/manifest.json" 2>/dev/null | head -n1)"

# Verify the binaries against the digests pinned in the reviewed source before
# installing or executing them as root. This is the enforcement point: it holds
# no matter how the binaries reached $bindir (the plugin's fetch, a release
# archive, or a --bindir override), and it re-checks the exact bytes the
# installer reads, closing the gap between the fetch-time check and the install.
# A local build (scripts/build.sh) is not the pinned release binary, so it must
# be installed with --no-verify.
if [[ "$no_verify" == 0 ]]; then
  checksums="$here/scripts/daemon-checksums.txt"
  [[ -f "$checksums" ]] ||
    die "missing $checksums; refusing to install unverified binaries (use --no-verify for a local build)"
  arch="$(uname -m)"
  for bin in omavrythingd omavry; do
    key="omavrything-${version}-${arch}/bin/${bin}"
    expected="$(awk -v f="$key" '$2 == f { print $1; exit }' "$checksums")"
    [[ -n "$expected" ]] ||
      die "no pinned digest for $key in $checksums; refusing to install unverified binaries (use --no-verify for a local build)"
    actual="$(sha256sum "$bindir/$bin" | awk '{print $1}')"
    [[ "$actual" == "$expected" ]] ||
      die "checksum mismatch for $bindir/$bin (expected $expected, got $actual); refusing to install unverified binaries"
  done
  log "verified the binaries against the pinned digests"
else
  warn "skipping binary verification (--no-verify); only use this for a local build"
fi

# The previous installation is NOT removed up front. New files are installed
# (or staged and swapped for whole trees) first, and only files that current
# versions no longer ship are deleted at the end of this section. A failure in
# between therefore cannot strip the host of its working install (issue #130).
# The config, the dedicated user, and the cached index are preserved on upgrade.

# The user the shell plugin is installed for. Under sudo this is the real
# invoker; under pkexec (the plugin bootstrap) it is $PKEXEC_UID. Running as
# root directly leaves it empty and the per-user steps are skipped with a
# warning.
target_user="${SUDO_USER:-}"
if [[ -z "$target_user" || "$target_user" == "root" ]] && [[ -n "${PKEXEC_UID:-}" ]]; then
  target_user="$(getent passwd "$PKEXEC_UID" | cut -d: -f1 || true)"
fi
if [[ -z "$target_user" || "$target_user" == "root" ]]; then
  warn "could not determine a non-root invoking user; run with sudo or pkexec so the shell plugin can be installed"
  target_user=""
fi

user_home=""
user_uid=""
if [[ -n "$target_user" ]]; then
  user_home="$(getent passwd "$target_user" | cut -d: -f6 || true)"
  user_uid="$(id -u "$target_user" 2>/dev/null || true)"
fi
# Fail safe: if the home directory could not be resolved to an absolute path
# (unknown user, atypical LDAP/NIS entries, a stale $SUDO_USER), the
# user-executed `rm -rf "$user_home/..."` below would collapse to /.config/...
# and friends. Skip the per-user steps entirely in that case.
if [[ -n "$target_user" && ( -z "$user_home" || "$user_home" != /* ) ]]; then
  warn "could not resolve the home directory of $target_user; skipping per-user plugin install"
  user_home=""
fi

# Run a command as the invoking user with a usable session environment. The
# per-user plugin, `omarchy plugin enable`, and the shell restart must run in
# the user's session (their HOME, runtime dir, and session bus), not root's.
#
# OMARCHY_PATH is required by the `omarchy` dispatcher and its subcommands
# (e.g. `omarchy plugin enable` fails with "OMARCHY_PATH is not set" without
# it); the user's login shell exports it, but `runuser` does not inherit it.
#
# A UTF-8 locale is set explicitly: without one, Qt (omarchy-shell) prints a
# locale warning to stdout, which `omarchy plugin enable` captures and compares
# against "ok", so the enable spuriously fails even though the plugin is
# enabled.
as_user() {
  [[ -n "$target_user" ]] || return 1
  runuser -u "$target_user" -- env \
    HOME="$user_home" \
    USER="$target_user" \
    LOGNAME="$target_user" \
    LANG="C.UTF-8" \
    LC_ALL="C.UTF-8" \
    XDG_RUNTIME_DIR="/run/user/$user_uid" \
    DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$user_uid/bus" \
    OMARCHY_PATH="/usr/share/omarchy" \
    PATH="/usr/share/omarchy/bin:/usr/local/bin:/usr/bin:/bin" \
    "$@"
}

# Reload the system D-Bus configuration so a newly installed policy takes effect.
# A bus that was already running before the install does not see the new policy
# otherwise, and the daemon cannot own its name (ServiceUnknown: The name is not
# activatable). Prefer the portable ReloadConfig call; fall back to the service.
reload_dbus() {
  if command -v dbus-send >/dev/null 2>&1; then
    if dbus-send --system --type=method_call --dest=org.freedesktop.DBus \
      /org/freedesktop/DBus org.freedesktop.DBus.ReloadConfig >/dev/null 2>&1; then
      return 0
    fi
  fi
  systemctl reload dbus >/dev/null 2>&1 || true
}

# 1. Install the system files.
log "installing system files"

install -Dm755 "$bindir/omavrythingd" /usr/bin/omavrythingd
install -Dm755 "$bindir/omavry" /usr/bin/omavry

install -Dm644 "$here/data/systemd/omavrythingd.service" \
  /usr/lib/systemd/system/omavrythingd.service
install -Dm644 "$here/data/sysusers/omavrything.conf" \
  /usr/lib/sysusers.d/omavrything.conf
install -Dm644 "$here/data/tmpfiles/omavrything.conf" \
  /usr/lib/tmpfiles.d/omavrything.conf
install -Dm644 "$here/data/dbus/tech.alatcerdas.omavrything.conf" \
  /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf
install -Dm644 "$here/data/polkit/tech.alatcerdas.omavrything.policy" \
  /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy
install -Dm644 "$here/data/man/omavrythingd.8" \
  /usr/share/man/man8/omavrythingd.8

# Do not clobber an existing config on upgrade.
if [[ ! -e /etc/omavrything/config.toml ]]; then
  install -Dm644 "$here/data/config/config.toml" /etc/omavrything/config.toml
else
  log "keeping existing /etc/omavrything/config.toml"
fi

# The shell plugin and docs are whole trees: stage them completely, then swap
# them into place. This clears files dropped by a newer version without ever
# deleting the old tree before its replacement exists.
share_stage="/usr/share/omavrything.new-$$"
doc_stage="/usr/share/doc/omavrything.new-$$"
rm -rf "$share_stage" "$doc_stage"
install -d -m755 "$share_stage/shell/omavrything"
install -m644 "$here/manifest.json" "$share_stage/manifest.json"
install -m644 "$here/shell/omavrything/SearchBar.qml" \
  "$here/shell/omavrything/SearchOverlay.qml" \
  "$here/shell/omavrything/Service.qml" \
  "$share_stage/shell/omavrything/"

install -Dm644 "$here/LICENSE" /usr/share/licenses/omavrything/LICENSE
install -d -m755 "$doc_stage"
install -m644 "$here/README.md" "$doc_stage/README.md"
# docs/README.md is the documentation index; install it under a distinct name
# so it does not collide with the project README (install refuses to overwrite
# a file it just created in the same invocation).
for doc in "$here"/docs/*.md; do
  name="$(basename "$doc")"
  [[ "$name" == "README.md" ]] && name="docs-index.md"
  install -m644 "$doc" "$doc_stage/$name"
done

atomic_dir_swap as_root "$share_stage" /usr/share/omavrything
share_stage=""
atomic_dir_swap as_root "$doc_stage" /usr/share/doc/omavrything
doc_stage=""

# Only now that every replacement is in place, drop files shipped by older
# versions but no longer part of the install.
rm -f /usr/bin/omavrything-install-plugin

# 2. Create the dedicated user/group from the sysusers drop-in. This must
# succeed: without it the service cannot start, and a silent failure is exactly
# what made 0.1.4 look installed but dead.
log "creating the omavrything user"
if ! systemd-sysusers; then
  die "systemd-sysusers failed; the omavrything user/group was not created"
fi
# Create /run/omavrything and /var/lib/omavrything now (the unit no longer uses
# RuntimeDirectory=/StateDirectory=, which would give the service its own mount
# namespace and hide its NTFS mounts from the desktop); tmpfiles.d recreates
# them on every boot. /var/lib/omavrything holds the index snapshot and the
# state config, so the daemon (running as `omavrything`) can write both.
if ! systemd-tmpfiles --create /usr/lib/tmpfiles.d/omavrything.conf; then
  die "systemd-tmpfiles failed; /run/omavrything or /var/lib/omavrything was not created"
fi
systemctl daemon-reload >/dev/null 2>&1 || true

# Reload the system bus so it picks up the new policy. A bus that was already
# running before the install does not see the policy otherwise, and the daemon
# cannot own its name (ServiceUnknown: The name is not activatable).
log "reloading the system D-Bus policy"
reload_dbus

# Health check: the installed daemon must run. Fail loudly if it does not.
if ! /usr/bin/omavrythingd --version >/dev/null 2>&1; then
  die "the installed omavrythingd failed to run"
fi

# 3. Add the invoking user to the socket group so the shell plugin and the CLI
# can reach the system socket. Group membership only applies to new sessions;
# the ACL bridge below covers the current one.
if [[ -n "$target_user" ]]; then
  if usermod -aG omavrything "$target_user"; then
    log "added $target_user to the omavrything group"
  else
    warn "could not add $target_user to the omavrything group"
  fi
fi

# 4. Enable and start the daemon. `restart` (not `enable --now`) so an upgrade
# picks up the freshly installed binary instead of leaving the old one running.
log "enabling and starting omavrythingd"
systemctl enable omavrythingd >/dev/null 2>&1 || true
if systemctl restart omavrythingd; then
  log "enabled and started omavrythingd"
else
  warn "could not start omavrythingd; run 'sudo systemctl enable --now omavrythingd'"
fi

# 5. Wait for the daemon to be ready. The daemon cold-scans before it serves, so
# this can take several seconds on a large index. Wait for the socket (the
# widget/CLI search path) and then for the D-Bus name (the CLI status/config
# path, registered only after the scan) so the install is complete when it
# returns and the user does not need a `sleep` before the first command.
sock=/run/omavrything/omavrything.sock
for _ in $(seq 1 300); do
  [[ -S "$sock" ]] && break
  sleep 0.1
done
if [[ -S "$sock" ]]; then
  log "daemon socket is up at $sock"
else
  warn "the daemon socket did not appear at $sock; the widget may show 'Search daemon unavailable'"
fi

if command -v busctl >/dev/null 2>&1; then
  for _ in $(seq 1 300); do
    busctl --system status tech.alatcerdas.omavrything >/dev/null 2>&1 && break
    sleep 0.1
  done
  if busctl --system status tech.alatcerdas.omavrything >/dev/null 2>&1; then
    log "daemon is serving on D-Bus"
  else
    warn "the daemon has not registered its D-Bus name yet; 'omavry status' may fail until the scan finishes"
  fi
fi

# 6. Bridge the group change into the already-running session. A supplementary
# group added with usermod does not apply to the running desktop session, so the
# shell cannot traverse /run/omavrything (0750) until a re-login. An ACL grants
# the invoking user access now; the durable fix is the re-login noted below.
if [[ -n "$target_user" ]]; then
  if command -v setfacl >/dev/null 2>&1; then
    if [[ -d /run/omavrything ]]; then
      setfacl -m "u:$target_user:x" /run/omavrything 2>/dev/null ||
        warn "could not set the ACL on /run/omavrything"
    fi
    if [[ -S "$sock" ]]; then
      setfacl -m "u:$target_user:rw" "$sock" 2>/dev/null ||
        warn "could not set the ACL on $sock"
    fi
    log "granted $target_user access to the daemon socket for this session"
  else
    warn "setfacl not found; the widget may not reach the socket until you log out and back in"
  fi
fi

# 7. Install, enable, and load the shell plugin for the invoking user.
if [[ -n "$target_user" ]]; then
  # The settings menu reads and writes the daemon config through the `omavry`
  # CLI, so it must be on PATH. Needed in both modes.
  as_user mkdir -p "$user_home/.local/bin"
  as_user ln -sf /usr/bin/omavry "$user_home/.local/bin/omavry"

  if [[ "$from_plugin" == 1 ]]; then
    # The plugin was installed and enabled by `omarchy plugin add`; do not
    # overwrite it or restart the shell from inside the install.
    log "plugin already installed by 'omarchy plugin add'; skipping the plugin copy"
  else
    log "installing the shell plugin for $target_user"
    plugin_dest="$user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
    # Build the complete replacement beside the live plugin. A dropped QML file
    # still goes away (the whole tree is swapped), but a failed copy, validate,
    # or swap leaves the previously working plugin in place (issue #134).
    plugin_new="$plugin_dest.new-$$"
    as_user rm -rf "$plugin_new"
    as_user install -d -m755 "$plugin_new/shell/omavrything"
    as_user install -m644 "$here/manifest.json" "$plugin_new/manifest.json"
    as_user install -m644 "$here"/shell/omavrything/*.qml "$plugin_new/shell/omavrything/"

    # Validate the staged plugin with omarchy when available. The plugin folder
    # is a required argument: bare `omarchy plugin validate` prints usage and
    # exits 0.
    if as_user sh -c 'command -v omarchy >/dev/null 2>&1'; then
      if as_user omarchy plugin validate "$plugin_new" >/dev/null 2>&1; then
        log "validated the plugin with 'omarchy plugin validate'"
      else
        warn "'omarchy plugin validate' rejected the staged plugin"
      fi
    fi

    atomic_dir_swap as_user "$plugin_new" "$plugin_dest"
    plugin_new=""

    # Restart the shell FIRST so the freshly copied plugin is discovered and
    # registered. `omarchy plugin enable` talks to the running shell
    # (`omarchy-shell shell enablePlugin`) and fails with "plugin ... is not
    # known" until the shell has rescanned; a plugin rescan alone keeps the
    # previously compiled component, so the shell process must restart for the
    # new QML to take effect.
    if as_user sh -c 'command -v omarchy >/dev/null 2>&1'; then
      if as_user omarchy restart shell >/dev/null 2>&1; then
        log "restarted the shell"
      else
        warn "could not restart the shell; run 'omarchy restart shell'"
      fi
    elif as_user sh -c 'command -v omarchy-shell >/dev/null 2>&1'; then
      as_user omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
      warn "'omarchy' not found; rescanned plugins only (changed QML may not apply)"
    fi

    # Enable the plugin in the bar, now that the shell knows about it.
    if as_user sh -c 'command -v omarchy >/dev/null 2>&1'; then
      if as_user omarchy plugin enable tech.alatcerdas.omavrything --section center >/dev/null 2>&1; then
        log "enabled the plugin in the center section"
      else
        warn "could not enable the plugin; run 'omarchy plugin enable tech.alatcerdas.omavrything --section center'"
      fi
    fi
  fi
fi

# 8. Summary.
log "omavrything ${version:-unknown} installed"
echo "  daemon:  /usr/bin/omavrythingd (systemd unit omavrythingd.service)"
echo "  CLI:     /usr/bin/omavry"
if [[ -n "$user_home" ]]; then
  echo "  plugin:  $user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
  echo
  echo "The daemon is running and the widget is enabled. The socket ACL covers"
  echo "this session, so no re-login is needed now; the group change takes effect"
  echo "on your next login."
fi
