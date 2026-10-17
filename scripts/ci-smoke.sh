#!/usr/bin/env bash
# End-to-end smoke test: build -> install -> use -> uninstall.
#
# Every step is asserted: exit codes, expected output, and usage-failure
# handling (--version, --help, unknown commands, missing arguments all have
# expected exit statuses that must match). The client is exercised as an
# unprivileged user so the socket permission model is tested the way a real
# user experiences it; running the client as root would bypass the group check
# entirely and prove nothing.
#
# It also covers the M17 snapshot-staleness warm start (a file created while
# the daemon is down is found after a restart) and the M18 mount lifecycle
# (ownership is recorded, reconciled on startup, and forgotten on shutdown).
#
# Needs root (the system install writes to /usr); this is what the CI smoke job
# runs inside the Arch container.
#
# The installer is the unified, option-free scripts/install.sh: one command
# installs the system integration, creates the omavrything user, adds the
# invoking user to the group, reloads D-Bus, enables+starts the daemon, installs
# and enables the per-user shell plugin, restarts the shell, and applies the ACL
# bridge. There is no --system/--user/--no-start any more, and
# scripts/install-plugin.sh is gone.
#
# Usage: sudo scripts/ci-smoke.sh [--keep]
#
#   --keep   Leave the install and the temporary directory in place instead of
#            uninstalling and removing them (useful for inspecting a run).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

keep=0
for arg in "$@"; do
  case "$arg" in
    --keep) keep=1 ;;
    -h | --help)
      sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "error: unknown option: $arg" >&2
      exit 2
      ;;
  esac
done

if [[ "$(id -u)" -ne 0 ]]; then
  echo "error: this smoke test must run as root (use sudo)" >&2
  exit 1
fi

tmp="$(mktemp -d)"
# mktemp creates the directory 0700 root; the unprivileged client user must be
# able to traverse it to reach the daemon socket under $tmp/run.
chmod 755 "$tmp"
smoke_user="omavrything-smoke"
failures=0

# Remove the smoke user. runuser leaves a lingering systemd user session
# (systemd --user + sd-pam) behind, which makes plain `userdel` fail with
# "user is currently used by process"; terminate the session first and wait
# for its processes to exit.
remove_smoke_user() {
  loginctl terminate-user "$smoke_user" 2>/dev/null || true
  for _ in $(seq 1 20); do
    pgrep -u "$smoke_user" >/dev/null 2>&1 || break
    sleep 0.1
  done
  pkill -u "$smoke_user" 2>/dev/null || true
  sleep 0.2
  userdel "$smoke_user" >/dev/null 2>&1 || true
}

ok() { printf 'ok   - %s\n' "$*"; }
fail() {
  printf 'FAIL - %s\n' "$*" >&2
  failures=$((failures + 1))
}

# Run a command and assert its exit status and, optionally, its output.
#   expect <description> <expected-exit> [--grep <pattern>] -- <command...>
# A usage failure (an unexpected exit status or missing output) is recorded as
# a failure; it is never silently ignored.
expect() {
  local desc="$1" want="$2"
  shift 2
  local grep_pattern=""
  if [[ "${1:-}" == "--grep" ]]; then
    grep_pattern="$2"
    shift 2
  fi
  [[ "${1:-}" == "--" ]] && shift
  local out rc
  out="$("$@" 2>&1)" && rc=0 || rc=$?
  if [[ "$rc" -ne "$want" ]]; then
    fail "$desc (expected exit $want, got $rc)"
    printf '%s\n' "$out" | sed 's/^/       /' >&2
    return 0
  fi
  if [[ -n "$grep_pattern" ]] && ! grep -q -- "$grep_pattern" <<<"$out"; then
    fail "$desc (output missing '$grep_pattern')"
    printf '%s\n' "$out" | sed 's/^/       /' >&2
    return 0
  fi
  ok "$desc"
}

# Assert two strings are identical (used for -V vs --version).
expect_eq() {
  if [[ "$2" == "$3" ]]; then
    ok "$1"
  else
    fail "$1 ('$2' != '$3')"
  fi
}

# Assert a path exists ('present') or does not ('absent').
expect_path() {
  local desc="$1" path="$2" want="$3" there=0
  [[ -e "$path" ]] && there=1
  if [[ "$want" == "present" && "$there" == 1 ]] || [[ "$want" == "absent" && "$there" == 0 ]]; then
    ok "$desc"
  else
    fail "$desc ($path)"
  fi
}

# Assert a file contains a pattern.
expect_grep() {
  if grep -q -- "$3" "$2" 2>/dev/null; then
    ok "$1"
  else
    fail "$1 ($2 missing '$3')"
  fi
}

# Assert a file does NOT match an extended regex. Used to prove a
# clipboard-connected text input is absent from the overlay QML.
expect_no_grep_re() {
  if grep -Eq -- "$3" "$2" 2>/dev/null; then
    fail "$1 ($2 matches '$3')"
  else
    ok "$1"
  fi
}

