#!/usr/bin/env bash
# Install omavrything from a release archive or a source checkout.
#
# Usage:
#   sudo scripts/install.sh --system   # daemon, CLI, systemd unit, D-Bus, polkit
#        scripts/install.sh --user     # shell plugin + CLI for the current user
#
# The script auto-detects the layout: a release archive has bin/ and data/ at the
# top level, while a source checkout has target/release/ and the same data/ tree.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

mode="system"
case "${1:-}" in
  --system) mode="system" ;;
  --user) mode="user" ;;
  "" ) mode="system" ;;
  -h|--help)
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    echo "error: unknown option: $1" >&2
    exit 2
    ;;
esac

# Locate the built binaries: release archive first, then a source checkout.
if [[ -x "$here/bin/omavrythingd" ]]; then
  bindir="$here/bin"
elif [[ -x "$here/target/release/omavrythingd" ]]; then
  bindir="$here/target/release"
else
  echo "error: omavrythingd not found; build it with 'cargo build --release'" >&2
  exit 1
fi

install_user() {
  local plugin_src="$here/shell/omavrything"
  local dest="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/omavrything"
  mkdir -p "$dest"
  cp -f "$plugin_src"/*.qml "$plugin_src"/manifest.json "$dest"/
  echo "Installed shell plugin to $dest"

  local user_bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
  mkdir -p "$user_bin"
  ln -sf "$bindir/omavry" "$user_bin/omavry"
  echo "Linked $user_bin/omavry -> $bindir/omavry"
  echo "Enable the plugin with: omarchy plugin enable omavrything --section center"
}

install_system() {
  if [[ "$(id -u)" -ne 0 ]]; then
    echo "error: --system requires root (use sudo)" >&2
    exit 1
  fi

  install -Dm755 "$bindir/omavrythingd" /usr/bin/omavrythingd
  install -Dm755 "$bindir/omavry" /usr/bin/omavry

  install -Dm644 "$here/data/systemd/omavrythingd.service" \
    /usr/lib/systemd/system/omavrythingd.service
  install -Dm644 "$here/data/sysusers/omavrything.conf" \
    /usr/lib/sysusers.d/omavrything.conf
  install -Dm644 "$here/data/dbus/org.omarchy.omavrything.conf" \
    /usr/share/dbus-1/system.d/org.omarchy.omavrything.conf
  install -Dm644 "$here/data/polkit/org.omarchy.omavrything.policy" \
    /usr/share/polkit-1/actions/org.omarchy.omavrything.policy

  # Do not clobber an existing config on upgrade.
  if [[ ! -e /etc/omavrything/config.toml ]]; then
    install -Dm644 "$here/data/config/config.toml" /etc/omavrything/config.toml
  else
    echo "Keeping existing /etc/omavrything/config.toml"
  fi

  # The shell plugin, installed system-wide; the helper copies it per user.
  install -d /usr/share/omavrything/shell/omavrything
  install -m644 "$here/shell/omavrything/manifest.json" \
    "$here/shell/omavrything/SearchBar.qml" \
    "$here/shell/omavrything/SearchOverlay.qml" \
    "$here/shell/omavrything/Service.qml" \
    /usr/share/omavrything/shell/omavrything/
  install -Dm755 "$here/scripts/install-plugin.sh" /usr/bin/omavrything-install-plugin

  install -Dm644 "$here/LICENSE" /usr/share/licenses/omavrything/LICENSE
  install -d /usr/share/doc/omavrything
  install -m644 "$here/README.md" "$here"/docs/*.md /usr/share/doc/omavrything/

  systemd-sysusers >/dev/null 2>&1 || true
  systemctl daemon-reload >/dev/null 2>&1 || true

  echo "Installed omavrything system-wide."
  echo "Enable the daemon with: sudo systemctl enable --now omavrythingd"
  echo "Add your user to the socket group with: sudo usermod -aG omavrything \"\$USER\""
}

case "$mode" in
  user) install_user ;;
  system) install_system ;;
esac
