# Decision log (ADRs)

Each entry records a resolved decision point, the choice, and the rationale.
These are the architectural decisions behind `omavrything`; the code and the rest
of `docs/` reference them by number.

---

## ADR-001 — Privilege model and indexing scope

**Decision:** A single **root system daemon** (`omavrythingd`, a `systemd` system
service) indexes all mounted filesystems and handles NTFS directly.

**Rationale:** Closest to Everything's "index everything" model and the simplest
process model — one owner of the index, one place that touches raw devices and
mounts. Avoids the coordination overhead of a user daemon plus a privileged
helper.

**Consequences:** Larger security surface; the daemon must be carefully
hardened, run with minimal capabilities, and expose only a narrow IPC surface.
The search hot path is still served over a Unix socket to unprivileged clients.

**Alternatives considered:** Unprivileged user daemon + polkit-gated root helper;
user daemon only with udisks2 mounts and no raw MFT access.

---

## ADR-002 — Daemon language

**Decision:** **Rust** for the daemon, core library, NTFS bindings, and CLI.

**Rationale:** Memory safety and predictable performance for a long-running root
daemon; a strong ecosystem for filesystem traversal, inotify, D-Bus (`zbus`),
and FFI to C libraries; matches the conventions of the author's prior Omarchy
projects.

**Alternatives considered:** C++ (Qt-native), Go, Zig.

---

## ADR-003 — Index storage

**Decision:** **In-RAM index + periodic on-disk snapshot** for warm start.

**Rationale:** Everything keeps the whole index in RAM, which is what makes
queries instant. A snapshot avoids a full cold scan on every boot while keeping
the query path purely in-memory.

**Consequences:** Memory scales with file count (~100–200 bytes/file). A very
large index may need a memory cap or a future memory-mapped/trigram design.

**Alternatives considered:** In-RAM only, rebuilt on boot; memory-mapped LMDB;
SQLite FTS.

---

## ADR-004 — IPC transport

**Decision:** **Unix domain socket** for the search hot path + **D-Bus** for
control and status.

**Rationale:** The Unix socket gives the lowest-latency, highest-throughput path
for per-keystroke queries and is directly usable from QML via
`Quickshell.Io.Socket`. D-Bus is the standard, discoverable surface for control
operations and integrates with polkit.

**Consequences:** Two protocols to maintain. The socket protocol is
newline-delimited JSON for simplicity and QML-friendliness; the result set is
bounded so serialization is not the bottleneck.

**Alternatives considered:** D-Bus only (simpler, higher per-call latency);
custom binary framing (faster, harder to consume from QML).

---

## ADR-005 — Live updates and rescans

**Decision:** **inotify** for live updates + **idle-gated periodic rescans**.

**Rationale:** inotify gives immediate, low-cost change events for Linux
filesystems. Periodic rescans catch events missed due to watch limits or
filesystems that do not emit events, and gating them on system idleness keeps the
daemon from competing with foreground work.

**Consequences:** Watch-limit exhaustion must be detected and reported; the
rescan is the safety net. Idle detection reads `/proc/loadavg`, `/proc/stat`,
`/proc/diskstats`, and the login1 idle hint.

**Alternatives considered:** fanotify (whole-FS, needs root — viable since the
daemon is root, but more complex); polling only.

---

## ADR-006 — NTFS access

**Decision:** **FFI to libntfs-ng (ntfs-3g)** for MFT enumeration and USN journal
parsing; the daemon mounts read-only via the **kernel `ntfs3`** driver for
indexing and triggers a **udisks2** user mount so the desktop sees the partition.

**Rationale:** libntfs-ng is the mature, battle-tested NTFS implementation and
reads the MFT directly from the raw device — the fastest path and the Everything
approach. udisks2 keeps the user-visible mount consistent with the rest of the
desktop.