cleanup() {
  # Best-effort teardown so a failure does not leave a daemon or install behind.
  pkill -x omavrythingd 2>/dev/null || true
  # M25: release the simulated stale mount used by the reconciliation check.
  umount -l /run/omavrything/ntfs/stale-smoke 2>/dev/null || true
  rmdir /run/omavrything/ntfs/stale-smoke 2>/dev/null || true
  # M18: drop the seeded mount-ownership state so a failed run does not leave a
  # stale record behind for the next one.
  rm -f /var/lib/omavrything/mounts.state 2>/dev/null || true
  remove_smoke_user
  if [[ "$keep" == 1 ]]; then
    echo "keeping the install and $tmp (--keep)"
    return
  fi
  "$here/scripts/uninstall.sh" >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT

# Build the release binaries into bin/ via scripts/build.sh, the single build
# step. install.sh reads the binaries from bin/, so building with a bare
# `cargo build` (which only populates target/release) would leave the installer
# with nothing to install. Under sudo, build as the invoking user so target/
# and bin/ are not left with root-owned artifacts (and cargo does not create a
# separate root-owned cache); in a root-only CI container this is a no-op
# branch and the build runs as root.
if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
  runuser -u "$SUDO_USER" -- "$here/scripts/build.sh"
else
  "$here/scripts/build.sh"
fi

# Guard against the sudo-build poisoning seen before: when the build ran as
# $SUDO_USER, no file under target/ or bin/ may be owned by root.
if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
  root_owned="$(find target bin -user root 2>/dev/null | wc -l)"
  if [[ "$root_owned" -eq 0 ]]; then
    ok "no root-owned files in target/ or bin/ after the sudo build"
  else
    fail "no root-owned files in target/ or bin/ after the sudo build ($root_owned found)"
  fi
fi

# 0b. The release archive must carry the binaries and the manifest; a package
# missing bin/ entries installs "successfully" and then fails at use-time.
if compgen -G 'dist/omavrything-*.zip' >/dev/null; then
  archive="$(ls dist/omavrything-*.zip | head -1)"
  archive_listing="$(unzip -l "$archive" 2>/dev/null || true)"
  for entry in bin/omavry bin/omavrythingd manifest.json INSTALL.txt scripts/benchmark.sh; do
    if grep -q "$entry" <<<"$archive_listing"; then
      ok "release archive contains $entry"
    else
      fail "release archive contains $entry"
    fi
  done
else
  printf 'skip - release archive checks (no dist/omavrything-*.zip)\n'
fi

# 1. System install (idempotent: run twice), then verify what landed. The
# unified installer takes no options; it does the whole job in one command.
expect "install.sh succeeds" 0 --grep "installed" -- scripts/install.sh
expect "install.sh is idempotent" 0 -- scripts/install.sh
for f in \
  /usr/bin/omavrythingd \
  /usr/bin/omavry \
  /usr/lib/systemd/system/omavrythingd.service \
  /usr/lib/sysusers.d/omavrything.conf \
  /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf \
  /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy \
  /usr/share/man/man8/omavrythingd.8 \
  /etc/omavrything/config.toml; do
  expect_path "installed: $f" "$f" present
done

# The install must create the dedicated user and add the invoking user to the
# socket group; without both the daemon cannot start and the client cannot
# connect (the 0.1.4 "Search daemon unavailable" regression).
if id omavrything >/dev/null 2>&1; then
  ok "install created the omavrything user"
else
  fail "install created the omavrything user"
fi
if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
  if id -nG "$SUDO_USER" 2>/dev/null | tr ' ' '\n' | grep -qx omavrything; then
    ok "install added $SUDO_USER to the omavrything group"
  else
    fail "install added $SUDO_USER to the omavrything group"
  fi
fi
# The new polkit action must be installed so the settings panel can save.
expect_grep "polkit set-config action installed" \
  /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy \
  "tech.alatcerdas.omavrything.set-config"
# Source-level guards for the unified installer.
expect_grep "install.sh adds the user to the group" scripts/install.sh \
  "usermod -aG omavrything"
expect_grep "install.sh reloads D-Bus" scripts/install.sh "ReloadConfig"
expect_grep "install.sh is option-free" scripts/install.sh "takes no options"
expect_grep "install.sh applies the ACL bridge" scripts/install.sh "setfacl"
expect_grep "install.sh installs the per-user plugin" scripts/install.sh \
  "plugins/tech.alatcerdas.omavrything"

# 2. The usage contract of the installed binaries. Every binary must support
# --version, and every usage error must exit 2 with a diagnostic.
daemon_version="$(/usr/bin/omavrythingd --version 2>&1 || true)"
cli_version="$(/usr/bin/omavry --version 2>&1 || true)"
expect "omavrythingd --version prints 'omavrythingd <version>'" 0 \
  --grep '^omavrythingd [0-9]' -- /usr/bin/omavrythingd --version
expect "omavry --version prints 'omavry <version>'" 0 \
  --grep '^omavry [0-9]' -- /usr/bin/omavry --version
expect_eq "omavrythingd -V matches --version" "$(/usr/bin/omavrythingd -V 2>&1 || true)" "$daemon_version"
expect_eq "omavry -V matches --version" "$(/usr/bin/omavry -V 2>&1 || true)" "$cli_version"
expect "omavrythingd --help prints usage" 0 --grep 'Usage' -- /usr/bin/omavrythingd --help
expect "omavry --help prints usage" 0 --grep 'Usage: omavry' -- /usr/bin/omavry --help
expect "omavry rejects an unknown command with exit 2" 2 \
  --grep 'unknown command' -- /usr/bin/omavry not-a-command
expect "omavry search without a query exits 2" 2 -- /usr/bin/omavry search
expect "omavry mount without a uuid exits 2" 2 -- /usr/bin/omavry mount
expect "omavry config without a subcommand exits 2" 2 -- /usr/bin/omavry config
expect "omavrythingd rejects an unknown argument with exit 2" 2 -- /usr/bin/omavrythingd --not-an-option

# 3. The per-user shell plugin, installed by the system install for the
# invoking user. In a root-only CI container there is no $SUDO_USER, so the
# installer skips the per-user steps and these checks are skipped too.
if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
  target_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
  plugin_dir="$target_home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
  expect_path "per-user plugin manifest installed" "$plugin_dir/manifest.json" present
  expect_path "per-user plugin QML installed" \
    "$plugin_dir/shell/omavrything/SearchOverlay.qml" present
  # Regression guard (2026-10-12): the widget used to hardcode
  # $XDG_RUNTIME_DIR/omavrything.sock, which the system daemon never creates, so
  # the overlay always showed "Search daemon unavailable". The QML must select
  # the system socket the same way the CLI's connect_default() does.
  overlay_qml="$plugin_dir/shell/omavrything/SearchOverlay.qml"
  expect_grep "overlay QML references the system socket" "$overlay_qml" \
    "/run/omavrything/omavrything.sock"
  expect_grep "overlay QML probes the socket path before connecting" "$overlay_qml" \
    "socketProbe"
  # The probe must fall back to the user socket and force a reconnect, so a
  # system socket the user cannot reach does not leave the overlay stuck on
  # "unavailable".
  expect_grep "overlay QML falls back to the user socket" "$overlay_qml" "echo user"
  expect_grep "overlay QML reports no usable socket" "$overlay_qml" "echo none"
  expect_grep "overlay QML forces a reconnect on open" "$overlay_qml" \
    "root.opened = false"
  # Regression guard (2026-10-13): the plugin is `keepLoaded`, and on Qt 6.12 a
  # native editable TextInput connects to QClipboard::dataChanged and
  # synchronously reads the foreign text over the Wayland transfer with no
  # aggregate byte limit and no total deadline, so a streaming clipboard owner
  # could stall or exhaust the shared shell with no paste action and no field
  # focus. Gating the input on the open state (the 0.1.6 fix) only removed it
  # while the window was hidden; the class remained while the overlay was open.
  # The overlay must therefore contain no clipboard-connected text input at all
  # — the search field is a custom Text + Keys input — and it must still be
  # gated on the open state. The regex matches a type instantiation
  # (`TextInput {`), not the explanatory comment that names the type.
  expect_no_grep_re "overlay QML has no clipboard-connected text input" \
    "$overlay_qml" '(TextInput|TextEdit|TextField|TextArea)[[:space:]]*\{'
  expect_grep "overlay QML uses a custom non-clipboard input" "$overlay_qml" \
    "Keys.onPressed"
  expect_grep "overlay QML gates the input on the open state" "$overlay_qml" \
    "active: root.opened"
  # Regression guard (2026-10-14): Text defaults to AutoText, which hands a
  # string that "looks like" rich text to Qt's rich-text engine. File names and
  # parent paths are attacker-controlled (any local user can create a directory
  # whose name is HTML), so an <img src="..."> in a directory name would make
  # the overlay load and decode an attacker-chosen local file (PNG/JPG/SVG/
  # PDF/...) during a search — a memory-exhaustion vector in the shared shell.
  # Every Text that renders index-derived data must set textFormat: Text.PlainText.
  if awk '
    /Text[[:space:]]*\{/ { intext = 1; plain = 0; dyn = 0; next }
    intext && /textFormat:[[:space:]]*Text\.PlainText/ { plain = 1 }
    intext && /text:[[:space:]]*rowDelegate\./ { dyn = 1 }
    intext && /^[[:space:]]*\}/ {
      if (dyn && !plain) { printf "unsafe Text at line %d\n", NR; bad = 1 }
      intext = 0
    }
    END { exit bad }
  ' "$overlay_qml"; then
    ok "overlay QML renders index-derived text as plain text"
  else
    fail "overlay QML renders index-derived text with AutoText (rich-text <img> load)"
  fi
  # Regression guard (2026-10-14): the overlay writes the query to a runtime
  # file the bar watches. A predictable path under /tmp lets another local user
  # pre-create it as a symlink (the overlay would overwrite the link target) or
  # feed the bar arbitrary content. The mirror must be derived from
  # $XDG_RUNTIME_DIR and disabled when it is unset — never fall back to /tmp.
  bar_qml="$plugin_dir/shell/omavrything/SearchBar.qml"
  if grep -q 'XDG_RUNTIME_DIR") || ""' "$overlay_qml" &&
    grep -q 'XDG_RUNTIME_DIR") || ""' "$bar_qml" &&
    ! grep -Eq 'queryFile:.*"/tmp"' "$overlay_qml" &&
    ! grep -Eq 'queryFile:.*"/tmp"' "$bar_qml"; then
    ok "query mirror uses a private per-user path (no /tmp fallback)"
  else
    fail "query mirror may fall back to a world-writable /tmp path"
  fi
  expect_path "CLI symlink installed" "$target_home/.local/bin/omavry" present
else
  printf 'skip - per-user plugin checks (no SUDO_USER)\n'
fi

# 4. Use the daemon: search over the socket as an unprivileged user. The system
# daemon started by install.sh owns /run/omavrything/omavrything.sock; stop it
# so the CLI's connect_default() falls back to the per-user socket under
# $tmp/run and this section stays hermetic (it must not depend on the real
# filesystem index).
if [[ "$(ps -p 1 -o comm= 2>/dev/null || true)" == "systemd" ]]; then
  systemctl stop omavrythingd >/dev/null 2>&1 || true
fi
run_dir="$tmp/run"
root_dir="$tmp/root"
mkdir -p "$run_dir" "$root_dir"
chmod 755 "$run_dir" # the client user must be able to traverse to the socket
: > "$root_dir/omavrything-smoke-marker.txt"
# Many files matching one query, so piping the results into `head` reliably
# closes the pipe while the CLI is still writing (broken-pipe regression).
for i in $(seq 1 100); do : > "$root_dir/omavrything-pipe-$i.txt"; done
# --no-config so the installed /etc/omavrything/config.toml (auto = true) does
# not override the explicit --root. --log-level info because the default is now
# `error` (a production daemon is quiet); the check below asserts the startup
# `listening` line, which is logged at info.
/usr/bin/omavrythingd \
  --no-config --root "$root_dir" --socket "$run_dir/omavrything.sock" \
  --no-dbus --no-snapshot --no-ntfs --no-watch --log-level info \
  >"$tmp/daemon.log" 2>&1 &
daemon_pid=$!

for _ in $(seq 1 100); do
  [[ -S "$run_dir/omavrything.sock" ]] && break
  sleep 0.1
done
if [[ -S "$run_dir/omavrything.sock" ]]; then
  ok "daemon created its socket"
else
  fail "daemon created its socket"
  cat "$tmp/daemon.log" >&2 || true
fi
expect_grep "daemon logged 'listening'" "$tmp/daemon.log" "listening"

if ! id "$smoke_user" >/dev/null 2>&1; then
  useradd -M -s /bin/sh "$smoke_user"
fi
mkdir -p "$tmp/smoke-home"
as_smoke() {
  runuser -u "$smoke_user" -- env \
    HOME="$tmp/smoke-home" PATH="/usr/bin:/bin" \
    XDG_RUNTIME_DIR="$run_dir" "$@"
}

# Whether `user` has an active login session. polkit's `allow_active=yes` only
# applies to a caller in an active session, so the polkit SetConfig guard below
# can only run when one exists.
user_has_active_session() {
  local user="$1" sid
  command -v loginctl >/dev/null 2>&1 || return 1
  while read -r sid; do
    if [[ "$(loginctl show-session "$sid" -p Active --value 2>/dev/null)" == "yes" ]]; then
      return 0
    fi
  done < <(loginctl list-sessions --no-legend 2>/dev/null | awk -v u="$user" '$3 == u { print $1 }')
  return 1
}

# Run a command in `user`'s active session. `runuser` alone runs the process in
# root's session, which polkit treats as inactive, so a cosmetic SetConfig would
# be denied by design and the check would be meaningless. `systemd-run --user`
# places the process in the user's session cgroup, which polkit resolves to the
# active session. Returns the command's exit status.
as_user_session() {
  local user="$1"; shift
  local uid runtime
  uid="$(id -u "$user")"
  runtime="/run/user/$uid"
  runuser -u "$user" -- env \
    XDG_RUNTIME_DIR="$runtime" \
    DBUS_SESSION_BUS_ADDRESS="unix:path=$runtime/bus" \
    systemd-run --user --wait --pipe --collect --quiet "$@"
}

# Before the documented group-membership step the socket must refuse the
# connection; this proves the socket is not world-accessible.
expect "search is refused outside the omavrything group" 1 \
  --grep "cannot connect" -- as_smoke /usr/bin/omavry search omavrything-smoke-marker

# After group membership (the documented setup step) search must work.
if usermod -aG omavrything "$smoke_user" 2>/dev/null; then
  ok "added the smoke user to the omavrything group"
else
  fail "added the smoke user to the omavrything group"
fi
expect "search finds the marker file" 0 \
  --grep "omavrything-smoke-marker" -- as_smoke /usr/bin/omavry search omavrything-smoke-marker
expect "a no-match search exits 0" 0 -- as_smoke /usr/bin/omavry search zzzz-no-such-file-zzzz

# The JSON contract must hold for empty results too: total == 0, no results.
nomatch_json="$(as_smoke /usr/bin/omavry search --json zzzz-no-such-file-zzzz)"
if command -v python3 >/dev/null 2>&1; then
  if printf '%s' "$nomatch_json" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data.get("total", -1) == 0, data
assert data.get("results") == [], data
assert "took_ms" in data, data
' >/dev/null 2>&1; then
    ok "search --json for a no-match query returns total 0 and empty results"
  else
    fail "search --json for a no-match query returns total 0 and empty results"
    printf '%s\n' "$nomatch_json" | sed 's/^/       /' >&2
  fi
fi

# Piping into `head` closes the pipe early; the CLI must exit 0 quietly like
# standard Unix tools, not panic with "failed printing to stdout: Broken pipe".
pipe_out="$(as_smoke sh -c 'set -o pipefail; /usr/bin/omavry search omavrything-pipe 2>&1 | head -1')"; pipe_rc=$?
if [[ $pipe_rc -eq 0 ]] && ! grep -q "panicked" <<<"$pipe_out"; then
  ok "search piped into head exits 0 without a broken-pipe panic"
else
  fail "search piped into head exits 0 without a broken-pipe panic (rc=$pipe_rc)"
  printf '%s\n' "$pipe_out" | sed 's/^/       /' >&2
fi

json_out="$(as_smoke /usr/bin/omavry search --json omavrything-smoke-marker)"
if command -v python3 >/dev/null 2>&1; then
  if printf '%s' "$json_out" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data.get("total", 0) >= 1, data
assert data.get("results"), data
assert "took_ms" in data, data
' >/dev/null 2>&1; then
    ok "search --json returns valid JSON with results"
  else
    fail "search --json returns valid JSON with results"
    printf '%s\n' "$json_out" | sed 's/^/       /' >&2
  fi
fi

# The JSON result entries must carry the documented schema: every result has
# name, path, size, mtime and is_dir, and took_ms is a non-negative integer.
if command -v python3 >/dev/null 2>&1; then
  if printf '%s' "$json_out" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data.get("results"), data
for r in data["results"]:
    assert set(("name", "path", "size", "mtime", "is_dir")) <= set(r), r
    assert isinstance(r["size"], int) and r["size"] >= 0, r
    assert isinstance(r["mtime"], int), r
    assert isinstance(r["is_dir"], bool), r
assert isinstance(data["took_ms"], int) and data["took_ms"] >= 0, data
' >/dev/null 2>&1; then
    ok "search --json result entries match the documented schema"
  else
    fail "search --json result entries match the documented schema"
    printf '%s\n' "$json_out" | sed 's/^/       /' >&2
  fi
  # The marker file must be found by its exact path, not just by substring.
  marker_path="$root_dir/omavrything-smoke-marker.txt"
  if printf '%s' "$json_out" | MARKER_PATH="$marker_path" python3 -c '
import json, os, sys
data = json.load(sys.stdin)
want = os.environ["MARKER_PATH"]
assert any(r["path"] == want for r in data["results"]), data
' >/dev/null 2>&1; then
    ok "search --json returns the marker file at its exact path"
  else
    fail "search --json returns the marker file at its exact path"
    printf '%s\n' "$json_out" | sed 's/^/       /' >&2
  fi
fi

# A second daemon on the same socket must refuse to start.
expect "a second daemon on the same socket refuses to start" 1 \
  --grep "another omavrythingd" -- /usr/bin/omavrythingd \
  --no-config --root "$root_dir" --socket "$run_dir/omavrything.sock" \
  --no-dbus --no-snapshot --no-ntfs --no-watch

kill "$daemon_pid" 2>/dev/null || true
wait "$daemon_pid" 2>/dev/null || true

# M25 (B9): a SIGTERM must leave no daemon-created private mount behind.
if grep -qs 'omavrything/ntfs' /proc/mounts; then
  fail "SIGTERM leaves no /run/omavrything/ntfs mount"
else
  ok "SIGTERM leaves no /run/omavrything/ntfs mount"
fi

# 4b. Idle eviction: with a snapshot and a short idle period the daemon must
# evict the owned index (the candidate arenas drop to zero) and still serve
# searches from the snapshot image. Regression guard for the twelfth E2E run,
# where ignored inotify events kept resetting the idle timer so the index
# never evicted at the default 20 s.
evict_snap="$tmp/evict.snapshot"
rm -f "$evict_snap"
/usr/bin/omavrythingd \
  --no-config --root "$root_dir" --socket "$run_dir/omavrything.sock" \
  --snapshot "$evict_snap" --evict-idle-secs 2 \
  --no-dbus --no-ntfs --no-watch --log-level info \
  >"$tmp/daemon-evict.log" 2>&1 &
evict_pid=$!
for _ in $(seq 1 100); do
  [[ -S "$run_dir/omavrything.sock" ]] && break
  sleep 0.1
done
if [[ -S "$run_dir/omavrything.sock" ]]; then
  ok "eviction-test daemon created its socket"
else
  fail "eviction-test daemon created its socket"
  cat "$tmp/daemon-evict.log" >&2 || true
fi
# One search so the index is warm and the timer starts from a known touch.
expect "search works before eviction" 0 \
  --grep "omavrything-smoke-marker" -- as_smoke /usr/bin/omavry search omavrything-smoke-marker
# Wait past the 2 s idle period (the evictor polls every 2 s) with no queries
# and no watch events, so nothing can reset the timer. The daemon logs
# "index evicted (idle)" at info when it evicts.
evicted_line=""
for _ in $(seq 1 50); do
  if grep -q "index evicted (idle)" "$tmp/daemon-evict.log" 2>/dev/null; then
    evicted_line=yes
    break
  fi
  sleep 0.3
done
if [[ -n "$evicted_line" ]]; then
  ok "the index evicted after the idle period"
else
  fail "the index evicted after the idle period"
  cat "$tmp/daemon-evict.log" >&2 || true
fi
# The first query while evicted returns `state: "loading"` and starts a
# background materialization (ADR-017); the next query is served at full
# speed. Retry until the marker appears, so the check still proves the
# evicted index recovers without a linear scan.
evicted_search_ok=""
for _ in $(seq 1 50); do
  if as_smoke /usr/bin/omavry search omavrything-smoke-marker 2>/dev/null \
      | grep -q "omavrything-smoke-marker"; then
    evicted_search_ok=yes
    break
  fi
  sleep 0.2
done
if [[ -n "$evicted_search_ok" ]]; then
  ok "search still works from the evicted image"
else
  fail "search still works from the evicted image"
  as_smoke /usr/bin/omavry search omavrything-smoke-marker >&2 || true
fi
kill "$evict_pid" 2>/dev/null || true
wait "$evict_pid" 2>/dev/null || true

# 4c. M17 (A4): snapshot staleness on a non-btrfs root. The first daemon saves
# a snapshot on SIGTERM; a file is then created while the daemon is down; the
# second daemon warm-starts from the snapshot and must detect the moved root
# mtime watermark, rescan the root, and serve the offline-created file.
warm_snap="$tmp/warm.snapshot"
rm -f "$warm_snap"
/usr/bin/omavrythingd \
  --no-config --root "$root_dir" --socket "$run_dir/omavrything.sock" \
  --snapshot "$warm_snap" --no-dbus --no-ntfs --no-watch --scan-interval 0 \
  --log-level info >"$tmp/daemon-warm1.log" 2>&1 &
warm1_pid=$!
for _ in $(seq 1 100); do
  [[ -S "$run_dir/omavrything.sock" ]] && break
  sleep 0.1
done
expect "pre-shutdown search finds the marker" 0 \
  --grep "omavrything-smoke-marker" -- as_smoke /usr/bin/omavry search omavrything-smoke-marker
kill "$warm1_pid" 2>/dev/null || true
wait "$warm1_pid" 2>/dev/null || true
expect_path "the shutdown wrote the warm-start snapshot" "$warm_snap" present
# The offline change: created after the daemon stopped, so only a warm start
# that detects the moved watermark can find it. Sleep first so the root
# directory's mtime (the A4 watermark, in seconds) is guaranteed to move.
sleep 1
: > "$root_dir/omavrything-warm-late.txt"
/usr/bin/omavrythingd \
  --no-config --root "$root_dir" --socket "$run_dir/omavrything.sock" \
  --snapshot "$warm_snap" --no-dbus --no-ntfs --no-watch --scan-interval 0 \
  --log-level info >"$tmp/daemon-warm2.log" 2>&1 &
warm2_pid=$!
for _ in $(seq 1 100); do
  [[ -S "$run_dir/omavrything.sock" ]] && break
  sleep 0.1
done
# The warm start rescans the root in the background; poll until the offline
# file is served (the socket is bound before the rescan completes).
warm_found=""
for _ in $(seq 1 50); do
  if as_smoke /usr/bin/omavry search omavrything-warm-late 2>/dev/null |
    grep -q "omavrything-warm-late"; then
    warm_found=yes
    break
  fi
  sleep 0.2
done
if [[ -n "$warm_found" ]]; then
  ok "a warm start finds a file created while the daemon was down"
else
  fail "a warm start finds a file created while the daemon was down"
  cat "$tmp/daemon-warm2.log" >&2 || true
fi
kill "$warm2_pid" 2>/dev/null || true
wait "$warm2_pid" 2>/dev/null || true

# 4d. M18 (B1/B2/B8) source guards: the daemon records which mounts it created,
# reconciles that ownership on startup, and drops a disappeared mount's index
# entries. The behavioural checks live in the ntfs unit tests and the M25
# stale-mount section below.
expect_grep "ntfs records daemon-created mounts" \
  crates/omavrythingd/src/ntfs.rs "mount_index_and_remember_with_ownership"
expect_grep "ntfs reconciles recorded ownership on startup" \
  crates/omavrythingd/src/ntfs.rs "reconcile_stale_mounts"
expect_grep "ntfs drops entries for a disappeared mount" \
  crates/omavrythingd/src/ntfs.rs "drop_mount_entries"
expect_grep "ntfs never unmounts a pre-existing mount" \
  crates/omavrythingd/src/ntfs.rs "is_owned"

# 5. D-Bus status, only where systemd is available (the CI container has none).
if [[ "$(ps -p 1 -o comm= 2>/dev/null || true)" == "systemd" ]]; then
  expect "systemctl start omavrythingd" 0 -- systemctl start omavrythingd
  # install.sh must have enabled the unit at boot, not just installed it.
  enabled="$(systemctl is-enabled omavrythingd 2>/dev/null || true)"
  if [[ "$enabled" == "enabled" ]]; then
    ok "install enabled omavrythingd at boot"
  else
    fail "install enabled omavrythingd at boot (got: ${enabled:-unknown})"
  fi
  for _ in $(seq 1 150); do
    busctl --system status tech.alatcerdas.omavrything >/dev/null 2>&1 && break
    sleep 0.2
  done
  if busctl --system status tech.alatcerdas.omavrything >/dev/null 2>&1; then
    ok "daemon owns its D-Bus name"
  else
    fail "daemon owns its D-Bus name"
  fi
  expect "omavry status works over D-Bus" 0 --grep "entries" -- as_smoke /usr/bin/omavry status
  expect "omavry status --json works over D-Bus" 0 --grep "entries" -- as_smoke /usr/bin/omavry status --json
  expect "omavry config get works over D-Bus" 0 -- as_smoke /usr/bin/omavry config get
  if command -v python3 >/dev/null 2>&1; then
    status_json="$(as_smoke /usr/bin/omavry status --json)"
    if printf '%s' "$status_json" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert int(data["entries"]) >= 0, data
assert data.get("scanning") in ("true", "false"), data
assert "version" in data, data
' >/dev/null 2>&1; then
      ok "status --json parses as valid JSON with the expected keys"
    else
      fail "status --json parses as valid JSON with the expected keys"
      printf '%s\n' "$status_json" | sed 's/^/       /' >&2
    fi
  fi
  # The socket permission model is part of the contract: 0660 owned by the
  # omavrything user and group, inside a 0750 runtime directory.
  sock_mode="$(stat -c '%a %U %G' /run/omavrything/omavrything.sock 2>/dev/null || true)"
  if [[ "$sock_mode" == "660 omavrything omavrything" ]]; then
    ok "system socket is 0660 omavrything:omavrything"
  else
    fail "system socket is 0660 omavrything:omavrything (got: ${sock_mode:-missing})"
  fi
  run_dir_mode="$(stat -c '%a' /run/omavrything 2>/dev/null || true)"
  if [[ "$run_dir_mode" == "750" ]]; then
    ok "runtime directory /run/omavrything is 0750"
  else
    fail "runtime directory /run/omavrything is 0750 (got: ${run_dir_mode:-missing})"
  fi
  expect "config get exposes the documented keys" 0 \
    --grep "scan_interval" -- as_smoke /usr/bin/omavry config get
  # Regression guard (2026-10-12, §3.37): the daemon's polkit
  # CheckAuthorization call used the pre-0.126 argument order, so every
  # non-root SetConfig was denied and the settings menu did not persist. The
  # cosmetic `set-config` action is allow_active=yes, so a caller in an active
  # session must be able to set it without a prompt. `runuser` alone runs in
  # root's session (inactive), so the check goes through `systemd-run --user`
  # in the invoking user's session; it is skipped only when there is no active
  # session to test from (e.g. a root-only CI container).
  if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]] \
    && command -v systemd-run >/dev/null 2>&1 \
    && user_has_active_session "$SUDO_USER"; then
    if as_user_session "$SUDO_USER" /usr/bin/omavry config set background_opacity 0.8; then
      get_out="$(as_smoke /usr/bin/omavry config get 2>&1 || true)"
      if grep -q "background_opacity: 0.8" <<<"$get_out"; then
        ok "non-root SetConfig is authorized and persists (polkit argument order)"
      else
        fail "non-root SetConfig persisted (got: $(grep background_opacity <<<"$get_out" || echo none))"
      fi
    else
      fail "non-root SetConfig is authorized in an active session (polkit argument order)"
    fi
    # Restore the default so the check is idempotent.
    as_user_session "$SUDO_USER" /usr/bin/omavry config set background_opacity 0.75 >/dev/null 2>&1 || true
  else
    printf 'skip - non-root SetConfig check (no active session to test from)\n'
  fi
  # Fail-closed guard: the cosmetic `set-config` action must not be usable to
  # change a privileged key. The smoke user has no session, so the `control`
  # action (auth_admin_keep) denies it; the daemon must return AccessDenied,
  # never silently apply the change.
  expect "a non-root caller cannot set a privileged key" 1 \
    --grep "not authorized" -- as_smoke /usr/bin/omavry config set excludes /tmp
  expect "a non-root caller cannot rescan" 1 \
    --grep "not authorized" -- as_smoke /usr/bin/omavry rescan
  # Stopping the daemon must remove the socket with it (RuntimeDirectory
  # cleanup): a stale socket would make clients hang or fail confusingly.
  systemctl stop omavrythingd >/dev/null 2>&1 || true
  expect_path "stopping the daemon removes the system socket" \
    /run/omavrything/omavrything.sock absent

  # 5b. M25: NTFS forced-close and shutdown safety.
  unit=/usr/lib/systemd/system/omavrythingd.service
  expect_grep "unit sets KillSignal=SIGTERM" "$unit" "KillSignal=SIGTERM"
  expect_grep "unit bounds the stop timeout" "$unit" "TimeoutStopSec"
  expect_grep "unit sweeps private mounts on stop" "$unit" "ExecStopPost"
  # B10: a stale daemon-created mount (a SIGKILL/crash leftover) is reconciled
  # on startup, and the daemon still serves searches (B14) even if it cannot
  # be released. A read-only tmpfs under the private mount root simulates the
  # leftover; the daemon's startup reconciliation must unmount it.
  #
  # The daemon is run directly rather than through systemd: a pre-existing
  # mount inside the unit's RuntimeDirectory makes systemd's own
  # RUNTIME_DIRECTORY setup fail ("Read-only file system"), which would test
  # systemd's namespace handling rather than the daemon's reconciliation.
  mkdir -p /run/omavrything/ntfs/stale-smoke
  if mount -t tmpfs -o ro,size=1m tmpfs /run/omavrything/ntfs/stale-smoke 2>/dev/null; then
    # M18 (B2): seed the mount-ownership state with the simulated stale mount,
    # so startup reconciliation reads the recorded ownership (not just the
    # mount table) and the shutdown sweep clears the record.
    mkdir -p /var/lib/omavrything
    printf '/run/omavrything/ntfs/stale-smoke\t1\n' \
      >/var/lib/omavrything/mounts.state
    /usr/bin/omavrythingd \
      --no-config --root "$root_dir" --socket "$run_dir/omavrything.sock" \
      --no-dbus --no-snapshot --no-watch --log-level info \
      >"$tmp/daemon-ntfs.log" 2>&1 &
    ntfs_pid=$!
    for _ in $(seq 1 100); do
      [[ -S "$run_dir/omavrything.sock" ]] && break
      sleep 0.1
    done
    # Give the NTFS startup thread time to reconcile the stale mount.
    for _ in $(seq 1 50); do
      grep -qs 'ntfs/stale-smoke' /proc/mounts || break
      sleep 0.2
    done
    if grep -qs 'ntfs/stale-smoke' /proc/mounts; then
      fail "startup reconciles a stale private mount"
    else
      ok "startup reconciles a stale private mount"
    fi
    # B14: the daemon must still serve searches regardless.
    expect "daemon serves searches despite the stale mount" 0 \
      --grep "omavrything-smoke-marker" -- as_smoke /usr/bin/omavry search omavrything-smoke-marker
    # B9: a SIGTERM unmounts any daemon-created private mount.
    kill "$ntfs_pid" 2>/dev/null || true
    wait "$ntfs_pid" 2>/dev/null || true
    if grep -qs 'omavrything/ntfs' /proc/mounts; then
      fail "SIGTERM leaves no /run/omavrything/ntfs mount"
    else
      ok "SIGTERM leaves no /run/omavrything/ntfs mount"
    fi
    # M18 (B1/B2): the shutdown sweep forgets the released mount, so the
    # ownership state no longer records it.
    if grep -qs 'stale-smoke' /var/lib/omavrything/mounts.state 2>/dev/null; then
      fail "shutdown forgets a released daemon-created mount"
      cat /var/lib/omavrything/mounts.state >&2 || true
    else
      ok "shutdown forgets a released daemon-created mount"
    fi
    # Safety net: never leave the simulated mount behind for the uninstall.
    umount -l /run/omavrything/ntfs/stale-smoke 2>/dev/null || true
  else
    printf 'skip - stale-mount reconciliation (tmpfs mount unavailable)\n'
  fi
  # B9/B11: a clean stop (and the ExecStopPost sweep) leaves no private mount.
  if grep -qs 'omavrything/ntfs' /proc/mounts; then
    fail "systemctl stop leaves no /run/omavrything/ntfs mount"
  else
    ok "systemctl stop leaves no /run/omavrything/ntfs mount"
  fi
