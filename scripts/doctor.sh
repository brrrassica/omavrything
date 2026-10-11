#!/usr/bin/env bash
# Diagnose an omavrything install.
#
# Queries the daemon and shell-plugin components in one pass and prints a status
# report with remediation hints. Run it with sudo so it can inspect the system
# service, the socket, and the daemon's state:
#
#   sudo scripts/doctor.sh
#
# Exit status is non-zero if a critical component is broken.
set -uo pipefail

ok() { printf '\033[1;32m[ ok ]\033[0m %s\n' "$*"; }
warn() {
  printf '\033[1;33m[warn]\033[0m %s\n' "$*"
  warns=$((warns + 1))
}
bad() {
  printf '\033[1;31m[fail]\033[0m %s\n' "$*"
  fails=$((fails + 1))
}
info() { printf '       %s\n' "$*"; }

fails=0
warns=0

if [[ "$(id -u)" -ne 0 ]]; then
  printf '\033[1;33m==>\033[0m run with sudo for full diagnostics\n' >&2
fi

printf '\033[1;34m==>\033[0m omavrything doctor\n\n'

# --- Binaries ---------------------------------------------------------------
if [[ -x /usr/bin/omavrythingd ]]; then
  if version="$(/usr/bin/omavrythingd --version 2>/dev/null)"; then
    ok "daemon: $version"
  else
    bad "daemon binary present but failed to run"
    info "reinstall it with: sudo scripts/install.sh --system"
  fi
else
  bad "daemon not installed at /usr/bin/omavrythingd"
  info "install it with: sudo scripts/install.sh --system"
fi
if [[ -x /usr/bin/omavry ]]; then
  ok "CLI: /usr/bin/omavry"
else
  warn "CLI not installed at /usr/bin/omavry"
fi

# --- systemd unit -----------------------------------------------------------
if [[ -f /usr/lib/systemd/system/omavrythingd.service ]]; then
  state="$(systemctl is-active omavrythingd.service 2>/dev/null || true)"
  enabled="$(systemctl is-enabled omavrythingd.service 2>/dev/null || true)"
  if [[ "$state" == "active" ]]; then
    ok "systemd unit active (enabled: ${enabled:-unknown})"
  else
    warn "systemd unit is ${state:-unknown} (enabled: ${enabled:-unknown})"
    info "start it with: sudo systemctl enable --now omavrythingd"
  fi
  if [[ "$enabled" == "enabled" ]]; then
    ok "systemd unit enabled at boot"
  else
    warn "systemd unit is not enabled at boot (enabled: ${enabled:-unknown})"
    info "enable it with: sudo systemctl enable omavrythingd"
  fi
else
  warn "systemd unit not installed"
fi

# --- D-Bus policy and name --------------------------------------------------
if [[ -f /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf ]]; then
  ok "D-Bus policy installed"
else
  warn "D-Bus policy missing"
fi
if command -v busctl >/dev/null 2>&1; then
  if busctl --system status tech.alatcerdas.omavrything >/dev/null 2>&1; then
    ok "D-Bus name owned: tech.alatcerdas.omavrything"
  else
    warn "D-Bus name not owned (daemon not running, or policy not loaded)"
    info "reload the bus after installing the policy: sudo systemctl reload dbus"
  fi
fi

# --- polkit -----------------------------------------------------------------
policy=/usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy
if [[ -f "$policy" ]]; then
  ok "polkit action installed"
  if grep -q 'tech.alatcerdas.omavrything.set-config' "$policy" 2>/dev/null; then
    ok "polkit set-config action installed"
  else
    warn "polkit set-config action missing (the settings panel cannot save)"
  fi
else
  warn "polkit action missing"
fi

# --- Socket -----------------------------------------------------------------
sock=/run/omavrything/omavrything.sock
if [[ -S "$sock" ]]; then
  perms="$(stat -c '%A %U:%G' "$sock" 2>/dev/null || true)"
  ok "socket present: $sock ($perms)"
