# Installation and setup

`omavrything` is at **v0.1.6**:, but released and used day to day. Install it
from a release archive or from source (§1–2). It targets
**Omarchy Quattro (4.x)** only — Hyprland, `omarchy-shell` (Quickshell), Arch
Linux. There is no backwards compatibility with earlier Omarchy releases that
used waybar.

## 1. Build the daemon and CLI

```sh
scripts/build.sh
```

This compiles the workspace and writes the `omavrythingd` daemon and the
`omavry` CLI into `bin/`, the same layout the release archive ships. See
[`development.md`](development.md) for the full build, test, and lint workflow.

## 2. Run the daemon

```sh
scripts/run-daemon.sh                              # indexes $HOME
scripts/run-daemon.sh --root /home/sam/Documents   # indexes a subtree
scripts/run-daemon.sh --auto                       # indexes all non-pseudo mounts
omavry search "report 2024"                        # query from the CLI
```

The daemon serves newline-delimited JSON over
`$XDG_RUNTIME_DIR/omavrything.sock`. See [`daemon.md`](daemon.md) for the full
flag list and the protocol.

To index NTFS partitions the daemon must run as root:

```sh
scripts/restart-all.sh --sudo
```

## 3. Install the shell plugin

The search UI is an `omarchy-shell` plugin (Quickshell/QML). The unified
installer (§6) installs, validates, enables, and loads it for the invoking user,
so `sudo scripts/install.sh` is all you need. To do it by hand from a checkout:

```sh
plugin=~/.config/omarchy/plugins/tech.alatcerdas.omavrything
mkdir -p "$plugin/shell/omavrything"
install -m644 manifest.json "$plugin/manifest.json"
install -m644 shell/omavrything/*.qml "$plugin/shell/omavrything/"
omarchy restart shell   # the shell must rescan before it knows the plugin
omarchy plugin enable tech.alatcerdas.omavrything --section center
```

## 4. Bind a hotkey

Add the binding to `~/.config/hypr/bindings.lua`:

```lua
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")
```

`SUPER + ALT + SPACE` is reassigned from the Omarchy "Apps menu" binding.

## 5. D-Bus control interface (optional)

Control and status are exposed on the system bus. The daemon can only own the
well-known name if the system-bus policy is installed:

```sh
sudo install -Dm644 data/dbus/tech.alatcerdas.omavrything.conf \
  /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf
sudo install -Dm644 data/polkit/tech.alatcerdas.omavrything.policy \
  /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy
sudo systemctl reload dbus
```

Without the policy the daemon logs `Request to own name refused by policy` and
continues serving the socket search path. See [`daemon.md`](daemon.md) for the
D-Bus methods and the polkit action.

## 6. Run as a system service (optional)

For a persistent, hardened daemon that starts at boot, `install.sh` installs the
systemd unit, the dedicated user, and the default configuration, then creates the
user, adds you to the `omavrything` group, reloads D-Bus, enables and starts the
daemon, installs and enables the shell plugin, and applies a socket ACL so the
widget works immediately:

```sh
sudo scripts/install.sh
```

The group change takes effect on your next login.

The equivalent manual steps are:

```sh
sudo install -Dm644 data/systemd/omavrythingd.service /usr/lib/systemd/system/omavrythingd.service
sudo install -Dm644 data/sysusers/omavrything.conf /usr/lib/sysusers.d/omavrything.conf
sudo install -Dm644 data/config/config.toml /etc/omavrything/config.toml
sudo systemd-sysusers
sudo systemctl daemon-reload
sudo systemctl enable --now omavrythingd
sudo usermod -aG omavrything "$USER"   # so the shell plugin can reach the socket
```

The service runs as the unprivileged `omavrything` user with only the minimal
capability set it needs, and a hardened profile
(`ProtectSystem=strict`, `NoNewPrivileges`, a syscall filter, and resource
limits). Edit `/etc/omavrything/config.toml` to choose roots, excludes, and
intervals, then `sudo systemctl restart omavrythingd`. See
[`daemon.md`](daemon.md) for the configuration keys and the hardening details.

## 7. Uninstall

`scripts/uninstall.sh` reverses the install. It is system-only, matching the
installer:

```sh
sudo scripts/uninstall.sh       # the daemon, system integration, and plugin
```

`--keep-config` keeps `/etc/omavrything/config.toml`, `--keep-user` keeps the
`omavrything` system user and group, and `--keep-state` keeps the cached index
snapshot under `/var/lib/omavrything`. The release archive ships the script, so
an archive install can be removed the same way.

## 8. Diagnostics

`scripts/doctor.sh` queries every component in one pass and prints a status
report with remediation hints. Run it with `sudo` so it can inspect the system
service, the socket, and the daemon's state:

```sh
sudo scripts/doctor.sh
```

The daemon also ships a man page: `man omavrythingd`.

## Next steps

- [`omarchy-shell-integration.md`](omarchy-shell-integration.md) — the plugin
  structure and the focus constraint that shapes it.
- [`daemon.md`](daemon.md) — the daemon, its flags, and the socket protocol.
- [`search-syntax.md`](search-syntax.md) — the query syntax and ranking.
- [`troubleshooting.md`](troubleshooting.md) — common issues (socket
  permissions, inotify limits, NTFS mounts).
