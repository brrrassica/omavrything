#!/usr/bin/env bash
# Install the omavrything omarchy-shell plugin for the current user.
#
# Copies the plugin (root manifest.json plus shell/omavrything/) into
# ~/.config/omarchy/plugins/tech.alatcerdas.omavrything/, links the `omavry` CLI
# onto PATH (the settings menu shells out to it), and restarts the shell.
#
# The destination mirrors this repository's layout exactly, so a local install
# is byte-for-byte the same shape as `omarchy plugin add` produces from the git
# URL. That keeps a single manifest.json authoritative for both paths.
#
# A full shell restart is required, not just a plugin rescan: `omarchy-shell
# shell rescanPlugins` reloads the plugin but the QML engine keeps the
# previously compiled component, so changed QML does not take effect until the
# shell process restarts.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$here"
# A packaged install places this helper in /usr/bin and the plugin in
# /usr/share/omavrything, so fall back to that layout.
if [[ ! -f "$src/manifest.json" && -f /usr/share/omavrything/manifest.json ]]; then
  src=/usr/share/omavrything
fi
dest="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/tech.alatcerdas.omavrything"

if [[ ! -f "$src/manifest.json" ]]; then
  echo "error: plugin manifest not found at $src/manifest.json" >&2
  exit 1
fi

mkdir -p "$dest/shell/omavrything"
install -m644 "$src/manifest.json" "$dest/manifest.json"
install -m644 "$src"/shell/omavrything/*.qml "$dest/shell/omavrything/"
echo "Installed plugin to $dest"

# The settings menu reads and writes the daemon config through the `omavry` CLI,
# so it must be on PATH. Link the release binary into ~/.local/bin for dev use;
# a packaged install already provides `omavry` on PATH, so use that instead.
cli="$here/target/release/omavry"
if [[ -x "$cli" ]]; then
  bindir="${XDG_BIN_HOME:-$HOME/.local/bin}"
  mkdir -p "$bindir"
  ln -sf "$cli" "$bindir/omavry"
  echo "Linked $bindir/omavry -> $cli"
elif command -v omavry >/dev/null 2>&1; then
  echo "Using omavry from PATH: $(command -v omavry)"
else
  echo "warning: omavry not found; build it with 'cargo build --release -p omavrything-cli'" >&2
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
