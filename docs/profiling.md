# Profiling: CPU wakeups, idle cost, and rescan tuning

This document records the M12 profiling of the daemon's idle behaviour and the
resulting guidance for the rescan interval. Query latency is covered separately
in [`development.md`](development.md).

## 1. Threads and their idle wakeup sources

`omavrythingd` runs a small, fixed set of threads. Only two of them wake
periodically when the system is idle:

| Thread | Idle behaviour | Wakeups / s |
| --- | --- | --- |
| main (accept loop) | blocks in `accept()` | 0 |
| inotify watcher | `poll` on the inotify fd with a 500 ms timeout | 2.0 |
| rescan scheduler | `sleep(5 s)`, then reads `/proc/loadavg`, `/proc/stat`, `/proc/diskstats` | 0.2 |
| snapshot writer | `sleep(snapshot_interval)` (default 300 s) | ~0.003 |
| D-Bus (zbus/tokio) | event-driven; no periodic timer | 0 |
| per-connection | only while a client is connected | 0 when idle |

**Total idle cost: ~2.2 wakeups/s**, dominated by the watcher's 500 ms poll. Each
watcher wakeup is one `poll` syscall that returns 0 (timeout) plus a scan of the
(usually empty) pending-move map. The scheduler wakeup reads three small `/proc`
files. Neither does meaningful work, so idle CPU is effectively zero, but the
wakeups are non-zero and matter for battery life.

### Why the watcher polls at all

The 500 ms poll exists to flush `IN_MOVED_FROM` events that never pair with an
`IN_MOVED_TO` (a move *out* of the watched tree). In-tree renames pair within the
same read batch, so the timeout only bounds the latency of move-out detection.
The pairing window itself is [`MOVE_PAIR_TIMEOUT`](../crates/omavrything-core/src/watch.rs:25)
(1 s); the poll timeout is the granularity at which that window is checked.

## 2. Measuring idle cost

```sh
# Context switches and CPU time over 60 s.
pidstat -w -p "$(pgrep omavrythingd)" 1 60

# Wakeup sources on the whole system (needs root).
sudo powertop --time=60

# Syscall counts while idle.
sudo strace -c -f -p "$(pgrep omavrythingd)" -e trace=poll,read,openat &
sleep 30; kill %1
```

Expected idle result: a handful of `poll` calls per second, no `read`/`openat`
traffic except the scheduler's three `/proc` reads every 5 s, and near-zero CPU.

## 3. Tuning the rescan interval

The rescan is a full re-walk of every root, reconciled into the index under a
brief write lock. It is **idle-gated**: it only runs when the 1-minute load is
below 1.0, CPU idle is above 50 %, and in-flight I/O is at most 4, and only after
`scan_interval` seconds have elapsed. inotify keeps the index current between
rescans; the rescan is the safety net for events inotify missed (watch-limit
exhaustion, changes while the daemon was down).

| Workload | Suggested `scan_interval` |
| --- | --- |
| Default / general desktop | 900 s (15 min) |
| Large, mostly-static tree | 1800–3600 s |
| Active development tree (build outputs, many renames) | 300–600 s |
| Fully covered by inotify, battery-sensitive | 0 (disabled) |

`scan_interval = 0` disables the periodic rescan entirely; live updates still
apply. Set it at runtime with `omavry config set scan_interval <secs>` (polkit)
or in `/etc/omavrything/config.toml`.

### Reducing idle wakeups further

The only meaningful lever is the watcher poll timeout
([`WATCH_POLL`](../crates/omavrythingd/src/main.rs:52), 500 ms). Raising it to
2 s cuts watcher wakeups from 2/s to 0.5/s, at the cost of up to 2 s extra
latency for detecting a move *out* of the watched tree. In-tree renames and
create/delete events are unaffected (they are delivered immediately by inotify).
This is a deliberate trade-off and is left at 500 ms for responsiveness.

## 4. Snapshot cost

The snapshot writer compacts the index and encodes it under the read lock, then
writes outside the lock. Encoding is O(index size) and the write is a few hundred
MB at most; it runs every `snapshot_interval` (default 300 s) and on clean
shutdown. It is not a meaningful idle cost, but on a very large index the encode
is a brief CPU burst — raise `snapshot_interval` if that is undesirable.

## 5. Memory cap behaviour

The daemon's memory is bounded by the systemd unit, not by an in-process cap:

- `MemoryHigh=3G` — the kernel starts reclaiming and throttling above this.
- `MemoryMax=4G` — a hard cap; exceeding it triggers the OOM killer for the
  cgroup.

The index itself is ~70 B/file steady state (see
[`memory-optimization.md`](memory-optimization.md)), so 4 GB accommodates roughly
50 M files. On a larger index the daemon is killed and restarted by systemd
(`Restart=on-failure`), which then warm-starts from the snapshot. To index more,
raise `MemoryMax` or reduce the indexed roots/excludes. A user-run daemon has no
such cap and is limited only by available RAM.
