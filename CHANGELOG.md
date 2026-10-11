# Changelog

Notable changes to `omavrything`. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.1.3]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.3
[0.1.2]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.2
[0.1.1]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.1
[0.1.0]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.0