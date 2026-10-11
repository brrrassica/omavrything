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

# --- C9: install/uninstall are idempotent -----------------------------------
home="$tmp/home-c9"
mkdir -p "$home"
run_user() {
  HOME="$home" XDG_CONFIG_HOME="$home/.config" XDG_BIN_HOME="$home/.local/bin" \
    OMARCHY_LOG="$tmp/omarchy-c9.log" PATH="$fake_bin:$PATH" "$@"
}
if run_user "$pkg/scripts/install.sh" --user >/dev/null 2>&1 &&
  run_user "$pkg/scripts/install.sh" --user >/dev/null 2>&1; then
  ok "install.sh --user is idempotent"
else
  bad "install.sh --user is not idempotent"
fi
if run_user "$pkg/scripts/uninstall.sh" --user >/dev/null 2>&1 &&
  run_user "$pkg/scripts/uninstall.sh" --user >/dev/null 2>&1; then
  ok "uninstall.sh --user is idempotent"
else
  bad "uninstall.sh --user is not idempotent"
fi

# --- C11: doctor.sh prints a report and flags a missing install -------------
out="$("$here/scripts/doctor.sh" 2>&1 || true)"
if printf '%s' "$out" | grep -q 'omavrything doctor'; then
  ok "doctor.sh prints a report"
else
  bad "doctor.sh did not print a report"
fi
if printf '%s' "$out" | grep -q '\[fail\]'; then
  ok "doctor.sh flags a missing install"
else
  bad "doctor.sh did not flag the missing install"
fi

# --- C1/C6/C11: the release archive ships the expected files ----------------
if [[ -x target/release/omavrythingd && -x target/release/omavry ]]; then
  if scripts/package-release.sh --no-build >/dev/null 2>&1; then
    archive="$(ls -1 dist/omavrything-*.zip 2>/dev/null | tail -n1 || true)"
    # Capture the listing first: `unzip -l | grep -q` would SIGPIPE unzip when
    # grep exits on the first match, which `set -o pipefail` turns into a failure.
    listing="$(unzip -l "$archive" 2>/dev/null || true)"
    for entry in scripts/uninstall.sh data/man/omavrythingd.8 scripts/doctor.sh; do
      if [[ -n "$listing" ]] && grep -qF "$entry" <<<"$listing"; then
        ok "the release archive contains $entry"
      else
        bad "the release archive does not contain $entry"
      fi
    done
  else
    bad "scripts/package-release.sh --no-build failed"
  fi
else
  printf 'skip - archive check (build target/release first)\n'
fi

# --- C12: the overlay QML targets the system socket --------------------------
# Regression guard (2026-10-12): the widget used to hardcode
# $XDG_RUNTIME_DIR/omavrything.sock, which the system daemon never creates, so
# the overlay always showed "Search daemon unavailable". The QML must select
# the system socket the same way the CLI's connect_default() does, and must
# resolve the path before the Socket connects (probe-before-connect).
if grep -q '/run/omavrything/omavrything.sock' "$here/shell/omavrything/SearchOverlay.qml" &&
  grep -q 'socketProbe' "$here/shell/omavrything/SearchOverlay.qml"; then
  ok "overlay QML targets the system socket via a startup probe"
else
  bad "overlay QML does not target the system socket (widget shows 'Search daemon unavailable')"
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]