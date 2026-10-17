# omavrything

<p align="center">
  <img src="assets/file-managers.jpg" alt="File managers" width="720">
</p>

Are you a Wind*ws ~~user~~ survivor? Do you consider instant file searches with
Voidtool's Everything indispensable and you miss it when daily driving Omarchy?

Omavrything is a local file search for **Omarchy Quattro (4.x)**, with
UX "inspired" by [Voidtools Everything](https://www.voidtools.com/).

The UI lives in the `omarchy-shell` (Quickshell/Qt) as a persistent search bar;
typing opens a centered, high-transparency scratchpad with a results table. A root
system daemon (`omavrythingd`) keeps an in-memory index of all mounted
filesystems — using directory traversal + inotify for Linux filesystems
(btrfs/ext4/xfs/f2fs) and the MFT for NTFS partitions.

> **Status:** v0.2.0-rc9. See [`docs/`](docs/) and the
> [changelog](CHANGELOG.md).

## Install

Download the latest `omavrything-<version>-<arch>.zip` from the
[releases page](https://github.com/brrrassica/omavrything/releases), extract it,
and run:

```sh
unzip omavrything-*.zip
cd omavrything-*
sudo ./scripts/install.sh
```

One command installs the daemon, CLI, systemd unit, D-Bus policy, polkit action,
man page, default config, and shell plugin; creates the `omavrything` user; adds
you to its group; reloads D-Bus; starts the daemon; and enables the plugin. The
group change applies on your next login.

The archive holds the `omavrythingd` daemon and `omavry` CLI under `bin/`, system
files under `data/`, the plugin under `shell/`, scripts under `scripts/`, and
docs under `docs/`.

An AUR package is prepared under [`packaging/`](packaging/) but is **not
published yet**; build it locally with `makepkg -si`, or use the release archive
or a source build.

## Quick start (from source)

```sh
scripts/build.sh
sudo scripts/install.sh
```

Then bind a hotkey in `~/.config/hypr/bindings.lua`:

```lua
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")
```

See [`docs/install.md`](docs/install.md) for the full setup, including the D-Bus control interface.

## Requirements

- **Omarchy Quattro (4.x)** — Hyprland and `omarchy-shell` (Quickshell).
- The shell plugin needs **no privileges**.
- **License:** MIT — see [`LICENSE`](LICENSE).

### Dependencies

| Dependency | Needed for | Notes |
| --- | --- | --- |
| Omarchy Quattro (4.x), Hyprland, `omarchy-shell` | the shell plugin | already present on Quattro |
| Rust stable with `cargo` | building from source | not needed for a release archive |
| `glibc`, `gcc-libs` | running the binaries | standard Arch base packages |
| `systemd` | the packaged daemon service | optional; a user-run daemon needs none |
| `polkit` | the D-Bus control interface | gates `tech.alatcerdas.omavrything.control` |


### Privileges and privacy

The daemon (`omavrythingd`) is a system service but does not run as full root: it
runs as the dedicated `omavrything` user with only the minimal capabilities it
needs (raw-device read, and a read-only NTFS mount when the kernel driver cannot
read the MFT directly). It indexes mounted filesystems by traversal + inotify and
reads NTFS partitions from the MFT on the block device.

**Nothing is sent anywhere.** The UI talks to the daemon over a local Unix socket
(`/run/omavrything/omavrything.sock` for the system service, or
`$XDG_RUNTIME_DIR/omavrything.sock` for a user-run daemon). There is no network
client and no telemetry.

## Documentation

Rendered at **<https://omavrything.alatcerdas.tech>** (mirror:
<https://brrrassica.github.io/omavrything>), generated from these sources by
`site/build.py`.

- [`CHANGELOG.md`](CHANGELOG.md) — release notes.
- [`docs/install.md`](docs/install.md) — installation and setup.
- [`docs/daemon.md`](docs/daemon.md) — the daemon, flags, and socket protocol.
- [`docs/search-syntax.md`](docs/search-syntax.md) — query syntax and ranking.
- [`docs/omarchy-shell-integration.md`](docs/omarchy-shell-integration.md) — the shell plugin.
- [`docs/architecture.md`](docs/architecture.md) — technical design.
- [`docs/decisions.md`](docs/decisions.md) — decision log (ADRs).
- [`docs/development.md`](docs/development.md) — build, test, and profiling.
- [`docs/profiling.md`](docs/profiling.md) — CPU wakeups, idle cost, and rescan tuning.
- [`docs/memory-optimization.md`](docs/memory-optimization.md) — memory model.
- [`docs/ffi-audit.md`](docs/ffi-audit.md) — FFI and raw-device access audit.
- [`docs/e2e-checklist.md`](docs/e2e-checklist.md) — manual release checklist.
- [`docs/troubleshooting.md`](docs/troubleshooting.md) — common issues (socket, inotify, NTFS).

## Removal

`scripts/uninstall.sh` removes everything the installer added (system-only,
matching the installer):

```sh
sudo scripts/uninstall.sh       # the daemon, system integration, and plugin
```

`--keep-config` keeps `/etc/omavrything/config.toml`, `--keep-user` keeps the
`omavrything` user and group, and `--keep-state` keeps the index snapshot under
`/var/lib/omavrything`. Remove the hotkey binding from
`~/.config/hypr/bindings.lua` if you added one.

With the AUR package (once published): `sudo pacman -Rns omavrything`.

## Target platform

Omarchy Quattro (4.x) only and later versions that uses omarchy-shell.
No backwards compatibility with earlier Omarchy releases which used waybar.

## Attribution

Thank you for the inspiration and years of blazing-fast searches to David Carpenter
at www.voidtools.com, where I've saved maybe weeks of my life from needlessly
organizing files and scrambling for files in tight moments in my life.

## License

MIT. See [`LICENSE`](LICENSE).
