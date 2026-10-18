# Changelog

Notable changes to `omavrything`. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0-rc10] — 2026-10-18

### Added

- **M19 mount correctness and hotplug.** NTFS mount and unmount commands are
  now time-boxed, so a hung `mount`/`udisksctl` cannot wedge the daemon. Both
  the udisks2 and private `ntfs3` paths mount `ro,nosuid,nodev,noexec`, and a
  pre-existing writable mount is remounted read-only. A UUID-less volume now
  uses its device basename for the private mount point instead of the shared
  constant `volume`. The NTFS policy file is written atomically under a lock.
  Mounting prefers the udisks2 D-Bus API through zbus 6, which returns the
  mount point directly; `udisksctl` and the private read-only `ntfs3` mount are
  fallbacks. A udev netlink monitor wakes the hotplug pass on block-device
  add/remove/change, with a 30 s fallback and a 2 s mount-table poll if the
  socket is unavailable. A remembered partition that appears is mounted and
  indexed; a mount that disappears has its entries dropped.

### Changed

- **Version bumped to `0.2.0-rc10`.** The systemd unit now states
  `PrivateMounts=no`/`PrivateTmp=no` explicitly to preserve the host mount
  namespace, and the tmpfiles drop-in creates `/run/omavrything/ntfs` with the
  service account's ownership. `uninstall.sh` falls back to `/proc/mounts` when
  `findmnt` is unavailable.

### Fixed

- **NTFS remount re-indexing.** Unmounting a volume now removes its root from
  the index, not just its records. Previously the stale root made the volume
  still report `indexed` with no searchable entries, and the hotplug monitor
  skipped re-indexing when the volume was mounted again. A remount now
  re-indexes and the files are searchable.

## [0.2.0-rc9] — 2026-10-17

### Added

- **NTFS forced-close and shutdown safety (M25).** The daemon now unmounts
  every mount it created on `SIGTERM`/`SIGINT`, retrying a busy unmount a
  bounded number of times before a lazy (`umount -l`) fallback, and reconciles
  stale daemon-created mounts under `/run/omavrything/ntfs` on startup after a
  `SIGKILL` or crash. Every mount is verified read-only before indexing, and a
  writable mount is refused. The systemd unit sets `KillSignal=SIGTERM` and an
  `ExecStopPost` sweep so `systemctl stop`, reboot, and poweroff release mounts
  before the local filesystems are unmounted.
- **Indexing correctness (M15).** The walker no longer crosses a nested mount
  that shares the root's device (bind mounts, btrfs subvolumes), using the
  `/proc/self/mountinfo` mount points, and tracks a visited `(dev, inode)` set
  so a traversal cycle cannot loop. NTFS `entry.path` values are validated
  before being joined onto the mount point, so a crafted MFT record cannot
  escape it.
- **Indexing observability and lifecycle (M16).** Traversal failures are
  recorded (an exact count plus a bounded path sample) and surfaced in
  `GetStatus` and the `ScanProgress` signal. A scan is cancelled promptly on
  shutdown or when a newer rescan supersedes it. Subtrees that could not be
  watched (`max_user_watches` exhaustion) are rescanned on a targeted cadence
  and counted in status. Traversal of network filesystems is time-boxed so a
  stalled mount cannot block a scan.

### Changed

