# End-to-end manual checklist

The automated suite (`cargo test`) covers the index, scanner, snapshot, protocol,
and a hermetic daemon integration test. The items below require a real Omarchy
Quattro desktop, a real filesystem, and (for NTFS) a real partition, so they are
verified by hand before a release.

Run the checklist on a **release build** (`cargo build --release`); a debug build
is 10–50× slower and will make the UI feel broken.

Record the result of each item as pass/fail with the date and the Omarchy version.

## 0. Environment

- [ ] Omarchy Quattro (4.x) on Hyprland, `omarchy-shell` running.
- [ ] `cargo build --release` succeeds.
- [ ] `omavrythingd --help` and `omavry --help` print usage.

## 1. Daemon startup and indexing

- [ ] `scripts/run-daemon.sh` starts and logs `listening` with the socket path.
- [ ] The startup log reports the indexed entry count and each root with its
      filesystem kind.
- [ ] `omavry status` reports a non-zero entry count and the same roots.
- [ ] Starting a second daemon on the same socket refuses to start
      (`another omavrythingd appears to be running`).

## 2. Search over the socket

- [ ] `omavry search "report"` prints matching paths.
- [ ] `omavry search --json "report"` prints a JSON response with `results`,
      `total`, and `took_ms`.
- [ ] A query with no matches returns an empty result set and exit code 0.
- [ ] A malformed request (e.g. `printf 'not json\n' | nc -U <socket>`) returns
      an `error` field and the daemon keeps serving.

## 3. Shell UI

- [ ] `scripts/install-plugin.sh` installs the plugin and restarts the shell.
- [ ] `omarchy plugin enable omavrything --section center` shows the bar widget.
- [ ] `SUPER + ALT + SPACE` focuses the search input.
- [ ] Typing shows the centered scratchpad with the results table
      (Name | Size | Modified | Path).
- [ ] The bar widget mirrors the query on every keystroke.
- [ ] `Enter` opens the selected file; `Esc` dismisses the overlay.
- [ ] Two files with the same name are distinguishable by path/size/mtime.
- [ ] With the daemon stopped, the overlay shows the daemon-down empty state.

## 4. Live updates

- [ ] `touch ~/omavrything-e2e-new.txt` appears in results within ~1 s.
- [ ] Renaming the file updates the result (no duplicate).
- [ ] Deleting the file removes it from results within ~1 s.
- [ ] Creating a directory and a file inside it indexes both.

## 5. Settings menu

- [ ] The gear button opens the settings menu.
- [ ] Changing `backgroundOpacity` updates the tile live.
- [ ] Toggling NTFS, changing the snapshot interval, and changing the log level
      apply to the daemon.
- [ ] The changes persist across a daemon restart (state config).

## 6. Persistence and warm start

- [ ] Stop the daemon cleanly (`SIGTERM`); the log reports `snapshot written`.
- [ ] Restart; the log reports `warm-started from snapshot` and no full scan.
- [ ] Corrupt the snapshot (`truncate -s 100 <snapshot>`); restart falls back to
      `<snapshot>.prev`, then to a cold scan if that is also bad.

## 7. D-Bus control (optional)

- [ ] With the policy installed, `omavry status` works over D-Bus.
- [ ] `omavry rescan` prompts for polkit authentication and rescans.
- [ ] `omavry config set scan_interval 1800` persists and is reflected by
      `omavry config get`.
- [ ] Without the policy, the daemon logs the refusal and the socket path still
      works.

## 8. NTFS (requires a real NTFS partition)

- [ ] The partition is detected and logged as a new offer.
- [ ] `omavry mount <uuid>` mounts it read-only and indexes it via the MFT.
- [ ] Results match the partition contents.
- [ ] After a restart the partition is auto-mounted and indexed (remembered).

## 9. systemd service and hardening

- [ ] `sudo systemctl enable --now omavrythingd` starts the service.
- [ ] The service survives `sudo systemctl restart omavrythingd`.
- [ ] `systemd-analyze security omavrythingd` reports a hardened profile.
- [ ] `journalctl -u omavrythingd` shows structured log lines.

## 10. Resource use

- [ ] `systemctl status omavrythingd` shows RSS within the documented cap
      (`MemoryHigh=3G`, `MemoryMax=4G`).
- [ ] The daemon is idle (near-zero CPU) when the desktop is idle.
- [ ] A rescan does not run while the system is under load.
- [ ] `OMAVRYTHING_PROFILE=1` logs a per-query breakdown; `total` is in the
      low-millisecond range on the test index.

## 11. Release archive

- [ ] `scripts/package-release.sh` produces `dist/omavrything-<version>-<arch>.zip`.
- [ ] The zip contains `bin/`, `data/`, `shell/`, `scripts/`, `docs/`, `README.md`,
      and `LICENSE`.
- [ ] Extracting the zip and running `sudo ./scripts/install.sh --system` installs
      cleanly on a fresh system.
- [ ] `./scripts/install-plugin.sh` installs the plugin from the extracted archive.
