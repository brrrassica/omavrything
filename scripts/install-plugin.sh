#!/usr/bin/env bash
# Install the omavrything omarchy-shell plugin for the current user.
#
# Copies shell/omavrything/ into ~/.config/omarchy/plugins/omavrything/, links
# the `omavry` CLI onto PATH (the settings menu shells out to it), and restarts
# the shell.
#
# A full shell restart is required, not just a plugin rescan: `omarchy-shell
# shell rescanPlugins` reloads the plugin but the QML engine keeps the
# previously compiled component, so changed QML does not take effect until the
# shell process restarts.
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

# The settings menu reads and writes the daemon config through the `omavry` CLI,
# so it must be on PATH. Link the release binary into ~/.local/bin for dev use.
cli="$here/target/release/omavry"
if [[ -x "$cli" ]]; then
  bindir="${XDG_BIN_HOME:-$HOME/.local/bin}"
  mkdir -p "$bindir"
  ln -sf "$cli" "$bindir/omavry"
  echo "Linked $bindir/omavry -> $cli"
else
  echo "warning: $cli not found; build it with 'cargo build --release -p omavrything-cli'" >&2
fi

# Restart the shell so the new QML is compiled and loaded.
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