- **`zbus` upgraded from 5 to 6.** The 5.x `zbus_names`, `zvariant`, and
  `zvariant_derive` crates are unmaintained (RUSTSEC-2026-0370/0371/0372) and
  were merged into `zbus` 6.0.0, so `cargo audit` failed the CI job. The
  workspace now depends on `zbus` 6 with the `tokio`, `proxy`, and `service`
  features, and the polkit subject uses the renamed `zbus::Variant` (formerly
  `zbus::zvariant::Value`). The workspace MSRV rises to 1.89 (zbus 6's minimum).

### Fixed

- **NTFS volumes mount, index, search, and open again.** The earlier
  host-namespace mount attempt (`nsenter -t 1 -m`) could never work: a
  non-root daemon cannot open `/proc/1/ns/mnt` (it requires ptrace access to a
  root process), so every mount fell back to the daemon's sandboxed mount
  namespace, the host mount table could not see it, and each volume failed with
  "mount ... vanished before the read-only check". The daemon now mounts in its
  own namespace and reads `/proc/mounts` again — and the unit no longer gives
  the service a private mount namespace at all: the filesystem-view options
  (`ProtectSystem=`, `ProtectHome=`, `PrivateTmp=`, `ProtectKernelTunables=`,
  `ProtectKernelLogs=`, `ProtectControlGroups=`, `ReadWritePaths=`) and the
  bind-mount options (`ProtectKernelModules=`, `ProtectHostname=`,
  `RuntimeDirectory=`, `StateDirectory=`) each create one, so they are dropped
  and the read-only `ntfs3` mount lands in the host namespace where the desktop
  can open the indexed files. `/run/omavrything` is now created by a tmpfiles.d
  drop-in instead of `RuntimeDirectory=`. The daemon keeps its dedicated
  unprivileged user, two-capability bounding set, `NoNewPrivileges`, syscall
  filter, and the remaining hardening.
- **`uninstall.sh` no longer recurses into a live NTFS mount.** The script
  removed `/run/omavrything` with a plain `rm -rf`, which descended into any
  daemon-created read-only mount under `/run/omavrything/ntfs/<uuid>`: on a
  read-only mount that produced a flood of failed-removal errors, and on a
  writable mount it would have deleted the user's files. The script now
  unmounts every mount under the runtime directory first (with a lazy fallback)
  and removes the directory with `rm -rf --one-file-system`, so a mount that
  could not be unmounted is still never crossed.
- **Removable media is now indexed only through the NTFS MFT path (ADR-018).**
  The generic walker no longer descends into `/run/media` (it is filtered from
  the walk roots and pruned in the walk), so a udisks2 NTFS mount is no longer
  double-indexed by the walker while the MFT path refuses it. The MFT path now
  remounts a writable pre-existing mount read-only and keeps it read-only, so
  the read-only invariant (B12) is enforced instead of silently bypassed. A
  writable NTFS partition therefore loses desktop write access while the daemon
  runs (safety over write access).
- **The daemon no longer logs a misleading `ERROR ntfs: failed to index` for a
  writable mount.** The writable-mount refusal is an expected, non-fatal
  outcome (the walker used to cover it) and is now logged at `info` with a
  clarifying message; `error` is reserved for genuine failures.
- **The smoke test's NTFS ownership guard named a function that never
  existed.** `ci-smoke.sh` asserted `record_owned_mount` in `ntfs.rs`; the
  function is `mount_index_and_remember_with_ownership`. The guard now names
  the real symbol (the check had been failing 113/114).
- **An evicted index is materialized on the first query instead of being
  linear-scanned.** A query while evicted now returns `state: "loading"`
  immediately and starts a background materialization of the owned index
  (folding the live-update delta in); the next query is served at full speed.
  Previously the query scanned the mmap image, which took 8–16 s at 1.38 M
  records, so the widget's first search after the 20 s idle eviction appeared
  to return nothing. The overlay's reload retry is spaced to 500 ms to match.
  The smoke test's evicted-image check now retries the search until the marker
  appears, matching the new loading-then-served contract.
- **Background indexing no longer resets the idle-eviction timer.**
  `IndexManager::with_index_mut` used to call `touch()`, so NTFS/volume
  indexing, snapshot encoding, and materialization all restarted the idle
  window and delayed eviction until the last background pass finished. Only
  user-driven work (a query, or a mutation requested through the control
  interface) now touches the timer; the D-Bus mount/unmount/rescan/config
  handlers call `touch()` explicitly. This is the same principle rc8 applied
  to filesystem events.
- **A failed eviction is no longer reported as a successful one.**
  `IndexManager::evict` now returns whether the index actually became evicted,
  and `evict_if_idle` propagates that result. Previously a snapshot write
  failure (for example an unwritable snapshot path) still logged
  `index evicted (idle)` and retried every ~1.2 s; the evictor now logs a
  warning and backs off for 30 s instead of spinning.
- **The installer and package now create `/var/lib/omavrything`.** The daemon
  runs as the unprivileged `omavrything` user and cannot create a directory
  under root-owned `/var/lib`, so the snapshot write and every `SetConfig`
  (which persists the state config) failed with `Permission denied`. The
  tmpfiles drop-in now creates `/var/lib/omavrything` with the daemon's
  ownership, and the installer and the pacman scriptlet run
  `systemd-tmpfiles --create`. This also lets the daemon write a snapshot on
  shutdown and warm-start from it, so the first eviction after a restart no
  longer waits for a full cold scan.

## [0.2.0-rc8] — 2026-10-17

### Fixed

- **The daemon failed to compile after the 0.1.9 fixes were rebased onto the
  rc6+ codebase.** The rebase resolution kept the pre-rc6 two-argument
  `spawn_thread` call at the D-Bus ntfs-startup site; it now passes the explicit
  `CONTROL_STACK` stack size like every other call site.
- **The idle evictor could be starved by filesystem events.** Event bursts kept
  resetting the idle window so eviction never ran on busy filesystems; the
  evictor now bounds how long event activity can defer an eviction pass.

## [Unreleased] — v0.2.0 roadmap

The next release is a **performance-first** robustness release. The work below is
planned, not yet shipped; this note records the direction.

### Changed (0.2.0-rc6 — daemon RSS overhead)

The rc5 index accounting reached 81.2 B/record on the 1 M synthetic tree, but the
end-to-end RSS/record was still dominated by the daemon's own overhead. rc6 cuts
that overhead (see [`plans/rss-overhead-0.2.0-rc6.md`](plans/rss-overhead-0.2.0-rc6.md)):

- **Release profile.** A `[profile.release]` with fat LTO, one codegen unit, and
  stripped symbols cut the daemon binary from 7.66 MB to 4.04 MB (−47 %). The
  resident text is the largest fixed cost. `panic = "abort"` is deliberately not
  set, because it would defeat the NTFS `catch_unwind` containment.
- **Threads and runtimes.** The rayon global pool is bounded to `min(cores, 4)`
  (override with `OMAVRYTHING_SCAN_THREADS`), the D-Bus tokio runtime uses 2
  worker threads, and the daemon's threads get explicit stack sizes. Thread count
  fell from 20 to 8.
- **Allocator.** `M_MMAP_THRESHOLD`/`M_TRIM_THRESHOLD` are lowered to 64 KiB and
  `trim_heap()` now runs after the live-update shrink.
- **Capacity slack.** The record and path vectors were found at exactly 2× their
  length on the live daemon (~75 MB of spare capacity) because the live-update
  and rescan paths never shrank them. A new `Index::shrink_if_slack()` is called
  from the watcher, and the full `shrink()` after a rescan.
- **Benchmark.** `scripts/benchmark.sh` now reports the RSS breakdown
  (`RssAnon`/`RssFile`/`RssShmem`/threads), an empty-index baseline, and the
  index capacity-vs-length figures.

Measured: the empty-index baseline is 5 MiB; the 500 k hermetic tree fell from
115.7 to ~104 B/record; the live-update burst RSS fell from 104 MiB to ~33 MiB
(451 → ~145 B/record). mimalloc was evaluated and rejected (315 MiB vs 49 MiB at
500 k).

### Planned

- **Indexing and search performance (highest priority).** The search path is
  currently a linear scan. v0.2.0 adds a candidate-narrowing (n-gram) index, a
  cache-friendly hot layout, a prefix/token index, incremental index maintenance,
  faster snapshot load/save, query-result caching, and parallelism/SIMD tuning,
  with p50/p95 performance budgets enforced in CI. This lands first so the rest
  of the release has timing headroom.
- **Toggleable menu-entry (app shortcut) indexing.** Freedesktop `.desktop`
  entries and Omarchy's own menu config become searchable, launchable results
  behind an `index_menu_entries` config toggle, with a distinct icon/type and
  `Enter`-to-launch.
- **NTFS mount/unmount safety on forced close.** Clean unmount on
  `SIGTERM`/`SIGINT`, startup reconciliation after `SIGKILL` or a crash, a
  systemd `ExecStopPost` sweep on stop/reboot/poweroff, a tmpfs mount root, and
  read-only mounts so a forced close cannot leak a mount or dirty the volume.
- **Robustness and setup.** Mount-boundary correctness, scan cancellation and
  error reporting, non-btrfs snapshot staleness, mount ownership and hotplug,
  install/uninstall hardening, and a single copyable setup command.

## [Unreleased]

### Fixed

- **The idle evictor could be starved by filesystem activity, so the index
  never evicted and the daemon's resident set stayed at its peak.** Every
  inotify event reset the idle-eviction timer, and a desktop fires events
  (real changes, excluded paths, metadata noise) far more often than the
  default 20 s idle period. Filesystem events no longer count as activity:
  "idle" means no queries and no user-driven mutations (config changes,
  rescans), which is when the memory should be released. While evicted,
  events accumulate in the delta as before. The evictor also now sleeps
  until the idle deadline instead of polling at fixed intervals, so eviction
  happens within about one idle period of the last query rather than up to
  two poll intervals late.
- **Added `--evict-idle-secs <secs>`** to `omavrythingd` (the idle period was
  previously configurable only through the config file), so tests and
  hermetic setups can exercise eviction directly.

## [0.1.11] — 2026-10-17

### Fixed

- **A client that kept supplying malformed requests without reading replies
  could retain all eight client slots across repeated write timeouts.** The
  malformed-request path ignored a failed error-response write and continued
  serving the connection, so the socket-search denial remained possible even
  after the 0.1.10 write timeout. A failed error-reply write now drops the
  connection and releases its slot; all reply writes fail closed.

## [0.1.10] — 2026-10-17

### Fixed

- **A search-authorized user could deny socket search to other users by
  stopping reading replies.** The daemon set only a read timeout on client
  connections; reply writes were blocking with no output deadline. A client
  that kept sending requests but never read responses (including
  malformed-request error replies, which require no large index) filled the
  socket buffer and blocked its serving thread forever in `write`, holding
  one of the eight client slots indefinitely. Writes now carry a 30-second
  write timeout, so a stalled reader's connection is dropped and its slot
  freed. The bundled socket client also bounds its blocking reads with the
  same deadline.

## [0.1.9] — 2026-10-16

### Fixed

- **A local group member could crash the system daemon via connection
  exhaustion.** The daemon admitted 64 concurrent client threads while the
  shipped systemd service runs with `TasksMax=64`, which must also cover the
  daemon's own threads (watcher, scheduler, snapshot writer, D-Bus, shutdown
  handler, tokio/zbus workers). A member of the search group could hold idle
  connections until the task budget was exhausted; the then-failing
  `thread::spawn` panicked the accept loop and terminated the service, and
  every reconnect after the automatic restart repeated the outage. Client
  threads are now conservatively capped at 8, and every thread spawn in the
  daemon (client connections, background tasks, the shutdown handler, and the
  D-Bus NTFS startup path) logs a spawn failure instead of panicking, so a
  tight task budget degrades gracefully rather than taking the service down.


