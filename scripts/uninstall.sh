#!/usr/bin/env bash
# Completely and cleanly remove omavrything: the daemon, the CLI, the systemd
# unit, the D-Bus/polkit policy, the dedicated user, and the omarchy-shell
# plugin (the search widget) for the invoking user.
#
# This is the inverse of scripts/install.sh (--system and --user) and
# scripts/install-plugin.sh. It needs root, so run it with sudo:
#
#   sudo scripts/uninstall.sh
#
# Usage:
#   sudo scripts/uninstall.sh [--keep-config] [--keep-user] [--user <name>]
#
# Options:
#   --keep-config   Keep /etc/omavrything/config.toml (and the directory).
#   --keep-user     Keep the `omavrything` system user and group.
#   --user <name>   Remove the shell plugin for <name> instead of the user that
#                   invoked sudo (defaults to $SUDO_USER, then $USER).
#   -h, --help      Show this help.
#
# Everything else is removed: /usr/bin/omavrythingd, /usr/bin/omavry,
# /usr/bin/omavrything-install-plugin, the systemd unit, the sysusers drop-in,
# the D-Bus and polkit policy, /usr/share/omavrything, the license and docs,
# /run/omavrything, /var/lib/omavrything, the user's plugin directory and the
# ~/.local/bin/omavry symlink, and any running daemon (user- or root-owned).
set -euo pipefail

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die() {
  printf '\033[1;31m==>\033[0m %s\n' "$*" >&2
  exit 1
}

keep_config=0
keep_user=0
target_user=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --keep-config)
      keep_config=1
      shift
      ;;
    --keep-user)
      keep_user=1
      shift
      ;;
    --user)
      [[ $# -ge 2 ]] || die "--user requires a username"
      target_user="$2"
      shift 2
      ;;
    -h | --help)
      sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

# The whole point is removing system files, so require root up front.
if [[ "$(id -u)" -ne 0 ]]; then
  die "this script must be run as root (use sudo)"
fi

# Resolve the user whose per-user plugin and symlink we should remove. When run
# under sudo, $SUDO_USER is the real invoker; otherwise fall back to $USER.
if [[ -z "$target_user" ]]; then
  target_user="${SUDO_USER:-${USER:-}}"
fi
if [[ -z "$target_user" || "$target_user" == "root" ]]; then
  warn "could not determine a non-root user; skipping the per-user plugin"
  target_user=""
fi

# 1. Stop and disable the system service, then kill any stray daemon. A daemon
# started by hand (scripts/restart-all.sh) may be user- or root-owned, so try
# both. SIGTERM lets it write a snapshot and exit cleanly.
if systemctl list-unit-files omavrythingd.service >/dev/null 2>&1; then
  log "stopping and disabling omavrythingd.service"
  systemctl disable --now omavrythingd.service >/dev/null 2>&1 || true
fi

if pgrep -x omavrythingd >/dev/null 2>&1; then
  log "stopping running daemon"
  pkill -x omavrythingd 2>/dev/null || true
  for _ in $(seq 1 50); do
    pgrep -x omavrythingd >/dev/null 2>&1 || break
    sleep 0.1
  done
  if pgrep -x omavrythingd >/dev/null 2>&1; then
    pkill -9 -x omavrythingd 2>/dev/null || true
  fi
fi

# 2. Remove the system-wide files installed by install.sh --system.
log "removing system binaries and data"
rm -f /usr/bin/omavrythingd
rm -f /usr/bin/omavry
rm -f /usr/bin/omavrything-install-plugin
rm -f /usr/lib/systemd/system/omavrythingd.service
rm -f /usr/lib/sysusers.d/omavrything.conf
rm -f /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf
rm -f /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy
rm -rf /usr/share/omavrything
rm -rf /usr/share/licenses/omavrything
rm -rf /usr/share/doc/omavrything

# 3. Remove the configuration unless asked to keep it.
if [[ "$keep_config" == 1 ]]; then
  log "keeping /etc/omavrything/config.toml"
else
  log "removing /etc/omavrything"
  rm -rf /etc/omavrything
fi

# 4. Remove the per-user shell plugin and the CLI symlink. The plugin lives in
# the user's config dir; the symlink in their bin dir. Resolve both from the
# target user's home so this works when the script itself runs as root.
if [[ -n "$target_user" ]]; then
  user_home="$(getent passwd "$target_user" | cut -d: -f6 || true)"
  if [[ -n "$user_home" && -d "$user_home" ]]; then
    log "removing shell plugin for $target_user"
    rm -rf "$user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
    rm -f "$user_home/.local/bin/omavry"
    # A stale socket/lock in the user's runtime dir would otherwise linger.
    user_uid="$(id -u "$target_user" 2>/dev/null || true)"
    if [[ -n "$user_uid" ]]; then
      rm -f "/run/user/$user_uid/omavrything.sock" \
        "/run/user/$user_uid/omavrything.sock.lock"
    fi
    # The daemon log written by scripts/restart-all.sh.
    rm -rf "$user_home/.local/state/omavrything"
  else
    warn "home directory for $target_user not found; skipping per-user cleanup"
  fi
fi

# 5. Remove the runtime and state directories owned by the service.
log "removing runtime and state directories"
rm -rf /run/omavrything
rm -rf /var/lib/omavrything

# 6. Remove the dedicated user and group created by the sysusers drop-in.
# userdel removes the group of the same name when it is the user's primary
# group; groupdel is a fallback for a group left behind.
if [[ "$keep_user" == 1 ]]; then
  log "keeping the omavrything user and group"
else
  if id omavrything >/dev/null 2>&1; then
    log "removing the omavrything user and group"
    userdel omavrything >/dev/null 2>&1 || warn "could not remove the omavrything user"
  fi
  if getent group omavrything >/dev/null 2>&1; then
    groupdel omavrything >/dev/null 2>&1 || warn "could not remove the omavrything group"
  fi
fi

# 7. Reload systemd and D-Bus so the removed unit and policy take effect.
systemctl daemon-reload >/dev/null 2>&1 || true
if command -v dbus-send >/dev/null 2>&1; then
  dbus-send --system --type=method_call --dest=org.freedesktop.DBus \
    /org/freedesktop/DBus org.freedesktop.DBus.ReloadConfig >/dev/null 2>&1 || true
fi

log "omavrything removed"
if [[ -n "$target_user" ]]; then
  echo "Restart the shell to drop the plugin: omarchy restart shell"
fi
