#!/usr/bin/env bash
# Acceptance checks for M13 — install/uninstall completeness.
#
# Runs unprivileged: it checks the unified installer's and uninstaller's
# option-free/root-only contract, the uninstaller's source-level guards, and the
# release archive contents. The system-mode checks (root) are documented in the
# milestone and exercised by the CI smoke job (M14/C10).
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

# --- C2: install.sh is the unified, option-free, root-only installer --------
# The old `--user` mode is gone: the single command installs the system
# integration and the per-user plugin for the invoking user. It must reject any
# option and refuse to run as a non-root user.
err="$tmp/install-opt.err"
if "$pkg/scripts/install.sh" --user >/dev/null 2>"$err"; then
  bad "install.sh --user unexpectedly succeeded (it must take no options)"
else
  if grep -q 'takes no options' "$err"; then
    ok "install.sh rejects options (unified, option-free installer)"
  else
    bad "install.sh did not reject --user: $(cat "$err")"
  fi
fi
err="$tmp/install-root.err"
if "$pkg/scripts/install.sh" >/dev/null 2>"$err"; then
  bad "install.sh unexpectedly succeeded as a non-root user"
else
  if grep -q 'must be run as root' "$err"; then
    ok "install.sh refuses to run as a non-root user"
  else
    bad "install.sh did not refuse a non-root user: $(cat "$err")"
  fi
fi
if grep -q 'usermod -aG omavrything' "$pkg/scripts/install.sh" &&
  grep -q 'setfacl' "$pkg/scripts/install.sh"; then
  ok "install.sh adds the user to the group and applies the ACL bridge"
else
  bad "install.sh does not add the user to the group / apply the ACL bridge"
fi

# --- C3: uninstall.sh is system-only (no --user mode) -----------------------
# The unified installer is system-only, so the uninstaller is too: it must
# reject the removed --user mode and refuse to run as a non-root user.
err="$tmp/uninstall-user.err"
if "$pkg/scripts/uninstall.sh" --user >/dev/null 2>"$err"; then
  bad "uninstall.sh --user unexpectedly succeeded (the --user mode is gone)"
else
  if grep -q 'unknown option' "$err"; then
    ok "uninstall.sh rejects the removed --user mode"
  else
    bad "uninstall.sh did not reject --user: $(cat "$err")"
  fi
fi
err="$tmp/uninstall-root.err"
if "$pkg/scripts/uninstall.sh" >/dev/null 2>"$err"; then
  bad "uninstall.sh unexpectedly succeeded as a non-root user"
else
  if grep -q 'must be run as root' "$err"; then
    ok "uninstall.sh refuses to run as a non-root user"
  else
    bad "uninstall.sh did not refuse a non-root user: $(cat "$err")"
  fi
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

# --- C9: uninstall.sh removes the per-user plugin in system mode ------------
# The system uninstall is the inverse of the system install: it must remove the
# per-user plugin (through omarchy when available) and the CLI symlink.
if grep -q 'plugins/tech.alatcerdas.omavrything' "$pkg/scripts/uninstall.sh" &&
  grep -q 'omarchy plugin remove' "$pkg/scripts/uninstall.sh" &&
  grep -q 'local/bin/omavry' "$pkg/scripts/uninstall.sh"; then
  ok "uninstall.sh removes the per-user plugin and CLI symlink"
else
  bad "uninstall.sh does not remove the per-user plugin / CLI symlink"
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
if [[ -x bin/omavrythingd && -x bin/omavry ]]; then
  if scripts/package-release.sh --no-build >/dev/null 2>&1; then
    archive="$(ls -1 dist/omavrything-*.zip 2>/dev/null | tail -n1 || true)"
    # Capture the listing first: `unzip -l | grep -q` would SIGPIPE unzip when
    # grep exits on the first match, which `set -o pipefail` turns into a failure.
    listing="$(unzip -l "$archive" 2>/dev/null || true)"
    for entry in scripts/install.sh scripts/uninstall.sh data/man/omavrythingd.8 scripts/doctor.sh scripts/benchmark.sh; do
      if [[ -n "$listing" ]] && grep -qF "$entry" <<<"$listing"; then
        ok "the release archive contains $entry"
      else
        bad "the release archive does not contain $entry"
      fi
    done
    # install-plugin.sh was deleted with the unified installer; the archive
    # must not ship it any more.
    if [[ -n "$listing" ]] && ! grep -qF "install-plugin.sh" <<<"$listing"; then
      ok "the release archive no longer ships install-plugin.sh"
    else
      bad "the release archive still ships install-plugin.sh"
    fi
  else
    bad "scripts/package-release.sh --no-build failed"
  fi
else
  printf 'skip - archive check (run scripts/build.sh first)\n'
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