## [0.1.8] — 2026-10-14

A security-hardening release for the `omarchy-shell` QML integration, closing
the findings that blocked marketplace verification.

### Security

- **Index-derived text is rendered as plain text.** `Text` defaults to
  `AutoText`, so a file name or parent path that "looks like" HTML was handed to
  Qt's rich-text engine; an `<img src="...">` in a directory name (any local user
  can create one) would load and decode an attacker-chosen local file
  (PNG/JPG/SVG/PDF/…) or fetch a remote URL during a search — a
  memory-exhaustion vector in the shared shell. Every `Text` that renders
  index- or query-derived data now sets `textFormat: Text.PlainText`.
- **The query mirror no longer falls back to `/tmp`.** The overlay writes the
  query to a runtime file the bar watches. With `XDG_RUNTIME_DIR` unset the path
  was `/tmp/omavrything-query`, world-writable: another local user could
  pre-create it as a symlink (the overlay would overwrite the link target with
  the query) or feed the bar arbitrary content. The mirror now uses
  `$XDG_RUNTIME_DIR` (0700) and is disabled when it is unset.
- **The socket response is bounded.** `handleResponse` trusted the daemon's
  result count; a spoofed socket or a compromised daemon could make the shell
  allocate an unbounded result set. The page is now sliced to `maxResults`.
