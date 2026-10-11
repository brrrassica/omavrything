#!/usr/bin/env bash
# Completely and cleanly remove omavrything.
#
# Two modes:
#   sudo scripts/uninstall.sh          # system: daemon, CLI, systemd unit,
#                                      # D-Bus/polkit policy, dedicated user,
#                                      # and the shell plugin for the invoking user
#   scripts/uninstall.sh --user        # user: the shell plugin and CLI symlink for
#                                      # the current user only (no root needed)
#
# The system mode is the inverse of scripts/install.sh --system and
# scripts/install-plugin.sh, and it also cleans up a development install made by
# scripts/restart-all.sh (a user-run daemon plus a plugin copied into
# ~/.config/omarchy/plugins/).
#
# Usage:
#   sudo scripts/uninstall.sh [--keep-config] [--keep-user] [--keep-state] [--target-user <name>]
#        scripts/uninstall.sh --user [--target-user <name>]
#
# Options:
#   --system            Remove the system install (default; requires root).
#   --user              Remove only the per-user plugin and CLI symlink (no root).
#   --keep-config       Keep /etc/omavrything/config.toml (and the directory).
#   --keep-user         Keep the `omavrything` system user and group.
#   --keep-state        Keep /var/lib/omavrything (the cached index snapshot).
#   --target-user <name>
#                       Act on <name> instead of the user that invoked sudo
#                       (defaults to $SUDO_USER, then $USER).
#   -h, --help          Show this help.
#
# The plugin is removed through `omarchy plugin remove` (run as the target user)
# so omarchy's registry and ~/.config/omarchy/shell.json are updated, not just
# the plugin directory. The shell is restarted at the end so the widget
# disappears, and the system mode refuses to exit while a daemon is still
# running.
set -euo pipefail

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die() {
  printf '\033[1;31m==>\033[0m %s\n' "$*" >&2
  exit 1
}

