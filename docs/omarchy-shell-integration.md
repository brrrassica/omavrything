# omarchy-shell integration

`omavrything` ships as an `omarchy-shell` plugin (Quickshell/QML). This document
describes the plugin structure, the focus constraint that shapes it, and how to
install and enable it.

## Plugin structure

The plugin lives in `shell/omavrything/` and is installed to
`~/.config/omarchy/plugins/tech.alatcerdas.omavrything/`. It declares three kinds:

| Kind | Entry point | Role |
| --- | --- | --- |
| `bar-widget` | `shell/omavrything/SearchBar.qml` | Persistent search bar in the bar's center section. |
| `overlay` | `shell/omavrything/SearchOverlay.qml` | Focusable input + floating results window. |
| `service` | `shell/omavrything/Service.qml` | Owns the persistent `tech.alatcerdas.omavrything` IPC target. |

## The focus constraint (why the input is in the overlay)

A layer-shell bar surface cannot receive keyboard input: the built-in bar's
`PanelWindow` does not set `WlrLayershell.keyboardFocus`, so it defaults to
`WlrKeyboardFocus.None`. A `TextInput` inside a bar widget therefore never
receives keystrokes.

The design therefore splits the search bar in two:

- **`SearchBar.qml`** is a *visual mirror*: it renders the search bar in the bar
  and summons the overlay on click. It does not host the input.
- **`SearchOverlay.qml`** hosts the real search input and the results list. It is
  a `PanelWindow` with `WlrLayershell.keyboardFocus: WlrKeyboardFocus.Exclusive`
  while open, rendered as a **centered scratchpad tile** (~70% x 70%, high
  transparency) rather than a dropdown under the bar.



## IPC

The `service` plugin registers a persistent IPC target:

```sh
omarchy-shell tech.alatcerdas.omavrything focus   # summon the overlay and focus the input
omarchy-shell tech.alatcerdas.omavrything ping    # health check
```

`focus` calls the shell's `summon` for the plugin, which loads the overlay and
calls its `open()`, which focuses the input.

## Hotkey

The Hyprland binding lives in `~/.config/hypr/bindings.lua`:

```lua
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")
```

`SUPER + ALT + SPACE` is reassigned from the Omarchy "Apps menu" binding.

### Ready-to-paste binding examples

Pick one and paste it into `~/.config/hypr/bindings.lua`, then reload Hyprland
(`hyprctl reload`). `hl.unbind` first releases the key from whatever Omarchy
bound it to; drop that line if the key is free.

```lua
-- Default: reassign the Omarchy "Apps menu" binding.
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")

-- Reassign the application launcher (SUPER + SPACE).
hl.unbind("SUPER + SPACE")
o.bind("SUPER + SPACE", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")

-- A free key: SUPER + F.
o.bind("SUPER + F", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")

-- A free key: SUPER + SHIFT + F.
o.bind("SUPER + SHIFT + F", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")
```

The command is the same for every binding: `omarchy-shell tech.alatcerdas.omavrything focus`
summons the overlay and focuses the input. To check that the IPC target is
alive:

```sh
omarchy-shell tech.alatcerdas.omavrything ping
```