- **`xdg-open` only receives an absolute path.** A spoofed daemon could return a
  URL or an option-like string (`--help`); the overlay now rejects anything that
  is not an absolute path.

### Added

- Regression guards **C14** (index-derived `Text` must be `PlainText`) and
  **C15** (the query mirror must not use `/tmp`) in `scripts/ci-smoke.sh` and
  `scripts/test-install-uninstall.sh`.

### Documentation

- `plans/security-posture-omacom-impact.md` §7 records the QML integration
  security review (fixed and pending), and `plans/risks.md` gains R22 (systemic
  `/tmp` socket fallback) and R23 (QML integration review readability).

## [0.1.7] — 2026-10-13

### Security

- **The overlay's search input is no longer a native editable `TextInput`.** The
  0.1.6 fix gated the input's `Loader` on the overlay's open state, which removed
  it only while the window was hidden; while the overlay was open the field was
  still an editable native `TextInput`. On Qt 6.12 a `TextInput` connects to
  `QClipboard::dataChanged` and synchronously reads the foreign text over the
  Wayland transfer with no aggregate byte limit and no total deadline, so a
  streaming clipboard owner could stall or exhaust the shared shell with no
  paste action and no field focus. The field is now a custom, non-native input
  (a focusable `Item` that renders the query with a `Text` element and edits it
  from `Keys.onPressed`), so it has no clipboard connection at all and the class
  cannot recur. The `Loader` still gates creation on the open state, and
  `scripts/ci-smoke.sh` and `scripts/test-install-uninstall.sh` guard the
  absence of a clipboard-connected text input.

