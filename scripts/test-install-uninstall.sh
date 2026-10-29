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

# --- C2: install.sh is the unified, root-only installer ---------------------
# The old `--user` mode is gone: the single command installs the system
# integration and the per-user plugin for the invoking user. It must reject an
# unknown option and refuse to run as a non-root user.
err="$tmp/install-opt.err"
if "$pkg/scripts/install.sh" --user >/dev/null 2>"$err"; then
  bad "install.sh --user unexpectedly succeeded (it must reject unknown options)"
else
  if grep -q 'unknown option' "$err"; then
    ok "install.sh rejects an unknown option"
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

# --- M19: uninstall.sh never recurses into a live NTFS mount ----------------
# The daemon mounts NTFS read-only under /run/omavrything/ntfs. The uninstaller
# must unmount those first and remove the runtime dir with --one-file-system.
if grep -q 'unmount_runtime_mounts' "$pkg/scripts/uninstall.sh" &&
  grep -q -- '--one-file-system' "$pkg/scripts/uninstall.sh" &&
  grep -q '/run/omavrything/' "$pkg/scripts/uninstall.sh"; then
  ok "uninstall.sh unmounts the runtime mounts and never crosses them"
else
  bad "uninstall.sh does not guard against recursing into a live mount"
fi

# --- C11: doctor.sh prints a report and reflects the install state ----------
# doctor.sh emits [fail] only when the daemon binary is absent (or present but
# broken); every other line is [ ok ] or [warn], and it exits non-zero iff it
# printed a [fail]. The check is therefore conditional on the host: on a clean
# host it must flag the missing install, and on a host with the daemon installed
# it must report all-ok and exit 0. Asserting the missing-install case
# unconditionally made the test non-hermetic — it failed on any host with
# omavrything installed (e.g. running the battery out of order).
out="$("$here/scripts/doctor.sh" 2>&1)" && rc=0 || rc=$?
if printf '%s' "$out" | grep -q 'omavrything doctor'; then
  ok "doctor.sh prints a report"
else
  bad "doctor.sh did not print a report"
fi
if [[ -x /usr/bin/omavrythingd ]]; then
  if [[ "$rc" -eq 0 ]] && ! printf '%s' "$out" | grep -q '\[fail\]'; then
    ok "doctor.sh reports all-ok and exits 0 with the daemon installed"
  else
    bad "doctor.sh did not report all-ok with the daemon installed (rc=$rc)"
  fi
else
  if printf '%s' "$out" | grep -q '\[fail\]'; then
    ok "doctor.sh flags a missing install"
  else
    bad "doctor.sh did not flag the missing install"
  fi
fi

# --- C1/C6/C11: the release archive ships the expected files ----------------
if [[ -x bin/omavrythingd && -x bin/omavry ]]; then
  if scripts/package-release.sh --no-build >/dev/null 2>&1; then
    archive="$(ls -1 dist/omavrything-*.zip 2>/dev/null | tail -n1 || true)"
    # Capture the listing first: `unzip -l | grep -q` would SIGPIPE unzip when
    # grep exits on the first match, which `set -o pipefail` turns into a failure.
    listing="$(unzip -l "$archive" 2>/dev/null || true)"
    for entry in scripts/install.sh scripts/uninstall.sh data/man/omavrythingd.8 scripts/doctor.sh scripts/benchmark.sh scripts/daemon-checksums.txt; do
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

# --- C13: the overlay QML has no clipboard-connected text input --------------
# Regression guard (2026-10-13): on Qt 6.12 a native editable TextInput connects
# to QClipboard::dataChanged and synchronously reads the foreign text over the
# Wayland transfer with no aggregate byte limit and no total deadline, so a
# streaming clipboard owner could stall or exhaust the shared shell with no
# paste action and no field focus. Gating the input on the open state only
# removed it while the window was hidden; the class remained while the overlay
# was open. The search field must be a custom Text + Keys input, not a
# TextInput/TextEdit/TextField/TextArea. The regex matches a type instantiation
# (`TextInput {`), not the explanatory comment that names the type.
if ! grep -Eq '(TextInput|TextEdit|TextField|TextArea)[[:space:]]*\{' \
  "$here/shell/omavrything/SearchOverlay.qml" &&
  grep -q 'Keys.onPressed' "$here/shell/omavrything/SearchOverlay.qml"; then
  ok "overlay QML has no clipboard-connected text input (custom Text + Keys field)"
else
  bad "overlay QML contains a clipboard-connected text input"
fi

# --- C14: index-derived text is rendered as plain text ------------------------
# Regression guard (2026-10-14): Text defaults to AutoText, which hands a string
# that "looks like" rich text to Qt's rich-text engine. File names and parent
# paths are attacker-controlled — any local user can create a directory whose
# name is HTML — so an <img src="..."> in a directory name would make the
# overlay load and decode an attacker-chosen local file (PNG/JPG/SVG/PDF/...)
# during a search, a memory-exhaustion vector in the shared shell. Every Text
# that renders index-derived data must set textFormat: Text.PlainText. The awk
# walks each flat `Text { ... }` block and fails if a block binds
# `text: rowDelegate.*` without a PlainText format.
if awk '
  /Text[[:space:]]*\{/ { intext = 1; plain = 0; dyn = 0; next }
  intext && /textFormat:[[:space:]]*Text\.PlainText/ { plain = 1 }
  intext && /text:[[:space:]]*rowDelegate\./ { dyn = 1 }
  intext && /^[[:space:]]*\}/ {
    if (dyn && !plain) { printf "unsafe Text at line %d\n", NR; bad = 1 }
    intext = 0
  }
  END { exit bad }
' "$here/shell/omavrything/SearchOverlay.qml"; then
  ok "overlay QML renders index-derived text as plain text (no rich-text <img> load)"
else
  bad "overlay QML renders index-derived text with AutoText (rich-text <img> load)"
fi

# --- C15: the query mirror never uses a world-writable path -------------------
# Regression guard (2026-10-14): the overlay writes the query to a runtime file
# the bar watches. A predictable path under /tmp lets another local user
# pre-create it as a symlink (the overlay would then overwrite the link target
# with the query) or feed the bar arbitrary content. The mirror must be derived
# from $XDG_RUNTIME_DIR and disabled when it is unset — never fall back to /tmp.
if grep -q 'XDG_RUNTIME_DIR") || ""' "$here/shell/omavrything/SearchOverlay.qml" &&
  grep -q 'XDG_RUNTIME_DIR") || ""' "$here/shell/omavrything/SearchBar.qml" &&
  ! grep -Eq 'queryFile:.*"/tmp"' "$here/shell/omavrything/SearchOverlay.qml" &&
  ! grep -Eq 'queryFile:.*"/tmp"' "$here/shell/omavrything/SearchBar.qml"; then
  ok "query mirror uses a private per-user path (no /tmp fallback)"
else
  bad "query mirror may fall back to a world-writable /tmp path"
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]