If a binding does nothing, confirm the plugin is enabled
(`omarchy plugin enable tech.alatcerdas.omavrything --section center`) and that the shell was
restarted after installing it — a plugin rescan alone does not recompile the
QML. See [`troubleshooting.md`](troubleshooting.md#the-search-bar-does-not-appear).

## Install and enable

The standard path is the marketplace one-liner, which installs and enables the
plugin and lets it self-provision the system daemon:

```sh
omarchy plugin add https://github.com/brrrassica/omavrything.git --enable
```

The manual installer copies the plugin into
`~/.config/omarchy/plugins/tech.alatcerdas.omavrything/`, validates it, restarts
the shell, and enables it in the center section:

```sh
sudo scripts/install.sh
```

The bar widget is placed in the center section, right of the `omarchy.clock`
anchor. The overlay and service are enabled by the same command.

## System-daemon bootstrap (pkexec)

The `service` plugin (`Service.qml`) is loaded at shell startup, so it can
provision the system daemon before the overlay is ever summoned. On first start,
if the system daemon is absent, it runs `scripts/bootstrap.sh` once (guarded by
`bootstrapAttempted`):

1. `bootstrap.sh` exits immediately if the daemon binary or socket already
   exists, and is a no-op after a previous dismissal (a marker under
   `$XDG_CACHE_HOME/omavrything/`).
2. It locates the daemon binary: the plugin tree (`shell/omavrything/bin/`), then
   `bin/`, then `scripts/fetch-daemon.sh`, which downloads the release asset
   `omavrything-<version>-<arch>.zip` and verifies it against a SHA-256 digest
   pinned in the reviewed source (`scripts/daemon-checksums.txt`). The installer
   re-verifies the binaries against the per-binary digests in the same file
   before installing them as root.
3. It escalates with `pkexec <plugin>/scripts/install.sh --from-plugin --bindir
   <dir>`. `pkexec` uses the default `org.freedesktop.policykit.exec` action, so
   no custom `.policy` file is needed for the first run. The Omarchy polkit agent
   (`omarchy.polkit`, a shell plugin) shows the GUI authentication dialog; the
   installer runs as root and resolves the invoking user from `PKEXEC_UID`.

Exit codes: `126` means the prompt was dismissed (a marker is written so it does
not recur on every shell restart), `127` means the installer could not be run.
The `setup` IPC retries with `--force`:

```sh
omarchy-shell tech.alatcerdas.omavrything setup
```

`Service.qml` resolves its own directory with `Qt.resolvedUrl(".")` (the idiom
Omarchy's own plugins use), with a `$HOME/.config/omarchy/plugins/...` fallback.
The `focus` and `ping` IPC targets are unchanged.

## The scratchpad

The overlay is a centered, high-transparency scratchpad tile (~70% x 70%) with
the input at the top and a results table below. The table shows **name**,
**size**, **modified**, and **path** so duplicates are distinguishable.

The plugin is `keepLoaded`, so the overlay stays resident in the shell. Its
search field is a **custom, non-native input** — a focusable `Item` that renders
the query with a `Text` element and edits it from `Keys.onPressed` — not a
`TextInput`. On Qt 6.12 a native editable `TextInput` connects to
`QClipboard::dataChanged` and synchronously reads the foreign text over the
Wayland transfer with no aggregate byte limit and no total deadline, so a
streaming clipboard owner could stall or exhaust the shared shell with no paste
action and no field focus. Gating the input on the open state only removed it
while the window was hidden; the class remained while the overlay was open. A
custom input has no clipboard connection at all, so the class cannot recur. The
field is still created lazily by a `Loader` gated on the overlay's open state,
and torn down on close.

The column headers are clickable: clicking a header sorts the current results by
that column, and clicking it again toggles ascending/descending. The active
column shows an arrow. Sorting is applied client-side to the ranked page the
daemon returns; clearing the sort (a fresh query) restores the daemon's ranking.

`SearchOverlay.qml` exposes `widthFraction`, `heightFraction`, `maxWidth`,
`maxHeight`, `maxResults`, and `backgroundOpacity`. The settings menu (gear
button) reads the daemon's config with `omavry config get --json` and writes
changes back through `omavry config set` (the polkit-gated D-Bus `SetConfig`),
so the tile opacity, NTFS toggle, snapshot interval, idle-eviction period, and
log level persist in the daemon's state config rather than the plugin's
`shell.json` entry. The same panel lists detected volumes with mount/unmount
actions.

## Daemon socket

The overlay talks to the daemon over a Unix socket. It probes for the system
socket (`/run/omavrything/omavrything.sock`) first and falls back to the per-user
socket (`$XDG_RUNTIME_DIR/omavrything.sock`), mirroring the CLI's
`connect_default()`. A socket is only used if it exists and is writable, so a
system socket the user cannot reach (not in the `omavrything` group) falls
through to the user socket. The probe re-runs on every open, so a daemon that
started after the shell is still found.

## Bar query mirror

The bar widget mirrors the query typed in the scratchpad so it reacts to every
keystroke. The overlay writes the query to
`$XDG_RUNTIME_DIR/omavrything-query`; the bar widget watches that file. This is a
spike-level channel (the two components cannot share QML state directly) and can
be replaced by a shared singleton or a daemon broadcast later.

The mirror is a private per-user channel and is deliberately confined to
`$XDG_RUNTIME_DIR` (mode 0700). It does **not** fall back to `/tmp`: a
predictable path in a world-writable directory would let another local user
pre-create the file as a symlink (the overlay would then overwrite the link
target with the query) or feed the bar arbitrary content. When
`XDG_RUNTIME_DIR` is unset — which a Wayland session never does — the mirror is
disabled rather than made world-writable. (The daemon socket keeps its `/tmp`
fallback to stay consistent with the daemon and CLI; that is a separate,
cross-cutting concern.)
