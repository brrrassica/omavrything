# Changelog

Notable changes to `omavrything`. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`scripts/benchmark.sh` — an end-to-end search and indexing benchmark.** It
  drives a real daemon over the Unix socket and reports wall-clock latency
  distributions (p50/p95/p99/max) for selective, broad, and no-match queries,
  the cold-scan indexing throughput, and search latency *while the index is
  being rebuilt* (a rescan) and *while it is being updated live* (an inotify
  burst). It also benchmarks the live installed daemon when one is reachable,
  and can optionally run the Criterion micro-benchmark and the standalone
  profiler example. The script ships in the release archive under `scripts/`
  and in the AUR package under `/usr/share/omavrything/scripts/`; the archive
  checks in `scripts/ci-smoke.sh` and `scripts/test-install-uninstall.sh` guard
  its presence. The default run is a fast (~10 s) profile; `--full`/`--all` run
  the larger, more comprehensive sizes. The 0.1.6 baseline numbers are recorded
  in [`docs/benchmarks.md`](docs/benchmarks.md).

## [0.1.6] — 2026-10-12

A diagnostics-and-hardening release. It adds read-only NTFS mount and volume
diagnostics for the known mount-lifecycle defect, and closes a clipboard stall
in the overlay.

### Security

- **The overlay's editable input is now created only while the overlay is
  open.** The plugin is `keepLoaded`, so the `TextInput` in
  `SearchOverlay.qml` was instantiated permanently, including while the window
  was hidden. On Qt 6.12 a clipboard change makes a `TextInput` synchronously
  read the foreign text over the Wayland transfer with no total byte limit or
  deadline, so a streaming clipboard owner could stall or exhaust the shared
  shell with no paste action and no field focus. The input is now created by a
  `Loader` gated on the overlay's open state and torn down on close, and
  `scripts/ci-smoke.sh` guards the lazy instantiation.

### Added

- **NTFS mount and volume diagnostics.** The daemon now logs, at `info`, every
  NTFS mount attempt and result (udisks2 and the private `ntfs3` fallback),
  whether a mount was reused or daemon-created, and the filesystem type and
  options from `/proc/mounts`. It also reads the `$Volume` record's
  `$VOLUME_INFORMATION` flags (a read-only probe) and logs the dirty bit before
  the MFT read, so a volume Windows did not unmount cleanly is visible in the
  logs. On shutdown the daemon warns with the list of daemon-created mounts it
  does **not** unmount (the known defect, `plans/risks.md` R13), and
  `scripts/uninstall.sh` warns about any live mounts under
  `/run/omavrything/ntfs` before removing the runtime directory.

## [0.1.5] — 2026-10-12

A release that fixes four defects that made a fresh 0.1.4 install unusable, and
replaces the multi-command install flow with a single, option-free installer.

### Fixed

- **A fresh install never started the daemon.** `scripts/install.sh` installed
  the systemd unit but did not enable or start it, did not add the invoking user
  to the `omavrything` group, and swallowed `systemd-sysusers` errors. After the
  documented commands the unit was inactive and the user was not in the socket
  group, so the widget reported "Search daemon unavailable" and the CLI could not
  connect. The installer now creates the user (failing loudly), adds `$SUDO_USER`
  to the group, and enables and starts the daemon. The release instructions and
  `INSTALL.txt` were updated.
- **The D-Bus policy was installed but never reloaded.** A bus that was already
  running before the install did not see the new policy, so the daemon could not
  own its name and every control command returned
  `ServiceUnknown: The name is not activatable`. `install.sh` and the AUR
  install scriptlet now reload the bus.
- **The overlay could not reach the system socket.** The socket probe only
  checked that the system socket existed, not that it was reachable, and the
  `Socket` held a failed connection instead of retrying. The probe now requires
  the socket to be writable, falls back to the per-user socket, and forces a
  reconnect on every open.
- **polkit `SetConfig` could never prompt.** The daemon used a `unix-user`
  subject, which polkit cannot prompt for, so `SetConfig` failed instantly with
  `AccessDenied`. It now uses a `system-bus-name` subject. The cosmetic
  `background_opacity` key moved to a new
  `tech.alatcerdas.omavrything.set-config` action (`allow_active=yes`), so the
  settings panel saves without a prompt and without sharing the privileged
  `control` action with `MountNtfs`.
- **polkit `CheckAuthorization` used the pre-0.126 argument order.** The daemon
  sent `s(sa{sv})a{ss}u` (action first); polkit 127 expects `(sa{sv})sa{ss}us`
  (subject first, plus a trailing `cancellation_id`), so the call failed with
  `InvalidArgs` and every non-root `SetConfig` was denied. The daemon now sends
  `(subject, action, details, flags, "")`. polkit 127 also only lets trusted
  callers (uid 0 or an action owner) check another identity, so both actions
  carry the `org.freedesktop.policykit.owner` annotation for
  `unix-user:omavrything`; without it the unprivileged daemon is refused with
  `NotAuthorized`.

