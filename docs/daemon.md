# Daemon and protocol

`omavrythingd` is the index daemon. It scans mounted filesystems into an
in-memory index and serves queries over a Unix domain socket.

## Running

```sh
scripts/restart-all.sh                             # restart the daemon + reload the shell plugin
scripts/restart-all.sh --rebuild                   # rebuild the release daemon first (-r)
scripts/restart-all.sh --sudo                      # run the daemon as root (to index NTFS)
scripts/restart-all.sh -- --auto                   # pass daemon args through
scripts/run-daemon.sh                              # indexes $HOME
scripts/run-daemon.sh --root /home/sam/Documents   # indexes a subtree
scripts/run-daemon.sh --auto                       # indexes all non-pseudo mounts
scripts/run-daemon.sh --root / --exclude '**/.git/**' --exclude '**/node_modules/**'
scripts/run-daemon.sh --no-watch                   # disable inotify live updates
scripts/run-daemon.sh --scan-interval 1800         # idle-gated rescan every 30 min
scripts/run-daemon.sh --snapshot /tmp/idx.snap     # custom snapshot path
scripts/run-daemon.sh --no-snapshot                # disable persistence
scripts/run-daemon.sh --no-ntfs                    # disable NTFS detection/indexing
scripts/run-daemon.sh --no-metadata                # names-only scan (skip per-file size/mtime stats)
```

