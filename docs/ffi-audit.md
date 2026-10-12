# FFI and raw-device access audit

This document records the audit of every `unsafe` block, every FFI boundary, and
every raw-device access in the workspace, as required by M12. The goal is to show
that the privileged daemon's unsafe surface is small, well-understood, and
confined to thin wrappers around libc.

## 1. Summary

- **No C-library FFI.** The NTFS reader is the **pure-Rust** implementation
  documented as the ADR-006 alternative. There is no `libntfs-ng` binding, no
  `bindgen`, and no `build.rs` that links a C library. The only foreign function
  interface is to **libc** (the C standard library and the syscalls it wraps).
- **All `unsafe` is libc.** Every `unsafe` block in the workspace calls a libc
  function or constructs a libc struct. There is no `unsafe` in the index,
  search, scanner, snapshot, protocol, or live-update code.
- **Raw-device access is read-only.** The daemon opens a block device only to
  read the NTFS boot sector and MFT. It never writes to a device.
- **Every `unsafe` block carries a `SAFETY:` comment** stating the invariant that
  makes it sound.

## 2. libc FFI inventory

| Location | Call | Purpose | Soundness argument |
| --- | --- | --- | --- |
| [`main.rs`](../crates/omavrythingd/src/main.rs) | `geteuid` | choose root vs user defaults | no preconditions |
| [`main.rs`](../crates/omavrythingd/src/main.rs) | `chown` | set the socket's group | path is a valid NUL-terminated `CString` |
| [`main.rs`](../crates/omavrythingd/src/main.rs) | `flock` | single-daemon lock | valid open fd |
| [`main.rs`](../crates/omavrythingd/src/main.rs) | `pipe`, `read`, `write` | async-signal-safe shutdown self-pipe | valid fds, 1-byte buffer |
| [`main.rs`](../crates/omavrythingd/src/main.rs) | `signal` | install SIGTERM/SIGINT handler | handler is a valid `extern "C"` fn |
| [`main.rs`](../crates/omavrythingd/src/main.rs) | `malloc_trim`, `mallopt` | return freed pages to the OS | no preconditions; valid option |
| [`peer.rs`](../crates/omavrythingd/src/peer.rs) | `getsockopt(SO_PEERCRED)` | peer identity | `ucred` and its length are valid |
| [`peer.rs`](../crates/omavrythingd/src/peer.rs) | `getgrnam`, `getpwuid`, `getgrouplist` | group membership | pointers checked for null; buffers sized |
| [`peer.rs`](../crates/omavrythingd/src/peer.rs) | `geteuid`, `getegid` | self identity | no preconditions |
| [`watch.rs`](../crates/omavrything-core/src/watch.rs) | `poll` | bounded inotify wait | single valid `pollfd` |
| [`snapshot.rs`](../crates/omavrything-core/src/snapshot.rs) | `ioctl(BTRFS_IOC_GET_SUBVOL_INFO)` | btrfs subvolume generation | over-sized buffer; ioctl writes within it |

### Notes on the riskier calls

- **`ioctl(BTRFS_IOC_GET_SUBVOL_INFO)`** ([`snapshot.rs`](../crates/omavrything-core/src/snapshot.rs:114))
  is the only ioctl. The kernel struct is 4624 bytes; the code allocates an
  8192-byte buffer so a future kernel that grows the struct cannot overflow it,
  and reads the `generation` field at a fixed offset only after the ioctl
  succeeds. A failure returns `None` and the root is treated as having no
  generation (forcing a rescan), which is safe.
- **`getgrouplist`** ([`peer.rs`](../crates/omavrythingd/src/peer.rs:57)) is
  called twice: once to size the group list and once to fill it. The second call
  is only made when the first reports the buffer was too small, and the buffer is
  resized to the reported `ngroups` first.
- **The signal handler** ([`main.rs`](../crates/omavrythingd/src/main.rs:589))
  is async-signal-safe: it only loads an atomic and calls `write`. All real work
  (the snapshot write) happens on a dedicated thread woken through the self-pipe.

## 3. Raw-device access

| Location | Access | Mode |
| --- | --- | --- |
| [`detect.rs`](../crates/omavrything-ntfs/src/detect.rs:27) | open a block device, read the 512-byte boot sector | read-only |
| [`ntfs.rs`](../crates/omavrythingd/src/ntfs.rs:127) | open the device, hand the `File` to `NtfsVolume::open` | read-only |
| [`volume.rs`](../crates/omavrything-ntfs/src/volume.rs:40) | seek + read the boot sector and MFT in bounded chunks | read-only |

The device is opened with `std::fs::File::open` (read-only) and only `Read` +
`Seek` are used. There is no `OpenOptions::write`, no `ioctl` on the device, and
no raw write path anywhere in the workspace.

### Mounting

Mounting is delegated to external tools, never done by writing to the kernel
directly:

- **udisks2** — `udisksctl mount -o ro` (preferred, so the desktop sees the
  partition).
- **`ntfs3` fallback** — a private read-only mount at
  `/run/omavrything/ntfs/<uuid>`.

Both are spawned as subprocesses. The daemon's systemd unit grants only the
minimal capability set it needs (raw-device read and the fallback mount), and
`ProtectSystem=strict` makes the rest of the filesystem read-only.

## 4. Privilege and input boundaries

- **Socket input** is untrusted. It is parsed by
  [`protocol::parse_request`](../crates/omavrything-core/src/protocol.rs), which
  enforces a 64 KiB line cap and rejects invalid UTF-8 and malformed JSON. This
  function is the target of the `protocol_parse` fuzzer.
- **Snapshot input** is untrusted (it lives on disk and could be corrupted). It
  is parsed by [`snapshot::decode`](../crates/omavrything-core/src/snapshot.rs),
  which validates the magic, version, checksum, record count against the
  remaining bytes, and every arena offset before use. This function is the target
  of the `snapshot_decode` fuzzer.
- **Peer identity** is enforced with `SO_PEERCRED`; only root, the daemon's own
  uid, and members of the `omavrything` group may connect.
- **Control methods** (`Rescan`, `SetConfig`, `MountNtfs`) are gated by the
  `tech.alatcerdas.omavrything.control` polkit action.

## 5. Findings and follow-ups

- **No blocking issues found.** The unsafe surface is small and each block is
  justified.
- **`getpwuid`/`getgrnam` are not thread-safe** (they return pointers to static
  storage). They are only called during connection setup and group lookup, and
  the results are copied out immediately. If the daemon ever calls them from
  multiple threads concurrently, switch to the `_r` variants.
- **The btrfs ioctl offset is kernel-version dependent.** It is guarded by a
  success check and a fallback, so a mismatch degrades to "no generation" rather
  than memory unsafety. Revisit if the kernel struct changes.
- **Fuzzing** covers the two untrusted parsers (snapshot and protocol). The
  remaining unsafe code is thin libc wrappers with no attacker-controlled
  pointers.
