# Installation and setup

`omavrything` is **pre-alpha** and currently installed from source. It targets
**Omarchy Quattro (4.x)** only — Hyprland, `omarchy-shell` (Quickshell), Arch
Linux. There is no backwards compatibility with earlier Omarchy releases that
used waybar.

## 1. Build the daemon and CLI

```sh
cargo build --release
```

This produces the `omavrythingd` daemon and the `omavry` CLI. See
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

The search UI is an `omarchy-shell` plugin (Quickshell/QML). Install and enable
it with:

```sh
scripts/install-plugin.sh
omarchy plugin enable omavrything --section center
```

## 4. Bind a hotkey

Add the binding to `~/.config/hypr/bindings.lua`:

```lua
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell omavrything focus")
```

`SUPER + ALT + SPACE` is reassigned from the Omarchy "Apps menu" binding.

## 5. D-Bus control interface (optional)

Control and status are exposed on the system bus. The daemon can only own the
well-known name if the system-bus policy is installed:

```sh
sudo install -Dm644 data/dbus/org.omarchy.omavrything.conf \
  /usr/share/dbus-1/system.d/org.omarchy.omavrything.conf
sudo install -Dm644 data/polkit/org.omarchy.omavrything.policy \
  /usr/share/polkit-1/actions/org.omarchy.omavrything.policy
sudo systemctl reload dbus
```

Without the policy the daemon logs `Request to own name refused by policy` and
continues serving the socket search path. See [`daemon.md`](daemon.md) for the
D-Bus methods and the polkit action.

## 6. Run as a system service (optional)

For a persistent, hardened daemon that starts at boot, install the systemd unit,
the dedicated user, and the default configuration:

```sh
sudo install -Dm644 data/systemd/omavrythingd.service /usr/lib/systemd/system/omavrythingd.service
sudo install -Dm644 data/sysusers/omavrything.conf /usr/lib/sysusers.d/omavrything.conf
sudo install -Dm644 data/config/config.toml /etc/omavrything/config.toml
sudo systemd-sysusers
sudo systemctl daemon-reload
sudo systemctl enable --now omavrythingd
sudo usermod -aG omavrything "$USER"   # so the shell plugin can reach the socket
```

The service runs as the unprivileged `omavrything` user with only
`CAP_DAC_READ_SEARCH` and `CAP_SYS_ADMIN`, and a hardened profile
(`ProtectSystem=strict`, `NoNewPrivileges`, a syscall filter, and resource
limits). Edit `/etc/omavrything/config.toml` to choose roots, excludes, and
intervals, then `sudo systemctl restart omavrythingd`. See
[`daemon.md`](daemon.md) for the configuration keys and the hardening details.

## Next steps

- [`omarchy-shell-integration.md`](omarchy-shell-integration.md) — the plugin
  structure and the focus constraint that shapes it.
- [`daemon.md`](daemon.md) — the daemon, its flags, and the socket protocol.
- [`search-syntax.md`](search-syntax.md) — the query syntax and ranking.
- [`troubleshooting.md`](troubleshooting.md) — common issues (socket
  permissions, inotify limits, NTFS mounts).
