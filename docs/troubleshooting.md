# Troubleshooting

Common issues and how to diagnose them. Start with the logs: the daemon writes
structured events to stderr, which journald captures for the system service.

```sh
journalctl -u omavrythingd -f          # system service
tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/omavrything/daemon.log"  # user-run daemon
```

The default log level is `error`, so a healthy daemon is quiet. Raise the
verbosity with `--log-level info` (or `debug`), the `log_level` config key, or
`OMAVRYTHING_LOG=omavrythingd=debug`.

For a one-shot diagnosis of every component, run:

```sh
sudo scripts/doctor.sh
```

It checks the daemon, the systemd unit, the D-Bus and polkit policy, the socket,
the index snapshot, the inotify limits, and the shell plugin, and prints
remediation hints for anything that is broken.

## The shell plugin cannot reach the daemon

**Symptom:** the scratchpad opens but shows no results, or `omavry search` prints
`cannot connect to the daemon`.

The daemon and the client must agree on the socket path, and the client must be
allowed to connect.

1. **Is the daemon running?**

   ```sh
   pgrep -x omavrythingd
   systemctl status omavrythingd
   ```

2. **Which socket is it using?** The system service uses
   `/run/omavrything/omavrything.sock`; a user-run daemon uses
   `$XDG_RUNTIME_DIR/omavrything.sock` (falling back to `/tmp/omavrything.sock`).
   A user-run daemon and the system service do **not** share a socket, so a
   client pointed at one will not see the other.

   ```sh
   ls -l /run/omavrything/omavrything.sock
   ls -l "${XDG_RUNTIME_DIR:-/tmp}/omavrything.sock"
   ```

3. **Socket permissions.** The socket is created mode `0660`, group
   `omavrything`. Your user must be in that group to connect to the system
   service:

   ```sh
   id -nG | tr ' ' '\n' | grep omavrything
   sudo usermod -aG omavrything "$USER"
   ```

   Group membership only applies to **new** sessions: log out and back in. The
   installer also applies a socket ACL for the current session, so the widget
   works immediately without a re-login.

4. **A stale root-owned socket or lock.** If the daemon was once run as root, the
   socket and its `<socket>.lock` file are root-owned and a user-run daemon
   cannot replace them. `scripts/restart-all.sh` removes them with `sudo`; to do
   it by hand:

   ```sh
   sudo rm -f "${XDG_RUNTIME_DIR:-/tmp}/omavrything.sock" \
              "${XDG_RUNTIME_DIR:-/tmp}/omavrything.sock.lock"
   ```

5. **Only one daemon per socket.** The daemon takes an exclusive `flock` on
   `<socket>.lock`, so a second instance refuses to start. Stop the first one
   (`pkill -x omavrythingd`) before starting another.

## D-Bus control is unavailable

**Symptom:** `omavry status` or `omavry config` fails with
`ServiceUnknown: The name is not activatable`, or the log shows
`Request to own name refused by policy`.

The daemon can only own the well-known name `tech.alatcerdas.omavrything` if the
system-bus policy is installed **and the running bus has reloaded it**. A bus
that was already running before the install does not see a new policy until it
is reloaded:

```sh
sudo systemctl reload dbus
```

`scripts/install.sh` installs the policy and reloads the bus for you.
To do it by hand:

```sh
sudo install -Dm644 data/dbus/tech.alatcerdas.omavrything.conf \
  /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf
sudo install -Dm644 data/polkit/tech.alatcerdas.omavrything.policy \
  /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy
sudo systemctl reload dbus
```

A **user-run** daemon cannot own the name at all (the policy only permits the
`omavrything` user and root), so it serves the socket search path but not D-Bus
control. Use the system service for `status`/`config`/`rescan`/`mount`.

`Rescan` and `MountNtfs`, and the privileged `SetConfig` keys (`excludes`,
`ntfs`, `scan_interval`, `snapshot_interval`, `log_level`), are gated by the
`tech.alatcerdas.omavrything.control` polkit action (`auth_admin_keep`) and
prompt for authentication. The cosmetic `background_opacity` key uses the
separate `tech.alatcerdas.omavrything.set-config` action (`allow_active=yes`), so
the settings panel can save the tile opacity without a prompt and without
granting `MountNtfs`. Both actions carry the
`org.freedesktop.policykit.owner` annotation for `unix-user:omavrything`, so the
daemon (which runs as that unprivileged user, not root) is allowed to query
polkit for the caller's authorization.

## inotify watch limits

**Symptom:** the log reports `max_user_watches` exhaustion, and new or renamed
files do not appear until the next rescan.

The daemon watches every indexed directory recursively. On a large tree the
default `fs.inotify.max_user_watches` can be exhausted.

```sh
sysctl fs.inotify.max_user_watches
cat /proc/sys/fs/inotify/max_user_watches
```

Raise it persistently:

```sh
echo 'fs.inotify.max_user_watches=524288' | sudo tee /etc/sysctl.d/99-omavrything.conf
sudo sysctl --system
```

When watches are exhausted the daemon logs the affected directories and keeps
serving; the periodic idle-gated rescan is the safety net, so results still
converge, just not instantly. You can also reduce the watched set with
`excludes` (for example `**/.cache/**`, `**/node_modules/**`) or disable live
updates entirely with `--no-watch`.

## NTFS partitions are not indexed

**Symptom:** NTFS files never appear in results, or the log shows a permission
error reading the raw device.

Indexing NTFS reads the **MFT directly from the raw block device**, which
requires raw-device read access (or root). The system service has that
capability; a user-run daemon does not.

