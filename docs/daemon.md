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

The socket defaults to `$XDG_RUNTIME_DIR/omavrything.sock` (falling back to
`/tmp/omavrything.sock`). The daemon prints the indexed entry count, the roots
and their filesystem kinds, and the socket path on startup.

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

## CLI

```sh
omavry search "report 2024"
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

## Current limitations

- No persistence; the index is rebuilt on every start (M6).
- The daemon runs as the user; the root system service arrives later (M9).
- NTFS partitions are not yet read via the MFT (M8).
- The login1 idle hint is not yet consulted (M7).
