<div class="hero">
<h1>omavrything</h1>
<p class="tagline">Instant, index-backed file search: "Everything" built for Omarchy.</p>
<p class="lede">A search bar in your <code>omarchy-shell</code>. Start typing and a scratchpad pane of results drops over whatever you are doing, backed by a daemon that keeps every mounted filesystem indexed in memory.</p>
<div class="badges">
<span class="badge ok">v0.1.10</span>
<span class="badge">Omarchy Quattro 4.x</span>
<span class="badge">Quickshell / Qt</span>
<span class="badge">MIT</span>
<span class="badge">No telemetry</span>
</div>
<div class="buttons">
<a class="btn primary" href="docs/install.html">Install omavrything</a>
<a class="btn" href="docs/index.html">Read the docs</a>
<a class="btn ghost" href="https://github.com/brrrassica/omavrything">View source ↗</a>
</div>
</div>

<!--screenshot-->

<figure>
<img src="assets/file-managers.jpg" alt="Meme: File managers? Where we're going we don't need file managers.">
<figcaption>Where we're going, we don't need file managers.</figcaption>
</figure>

## What it is

Are you a Windows refugee or simply trying out Omarchy with dual-booting? Are Voidtool's Everything a must-have on your tool belt?
Do you hate doing file management and consider instant file searching indispensable; the thing you miss most since moving to Omarchy?

Omavrything brings it back. The UI lives in the `omarchy-shell` as a persistent
search bar; typing opens a centered, high-transparency scratchpad with a results
table. A root system daemon (`omavrythingd`) keeps an in-memory index of all
mounted filesystems — using directory traversal plus `inotify` for Linux
filesystems (btrfs/ext4/xfs/f2fs) and the MFT for NTFS partitions.

The UX is openly inspired by [Voidtools Everything](https://www.voidtools.com/).

## Highlights

<div class="grid">
<div class="card"><span class="ico">⚡</span><h3>Instant results</h3><p>Queries are answered from an in-memory index — no directory walking at search time, so results land as fast as you can type.</p></div>
<div class="card"><span class="ico">💽</span><h3>Whole-disk by default</h3><p>Every mounted filesystem is indexed. Linux filesystems are walked directly; NTFS partitions are read straight from the MFT on the block device.</p></div>
<div class="card"><span class="ico">🔄</span><h3>Live index</h3><p><code>inotify</code> follows changes on Linux filesystems, so the index keeps up with files being created, moved and deleted under it.</p></div>
<div class="card"><span class="ico">🪟</span><h3>Native to Omarchy</h3><p>A Quickshell bar widget plus a centred overlay, wired to your Hyprland hotkey. It is a plugin, not a separate app you alt-tab to.</p></div>
<div class="card"><span class="ico">🔒</span><h3>Least privilege</h3><p>The daemon runs as a dedicated <code>omavrything</code> user holding only the minimal capability set it needs — not full root.</p></div>
<div class="card"><span class="ico">📴</span><h3>Local only</h3><p>There is no network client in the daemon and no telemetry. The UI talks to it over a local Unix socket.</p></div>
<div class="card"><span class="ico">🪫</span><h3>Degraded, not silent</h3><p>Without the daemon the bar and overlay still open and say the index is unavailable, instead of quietly doing nothing.</p></div>
<div class="card"><span class="ico">🧠</span><h3>Memory-capped</h3><p>The index is engineered to stay inside a fixed memory ceiling — roughly 1.5&nbsp;GB even across millions of files. See the <a href="docs/memory-optimization.html">memory notes</a>.</p></div>
</div>

## Install

Grab the latest release archive.

**From a release** — download `omavrything-<version>-<arch>.zip` from the
[releases page](https://github.com/brrrassica/omavrything/releases):

```sh
unzip omavrything-*.zip
cd omavrything-*
sudo ./scripts/install.sh   # daemon, CLI, systemd unit, D-Bus, polkit, shell plugin
```

An AUR package is prepared under `packaging/` but is **not published yet**;
build it locally with `makepkg -si` if you prefer a package-managed install.

Then bind a hotkey in `~/.config/hypr/bindings.lua`:

```lua
hl.unbind("SUPER + ALT + SPACE")
o.bind("SUPER + ALT + SPACE", "Search files", "omarchy-shell tech.alatcerdas.omavrything focus")
```

Full setup, including the optional D-Bus control interface, is in
[Install](docs/install.html).

<div class="note">
<strong>Upgrading from v0.1.0?</strong> That release shipped the plugin under the id
<code>omavrything</code> and the daemon under the <code>org.omarchy.*</code> D-Bus
namespace. Both moved to <code>tech.alatcerdas.omavrything</code> in v0.1.1, so a
0.1.0 install needs the short upgrade note at the top of
<a href="https://github.com/brrrassica/omavrything/blob/main/CHANGELOG.md">CHANGELOG.md</a>.
</div>

## Requirements

- **Omarchy Quattro (4.x)** — Hyprland and `omarchy-shell` (Quickshell). Arch Linux.
- No backwards compatibility with earlier Omarchy releases, which used waybar.
- The shell plugin on its own needs **no privileges**. The index daemon is
  **optional**: without it the plugin still loads, there are simply no results yet.

## Documentation

- [Install](docs/install.html) — installation and setup.
- [Search syntax](docs/search-syntax.html) — query syntax and ranking.
- [Shell integration](docs/omarchy-shell-integration.html) — the plugin, IPC, and the hotkey.
- [The daemon](docs/daemon.html) — flags, the socket protocol, and the cache.
- [Architecture](docs/architecture.html) — how the pieces fit together.
- [Decisions](docs/decisions.html) — the decision log (ADRs).
- [Troubleshooting](docs/troubleshooting.html) — sockets, inotify, NTFS.
- [Full index](docs/index.html)

## Attribution

Thank you to David Carpenter at [voidtools](https://www.voidtools.com/) — for the
inspiration, and for the years of blazing-fast searches that saved a lot of
scrambling. This project is a grateful homage.
