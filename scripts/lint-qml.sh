#!/usr/bin/env bash
# Lint the omarchy-shell plugin QML with qmllint.
#
# qmllint ships in /usr/lib/qt6/bin (qt6-declarative), which is not always on
# PATH, so this script adds it. The shell's `qs.*` modules live under
# $OMARCHY_PATH/shell, so a temporary `qs` symlink is created to satisfy
# `import qs.Ui` / `import qs.Commons`.
#
# qmllint cannot resolve the Quickshell singletons (`Color`, `Style`) or the
# `PanelWindow` type, so a handful of warnings are expected and harmless.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

# Make qmllint available (qt6-declarative installs it outside PATH).
if ! command -v qmllint >/dev/null 2>&1; then
  for dir in /usr/lib/qt6/bin /usr/lib64/qt6/bin; do
    if [[ -x "$dir/qmllint" ]]; then
      PATH="$dir:$PATH"
      break
    fi
  done
fi
if ! command -v qmllint >/dev/null 2>&1; then
  echo "error: qmllint not found (install qt6-declarative)" >&2
  exit 1
fi

# Resolve `import qs.*` to the shell's modules. On a machine without the omarchy
# shell (for example a CI container) the modules are absent; qmllint then reports
# the unresolved `qs.*` imports as warnings, which is expected and harmless.
shell_dir="${OMARCHY_PATH:-/usr/share/omarchy}/shell"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if [[ -d "$shell_dir" ]]; then
  ln -sfn "$shell_dir" "$tmp/qs"
else
  echo "note: $shell_dir not found; qs.* imports will not resolve" >&2
fi

qmllint -I "$tmp" -I /usr/lib/qt6/qml shell/omavrything/*.qml