### Added

- **`scripts/benchmark.sh` — an end-to-end search and indexing benchmark.** It
  drives a real daemon over the Unix socket and reports wall-clock latency
  distributions (p50/p95/p99/max) for selective, broad, and no-match queries,
  the cold-scan indexing throughput, and search latency *while the index is
  being rebuilt* (a rescan) and *while it is being updated live* (an inotify
  burst). It also samples the daemon's resident set size (RSS) and its peak
  (`VmHWM`), and reports the per-record memory cost — including the index's own
  bytes per record for the live daemon — for both the live and hermetic modes.
  It benchmarks the live installed daemon when one is reachable, and can
  optionally run the Criterion micro-benchmark and the standalone profiler
  example. The script ships in the release archive under `scripts/` and in the
  AUR package under `/usr/share/omavrything/scripts/`; the archive checks in
  `scripts/ci-smoke.sh` and `scripts/test-install-uninstall.sh` guard its
  presence. The default run is a fast (~10 s) profile; `--full`/`--all` run the
  larger, more comprehensive sizes. The 0.1.7 baseline numbers are recorded in
  [`docs/benchmarks.md`](docs/benchmarks.md).

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

[0.1.9]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.9
[0.1.8]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.8
[0.1.7]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.7
[0.1.6]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.6
[0.1.5]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.5
[0.1.3]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.3
[0.1.2]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.2
[0.1.1]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.1
[0.1.0]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.0