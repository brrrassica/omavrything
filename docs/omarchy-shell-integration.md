# omarchy-shell integration

`omavrything` ships as an `omarchy-shell` plugin (Quickshell/QML). This document
describes the plugin structure, the focus constraint that shapes it, and how to
install and enable it.

## Plugin structure

The plugin lives in `shell/omavrything/` and is installed to
`~/.config/omarchy/plugins/omavrything/`. It declares three kinds:

| Kind | Entry point | Role |
| --- | --- | --- |
| `bar-widget` | `SearchBar.qml` | Persistent search bar in the bar's center section. |
| `overlay` | `SearchOverlay.qml` | Focusable input + floating results window. |
| `service` | `Service.qml` | Owns the persistent `omavrything` IPC target. |

## The focus constraint (why the input is in the overlay)

A layer-shell bar surface cannot receive keyboard input: the built-in bar's
`PanelWindow` does not set `WlrLayershell.keyboardFocus`, so it defaults to
`WlrKeyboardFocus.None`. A `TextInput` inside a bar widget therefore never
receives keystrokes.

The design therefore splits the search bar in two:

- **`SearchBar.qml`** is a *visual mirror*: it renders the search bar in the bar
  and summons the overlay on click. It does not host the input.
- **`SearchOverlay.qml`** hosts the real `TextInput` and the results list. It is
  a `PanelWindow` with `WlrLayershell.keyboardFocus: WlrKeyboardFocus.Exclusive`
  while open, anchored to the top and right-center aligned so it appears under
  the bar's search widget.

This is the fallback recorded in the project's ADR-008; the primary design (a
bar widget hosting the input) was ruled out by the M1 spike.

## IPC

The `service` plugin registers a persistent IPC target:

```sh
omarchy-shell omavrything focus   # summon the overlay and focus the input
omarchy-shell omavrything ping    # health check
```

`focus` calls the shell's `summon` for the plugin, which loads the overlay and
calls its `open()`, which focuses the input.

## Hotkey

The Hyprland binding lives in `~/.config/hypr/bindings.lua`:

```lua
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell omavrything focus")
```

`SUPER + ALT + SPACE` is reassigned from the Omarchy "Apps menu" binding.

## Install and enable

```sh
scripts/install-plugin.sh                          # copy into ~/.config/omarchy/plugins/
omarchy plugin enable omavrything --section center # place the widget + enable the plugin
```

The bar widget is placed in the center section, right of the `omarchy.clock`
anchor. The overlay and service are enabled by the same command.

## The scratchpad

The overlay is a centered, high-transparency scratchpad tile (~70% x 70%) with
the input at the top and a results table below. The table shows **name**,
**size**, **modified**, and **path** so duplicates are distinguishable.

`SearchOverlay.qml` exposes `widthFraction`, `heightFraction`, `maxWidth`,
`maxHeight`, `maxResults`, and `backgroundOpacity`. They are currently hardcoded
defaults; wiring them to the plugin's `shell.json` entry settings is a follow-up.

## Bar query mirror

The bar widget mirrors the query typed in the scratchpad so it reacts to every
keystroke. The overlay writes the query to
`$XDG_RUNTIME_DIR/omavrything-query`; the bar widget watches that file. This is a
spike-level channel (the two components cannot share QML state directly) and can
be replaced by a shared singleton or a daemon broadcast later.
