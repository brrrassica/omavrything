# Daemon and protocol

`omavrythingd` is the index daemon. It scans mounted filesystems into an
in-memory index and serves queries over a Unix domain socket.

## Running

```sh
scripts/run-daemon.sh                              # indexes $HOME
scripts/run-daemon.sh --root /home/sam/Documents   # indexes a subtree
scripts/run-daemon.sh --auto                       # indexes all non-pseudo mounts
scripts/run-daemon.sh --root / --exclude '**/.git/**' --exclude '**/node_modules/**'
scripts/run-daemon.sh --no-watch                   # disable inotify live updates
scripts/run-daemon.sh --scan-interval 1800         # idle-gated rescan every 30 min
scripts/run-daemon.sh --snapshot /tmp/idx.snap     # custom snapshot path
scripts/run-daemon.sh --no-snapshot                # disable persistence
scripts/run-daemon.sh --no-ntfs                    # disable NTFS detection/indexing
```

Roots are either explicit (`--root`, repeatable) or discovered from the mount
table (`--auto`). With no arguments the daemon indexes `$HOME` as a development
convenience. Pseudo-filesystems (`proc`, `sysfs`, `tmpfs`, `cgroup2`, `overlay`,
…) and network filesystems (`nfs`, `cifs`, `9p`, `fuse.sshfs`, …) are skipped, as
are the pseudo mount points `/proc`, `/sys`, and `/dev`. `/run` is **not**
excluded, so removable media mounted under `/run/media/<user>/<volume>` (including
NTFS) remains discoverable. `--exclude` globs prune matching paths during
traversal. The scanner walks each root in parallel and does not cross into nested
mounts.

The socket defaults to `/run/omavrything/omavrything.sock` when running as root
(the M9 system service) and `$XDG_RUNTIME_DIR/omavrything.sock` (falling back to
`/tmp/omavrything.sock`) otherwise. It is created mode `0660` and, as root,
group `omavrything`. Every connection is checked with `SO_PEERCRED`: root, the
daemon's own uid, and members of the `omavrything` group are allowed. A single
daemon per socket is enforced with an exclusive `flock` on `<socket>.lock`, so a
second instance refuses to start. Each request must complete within 30 seconds
and be at most 64 KiB, so a slow or oversized client cannot hold a connection
slot. The daemon prints the indexed entry count, the roots and their filesystem
kinds, and the socket path on startup.

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
`org.omarchy.omavrything.Manager` at `/org/omarchy/omavrything`:

| Method | Description |
| --- | --- |
| `GetStatus` | version, entries, roots, memory, scan state, uptime |
| `Rescan(root)` | rescan all roots (empty `root`) or one configured root; returns entries added |
| `GetConfig` | the current configuration |
| `SetConfig(key, value)` | set `excludes`, `scan_interval`, or `snapshot_interval`; changing `excludes` triggers a rescan |
| `ListRoots` | the indexed roots |
| `MountNtfs(uuid)` | mount an NTFS partition and index it via the MFT; returns the mount point |

A rescan runs against a clone of the index that is swapped in on completion, so
socket searches keep serving the previous index instead of blocking on a long
write lock. A non-empty `root` must be one of the configured roots.

Signals: `IndexChanged(entries)` and `ScanProgress(phase, entries)`.

The read-only methods are open to all; `Rescan`, `SetConfig`, and `MountNtfs`
are gated by the `org.omarchy.omavrything.control` polkit action.

The daemon can only own the well-known name if the system-bus policy is
installed. Without it the daemon logs `Request to own name refused by policy`
and continues serving the socket search path. Install the policy and the polkit
action, then reload D-Bus:

```sh
sudo install -Dm644 data/dbus/org.omarchy.omavrything.conf \
  /usr/share/dbus-1/system.d/org.omarchy.omavrything.conf
sudo install -Dm644 data/polkit/org.omarchy.omavrything.policy \
  /usr/share/polkit-1/actions/org.omarchy.omavrything.policy
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
omavry search "report 2024"   # Unix socket
omavry status                 # D-Bus
```

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
`--scan-interval` seconds (default 900; 0 disables). `--no-watch` disables
inotify entirely.

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
`omavrything-ntfs` (ADR-006's documented alternative to libntfs-ng FFI): it
parses the boot sector, reads the `$MFT` record to obtain the MFT's run list,
then walks every MFT record and reconstructs paths from `$FILE_NAME` parent
references.

Mounting prefers **udisks2** (`udisksctl mount -o ro`) so the desktop sees the
partition, and falls back to a private read-only **`ntfs3`** mount at
`/run/omavrything/ntfs/<uuid>`.

The **auto-mount policy** remembers previously indexed partitions by UUID in a
state file (`/var/lib/omavrything/ntfs.state` as root, else
`$XDG_STATE_HOME/omavrything/ntfs.state`). At startup, a remembered partition is
auto-mounted and indexed; a newly seen one is only logged as an offer and is
indexed on demand via `MountNtfs`. `--no-ntfs` disables detection entirely.

## Current limitations

- The daemon runs as the user; the root system service arrives later (M9).
- The D-Bus control interface is only reachable when the daemon owns the
  well-known name, which the bus policy permits for root only; a user-run daemon
  therefore serves the socket search path but not D-Bus control until M9.
- NTFS live updates use the periodic rescan; USN journal parsing is deferred to
  v0.2 (ADR-006).
- An NTFS root is validated by its mount point's device identity, so if the
  partition is not mounted at startup its MFT-indexed entries are dropped and
  re-indexed when it is mounted again.
