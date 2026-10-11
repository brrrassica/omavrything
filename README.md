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

> **Status:** v0.1.0. See [`docs/`](docs/) for documentation.

## Install from a release

Download the latest `omavrything-<version>-<arch>.zip` from the
[releases page](https://github.com/brrrassica/omavrything/releases), extract it,
and run the installer:

```sh
unzip omavrything-*.zip
cd omavrything-*
sudo ./scripts/install.sh --system   # daemon, CLI, systemd unit, D-Bus, polkit
./scripts/install-plugin.sh          # shell plugin for the current user
omarchy plugin enable tech.alatcerdas.omavrything --section center
```

The archive contains the `omavrythingd` daemon and the `omavry` CLI under `bin/`,
the system integration files under `data/`, the shell plugin under `shell/`, the
install scripts under `scripts/`, and the documentation under `docs/`.

On Arch/Omarchy you can also install from the AUR (see
[`packaging/`](packaging/)):

```sh
yay -S omavrything
omavrything-install-plugin
omarchy plugin enable tech.alatcerdas.omavrything --section center
```

## Quick start (from source)

```sh
cargo build --release
scripts/run-daemon.sh
scripts/install-plugin.sh
omarchy plugin enable tech.alatcerdas.omavrything --section center
```

Then bind a hotkey in `~/.config/hypr/bindings.lua`:

```lua
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")
```

See [`docs/install.md`](docs/install.md) for the full setup, including the
optional D-Bus control interface.

## Documentation

- [`docs/install.md`](docs/install.md) — installation and setup.
- [`docs/daemon.md`](docs/daemon.md) — the daemon, its flags, and the socket protocol.
- [`docs/search-syntax.md`](docs/search-syntax.md) — query syntax and ranking.
- [`docs/omarchy-shell-integration.md`](docs/omarchy-shell-integration.md) — the shell plugin.
- [`docs/architecture.md`](docs/architecture.md) — technical design.
- [`docs/decisions.md`](docs/decisions.md) — decision log (ADRs).
- [`docs/development.md`](docs/development.md) — build, test, and query profiling.
- [`docs/profiling.md`](docs/profiling.md) — CPU wakeups, idle cost, rescan tuning, and the memory cap.
- [`docs/memory-optimization.md`](docs/memory-optimization.md) — memory model and optimization analysis.
- [`docs/ffi-audit.md`](docs/ffi-audit.md) — FFI and raw-device access audit.
- [`docs/e2e-checklist.md`](docs/e2e-checklist.md) — manual end-to-end release checklist.
- [`docs/troubleshooting.md`](docs/troubleshooting.md) — common issues (socket, inotify, NTFS).

## Releasing

Releases are built by CI. Pushing a `v*` tag (e.g. `v0.1.0`) runs
[`.github/workflows/release.yml`](.github/workflows/release.yml), which builds the
release archive with [`scripts/package-release.sh`](scripts/package-release.sh)
and creates a GitHub release with the zip attached. The same archive can be built
locally with `scripts/package-release.sh`.

## Target platform

Omarchy Quattro (4.x) only, maybe several versions up. Hyprland, `omarchy-shell` (Quickshell), Arch Linux.
No backwards compatibility with earlier Omarchy releases which used waybar.

## Attribution

Thank you for the inspiration and years of blazing-fast searches to David Carpenter
at www.voidtools.com, where I've saved maybe weeks of my life from needlessly
organizing files and scrambling for files in tight moments in my life.

## License

MIT. See [`LICENSE`](LICENSE).
