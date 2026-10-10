# omavrything

Are you a Wind*ws ~~user~~ survivor? Do you consider instant file searches with
Voidtool's Everything indispensable and you miss it when daily driving Omarchy?

Omavrything is a local file search for **Omarchy Quattro (4.x)**, with 
UX "inspired" by [Voidtools Everything](https://www.voidtools.com/).

The UI lives in the `omarchy-shell` (Quickshell/Qt) as a persistent search bar;
typing opens a floating results window below it. A root system daemon
(`omavrythingd`) keeps an in-memory index of all mounted filesystems — using
directory traversal + inotify for Linux filesystems (btrfs/ext4/xfs/f2fs) and
the MFT for NTFS partitions.

> **Status:** pre-alpha. See [`docs/`](docs/) for documentation.

## Components

| Component | Artifact | Role |
| --- | --- | --- |
| `omavrything-core` | library | Index, search, scanner, IPC protocol. |
| `omavrything-ntfs` | library | libntfs-ng FFI for MFT/USN. |
| `omavrythingd` | binary | Root system daemon. |
| `omavrything-cli` | binary (`omavry`) | CLI client. |
| `omavrything` shell plugin | QML | Search bar + results overlay. |

## Build

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Running the daemon

```sh
scripts/run-daemon.sh                              # indexes $HOME
scripts/run-daemon.sh --root /home/sam/Documents   # indexes a subtree
omavry search "report 2024"                        # query from the CLI
```

The daemon serves newline-delimited JSON over
`$XDG_RUNTIME_DIR/omavrything.sock`. See
[`docs/daemon.md`](docs/daemon.md) for the protocol.

## Shell plugin (omarchy-shell)

The search UI is an `omarchy-shell` plugin (Quickshell/QML). Install and enable
it with:

```sh
scripts/install-plugin.sh
omarchy plugin enable omavrything --section center
```

Then bind a hotkey in `~/.config/hypr/bindings.lua`:

```lua
o.bind("SUPER + SHIFT + SPACE", "Search files", "omarchy-shell omavrything focus")
```

See [`docs/omarchy-shell-integration.md`](docs/omarchy-shell-integration.md) for
the plugin structure and the focus constraint that shapes it.

## Target platform

Omarchy Quattro (4.x) only, maybe several versions up. Hyprland, `omarchy-shell` (Quickshell), Arch Linux.
No backwards compatibility with earlier Omarchy releases which used waybar.

## Attribution

Thank you for the inspiration and years of blazing-fast searches to David Carpenter
at www.voidtools.com, where I've saved maybe weeks of my life from needlessly 
organizing files and scrambling for files in tight moments in my life.

## License

MIT. See [`LICENSE`](LICENSE).