**Consequences:** A C dependency and unsafe FFI surface; the daemon needs raw
block-device access (it is root). USN **live** updates are deferred to v0.2; v0.1
uses MFT read-only plus periodic rescan. The shipped implementation instead uses
the **pure-Rust** reader documented here as the alternative (see
[`architecture.md`](architecture.md) and [`daemon.md`](daemon.md)), which removes
the C dependency and the FFI surface.

**Alternatives considered:** Pure-Rust NTFS crate (no C dependency, less mature);
shelling out to `ntfsprogs` (simplest, slowest, fragile).

---

## ADR-007 — UI model

**Decision:** A **persistent search bar** in the `omarchy-shell` bar
(right-center aligned) with a **summoned floating results window** below it.
`SUPER + <hotkey>` focuses the search input; `Esc` dismisses the results window.

**Rationale:** Matches the requested UX: the search affordance is always present,
but the results window only appears when there is something to show, keeping the
desktop uncluttered.

**Consequences:** The bar widget hosts the `TextInput` and must receive keyboard
focus on hotkey. A layer-shell bar normally has `keyboardFocus: None`, so this is
validated by an early feasibility spike with a documented fallback (see ADR-008).

**Alternatives considered:** Hotkey-summoned overlay for both input and results;
a standalone always-visible panel.

---

## ADR-008 — Search bar focus mechanism

**Decision:** The **bar widget is a visual mirror** and a **companion overlay**
hosts the input and results. The overlay is a `PanelWindow` with
`WlrLayershell.keyboardFocus: WlrKeyboardFocus.Exclusive` while open, anchored to
the top and right-center aligned.

**Rationale:** An early feasibility spike confirmed the primary design is
infeasible: the built-in bar's `PanelWindow` does not set
`WlrLayershell.keyboardFocus`, so it defaults to `WlrKeyboardFocus.None` and a
`TextInput` inside a bar widget never receives keystrokes. The fallback
(documented here in advance) is therefore adopted.

**Consequences:** The persistent search bar in the bar is a visual mirror; the
interactive input lives in the overlay, which appears under the bar widget when
summoned. A persistent `tech.alatcerdas.omavrything` IPC target (a `service` plugin) provides
`focus`, which summons the overlay and focuses the input.

**Alternatives considered:** Bar widget hosting the input directly (ruled out by
the spike); a standalone always-visible panel.

---

## ADR-009 — Search semantics (v0.1)

**Decision:** Case-insensitive **substring** matching over the **name and full
path**, with **space-separated AND terms**, ranked by match position, name vs
path, path length, depth, and recency.

**Rationale:** This is the Everything default and covers the overwhelming
majority of real queries without the complexity of regex/glob or a query parser.

**Consequences:** Regex/glob, whole-word, and content search are deferred.

**Alternatives considered:** Regex/glob from the start; whole-word default.

---

## ADR-010 — v0.1 scope

**Decision:** v0.1 ships btrfs/ext4/xfs/f2fs traversal + inotify; NTFS MFT
read-only; substring search with AND terms; actions = open file, open containing
folder, copy path; configurable result window size. **Deferred:** NTFS USN live
updates, regex/glob, content search.

**Rationale:** Delivers the core "instant search" experience end-to-end on the
target machine (btrfs root, optional NTFS) before adding advanced matching and
live NTFS journaling.

**Consequences:** USN parsing is implemented in `omavrything-ntfs` but not wired
into live updates until v0.2.

**Alternatives considered:** Broader v0.1 including USN and regex/glob; minimal
v0.1 without NTFS.

---

## ADR-011 — Feasibility-first milestone ordering

**Decision:** Order the work **vertical-slice-first**: the first milestone is an
`omarchy-shell` UI spike (persistent search bar, hotkey focus, floating results
with fake data) and the second is a walking skeleton (minimal daemon + Unix
socket + UI end-to-end with real data). The full daemon work follows.

**Rationale:** The two riskiest assumptions are (a) that the bar can host a
focusable search input and (b) that the UI-to-daemon path is workable. Validating
both before building the index, scanner, and NTFS layers means the project fails
cheaply if it is not feasible, rather than after significant effort.

