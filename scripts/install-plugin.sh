#!/usr/bin/env bash
# Install the omavrything omarchy-shell plugin for the current user.
#
# Copies shell/omavrything/ into ~/.config/omarchy/plugins/omavrything/ and asks
# the running shell to rescan. Enabling the plugin (placing the bar widget and
# loading the service) is a separate step; see docs/omarchy-shell-integration.md.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$here/shell/omavrything"
dest="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/omavrything"

if [[ ! -f "$src/manifest.json" ]]; then
  echo "error: plugin source not found at $src" >&2
  exit 1
fi

mkdir -p "$dest"
cp -f "$src"/*.qml "$src"/manifest.json "$dest"/
echo "Installed plugin to $dest"

if command -v omarchy-shell >/dev/null 2>&1; then
  if omarchy-shell shell rescanPlugins >/dev/null 2>&1; then
    echo "Rescanned plugins."
  else
    echo "warning: could not rescan plugins; run 'omarchy restart shell' if needed" >&2
  fi
fi