1. **Run with the right privileges.** For a user-run daemon:

   ```sh
   scripts/restart-all.sh --sudo
   ```

   The script authenticates `sudo` up front and verifies the daemon really is
   root, because a backgrounded `sudo` that cannot prompt would otherwise leave a
   user-owned daemon that silently cannot read NTFS devices.

2. **Is NTFS enabled?** Check `ntfs = true` in `/etc/omavrything/config.toml`
   (or that `--no-ntfs` is not set). `omavry config get` shows the live value.

3. **Is the partition detected?** The daemon detects NTFS with `blkid` (falling
   back to a boot-sector OEM check). Confirm the device is visible:

   ```sh
   lsblk -f
   sudo blkid | grep -i ntfs
   ```

4. **Mounting.** Mounting prefers **udisks2** (`udisksctl mount -o ro`) so the
   desktop sees the partition, and falls back to a private read-only **`ntfs3`**
   mount at `/run/omavrything/ntfs/<uuid>`. If neither works, indexing still
   succeeds (the MFT is read from the raw device) but the files are not mounted
   for opening. Check that `udisks2` is installed and running, and that the
   `ntfs3` kernel module is available (`modprobe ntfs3`).

5. **Auto-mount policy.** Previously indexed partitions are remembered by UUID in
   a state file (`/var/lib/omavrything/ntfs.state` as root, else
   `$XDG_STATE_HOME/omavrything/ntfs.state`). A remembered partition is
   auto-mounted and indexed at startup; a newly seen one is only logged as an
   offer and is indexed on demand via `MountNtfs`:

   ```sh
   omavry mount <uuid>     # mounts and indexes the partition (D-Bus, polkit)
   ```

6. **A partition that is not mounted at startup** is validated by its mount
   point's device identity, so its MFT-indexed entries are dropped and re-indexed
   when it is mounted again. Mount it (or let the auto-mount policy do so) and
   rescan.

## Search feels slow

**Symptom:** typing lags, or a query takes hundreds of milliseconds.

The daemon must be built in **release** mode. A debug build is roughly 10x
slower, which turns a sub-millisecond query into hundreds of milliseconds.

```sh
cargo build --release -p omavrythingd
```

`scripts/run-daemon.sh` and `scripts/restart-all.sh --rebuild` always build the
release binary. To see where the time goes, set `OMAVRYTHING_PROFILE=1` and every
search logs a stage-by-stage breakdown:

```sh
OMAVRYTHING_PROFILE=1 scripts/run-daemon.sh
# search "report": total=0.87ms parse=0.00ms scan=0.85ms sort=0.01ms materialize=0.01ms scanned=50401 matched=50000 parallel
```

The `scan` stage dominates and scales linearly with the index size (1M records
≈ 6 ms, 2M ≈ 12 ms, 3M ≈ 18 ms). If `total` is far above that, the daemon is
likely a debug build or the index is much larger than expected. See
[`daemon.md`](daemon.md#profiling-query-performance) for the standalone profiler.

## The index is stale or wrong

- **Live updates lag.** inotify events are applied within a second; if they are
  not, see [inotify watch limits](#inotify-watch-limits).
- **A rescan never runs.** The periodic rescan is **idle-gated**: it only runs
  when the system is idle, and only every `scan_interval` seconds (default 900;
  0 disables). Under sustained load it is deferred by design. Trigger one
  explicitly with the D-Bus `Rescan` method.
- **A corrupt snapshot.** The snapshot is written atomically and the previous one
  is kept as `<path>.prev`, so a corrupt or truncated snapshot falls back to the
  last good one and ultimately to a fresh scan. To force a cold scan, stop the
  daemon and remove the snapshot:

  ```sh
  sudo rm -f /var/lib/omavrything/index.snapshot \
             /var/lib/omavrything/index.snapshot.prev
  ```

## The search bar does not appear

The UI is an `omarchy-shell` plugin. The unified installer installs, validates,
enables, and loads it, and restarts the shell:

```sh
sudo scripts/install.sh
```

A plugin **rescan** is not enough: the QML engine keeps the previously compiled
component, so changed QML only takes effect after the shell process restarts.
`scripts/install.sh` restarts the shell for you.

If the hotkey does nothing, confirm the binding is present in
`~/.config/hypr/bindings.lua`:

```lua
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")
```

and that the IPC target responds:

```sh
omarchy-shell tech.alatcerdas.omavrything ping
```

## The daemon will not start

- **`Address already in use` / lock held.** Another daemon owns the socket. Stop
  it first (`pkill -x omavrythingd`).
- **`Permission denied` on the socket or snapshot path.** A root-owned socket or
  lock is blocking a user-run daemon; remove it with `sudo` (see above). The
  system service uses `/run/omavrything` and `/var/lib/omavrything`, created by
  `RuntimeDirectory=`/`StateDirectory=`.
- **Config rejected.** Unknown keys in `/etc/omavrything/config.toml` are
  rejected so a typo is reported rather than silently ignored. The error names
  the offending key. See [`daemon.md`](daemon.md#configuration-file) for the
  valid keys.
- **`systemd-analyze security omavrythingd`** reports the hardening profile if
  the service fails to start under its sandbox; check `journalctl -u
  omavrythingd` for a syscall or path denial.

## Getting more help

- [`install.md`](install.md) — installation and setup.
- [`daemon.md`](daemon.md) — flags, configuration, protocol, and D-Bus.
- [`search-syntax.md`](search-syntax.md) — query syntax.
- [`omarchy-shell-integration.md`](omarchy-shell-integration.md) — the plugin and
  the focus constraint.
