#!/usr/bin/env bash
# Install omavrything from a release archive or a source checkout.
#
# Usage:
#   sudo scripts/install.sh --system [--no-start]   # daemon, CLI, systemd unit, D-Bus, polkit
#        scripts/install.sh --user                  # shell plugin + CLI for the current user
#
# --system installs the system integration, creates the omavrything user, adds the
# invoking user to the omavrything group, reloads D-Bus, and enables+starts the
# daemon. Pass --no-start to skip enabling/starting the daemon.
#
# The script auto-detects the layout: a release archive has bin/ and data/ at the
# top level, while a source checkout has target/release/ and the same data/ tree.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

mode="system"
no_start=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --system) mode="system" ;;
    --user) mode="user" ;;
    --no-start) no_start=1 ;;
    -h|--help)
      sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "error: unknown option: $1" >&2
      exit 2
      ;;
  esac
  shift
done

# Locate the built binaries: release archive first, then a source checkout.
if [[ -x "$here/bin/omavrythingd" ]]; then
  bindir="$here/bin"
elif [[ -x "$here/target/release/omavrythingd" ]]; then
  bindir="$here/target/release"
else
  echo "error: omavrythingd not found; build it with 'cargo build --release'" >&2
  exit 1
fi

# The version reported by the manifest, used for the install summary.
version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$here/manifest.json" 2>/dev/null | head -n1)"

install_user() {
  # The destination mirrors the repository layout (root manifest.json plus
  # shell/omavrything/), which is exactly what `omarchy plugin add` produces.
  local plugin_src="$here"
  local dest="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/tech.alatcerdas.omavrything"
  mkdir -p "$dest/shell/omavrything"
  install -m644 "$plugin_src/manifest.json" "$dest/manifest.json"
  install -m644 "$plugin_src"/shell/omavrything/*.qml "$dest/shell/omavrything/"
  echo "Installed shell plugin to $dest"

  # Validate the plugin with omarchy when available; fail loudly on a bad plugin.
  if command -v omarchy >/dev/null 2>&1; then
    if ! (cd "$dest" && omarchy plugin validate >/dev/null 2>&1); then
      echo "error: 'omarchy plugin validate' rejected the installed plugin" >&2
      exit 1
    fi
    echo "Validated the plugin with 'omarchy plugin validate'."
  fi

  local user_bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
  mkdir -p "$user_bin"
  ln -sf "$bindir/omavry" "$user_bin/omavry"
  echo "Linked $user_bin/omavry -> $bindir/omavry"

  # Restart the shell so the new QML is compiled and loaded, matching
  # install-plugin.sh. A plugin rescan alone keeps the previously compiled
  # component, so changed QML would not take effect until the shell restarts.
  if command -v omarchy >/dev/null 2>&1; then
    if omarchy restart shell >/dev/null 2>&1; then
      echo "Restarted the shell."
    else
      echo "warning: could not restart the shell; run 'omarchy restart shell'" >&2
    fi
  elif command -v omarchy-shell >/dev/null 2>&1; then
    omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
    echo "warning: 'omarchy' not found; rescanned plugins only (changed QML may not apply)" >&2
  fi

  echo "Enable the plugin with: omarchy plugin enable tech.alatcerdas.omavrything --section center"
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

install_system() {
  if [[ "$(id -u)" -ne 0 ]]; then
    echo "error: --system requires root (use sudo)" >&2
    exit 1
  fi

  # Clear stale files from a previous version so an upgrade leaves nothing
  # behind; the plugin tree is replaced wholesale below.
  rm -rf /usr/share/omavrything

  install -Dm755 "$bindir/omavrythingd" /usr/bin/omavrythingd
  install -Dm755 "$bindir/omavry" /usr/bin/omavry

  install -Dm644 "$here/data/systemd/omavrythingd.service" \
    /usr/lib/systemd/system/omavrythingd.service
  install -Dm644 "$here/data/sysusers/omavrything.conf" \
    /usr/lib/sysusers.d/omavrything.conf
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
    echo "Keeping existing /etc/omavrything/config.toml"
  fi

  # The shell plugin, installed system-wide; the helper copies it per user.
  install -d /usr/share/omavrything/shell/omavrything
  install -m644 "$here/manifest.json" /usr/share/omavrything/manifest.json
  install -m644 "$here/shell/omavrything/SearchBar.qml" \
    "$here/shell/omavrything/SearchOverlay.qml" \
    "$here/shell/omavrything/Service.qml" \
    /usr/share/omavrything/shell/omavrything/
  install -Dm755 "$here/scripts/install-plugin.sh" /usr/bin/omavrything-install-plugin

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

  # Create the dedicated user/group from the sysusers drop-in. This must succeed:
  # without it the service cannot start, and a silent failure is exactly what made
  # 0.1.4 look installed but dead.
  if ! systemd-sysusers; then
    echo "error: systemd-sysusers failed; the omavrything user/group was not created" >&2
    exit 1
  fi
  systemctl daemon-reload >/dev/null 2>&1 || true

  # Reload the system bus so it picks up the new policy. A bus that was already
  # running before the install does not see the policy otherwise, and the daemon
  # cannot own its name (ServiceUnknown: The name is not activatable).
  reload_dbus

  # Health check: the installed daemon must run. Fail loudly if it does not.
  if ! /usr/bin/omavrythingd --version >/dev/null 2>&1; then
    echo "error: the installed omavrythingd failed to run" >&2
    exit 1
  fi

  # Add the invoking user to the socket group so the shell plugin and the CLI can
  # reach the system socket. Group membership only applies to new sessions.
  local target_user="${SUDO_USER:-}"
  if [[ -n "$target_user" && "$target_user" != "root" ]]; then
    if usermod -aG omavrything "$target_user"; then
      echo "Added $target_user to the omavrything group."
    else
      echo "warning: could not add $target_user to the omavrything group" >&2
    fi
  fi

  echo "Installed omavrything ${version:-unknown} system-wide."

  if [[ "$no_start" == 1 ]]; then
    echo "Skipped starting the daemon (--no-start)."
    echo "Start it with: sudo systemctl enable --now omavrythingd"
  elif systemctl enable --now omavrythingd; then
    echo "Enabled and started omavrythingd."
  else
    echo "warning: could not enable/start omavrythingd; run 'sudo systemctl enable --now omavrythingd'" >&2
  fi

  if [[ -n "$target_user" && "$target_user" != "root" ]]; then
    echo "Log out and back in (or run 'newgrp omavrything') for the group change to take effect."
  fi
}

case "$mode" in
  user) install_user ;;
  system) install_system ;;
esac