**Consequences:** The first milestone carries a hard gate: if the bar cannot
focus the input and the fallback companion overlay is also unworkable, the
project stops and is reassessed. The daemon is built against a UI that is already
proven.

**Alternatives considered:** Build the daemon first and integrate the UI last
(the original ordering); build both in parallel.

---

## ADR-012 — Scratchpad UI shape

**Decision:** The search UI is a **centered scratchpad tile** (~70% width x up to
70% height) with **high transparency** (>40%), containing the input at the top
and the results table below. The bar widget is a summon affordance that mirrors
the query.

**Rationale:** A centered tile gives the results table enough width for the
columns that distinguish duplicates, and reads as an ephemeral scratchpad rather
than a cramped dropdown. High transparency keeps the desktop visible behind it.

**Consequences:** The tile is larger than the previous top-right card; the
overlay geometry and the bar widget's role change.

**Alternatives considered:** A wider top-anchored window; a narrow dropdown.

---

## ADR-013 — Result columns

**Decision:** Each result row shows **name** (basename), **size**, **modified**
(mtime), and **path** (parent directory), with a header row.

**Rationale:** Everything users rarely organize their files and accumulate
duplicates; showing size, modified, and path alongside the name is how Everything
lets users tell duplicates apart. The name alone is ambiguous.

**Consequences:** The protocol and index must carry size and mtime; the UI must
format them compactly.

**Alternatives considered:** Name + path only; a two-line row.

---

## ADR-014 — Bar widget query mirror

**Decision:** The bar widget mirrors the query typed in the scratchpad, so it
reacts to every keystroke. The mirror is carried over a small runtime file
(`$XDG_RUNTIME_DIR/omavrything-query`) that the overlay writes and the bar widget
watches.

**Rationale:** The bar widget and the overlay are separate components and cannot
share QML state directly; a runtime file is the simplest channel that keeps all
monitors' bar widgets in sync.

**Consequences:** A file write per keystroke (negligible); the mechanism is a
spike-level channel that can be replaced by a shared singleton or a daemon
broadcast later.

**Alternatives considered:** A single component hosting both the bar box and the
scratchpad (per-monitor query state); a daemon pub/sub channel.

---

## ADR-015 — Settings menu

**Decision:** The scratchpad exposes a **settings menu** opened from a **gear
button** in its top-right corner. Settings include the tile transparency, the
size fractions, and the result cap, persisted to the plugin's `shell.json` entry.

**Rationale:** The tile's appearance (transparency, size) is user-tunable, and a
gear button is the conventional affordance. Persisting to `shell.json` keeps the
shell's single-config model.

**Consequences:** The gear button is a placeholder until the menu is implemented.
Settings are read from the plugin entry and written via the shell's
`updateEntryInline`.

**Alternatives considered:** A separate config file; CLI-only settings.

---

## ADR-016 — Branching and release model

**Decision:** `main` is a **protected marketplace/release branch** that is never
written to directly. All work lands on the **`develop`** integration branch via
short-lived topic branches (`feat/*`, `fix/*`, `docs/*`, `chore/*`), and reaches
`main` only through a reviewed pull request. Releases are tagged on `main` after
the `develop` → `main` merge.

**Rationale:** The Omarchy Marketplace tracks `main`, so a direct commit there
changes what every marketplace user installs, with no review and no chance to run
the full CI matrix first. Keeping `main` releasable at all times and routing work
through `develop` gives a review gate and a stable release branch.

**Consequences:** Contributors and agents must branch off `develop` and open PRs
into `develop`; only a maintainer merges into `main` and tags a release. `main`
has **branch protection enabled on the upstream repository** as a server-side
failsafe, so a direct push is rejected even if the convention is forgotten. CI
already runs on every push and PR, so `develop` is covered without workflow
changes; the docs site still publishes only from `main`.

**Alternatives considered:** Committing directly to `main` (rejected: no review
gate on the marketplace-tracked branch); a `release/*` branch per version
(rejected: unnecessary for a single-package project); tagging on `develop`
(rejected: the marketplace and docs site track `main`).
