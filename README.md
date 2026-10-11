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

> **Status:** v0.1.2 (security release). See [`docs/`](docs/) and the
> [changelog](CHANGELOG.md).

## Install from a release

> **Use v0.1.2 or newer.** v0.1.1 and earlier wrote the index snapshot and the
> state configuration through a temporary file that was briefly group- and
> world-readable, and a malformed NTFS record could stop the indexer. Both are
> fixed in v0.1.2; see the [changelog](CHANGELOG.md). v0.1.0 additionally shipped
> the plugin under the id `omavrything` and the daemon under the `org.omarchy.*`
> D-Bus namespace, both renamed to `tech.alatcerdas.omavrything` in v0.1.1, so
> 0.1.0 installs need the upgrade note in
> [`CHANGELOG.md`](CHANGELOG.md#upgrade-from-010).

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

An AUR package is prepared under [`packaging/`](packaging/) but is **not
published yet**. Until it is, build it locally with `makepkg -si` in that
directory, or use the release archive / a source build as above.

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

## Requirements

- **Omarchy Quattro (4.x)** — Hyprland and `omarchy-shell` (Quickshell).
- The shell plugin on its own needs **no privileges**.
- The index daemon is **optional**: without it the plugin still loads, but there
  are no results to show.
- **License:** MIT — see [`LICENSE`](LICENSE). The same license is declared in
  `manifest.json` (`"license": "MIT"`) and in the `PKGBUILD` under
  `packaging/`.

### External dependencies

Nothing outside this repository is vendored, and the daemon ships no third-party
service. The full list:

| Dependency | Needed for | Notes |
| --- | --- | --- |
| Omarchy Quattro (4.x), Hyprland, `omarchy-shell` (Quickshell) | the shell plugin | already present on a Quattro system |
| Rust stable with `cargo` | building from source | not needed when installing a release archive |
| `glibc`, `gcc-libs` | running the binaries | standard Arch base packages |
| `systemd` | the packaged daemon service | optional; a user-run daemon needs no systemd |
| `polkit` | the optional D-Bus control interface | gates `tech.alatcerdas.omavrything.control` |

There is no network client in the daemon and no telemetry: the UI and the daemon
communicate over a local Unix socket.

The daemon (`omavrythingd`) is a system service, but it does not run as full
root. It runs as the dedicated `omavrything` user holding only two capabilities:
`CAP_DAC_READ_SEARCH` (to read paths the indexer could not otherwise see) and
`CAP_SYS_ADMIN` (to mount an NTFS volume read-only when the kernel driver cannot
read the MFT directly). It indexes mounted filesystems by traversal + inotify,
and reads NTFS partitions from the MFT on the block device.

**Nothing is sent anywhere.** The UI talks to the daemon over a local Unix socket
(`/run/omavrything/omavrything.sock` for the system service, or
`$XDG_RUNTIME_DIR/omavrything.sock` for a user-run daemon). There is no network
client in the daemon and no telemetry.

Control operations on the optional D-Bus interface are gated by the polkit action
`tech.alatcerdas.omavrything.control` (`auth_admin_keep`).

**Degraded mode.** Without the daemon the search bar still appears and the
overlay still opens; it reports that the daemon is unavailable rather than
returning results. The plugin never silently does nothing.

## Documentation

Rendered at **<https://omavrything.alatcerdas.tech>**, mirrored to
<https://brrrassica.github.io/omavrything>. The site is generated from these
sources by `site/build.py`.

- [`CHANGELOG.md`](CHANGELOG.md) — release notes, including the 0.1.0 → 0.1.1 upgrade notes.
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

Releases are built by CI. Pushing a `v*` tag (e.g. `v0.1.2`) runs
[`.github/workflows/release.yml`](.github/workflows/release.yml), which builds the
release archive with [`scripts/package-release.sh`](scripts/package-release.sh)
and creates a GitHub release with the zip attached. The same archive can be built
locally with `scripts/package-release.sh`.

## Removal

Uninstall the shell plugin:

```sh
omarchy plugin disable tech.alatcerdas.omavrything
omarchy plugin remove tech.alatcerdas.omavrything --yes
```

Remove the hotkey binding from `~/.config/hypr/bindings.lua` if you added one.

Uninstall the daemon and its system integration. With the AUR package (once
published):

```sh
sudo pacman -Rns omavrything
```

Or, for a release-archive or from-source install:

```sh
sudo systemctl disable --now omavrythingd
sudo rm -f /usr/bin/omavrythingd /usr/bin/omavry /usr/bin/omavrything-install-plugin
sudo rm -f /usr/lib/systemd/system/omavrythingd.service
sudo rm -f /usr/lib/sysusers.d/omavrything.conf
sudo rm -f /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf
sudo rm -f /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy
sudo rm -rf /etc/omavrything /usr/share/doc/omavrything /usr/share/licenses/omavrything
sudo rm -rf /var/lib/omavrything      # the cached index snapshot
sudo userdel omavrything
```

The only state the daemon writes is the index snapshot under
`/var/lib/omavrything` (`index.snapshot` plus its `.prev` fallback), so removing
that directory is what erases the index. Nothing is left behind elsewhere.

## Target platform

Omarchy Quattro (4.x) only, maybe several versions up. Hyprland, `omarchy-shell` (Quickshell), Arch Linux.
No backwards compatibility with earlier Omarchy releases which used waybar.

## Attribution

Thank you for the inspiration and years of blazing-fast searches to David Carpenter
at www.voidtools.com, where I've saved maybe weeks of my life from needlessly
organizing files and scrambling for files in tight moments in my life.

## License

MIT. See [`LICENSE`](LICENSE).