Roots are either explicit (`--root`, repeatable) or discovered from the mount
table (`--auto`). With no arguments the daemon indexes `$HOME` as a development
convenience. Pseudo-filesystems (`proc`, `sysfs`, `tmpfs`, `cgroup2`, `overlay`,
…) and network filesystems (`nfs`, `cifs`, `9p`, `fuse.sshfs`, …) are skipped, as
are the pseudo mount points `/proc`, `/sys`, and `/dev`. `/run` is **not**
excluded, so removable media mounted under `/run/media/<user>/<volume>` (including
NTFS) remains discoverable. `--exclude` globs prune matching paths during
traversal. The scanner walks each root in parallel and does not cross into nested
mounts. Entry type comes from `readdir`'s `d_type` (no extra syscall); size and
mtime require one `stat` per file. `--no-metadata` skips that stat for files
(directories are still stat'd so nested mounts are not crossed), which is much
faster but leaves `size`/`mtime` at zero in results.

The socket defaults to `/run/omavrything/omavrything.sock` when running as root
(the system service) and `$XDG_RUNTIME_DIR/omavrything.sock` (falling back to
`/tmp/omavrything.sock`) otherwise. It is created mode `0660` and, as root,
group `omavrything`. Every connection is checked with `SO_PEERCRED`: root, the
daemon's own uid, and members of the `omavrything` group are allowed. A single
daemon per socket is enforced with an exclusive `flock` on `<socket>.lock`, so a
second instance refuses to start. Each request must complete within 30 seconds
and be at most 64 KiB, so a slow or oversized client cannot hold a connection
slot. The daemon logs the indexed entry count, the roots and their filesystem
kinds, and the socket path on startup.

## Configuration file

The daemon reads `/etc/omavrything/config.toml` when it exists (override with
`--config <path>`, skip with `--no-config`). Every key is optional: a missing key
falls back to the built-in default, and a command-line flag overrides the file.
Unknown keys are rejected so a typo is reported rather than silently ignored.

```toml
auto = true                                    # discover roots from the mount table
# roots = ["/home", "/mnt/data"]               # explicit roots (when auto = false)
excludes = ["**/.git/**", "**/node_modules/**"]
scan_interval = 900                            # idle-gated rescan, seconds (0 disables)
snapshot_interval = 300                        # snapshot write, seconds (0 disables)
watch = true                                   # inotify live updates
dbus = true                                    # D-Bus control interface
ntfs = true                                    # detect and index NTFS partitions
metadata = true                                # collect per-file size/mtime
log_level = "info"                             # error | warn | info | debug | trace
background_opacity = 0.75                      # scratchpad tile opacity (UI setting)
# socket = "/run/omavrything/omavrything.sock"
# snapshot = "/var/lib/omavrything/index.snapshot"
```

Command-line `--exclude` globs extend the file's rather than replacing them. A
shipped default is installed to `/etc/omavrything/config.toml` (see
`data/config/config.toml`).

### Runtime settings and the state config

Changes made through the D-Bus `SetConfig` method (used by the shell plugin's
settings menu and by `omavry config set`) are applied to the running daemon and
persisted to a separate **state** config, so the shipped
`/etc/omavrything/config.toml` is never rewritten. The state path defaults to
`/var/lib/omavrything/config.toml` for the system service and
`$XDG_STATE_HOME/omavrything/config.toml` for a user-run daemon; override it with
`--state-config <path>`. On start the daemon loads the base file, then overlays
the state file, then applies command-line flags (CLI > state > base > defaults).

The settable keys are `excludes`, `scan_interval`, `snapshot_interval`, `ntfs`,
`log_level`, and `background_opacity`. `log_level` takes effect immediately;
`ntfs = true` triggers NTFS detection and indexing; `background_opacity` is a
UI-only value the daemon stores and serves but does not otherwise use.

## Logging

The daemon logs structured events to stderr in a stable
`TIMESTAMP LEVEL target: message field=value` format, which journald captures
and `journalctl -o json` can parse. The level comes from `--log-level`, the
`log_level` config key, or the `OMAVRYTHING_LOG` environment variable (which
accepts `RUST_LOG` syntax, e.g. `omavrythingd=debug,zbus=warn`), in that order of
precedence. The default is `info`.

## systemd service

`data/systemd/omavrythingd.service` runs the daemon as a hardened system service
under a dedicated `omavrything` user (created by
`data/sysusers/omavrything.conf`). It keeps only `CAP_DAC_READ_SEARCH` (read raw
block devices for the NTFS MFT) and `CAP_SYS_ADMIN` (the read-only `ntfs3`
fallback mount), and applies `ProtectSystem=strict`, `ProtectHome=read-only`,
`NoNewPrivileges`, `MemoryDenyWriteExecute`, a `@system-service @mount` syscall
filter, and memory, CPU, task, and file-descriptor limits. The socket and
snapshot paths are fixed on the command line so the service does not depend on
the invoking user's XDG directories.

```sh
sudo install -Dm644 data/systemd/omavrythingd.service /usr/lib/systemd/system/omavrythingd.service
sudo install -Dm644 data/sysusers/omavrything.conf /usr/lib/sysusers.d/omavrything.conf
sudo install -Dm644 data/config/config.toml /etc/omavrything/config.toml
sudo systemd-sysusers
sudo systemctl daemon-reload
sudo systemctl enable --now omavrythingd
sudo usermod -aG omavrything "$USER"   # so the shell plugin can reach the socket
```

`systemd-analyze security omavrythingd` reports the hardening profile.

## Protocol

Newline-delimited JSON over the Unix socket: one request per line, one response
per line.

Request:

```json
{"method":"search","q":"report 2024","limit":8,"offset":0}
```

- `method` — optional; defaults to `search`. The discriminator is present so
  control methods can be added without a breaking change.
- `q` — the query. Space-separated terms are ANDed (case-insensitive substring
  over the full path).
- `limit` — optional; defaults to 100, capped at 1000.
- `offset` — optional; number of leading matches to skip, for paging. Defaults
  to 0.

Response:

```json
{"results":[{"name":"report-2024.pdf","path":"/home/sam/report-2024.pdf","size":245760,"mtime":1710000000,"is_dir":false}],"total":42,"took_ms":0}
```

Each result carries the basename (`name`), the full `path`, the `size` in bytes,
and the last-modified time (`mtime`, Unix seconds) so the UI can disambiguate
duplicates.

`total` is the number of **all** matching entries, not just the returned page, so
a client can report "showing X of Y" and page with `offset`.

On a malformed request the daemon replies with an `error` field instead of
results:

```json
{"results":[],"total":0,"took_ms":0,"error":"expected value at line 1 column 1"}
```

## D-Bus control interface

Control and status are exposed on the system bus as
`tech.alatcerdas.omavrything.Manager` at `/tech/alatcerdas/omavrything`:

| Method | Description |
| --- | --- |
| `GetStatus` | version, entries, roots, memory, scan state, uptime |
| `Rescan(root)` | rescan all roots (empty `root`) or one configured root; returns entries added |
| `GetConfig` | the current configuration |
| `SetConfig(key, value)` | set `excludes`, `scan_interval`, `snapshot_interval`, `ntfs`, `log_level`, or `background_opacity`; changing `excludes` triggers a rescan |
| `ListRoots` | the indexed roots |
| `MountNtfs(uuid)` | mount an NTFS partition and index it via the MFT; returns the mount point |

A rescan walks the filesystem outside the index lock and then reconciles the
index under a brief write lock, so socket searches keep serving during the
traversal without cloning the whole index (which doubled peak memory). A
non-empty `root` must be one of the configured roots.

Signals: `IndexChanged(entries)` and `ScanProgress(phase, entries)`.

The read-only methods are open to all. `Rescan` and `MountNtfs`, and the
privileged `SetConfig` keys (`excludes`, `ntfs`, `scan_interval`,
`snapshot_interval`, `log_level`), are gated by the
`tech.alatcerdas.omavrything.control` polkit action. The cosmetic
`background_opacity` key uses `tech.alatcerdas.omavrything.set-config`
(`allow_active=yes`), so the settings panel can save without a prompt.

The daemon can only own the well-known name if the system-bus policy is
installed. Without it the daemon logs `Request to own name refused by policy`
and continues serving the socket search path. Install the policy and the polkit
action, then reload D-Bus:

```sh
sudo install -Dm644 data/dbus/tech.alatcerdas.omavrything.conf \
  /usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf
sudo install -Dm644 data/polkit/tech.alatcerdas.omavrything.policy \
  /usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy
sudo systemctl reload dbus
```

If the system bus is unavailable the daemon logs and continues, leaving the
socket search path unaffected; `--no-dbus` disables the interface.

## Client library

`omavrything-client` is the shared client used by the CLI and the shell plugin:
`SocketClient` speaks the search protocol over the Unix socket, and
`DbusClient` calls the control interface.

## CLI

```sh
omavry search "report 2024"        # Unix socket; prints matching paths
omavry search --json "report 2024" # the full JSON response, for scripting
omavry status                      # D-Bus; key: value lines
omavry status --json               # the status map as JSON
omavry rescan                      # rescan all roots (D-Bus, polkit)
omavry rescan --root /home/sam     # rescan one configured root
omavry mount <uuid>                # mount + index an NTFS partition (D-Bus, polkit)
omavry config get [--json]         # the current configuration
omavry config set <key> <value>    # set a value (D-Bus, polkit)
```

`search` and `status` are read-only; `rescan`, `mount`, and `config set` for a
privileged key are gated by the `tech.alatcerdas.omavrything.control` polkit
action and prompt for authentication. `config set background_opacity` uses the
`tech.alatcerdas.omavrything.set-config` action and does not prompt. Every
subcommand exits non-zero on failure, so it composes with shell scripts.

## Live updates

The daemon watches each indexed root recursively with inotify and applies
create/delete/rename/modify events to the index immediately, so query results
reflect changes within a second. `IN_MOVED_FROM`/`IN_MOVED_TO` pairs are matched
by cookie so a rename is a single update; a `MOVED_FROM` with no matching
`MOVED_TO` (a move out of the watched tree) is applied as a removal once the
pairing window elapses. Deleted or moved watched directories are dropped from the
watch set. If `max_user_watches` is exhausted, the affected directories are
reported and the periodic rescan acts as the safety net.

A background scheduler re-walks the roots and reconciles the index (adding new
entries, dropping stale ones) only when the system is idle — measured from
`/proc/loadavg`, `/proc/stat`, and `/proc/diskstats` — and only every
`--scan-interval` seconds (default 900; 0 disables). The walk runs outside the
index lock and the reconcile holds it only briefly, so a rescan neither blocks
searches for the duration of the traversal nor clones the index. `--no-watch`
disables inotify entirely.

## Persistence

The index is snapshotted to disk so a restart warm-starts instead of rescanning
everything. The snapshot is written on a timer (`--snapshot-interval`, default
300 s; 0 disables) and on a clean `SIGTERM`/`SIGINT` shutdown. Writes are
serialized between the timer and the shutdown handler, and the index is compacted
before encoding so tombstones are not persisted and their memory is reclaimed.

The default path is `/var/lib/omavrything/index.snapshot` when running as root
(the M9 system service) and `$XDG_STATE_HOME/omavrything/index.snapshot`
(falling back to `$HOME/.local/state/...`) otherwise; override with
`--snapshot <path>` or disable with `--no-snapshot`.

The format is versioned and length-prefixed with an FNV-1a checksum. Writes are
atomic (temp file + `rename`) and the previous snapshot is kept as
`<path>.prev`, so a corrupt or truncated snapshot falls back to the last good
one and ultimately to a fresh scan.

On load, each root is validated against the mount table and its filesystem
identity (device plus btrfs subvolume generation). Roots whose identity or
filesystem kind changed — or that are new — are rescanned; roots that are no
longer configured are dropped. A snapshot that fails to load triggers a full
cold scan.

## NTFS

NTFS partitions are detected with `blkid` (falling back to a boot-sector OEM
check) and indexed by reading the **MFT directly from the raw device** — no
mount is required for indexing. The reader is a pure-Rust implementation in
`omavrything-ntfs` (the alternative documented in
[ADR-006](decisions.md#adr-006)): it parses the boot sector, reads the `$MFT`
record to obtain the MFT's run list, then walks every MFT record and
reconstructs paths from `$FILE_NAME` parent references.

Mounting prefers **udisks2** (`udisksctl mount -o ro`) so the desktop sees the
partition, and falls back to a private read-only **`ntfs3`** mount at
`/run/omavrything/ntfs/<uuid>`.

The **auto-mount policy** remembers previously indexed partitions by UUID in a
state file (`/var/lib/omavrything/ntfs.state` as root, else
`$XDG_STATE_HOME/omavrything/ntfs.state`). At startup, a remembered partition is
auto-mounted and indexed; a newly seen one is only logged as an offer and is
indexed on demand via `MountNtfs`. `--no-ntfs` disables detection entirely.

## NTFS indexing and the index lock

NTFS volumes are indexed by reading the MFT directly from the raw device, which
requires root. The MFT read and parse is the expensive half of NTFS indexing
(seconds on a multi-million-record volume), so it runs **outside** the index
lock: [`enumerate_device`](../crates/omavrythingd/src/ntfs.rs) reads the MFT,
then [`apply_entries`](../crates/omavrythingd/src/ntfs.rs) inserts the result
under a brief write lock. This mirrors the `plan_rescan`/`apply_rescan` split, so
searches keep serving at full speed while a volume is being indexed. Run the
daemon as root with `scripts/restart-all.sh --sudo` to index NTFS.

## Profiling query performance

Search is a linear scan over every record, so its cost is proportional to the
index size. Two tools measure it:

- **Standalone profiler** — builds a synthetic index and times a query (or a
  simulated burst of typing) with a stage-by-stage breakdown:

  ```sh
  cargo run --release -p omavrything-core --example profile            # 1M records, typing simulation
  cargo run --release -p omavrything-core --example profile -- 3000000 report
  ```

  Always pass `--release`: a debug build is 10-50x slower and the example warns
  when it detects one. On a 1M-record index a query is ~6-18 ms; the cost scales
  linearly (1M ≈ 6 ms, 2M ≈ 12 ms, 3M ≈ 18 ms).

- **Daemon query profiler** — set `OMAVRYTHING_PROFILE=1` when starting the
  daemon and every search logs a breakdown to stderr:

  ```sh
  OMAVRYTHING_PROFILE=1 scripts/run-daemon.sh
  # search "report": total=0.87ms parse=0.00ms scan=0.85ms sort=0.01ms materialize=0.01ms scanned=50401 matched=50000 parallel
  ```

  The `scan` stage dominates; `parse`, `sort`, and `materialize` are negligible
  for a bounded page. If `total` is far above the standalone numbers, the daemon
  is likely a debug build or the index is much larger than expected.

The daemon must be built in **release** mode for interactive use. A debug build
is roughly 10x slower, which turns a sub-millisecond query into hundreds of
milliseconds and makes typing feel unusable. `scripts/run-daemon.sh` builds and
runs the release binary.

## Current limitations

- The D-Bus control interface is only reachable when the daemon owns the
  well-known name, which the bus policy permits for the `omavrything` user (or
  root); a user-run daemon therefore serves the socket search path but not D-Bus
  control.
- NTFS live updates use the periodic rescan; USN journal parsing is deferred to
  v0.2 ([ADR-006](decisions.md#adr-006)).
- An NTFS root is validated by its mount point's device identity, so if the
  partition is not mounted at startup its MFT-indexed entries are dropped and
  re-indexed when it is mounted again.
