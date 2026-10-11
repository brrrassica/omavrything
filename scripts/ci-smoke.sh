#!/usr/bin/env bash
# End-to-end smoke test: build -> install -> enable -> use -> uninstall.
#
# Every step is asserted: exit codes, expected output, and usage-failure
# handling (--version, --help, unknown commands, missing arguments all have
# expected exit statuses that must match). The client is exercised as an
# unprivileged user so the socket permission model is tested the way a real
# user experiences it; running the client as root would bypass the group check
# entirely and prove nothing.
#
# Needs root (the system install writes to /usr); this is what the CI smoke job
# runs inside the Arch container.
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
      sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
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

cleanup() {
  # Best-effort teardown so a failure does not leave a daemon or install behind.
  pkill -x omavrythingd 2>/dev/null || true
  remove_smoke_user
  if [[ "$keep" == 1 ]]; then
    echo "keeping the install and $tmp (--keep)"
    return
  fi
  "$here/scripts/uninstall.sh" >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT

# A fake `omarchy` so the plugin steps work without a running shell.
fake_bin="$tmp/bin"
mkdir -p "$fake_bin"
cat > "$fake_bin/omarchy" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${OMARCHY_LOG:-/dev/null}"
exit 0
EOF
chmod +x "$fake_bin/omarchy"
export PATH="$fake_bin:$PATH"
export OMARCHY_LOG="$tmp/omarchy.log"

# Build the release binaries. Under sudo, build as the invoking user so
# target/ is not left with root-owned artifacts (and cargo does not create a
# separate root-owned cache); in a root-only CI container this is a no-op
# branch and the build runs as root.
if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
  runuser -u "$SUDO_USER" -- cargo build --release --locked --workspace
else
  cargo build --release --locked --workspace
fi

# Guard against the sudo-build poisoning seen before: when the build ran as
# $SUDO_USER, no file under target/ may be owned by root.
if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
  root_owned="$(find target -user root 2>/dev/null | wc -l)"
  if [[ "$root_owned" -eq 0 ]]; then
    ok "no root-owned files in target/ after the sudo build"
  else
    fail "no root-owned files in target/ after the sudo build ($root_owned found)"
  fi
fi

# 1. System install (idempotent: run twice), then verify what landed.
expect "install.sh --system succeeds" 0 --grep "Installed omavrything" -- scripts/install.sh --system
expect "install.sh --system is idempotent" 0 -- scripts/install.sh --system
for f in \
  /usr/bin/omavrythingd \
  /usr/bin/omavry \
  /usr/bin/omavrything-install-plugin \
  /usr/lib/systemd/system/omavrythingd.service \
  /usr/lib/sysusers.d/omavrything.conf \
  /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf \
  /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy \
  /usr/share/man/man8/omavrythingd.8 \
  /etc/omavrything/config.toml; do
  expect_path "installed: $f" "$f" present
done

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

# 3. User install (idempotent: run twice) into a throwaway HOME.
user_home="$tmp/home"
mkdir -p "$user_home"
run_as_user() {
  env HOME="$user_home" XDG_CONFIG_HOME="$user_home/.config" \
    XDG_BIN_HOME="$user_home/.local/bin" "$@"
}
expect "install.sh --user succeeds" 0 --grep "Installed shell plugin" -- run_as_user scripts/install.sh --user
expect "install.sh --user is idempotent" 0 -- run_as_user scripts/install.sh --user
expect_path "plugin manifest installed" \
  "$user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything/manifest.json" present
expect_path "plugin QML installed" \
  "$user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything/shell/omavrything/SearchOverlay.qml" present
# Regression guard (2026-10-12): the widget used to hardcode
# $XDG_RUNTIME_DIR/omavrything.sock, which the system daemon never creates, so
# the overlay always showed "Search daemon unavailable". The QML must select
# the system socket the same way the CLI's connect_default() does.
overlay_qml="$user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything/shell/omavrything/SearchOverlay.qml"
expect_grep "overlay QML references the system socket" "$overlay_qml" \
  "/run/omavrything/omavrything.sock"
expect_grep "overlay QML probes the socket path before connecting" "$overlay_qml" \
  "socketProbe"
expect_path "CLI symlink installed" "$user_home/.local/bin/omavry" present
expect_grep "install.sh --user restarts the shell" "$tmp/omarchy.log" "restart shell"

# 4. Enable the plugin (recorded by the fake omarchy).
omarchy plugin enable tech.alatcerdas.omavrything --section center
expect_grep "plugin enable recorded" "$tmp/omarchy.log" \
  "plugin enable tech.alatcerdas.omavrything"

# 5. Use the daemon: search over the socket as an unprivileged user.
run_dir="$tmp/run"
root_dir="$tmp/root"
mkdir -p "$run_dir" "$root_dir"
chmod 755 "$run_dir" # the client user must be able to traverse to the socket
: > "$root_dir/omavrything-smoke-marker.txt"
# Many files matching one query, so piping the results into `head` reliably
# closes the pipe while the CLI is still writing (broken-pipe regression).
for i in $(seq 1 100); do : > "$root_dir/omavrything-pipe-$i.txt"; done
# --no-config so the installed /etc/omavrything/config.toml (auto = true) does
# not override the explicit --root.
/usr/bin/omavrythingd \
  --no-config --root "$root_dir" --socket "$run_dir/omavrything.sock" \
  --no-dbus --no-snapshot --no-ntfs --no-watch \
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

# Before the documented group-membership step the socket must refuse the
# connection; this proves the socket is not world-accessible.
expect "search is refused outside the omavrything group" 1 \
  --grep "cannot connect" -- as_smoke /usr/bin/omavry search omavrything-smoke-marker

# After group membership (the documented setup step) search must work.
usermod -aG omavrything "$smoke_user"
expect "search finds the marker file" 0 \
  --grep "omavrything-smoke-marker" -- as_smoke /usr/bin/omavry search omavrything-smoke-marker
expect "a no-match search exits 0" 0 -- as_smoke /usr/bin/omavry search zzzz-no-such-file-zzzz

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

# A second daemon on the same socket must refuse to start.
expect "a second daemon on the same socket refuses to start" 1 \
  --grep "another omavrythingd" -- /usr/bin/omavrythingd \
  --no-config --root "$root_dir" --socket "$run_dir/omavrything.sock" \
  --no-dbus --no-snapshot --no-ntfs --no-watch

kill "$daemon_pid" 2>/dev/null || true
wait "$daemon_pid" 2>/dev/null || true

# 6. D-Bus status, only where systemd is available (the CI container has none).
if [[ "$(ps -p 1 -o comm= 2>/dev/null || true)" == "systemd" ]]; then
  expect "systemctl start omavrythingd" 0 -- systemctl start omavrythingd
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
  systemctl stop omavrythingd >/dev/null 2>&1 || true
else
  printf 'skip - D-Bus status check (no systemd on pid 1)\n'
fi

# The smoke user is no longer needed; remove it here so the teardown section
# below can assert it is really gone (cleanup only runs on EXIT).
remove_smoke_user

# 7. Uninstall (idempotent: run twice), then verify nothing is left behind.
if [[ "$keep" == 1 ]]; then
  echo "skipping uninstall (--keep)"
else
  expect "uninstall.sh --user succeeds" 0 --grep "shell plugin removed" -- run_as_user scripts/uninstall.sh --user
  expect "uninstall.sh --user is idempotent" 0 -- run_as_user scripts/uninstall.sh --user
  expect_path "user uninstall removed the plugin" \
    "$user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything" absent
  expect_path "user uninstall removed the CLI symlink" "$user_home/.local/bin/omavry" absent

  expect "uninstall.sh (system) succeeds" 0 --grep "omavrything removed" -- scripts/uninstall.sh
  expect "uninstall.sh (system) is idempotent" 0 -- scripts/uninstall.sh

  for f in \
    /usr/bin/omavrythingd \
    /usr/bin/omavry \
    /usr/bin/omavrything-install-plugin \
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
fi

if [[ "$failures" -gt 0 ]]; then
  printf '\n%d check(s) failed\n' "$failures" >&2
  exit 1
fi
echo "smoke test passed"
