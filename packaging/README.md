# Packaging

AUR packaging for `omavrything`.

## Files

| File | Purpose |
| --- | --- |
| `PKGBUILD` | Builds the workspace and installs the daemon, CLI, systemd unit, sysuser, D-Bus policy, polkit action, default config, and shell plugin. |
| `omavrything.install` | pacman scriptlet: creates the `omavrything` user, reloads systemd, and prints post-install instructions. |

## Building locally

From a checkout of this repository:

```sh
cd packaging
makepkg -si
```

`makepkg` fetches the `v$pkgver` release tarball from GitHub. Before the first
release the tag does not exist yet, so build from the working tree instead:

```sh
# From the repository root, install into a staging root without a tarball:
cargo build --release --workspace
```

## Publishing to the AUR

1. Tag the release (`v0.1.0`) and push it, so the `source` URL resolves.
2. Replace the placeholder checksum:

   ```sh
   updpkgsums
   ```

3. Regenerate the AUR metadata and commit both files:

   ```sh
   makepkg --printsrcinfo > .SRCINFO
   ```

4. Push `PKGBUILD`, `.SRCINFO`, and `omavrything.install` to the AUR git
   repository.

## What the package installs

| Path | Contents |
| --- | --- |
| `/usr/bin/omavrythingd` | The index daemon. |
| `/usr/bin/omavry` | The CLI client. |
| `/usr/bin/omavrything-install-plugin` | Copies the shell plugin into the user's config. |
| `/usr/lib/systemd/system/omavrythingd.service` | Hardened system service. |
| `/usr/lib/sysusers.d/omavrything.conf` | The dedicated `omavrything` user/group. |
| `/usr/share/dbus-1/system.d/tech.alatcerdas.omavrything.conf` | System-bus policy. |
| `/usr/share/polkit-1/actions/tech.alatcerdas.omavrything.policy` | Polkit control action. |
| `/etc/omavrything/config.toml` | Default daemon configuration. |
| `/usr/share/omavrything/shell/omavrything/` | The `omarchy-shell` plugin. |
| `/usr/share/doc/omavrything/` | Documentation. |
| `/usr/share/licenses/omavrything/LICENSE` | MIT license. |

The daemon is **not** enabled by the package. Enable it explicitly with
`sudo systemctl enable --now omavrythingd`, or run it as a user process for
development. See [`../docs/install.md`](../docs/install.md).