### Changed

- **The installer is now a single, option-free command.** `scripts/install.sh`
  (system-only) replaces the old `--system`/`--user`/`--no-start` interface and
  the `scripts/install-plugin.sh` helper; `scripts/uninstall.sh` is likewise
  system-only. The installer removes any previous installation first (keeping the
  config, the dedicated user, and the cached index), waits for the daemon socket
  **and** the D-Bus name before returning, and applies the `setfacl` ACL bridge
  so the widget works in the current session without a re-login.
- **The daemon's default log level is now `error`.** A production daemon is
  quiet unless something is wrong; `info` was too verbose for end users. Raise it
  with `--log-level info`/`debug`, the `log_level` config key, or
  `OMAVRYTHING_LOG`. The shipped `config.toml`, the man page, and the docs were
  updated.
- `scripts/doctor.sh` now checks group membership, daemon enablement, the
  `set-config` polkit action, and hints at reloading D-Bus.
- `scripts/ci-smoke.sh` asserts the user/group, daemon enablement, the new polkit
  action, and the overlay socket fallback. It now also **exercises the daemon's
  polkit `CheckAuthorization` path** (a non-root `SetConfig` in an active session
  must be authorized and persist) and asserts the fail-closed inverse (a non-root
  caller cannot set a privileged key or rescan), so the polkit 127 argument-order
  regression is caught by CI.

## [0.1.4] — 2026-10-12

The install/uninstall completeness and hardening milestone (M13–M14), shipped as
a stable checkpoint before the indexing and mounting work.

### Added

- `scripts/uninstall.sh` ships in the release archive, with `--user` (no root
  for a plugin-only install) and `--keep-state` (preserve the index snapshot)
  modes; every install path now has a matching removal path.
- `scripts/doctor.sh` ships in the release archive: one sudo-run script that
  queries plugin and daemon component statuses (systemd unit, D-Bus and polkit,
  socket, index/snapshot, mounts, inotify watches) and prints a diagnosis with
  remediation hints.
- An `omavrythingd(8)` man page, installed by `install.sh` and the PKGBUILD.
- Post-install verification: `install.sh` runs `omarchy plugin validate` and a
  daemon health check and fails loudly on regression.
- A CI job (`scripts/ci-smoke.sh`) exercising install → enable → search →
  uninstall in the Arch container, asserting every step.

### Changed

- `install.sh --user` restarts the shell, matching `install-plugin.sh`.
- `install.sh --system` clears stale `/usr/share/omavrything` files and reports
  the installed version.
- Install and uninstall are idempotent: re-running either script is a no-op.
- `omavry --version` (and `-V`), matching `omavrythingd --version`, so both
  binaries answer the version query the same way.
- `scripts/ci-smoke.sh` now asserts every step instead of only running it:
  exit codes and output of the install/uninstall scripts, the `--version` /
  `--help` / usage-failure contract of both binaries, search as an
  unprivileged user (including the refusal outside the `omavrything` group),
  JSON search output, the second-daemon refusal, D-Bus status where systemd is
  available, and that uninstall leaves nothing behind. It also builds as the
  invoking user under sudo so `target/` is not left with root-owned artifacts.

## [0.1.3] — 2026-10-11

A security release. The defect was found during the marketplace review of 0.1.2,
and it is the same mistake 0.1.2 set out to fix, one step further on.

### Security

- **A temporary file left behind by an interrupted run was reopened rather than
  replaced.** `open_private`, the shared helper added in 0.1.2, still opened
  whatever was already at the temp path and corrected the mode afterwards.
  `open` ignores the mode for a file that already exists, so a `0644`
  `index.snapshot.tmp` left by an interrupted run was reopened `0644`, truncated
  and written, with the mode forced only after that -- the same interval as in
  0.1.2, and the guard added in 0.1.2 is what made it look closed. A local
  process that opened the leftover during the interval kept read access to the
  snapshot written through it, which lists the path names and metadata of every
  indexed file; the state configuration behaved identically. The helper now
  removes any leftover and creates a fresh inode with `O_EXCL`, so no data is
  ever written to an inode this process did not create, and a symlink planted at
  the temp path is removed rather than followed. Reported by @HANCORE-linux.
- **`<snapshot>.prev` was published before it was tightened.** The previous
  snapshot was renamed to `.prev` and only then corrected, so a snapshot left
  `0644` by an older build produced a complete, readable `.prev` for the length
  of that interval. The outgoing snapshot is now tightened before the rename --
  the same fix as above, one path over. Found while fixing the report; not
  reported separately.