else
  warn "socket not present: $sock"
  info "the daemon creates it on start; is omavrythingd running?"
fi

# --- Socket group membership ------------------------------------------------
# The invoking user must be in the omavrything group to reach the system socket.
target_user="${SUDO_USER:-${USER:-}}"
if [[ -n "$target_user" && "$target_user" != "root" ]]; then
  if id -nG "$target_user" 2>/dev/null | tr ' ' '\n' | grep -qx omavrything; then
    ok "$target_user is in the omavrything group"
  else
    warn "$target_user is not in the omavrything group"
    info "add it with: sudo usermod -aG omavrything $target_user"
    info "then log out and back in (or run 'newgrp omavrything')"
  fi
fi

# --- Index snapshot and config ---------------------------------------------
snap=/var/lib/omavrything/index.snapshot
if [[ -f "$snap" ]]; then
  ok "index snapshot present ($(du -h "$snap" 2>/dev/null | cut -f1))"
else
  warn "no index snapshot yet at $snap"
fi
if [[ -f /etc/omavrything/config.toml ]]; then
  ok "config present: /etc/omavrything/config.toml"
else
  warn "no config at /etc/omavrything/config.toml (defaults are used)"
fi

# --- Man page ---------------------------------------------------------------
if command -v man >/dev/null 2>&1 && man -w omavrythingd >/dev/null 2>&1; then
  ok "man page: $(man -w omavrythingd)"
else
  warn "man page not installed"
fi

# --- inotify limits ---------------------------------------------------------
watches="$(cat /proc/sys/fs/inotify/max_user_watches 2>/dev/null || echo '?')"
instances="$(cat /proc/sys/fs/inotify/max_user_instances 2>/dev/null || echo '?')"
ok "inotify limits: max_user_watches=$watches max_user_instances=$instances"
if [[ "$watches" =~ ^[0-9]+$ ]] && ((watches < 65536)); then
  warn "max_user_watches is low; large trees may not be fully watched"
  info "raise it with: echo fs.inotify.max_user_watches=524288 | sudo tee /etc/sysctl.d/99-omavrything.conf"
fi

# --- NTFS mounts ------------------------------------------------------------
if command -v findmnt >/dev/null 2>&1; then
  ntfs_mounts="$(findmnt -rn -o TARGET | grep '^/run/omavrything/ntfs' || true)"
  if [[ -n "$ntfs_mounts" ]]; then
    ok "NTFS mounts:"
    printf '%s\n' "$ntfs_mounts" | sed 's/^/       /'
  else
    info "no NTFS mounts under /run/omavrything/ntfs"
  fi
fi

# --- Daemon process ---------------------------------------------------------
if pgrep -x omavrythingd >/dev/null 2>&1; then
  ok "daemon process running (pids: $(pgrep -x omavrythingd | tr '\n' ' '))"
else
  warn "no omavrythingd process running"
fi

# --- Shell plugin for the invoking user -------------------------------------
if [[ -n "$target_user" && "$target_user" != "root" ]]; then
  home="$(getent passwd "$target_user" | cut -d: -f6 || true)"
  plugin="$home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
  if [[ -f "$plugin/manifest.json" ]]; then
    ok "shell plugin installed for $target_user"
  else
    warn "shell plugin not installed for $target_user"
    info "install it with: scripts/install-plugin.sh"
  fi
  shell_json="$home/.config/omarchy/shell.json"
  if [[ -f "$shell_json" ]] && grep -q 'tech.alatcerdas.omavrything' "$shell_json" 2>/dev/null; then
    ok "plugin registered in shell.json"
  else
    warn "plugin not registered in shell.json"
    info "enable it with: omarchy plugin enable tech.alatcerdas.omavrything --section center"
  fi
fi

printf '\n%d check(s) failed, %d warning(s)\n' "$fails" "$warns"
[[ "$fails" -eq 0 ]]
