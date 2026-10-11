# Changelog

Notable changes to `omavrything`. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.1.2]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.2
[0.1.1]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.1
[0.1.0]: https://github.com/brrrassica/omavrything/releases/tag/v0.1.0