### Changed

- `open_private` now fails (`O_EXCL`) rather than writing through a file that
  appears at the temp path between the removal and the creation, so two writers
  that fail to serialize can no longer write through the same temp file.

## [0.1.2] — 2026-10-11

A security release. Both defects were found during the marketplace review of
0.1.1.

### Security

- **The index snapshot and the state configuration were group- and
  world-readable while being written.** Both are written to a temporary file that
  is renamed into place, and that temporary file was created as `0666 & ~umask`
  (`0644` under the usual `022`), with the mode corrected by a later `chmod`.
  Permissions are checked only when a file is *opened*, so another local process
  that opened the temporary file during the interval between the two kept read
  access to everything written through its descriptor, however the mode ended up.
  The snapshot lists the path names and metadata of every indexed file, so this
  exposed the user's file list; the state configuration exposes the indexed roots
  and exclusions. Both files are now created with their final mode, so the
  interval does not exist. Reported by @HANCORE-linux.
- **A malformed NTFS record could stop the indexer.** `parse_record` accepted an
  eight-byte attribute ending exactly on the record's last used byte and then read
  the attribute header past the end of the record, which panicked the NTFS startup
  thread and left the remaining partitions unindexed. The read of a truncated
  `$DATA` attribute ran off the end in the same way, and a one-byte sector size in
  a crafted boot sector underflowed the update-sequence fixup offsets. All three
  reads are now bounds-checked, and a panic while parsing one partition is
  contained so the other partitions are still indexed. Reported by
  @HANCORE-linux.

### Changed

- `omavrything_core::private_file::open_private` is now the single implementation
  of the create-with-final-mode pattern, used by both the snapshot and the state
  configuration, so the two cannot drift apart again.

## [0.1.1] — 2026-10-11

Packaging and identity release: the project is now installable from the Omarchy
plugin marketplace. The search behaviour of the daemon and the UI is unchanged.

### Changed

- **Breaking: plugin id.** The `omarchy-shell` plugin id is now
  `tech.alatcerdas.omavrything` (was `omavrything`), and `manifest.json` moved
  from `shell/omavrything/` to the repository root, where the marketplace
  installer expects it. Anything that names the old id must be updated:
  `omarchy plugin enable …`, `omarchy-shell … focus`, the hotkey binding in
  `~/.config/hypr/bindings.lua`, and `~/.config/omarchy/plugins/omavrything/`.
- **Breaking: D-Bus and polkit.** The daemon's bus name, interface, object path
  and polkit action moved out of Omarchy's own `org.omarchy.*` namespace into
  ours:

  | | Before | After |
  | --- | --- | --- |
  | bus name | `org.omarchy.omavrything` | `tech.alatcerdas.omavrything` |
  | interface | `org.omarchy.omavrything.Manager` | `tech.alatcerdas.omavrything.Manager` |
  | object path | `/org/omarchy/omavrything` | `/tech/alatcerdas/omavrything` |
  | polkit action | `org.omarchy.omavrything.control` | `tech.alatcerdas.omavrything.control` |

- Local installs and release archives now produce the same plugin directory
  layout that `omarchy plugin add` produces from the git URL, so a single root
  `manifest.json` is authoritative for every install path.
- The systemd memory limits are set from measurement rather than guesswork:
  `MemoryHigh=1200M` and `MemoryMax=1536M` (previously `3G` / `4G`). The daemon
  settles below 300 MB RSS at idle, and a cold initial index of ~3M files never
  exceeded 900 MB.
- Documentation: the stale "pre-alpha" wording is gone, and the README gained
  Requirements, Removal, and an explicit description of the privilege boundary.

### Added

- `license` (MIT) and a real `author` in the plugin manifest.

### Upgrade from 0.1.0

Remove the old plugin and the old bus/polkit files, then install 0.1.1:

```sh
omarchy plugin disable omavrything
omarchy plugin remove omavrything --yes
sudo rm -f /usr/share/dbus-1/system.d/org.omarchy.omavrything.conf
sudo rm -f /usr/share/polkit-1/actions/org.omarchy.omavrything.policy
```

Then install 0.1.1, run `omavrything-install-plugin`, and enable
`tech.alatcerdas.omavrything`. Update the hotkey to
`omarchy-shell tech.alatcerdas.omavrything focus`.

## [0.1.0] — 2026-10-10

First release: the `omavrythingd` index daemon, the `omavry` CLI, and the
`omarchy-shell` search plugin (bar widget, overlay, service).

[0.1.6]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.6
[0.1.5]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.5
[0.1.3]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.3
[0.1.2]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.2
[0.1.1]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.1
[0.1.0]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.0