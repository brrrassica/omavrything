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

if [[ $# -gt 0 ]]; then
  echo "error: install.sh takes no options (got: $*)" >&2
  echo "usage: sudo scripts/install.sh" >&2
  exit 2
fi

if [[ "$(id -u)" -ne 0 ]]; then
  echo "error: install.sh must be run as root (use sudo)" >&2
  exit 1
fi

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die() {
  printf '\033[1;31m==>\033[0m %s\n' "$*" >&2
  exit 1
}

# Locate the release binaries. They live in bin/ at the top level, the same
# layout the release archive ships; a source checkout populates it with
# scripts/build.sh.
bindir="$here/bin"
if [[ ! -x "$bindir/omavrythingd" || ! -x "$bindir/omavry" ]]; then
  die "omavrythingd/omavry not found in bin/; run scripts/build.sh first"
fi

# Remove any previous installation first so an upgrade leaves nothing stale
# behind (e.g. the 0.1.4 `omavrything-install-plugin` helper, or QML files that
# a newer version dropped). This mirrors the uninstaller's file list but keeps
# the config, the dedicated user, and the cached index, which an upgrade
# preserves. The binaries are located above, so a missing build fails before
# anything is removed.
log "removing any previous installation"
rm -f /usr/bin/omavrythingd /usr/bin/omavry /usr/bin/omavrything-install-plugin
rm -f /usr/lib/systemd/system/omavrythingd.service
rm -f /usr/lib/sysusers.d/omavrything.conf
rm -f /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf
rm -f /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy
rm -f /usr/share/man/man8/omavrythingd.8
rm -rf /usr/share/omavrything /usr/share/licenses/omavrything /usr/share/doc/omavrything

# The version reported by the manifest, used for the install summary.
version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$here/manifest.json" 2>/dev/null | head -n1)"

# The user the shell plugin is installed for. Under sudo this is the real
# invoker; running as root directly leaves it empty and the per-user steps are
# skipped with a warning.
target_user="${SUDO_USER:-}"
if [[ -z "$target_user" || "$target_user" == "root" ]]; then
  warn "could not determine a non-root invoking user; run with sudo so the shell plugin can be installed"
  target_user=""
fi

user_home=""
user_uid=""
if [[ -n "$target_user" ]]; then
  user_home="$(getent passwd "$target_user" | cut -d: -f6 || true)"
  user_uid="$(id -u "$target_user" 2>/dev/null || true)"
fi

# Run a command as the invoking user with a usable session environment. The
# per-user plugin, `omarchy plugin enable`, and the shell restart must run in
# the user's session (their HOME, runtime dir, and session bus), not root's.
#
# OMARCHY_PATH is required by the `omarchy` dispatcher and its subcommands
# (e.g. `omarchy plugin enable` fails with "OMARCHY_PATH is not set" without
# it); the user's login shell exports it, but `runuser` does not inherit it.
as_user() {
  [[ -n "$target_user" ]] || return 1
  runuser -u "$target_user" -- env \
    HOME="$user_home" \
    USER="$target_user" \
    LOGNAME="$target_user" \
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

# The shell plugin, installed system-wide; the per-user copy is made below.
install -d /usr/share/omavrything/shell/omavrything
install -m644 "$here/manifest.json" /usr/share/omavrything/manifest.json
install -m644 "$here/shell/omavrything/SearchBar.qml" \
  "$here/shell/omavrything/SearchOverlay.qml" \
  "$here/shell/omavrything/Service.qml" \
  /usr/share/omavrything/shell/omavrything/

install -Dm644 "$here/LICENSE" /usr/share/licenses/omavrything/LICENSE
install -d /usr/share/doc/omavrything
install -m644 "$here/README.md" /usr/share/doc/omavrything/README.md
# docs/README.md is the documentation index; install it under a distinct name
# so it does not collide with the project README (install refuses to overwrite
# a file it just created in the same invocation).
for doc in "$here"/docs/*.md; do
  name="$(basename "$doc")"
  [[ "$name" == "README.md" ]] && name="docs-index.md"
  install -m644 "$doc" "/usr/share/doc/omavrything/$name"
done

# 2. Create the dedicated user/group from the sysusers drop-in. This must
# succeed: without it the service cannot start, and a silent failure is exactly
# what made 0.1.4 look installed but dead.
log "creating the omavrything user"
if ! systemd-sysusers; then
  die "systemd-sysusers failed; the omavrything user/group was not created"
fi
# Create /run/omavrything now (the unit no longer uses RuntimeDirectory=, which
# would give the service its own mount namespace and hide its NTFS mounts from
# the desktop); tmpfiles.d recreates it on every boot.
if ! systemd-tmpfiles --create /usr/lib/tmpfiles.d/omavrything.conf; then
  die "systemd-tmpfiles failed; /run/omavrything was not created"
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
  log "installing the shell plugin for $target_user"
  plugin_dest="$user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
  # Replace the plugin tree wholesale so a newer version's dropped files do not
  # linger next to the freshly installed ones.
  as_user rm -rf "$plugin_dest"
  as_user mkdir -p "$plugin_dest/shell/omavrything"
  as_user install -m644 "$here/manifest.json" "$plugin_dest/manifest.json"
  as_user install -m644 "$here"/shell/omavrything/*.qml "$plugin_dest/shell/omavrything/"

  # Validate the plugin with omarchy when available. The plugin folder is a
  # required argument: bare `omarchy plugin validate` prints usage and exits 0.
  if as_user sh -c 'command -v omarchy >/dev/null 2>&1'; then
    if as_user omarchy plugin validate "$plugin_dest" >/dev/null 2>&1; then
      log "validated the plugin with 'omarchy plugin validate'"
    else
      warn "'omarchy plugin validate' rejected the installed plugin"
    fi
  fi

  # The settings menu reads and writes the daemon config through the `omavry`
  # CLI, so it must be on PATH.
  as_user mkdir -p "$user_home/.local/bin"
  as_user ln -sf /usr/bin/omavry "$user_home/.local/bin/omavry"

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

# 8. Summary.
log "omavrything ${version:-unknown} installed"
echo "  daemon:  /usr/bin/omavrythingd (systemd unit omavrythingd.service)"
echo "  CLI:     /usr/bin/omavry"
if [[ -n "$target_user" ]]; then
  echo "  plugin:  $user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
  echo
  echo "The daemon is running and the widget is enabled. The socket ACL covers"
  echo "this session, so no re-login is needed now; the group change takes effect"
  echo "on your next login."
fi