else
  printf 'skip - D-Bus status check (no systemd on pid 1)\n'
fi

# The smoke user is no longer needed; remove it here so the teardown section
# below can assert it is really gone (cleanup only runs on EXIT).
remove_smoke_user

# 6. Uninstall (idempotent: run twice), then verify nothing is left behind.
# The unified installer is system-only, so the system uninstall is the inverse
# and also removes the per-user plugin for the invoking user.
if [[ "$keep" == 1 ]]; then
  echo "skipping uninstall (--keep)"
else
  expect "uninstall.sh succeeds" 0 --grep "omavrything removed" -- scripts/uninstall.sh
  expect "uninstall.sh is idempotent" 0 -- scripts/uninstall.sh

  for f in \
    /usr/bin/omavrythingd \
    /usr/bin/omavry \
    /usr/lib/systemd/system/omavrythingd.service \
    /usr/lib/sysusers.d/omavrything.conf \
    /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf \
    /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy \
    /usr/share/man/man8/omavrythingd.8 \
    /etc/omavrything \
    /usr/share/omavrything \
    /usr/share/licenses/omavrything \
    /usr/share/doc/omavrything \
    /var/lib/omavrything \
    /run/omavrything; do
    expect_path "removed: $f" "$f" absent
  done
  if pgrep -x omavrythingd >/dev/null 2>&1; then
    fail "no daemon is running after uninstall"
  else
    ok "no daemon is running after uninstall"
  fi
  if id omavrything >/dev/null 2>&1; then
    fail "the omavrything user was removed"
  else
    ok "the omavrything user was removed"
  fi
  if id "$smoke_user" >/dev/null 2>&1; then
    fail "the smoke-test user was removed"
  else
    ok "the smoke-test user was removed"
  fi
  # The per-user plugin and CLI symlink must be gone too.
  if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
    target_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
    expect_path "uninstall removed the per-user plugin" \
      "$target_home/.config/omarchy/plugins/tech.alatcerdas.omavrything" absent
    expect_path "uninstall removed the CLI symlink" "$target_home/.local/bin/omavry" absent
  fi
fi

if [[ "$failures" -gt 0 ]]; then
  printf '\n%d check(s) failed\n' "$failures" >&2
  exit 1
fi
echo "smoke test passed"