mode="system"
keep_config=0
keep_user=0
keep_state=0
target_user=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --system)
      mode="system"
      shift
      ;;
    --user)
      mode="user"
      shift
      ;;
    --keep-config)
      keep_config=1
      shift
      ;;
    --keep-user)
      keep_user=1
      shift
      ;;
    --keep-state)
      keep_state=1
      shift
      ;;
    --target-user)
      [[ $# -ge 2 ]] || die "--target-user requires a username"
      target_user="$2"
      shift 2
      ;;
    -h | --help)
      sed -n '2,35p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

# The system mode removes system files, so require root up front. The user mode
# only touches the invoking user's files and must work without root.
if [[ "$mode" == "system" && "$(id -u)" -ne 0 ]]; then
  die "the system mode must be run as root (use sudo); use --user for a plugin-only install"
fi

# Resolve the user whose per-user plugin and symlink we should remove. When run
# under sudo, $SUDO_USER is the real invoker; otherwise fall back to $USER.
if [[ -z "$target_user" ]]; then
  target_user="${SUDO_USER:-${USER:-$(id -un)}}"
fi
if [[ -z "$target_user" || "$target_user" == "root" ]]; then
  warn "could not determine a non-root user; skipping the per-user plugin"
  target_user=""
fi

user_home=""
user_uid=""
if [[ -n "$target_user" ]]; then
  if [[ "$mode" == "user" ]]; then
    # We are the target user: honour the live environment (HOME may be
    # overridden) rather than the passwd database.
    user_home="${HOME:-$(getent passwd "$target_user" | cut -d: -f6 || true)}"
    user_uid="$(id -u)"
  else
    user_home="$(getent passwd "$target_user" | cut -d: -f6 || true)"
    user_uid="$(id -u "$target_user" 2>/dev/null || true)"
  fi
fi

# Run a command as the target user with a usable session environment. In system
# mode we are root, so drop to the target user with runuser and set the session
# bus and runtime dir explicitly (omarchy talks to the running shell over them).
# In user mode we already are the target user, so run the command directly.
as_user() {
  [[ -n "$target_user" ]] || return 1
  if [[ "$mode" == "user" ]]; then
    "$@"
  else
    runuser -u "$target_user" -- env \
      HOME="$user_home" \
      USER="$target_user" \
      LOGNAME="$target_user" \
      XDG_RUNTIME_DIR="/run/user/$user_uid" \
      DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$user_uid/bus" \
      PATH="/usr/share/omarchy/bin:/usr/local/bin:/usr/bin:/bin" \
      "$@"
  fi
}

if [[ "$mode" == "system" ]]; then
  # 1. Stop and disable the system service, if one was installed.
  if systemctl list-unit-files omavrythingd.service >/dev/null 2>&1; then
    log "stopping and disabling omavrythingd.service"
    systemctl disable --now omavrythingd.service >/dev/null 2>&1 || true
  fi

  # 2. Kill any running daemon. A development daemon started by
  # scripts/restart-all.sh runs as the user; a packaged one runs as root. Try
  # both, then escalate to SIGKILL, and verify it is really gone.
  if pgrep -x omavrythingd >/dev/null 2>&1; then
    log "stopping running daemon"
    pkill -x omavrythingd 2>/dev/null || true
    for _ in $(seq 1 50); do
      pgrep -x omavrythingd >/dev/null 2>&1 || break
      sleep 0.1
    done
    if pgrep -x omavrythingd >/dev/null 2>&1; then
      log "daemon still alive; sending SIGKILL"
      pkill -9 -x omavrythingd 2>/dev/null || true
      for _ in $(seq 1 50); do
        pgrep -x omavrythingd >/dev/null 2>&1 || break
        sleep 0.1
      done
    fi
  fi

  # 3. Remove the system-wide files installed by install.sh --system.
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

  # 4. Remove the configuration unless asked to keep it.
  if [[ "$keep_config" == 1 ]]; then
    log "keeping /etc/omavrything/config.toml"
  else
    log "removing /etc/omavrything"
    rm -rf /etc/omavrything
  fi
fi

# 5. Remove the shell plugin (the widget). Use omarchy's own command so its
# registry and shell.json are updated; fall back to removing the directories
# directly. Both the current id and the older `omavrything` id are handled.
if [[ -n "$target_user" ]]; then
  log "removing shell plugin for $target_user"
  if as_user sh -c 'command -v omarchy >/dev/null 2>&1'; then
    as_user omarchy plugin remove tech.alatcerdas.omavrything --yes >/dev/null 2>&1 ||
      warn "omarchy plugin remove tech.alatcerdas.omavrything failed; removing files directly"
    as_user omarchy plugin remove omavrything --yes >/dev/null 2>&1 || true
  else
    warn "omarchy not found; removing plugin files directly"
  fi

  # Direct removal as a fallback / for a stale directory omarchy no longer knows.
  rm -rf "$user_home/.config/omarchy/plugins/tech.alatcerdas.omavrything"
  rm -rf "$user_home/.config/omarchy/plugins/omavrything"

  # Drop any leftover bar-layout entry and plugin registration from shell.json.
  shell_json="$user_home/.config/omarchy/shell.json"
  if [[ -f "$shell_json" ]] && command -v python3 >/dev/null 2>&1; then
    as_user python3 - "$shell_json" <<'PY' || warn "could not clean shell.json"
import json, sys
path = sys.argv[1]
ids = {"omavrything", "tech.alatcerdas.omavrything"}
try:
    with open(path) as f:
        data = json.load(f)
except Exception:
    sys.exit(0)
changed = False
layout = data.get("bar", {}).get("layout", {})
for section, entries in list(layout.items()):
    if isinstance(entries, list):
        kept = [e for e in entries if not (isinstance(e, dict) and e.get("id") in ids)]
        if len(kept) != len(entries):
            layout[section] = kept
            changed = True
plugins = data.get("plugins")
if isinstance(plugins, list):
    kept = [p for p in plugins if not (isinstance(p, dict) and p.get("id") in ids) and p not in ids]
    if len(kept) != len(plugins):
        data["plugins"] = kept
        changed = True
if changed:
    with open(path, "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")
PY
  fi

  # The CLI symlink and the daemon's per-user socket/log.
  rm -f "$user_home/.local/bin/omavry"
  if [[ -n "$user_uid" ]]; then
    rm -f "/run/user/$user_uid/omavrything.sock" \
      "/run/user/$user_uid/omavrything.sock.lock"
  fi
  rm -rf "$user_home/.local/state/omavrything"
fi

if [[ "$mode" == "system" ]]; then
  # 6. Remove the runtime and state directories owned by the service. The index
  # snapshot under /var/lib/omavrything is the only persistent state, so
  # --keep-state preserves it (and the runtime dir is always removed).
  if [[ "$keep_state" == 1 ]]; then
    log "keeping /var/lib/omavrything (index snapshot)"
    rm -rf /run/omavrything
  else
    log "removing runtime and state directories"
    rm -rf /run/omavrything
    rm -rf /var/lib/omavrything
  fi

  # 7. Remove the dedicated user and group created by the sysusers drop-in.
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

  # 8. Reload systemd and D-Bus so the removed unit and policy take effect.
  systemctl daemon-reload >/dev/null 2>&1 || true
  if command -v dbus-send >/dev/null 2>&1; then
    dbus-send --system --type=method_call --dest=org.freedesktop.DBus \
      /org/freedesktop/DBus org.freedesktop.DBus.ReloadConfig >/dev/null 2>&1 || true
  fi
fi

# 9. Restart the shell so the widget is dropped from the running bar.
if [[ -n "$target_user" ]]; then
  log "restarting the omarchy shell"
  if as_user sh -c 'command -v omarchy >/dev/null 2>&1'; then
    as_user omarchy restart shell >/dev/null 2>&1 ||
      warn "could not restart the shell; run 'omarchy restart shell' as $target_user"
  elif as_user sh -c 'command -v omarchy-shell >/dev/null 2>&1'; then
    as_user omarchy-shell shell rescanPlugins >/dev/null 2>&1 ||
      warn "could not rescan plugins; restart the shell manually"
  fi
fi

# 10. In system mode, refuse to exit while a daemon is still running.
if [[ "$mode" == "system" ]] && pgrep -x omavrythingd >/dev/null 2>&1; then
  die "a daemon is still running (pids: $(pgrep -x omavrythingd | tr '\n' ' ')); remove it manually"
fi

if [[ "$mode" == "user" ]]; then
  if [[ -n "$target_user" ]]; then
    log "omavrything shell plugin removed for $target_user"
  else
    log "omavrything shell plugin removed"
  fi
else
  log "omavrything removed; no daemon is running"
fi
