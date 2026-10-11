#!/usr/bin/env bash
# Acceptance checks for M13 — install/uninstall completeness.
#
# Runs unprivileged: it exercises the user-mode paths in a throwaway HOME and
# checks the release archive contents. The system-mode checks (root) are
# documented in the milestone and exercised by the CI smoke job (M14/C10).
#
# Usage: scripts/test-install-uninstall.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

pass=0
fail=0
ok() { printf 'ok   - %s\n' "$*"; pass=$((pass + 1)); }
bad() {
  printf 'FAIL - %s\n' "$*" >&2
  fail=$((fail + 1))
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# A fake `omarchy` that records the subcommands it is asked to run.
fake_bin="$tmp/bin"
mkdir -p "$fake_bin"
cat > "$fake_bin/omarchy" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${OMARCHY_LOG:?}"
exit 0
EOF
chmod +x "$fake_bin/omarchy"

# A throwaway package layout (bin/ + manifest.json + shell/ + scripts/) so the
# scripts see a release-archive shape without touching the real checkout.
pkg="$tmp/pkg"
mkdir -p "$pkg/bin" "$pkg/shell/omavrything" "$pkg/scripts"
printf '#!/bin/sh\nexit 0\n' > "$pkg/bin/omavrythingd"
printf '#!/bin/sh\nexit 0\n' > "$pkg/bin/omavry"
chmod +x "$pkg/bin/omavrythingd" "$pkg/bin/omavry"
cp manifest.json "$pkg/manifest.json"
cp shell/omavrything/*.qml "$pkg/shell/omavrything/"
cp scripts/install.sh scripts/uninstall.sh "$pkg/scripts/"

# --- C2: install.sh --user restarts the shell -------------------------------
home="$tmp/home-c2"
mkdir -p "$home"
log="$tmp/omarchy-c2.log"
: > "$log"
if HOME="$home" XDG_CONFIG_HOME="$home/.config" XDG_BIN_HOME="$home/.local/bin" \
  OMARCHY_LOG="$log" PATH="$fake_bin:$PATH" \
  "$pkg/scripts/install.sh" --user >/dev/null 2>&1; then
  if grep -q 'restart shell' "$log"; then
    ok "install.sh --user restarts the shell"
  else
    bad "install.sh --user did not restart the shell"
  fi
  if [[ -f "$home/.config/omarchy/plugins/tech.alatcerdas.omavrything/manifest.json" ]]; then
    ok "install.sh --user installs the plugin"
  else
    bad "install.sh --user did not install the plugin"
  fi
else
  bad "install.sh --user failed"
fi

# --- C3: uninstall.sh --user needs no root ----------------------------------
home="$tmp/home-c3"
plugin="$home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
mkdir -p "$plugin/shell/omavrything" "$home/.local/bin"
cp manifest.json "$plugin/manifest.json"
ln -sf /nonexistent "$home/.local/bin/omavry"
log="$tmp/omarchy-c3.log"
: > "$log"
if HOME="$home" XDG_CONFIG_HOME="$home/.config" XDG_BIN_HOME="$home/.local/bin" \
  OMARCHY_LOG="$log" PATH="$fake_bin:$PATH" \
  "$pkg/scripts/uninstall.sh" --user >/dev/null 2>&1; then
  if [[ ! -e "$plugin" ]]; then
    ok "uninstall.sh --user removes the plugin without root"
  else
    bad "uninstall.sh --user left the plugin directory behind"
  fi
  if [[ ! -e "$home/.local/bin/omavry" ]]; then
    ok "uninstall.sh --user removes the CLI symlink"
  else
    bad "uninstall.sh --user left the CLI symlink behind"
  fi
else
  bad "uninstall.sh --user failed (it must not require root)"
fi

# --- C4: uninstall.sh --keep-state is recognised ----------------------------
# As a non-root user the system mode must refuse with the root message, not an
# "unknown option" error, which proves --keep-state is parsed.
err="$tmp/keep-state.err"
if "$pkg/scripts/uninstall.sh" --keep-state >/dev/null 2>"$err"; then
  bad "uninstall.sh --keep-state unexpectedly succeeded as non-root"
else
  if grep -q 'must be run as root' "$err" && ! grep -q 'unknown option' "$err"; then
    ok "uninstall.sh --keep-state is recognised"
  else
    bad "uninstall.sh --keep-state was not recognised: $(cat "$err")"
  fi
fi
if grep -q 'keep_state' scripts/uninstall.sh &&
  grep -q '/var/lib/omavrything' scripts/uninstall.sh; then
  ok "uninstall.sh guards /var/lib/omavrything behind --keep-state"
else
  bad "uninstall.sh does not guard /var/lib/omavrything behind --keep-state"
fi

# --- C1: the release archive ships uninstall.sh -----------------------------
if [[ -x target/release/omavrythingd && -x target/release/omavry ]]; then
  if scripts/package-release.sh --no-build >/dev/null 2>&1; then
    archive="$(ls -1 dist/omavrything-*.zip 2>/dev/null | tail -n1 || true)"
    if [[ -n "$archive" ]] && unzip -l "$archive" | grep -q 'scripts/uninstall.sh'; then
      ok "the release archive contains scripts/uninstall.sh"
    else
      bad "the release archive does not contain scripts/uninstall.sh"
    fi
  else
    bad "scripts/package-release.sh --no-build failed"
  fi
else
  printf 'skip - C1 archive check (build target/release first)\n'
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]