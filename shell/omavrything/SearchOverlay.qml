import Quickshell
import Quickshell.Io
import Quickshell.Wayland
import QtQuick
import qs.Commons
import qs.Ui

// The search scratchpad: a centered, high-transparency tile with the input at
// the top and a results table below.
//
// The table shows name, size, modified, and path so duplicates are
// distinguishable — Everything users rarely organize their files, so the name
// alone is ambiguous.
//
// This is the ADR-008 fallback (see docs/decisions.md): because the bar surface
// cannot take keyboard focus, the input lives here rather than in the bar
// widget. The overlay is a layer-shell PanelWindow with keyboardFocus Exclusive
// while open.
//
// Results come from the `omavrythingd` daemon over a Unix domain socket
// (newline-delimited JSON). The query is mirrored to a runtime file so the bar
// widget can react to every keystroke.
Item {
  id: root

  property string omarchyPath: Quickshell.env("OMARCHY_PATH")
  property var shell: null
  property var manifest: null

  property bool opened: false
  property string filterText: ""
  property int selectedIndex: 0
  property bool cursorActive: false
  property var results: []
  property bool daemonAvailable: false
  // True while the daemon is mapping an evicted index; the empty state shows a
  // "reloading" message and the query is retried.
  property bool indexLoading: false

  // Column sorting: "" keeps the daemon's ranking; otherwise the results are
  // re-sorted client-side by the selected column.
  property string sortKey: ""
  property bool sortAscending: true

  // Scratchpad sizing (settable; defaults for the M2.5 spike).
  property real widthFraction: 0.7
  property real heightFraction: 0.7
  property int maxWidth: 1200
  property int maxHeight: 800
  property int maxResults: 200
  property real backgroundOpacity: 0.75

  // Settings menu state. The menu reads and writes the daemon configuration
  // through the `omavry` CLI (which speaks the polkit-gated D-Bus interface).
  property bool settingsOpen: false
  property bool settingsBusy: false
  property string settingsError: ""
  property var settings: ({})
  property string configCommand: "omavry"

  // Detected block volumes and their indexing status, shown at the bottom of
  // the settings panel. Refreshed while the panel is open so a mount/index
  // transition is visible without reopening it.
  property var volumes: []
  property bool volumesBusy: false
  property string volumesError: ""

  // Snapshot-interval choices: 30 s to 300 s in 30 s steps.
  readonly property var snapshotOptions: {
    var opts = []
    for (var s = 30; s <= 300; s += 30) opts.push({ value: String(s), label: s + " s" })
    return opts
  }

  // Eviction-cadence choices: 0 s (disables eviction) then 10 s to 60 s in 10 s
  // steps. The daemon evicts the index to its snapshot image after this idle
  // period, dropping RSS to the mapping plus a small delta; a query reloads it.
  readonly property var evictionOptions: {
    var opts = [{ value: "0", label: "0 s (disables eviction)" }]
    for (var s = 10; s <= 60; s += 10) opts.push({ value: String(s), label: s + " s" })
    return opts
  }

  readonly property string runtimeDir: Quickshell.env("XDG_RUNTIME_DIR") || "/tmp"
  // Socket selection mirrors the CLI's connect_default(): prefer the system
  // daemon's socket (/run/omavrything/omavrything.sock), fall back to the
  // per-user socket in $XDG_RUNTIME_DIR (user-mode daemon). socketProbe resolves
  // the path by checking that the socket exists *and* is writable (a Unix socket
  // needs write permission to connect), so a system socket the user cannot reach
  // falls through to the user socket instead of failing. Re-probed on every
  // open() so a daemon that started after the shell is still found.
  property string socketPath: runtimeDir + "/omavrything.sock"
  // Set when open() is waiting for socketProbe to resolve the socket path
  // before the Socket is allowed to connect.
  property bool pendingOpen: false
  // The query mirror is a private channel between the overlay and the bar. It
  // must never live in a world-writable directory: a predictable path under
  // /tmp lets another local user pre-create it as a symlink (the overlay would
  // then overwrite the link target with the query) or feed the bar arbitrary
  // content. It therefore uses $XDG_RUNTIME_DIR (0700) and is disabled when
  // that is unset — a Wayland session always sets it — rather than falling back
  // to /tmp. The socket path above keeps the /tmp fallback only to stay
  // consistent with the daemon and CLI.
  readonly property string queryDir: Quickshell.env("XDG_RUNTIME_DIR") || ""
  readonly property string queryFile: queryDir !== "" ? queryDir + "/omavrything-query" : ""

  // Theme: shares the [menu] surface tokens.
  property color background: Color.menu.background
  property color foreground: Color.menu.text
  property color border: Color.menu.border
  property var borderSpec: Border.surfaceSpec("menu", "border", border, Math.max(1, Style.space(2)))
  property color selectedBackground: Color.menu.selectedBackground
  property color selectedText: Color.menu.selectedText
  readonly property int cornerRadius: Style.cornerRadius
  property string fontFamily: Style.font.menuFamily
  property int contentMargin: Style.spacing.panelPadding
  property int inputHeight: Math.max(Style.space(48), Style.font.heading + Style.spacing.controlPaddingY * 2)
  property int headerHeight: Math.max(Style.space(24), Style.font.bodySmall + Style.spacing.controlPaddingY)
  property int rowHeight: Math.max(Style.space(30), Style.font.body + Style.spacing.rowPaddingX)
  property int contentSpacing: Style.spacing.xs
  property int columnGap: Style.space(12)

  // Column widths as fractions of the row width; path takes the remainder.
  readonly property real colNameFrac: 0.34
  readonly property real colSizeFrac: 0.10
  readonly property real colModifiedFrac: 0.16

  readonly property real contentWidth: card.width - card.contentLeftInset - card.contentRightInset
  readonly property real nameWidth: Math.round(contentWidth * colNameFrac)
  readonly property real sizeWidth: Math.round(contentWidth * colSizeFrac)
  readonly property real modifiedWidth: Math.round(contentWidth * colModifiedFrac)
  readonly property real pathWidth: Math.max(0, contentWidth - nameWidth - sizeWidth - modifiedWidth - columnGap * 3)

  // Volume table columns: mount point, filesystem type, label, status, then
  // the mount and unmount buttons.
  readonly property real volMountWidth: Math.round(contentWidth * 0.30)
  readonly property real volFsWidth: Math.round(contentWidth * 0.10)
  readonly property real volLabelWidth: Math.round(contentWidth * 0.18)
  readonly property int volButtonSize: Style.space(24)

  // Plugin lifecycle hooks, called by the shell host.
  function open(payloadJson) {
    root.settingsOpen = false
    root.filterText = ""
    root.selectedIndex = 0
    root.cursorActive = false
    root.results = []
    displayModel.clear()
    root.writeQuery("")
    if (sock.connected) {
      root.opened = true
      root.rebuild()
      Qt.callLater(root.focusInput)
    } else {
      // Resolve the daemon socket path before the Socket connects: binding
      // `connected` to `opened` makes the Socket connect the moment opened
      // flips, so the path must already be correct. Force `opened` false first
      // so a Socket that is holding a failed connection is torn down and
      // reconnects to the freshly resolved path.
      root.opened = false
      root.pendingOpen = true
      socketProbe.running = true
    }
  }

  function close() {
    root.opened = false
  }

  // Focus the custom input. The input is created lazily by inputLoader while
  // the overlay is open (see the Loader below), so this is a no-op if the item
  // does not exist yet.
  function focusInput() {
    if (inputLoader.item) inputLoader.item.forceActiveFocus()
  }

  function dismiss() {
    root.opened = false
    root.settingsOpen = false
    root.writeQuery("")
    if (root.shell && typeof root.shell.hide === "function")
      root.shell.hide((root.manifest && root.manifest.id) || "omavrything")
  }

  // Open the settings menu and load the current daemon configuration.
  function openSettings() {
    root.settingsOpen = true
    root.settingsError = ""
    root.volumesError = ""
    configGetProc.running = true
    root.refreshVolumes()
  }

  function closeSettings() {
    root.settingsOpen = false
  }

  // Load the detected volumes and their statuses from the daemon.
  function refreshVolumes() {
    if (!root.settingsOpen) return
    volumesGetProc.running = true
  }

  function applyVolumes(text) {
    var parsed
    try { parsed = JSON.parse(text) } catch (e) { root.volumesError = "Could not read volumes"; return }
    if (!Array.isArray(parsed)) return
    root.volumes = parsed
  }

  // Mount and index one volume through the daemon (polkit-gated).
  function mountVolume(device) {
    if (root.volumesBusy) return
    root.volumesBusy = true
    root.volumesError = ""
    volumeMountProc.command = [root.configCommand, "mount-volume", device]
    volumeMountProc.running = true
  }

  // Unmount one volume and drop its index entries (polkit-gated).
  function unmountVolume(device) {
    if (root.volumesBusy) return
    root.volumesBusy = true
    root.volumesError = ""
    volumeUnmountProc.command = [root.configCommand, "unmount-volume", device]
    volumeUnmountProc.running = true
  }

  // Parse `omavry config get --json` output and apply the UI-side setting.
  function applyConfig(text) {
    var parsed
    try { parsed = JSON.parse(text) } catch (e) { root.settingsError = "Could not read settings"; return }
    if (!parsed || typeof parsed !== "object") return
    root.settings = parsed
    var opacity = Number(parsed.background_opacity)
    if (!isNaN(opacity) && opacity > 0) root.backgroundOpacity = opacity
  }

  // Persist one setting through the daemon (polkit-gated D-Bus SetConfig).
  function setSetting(key, value) {
    root.settingsBusy = true
    root.settingsError = ""
    // Optimistically update the local copy so the control reflects the change.
    var next = {}
    for (var k in root.settings) next[k] = root.settings[k]
    next[key] = String(value)
    root.settings = next
    configSetProc.command = [root.configCommand, "config", "set", key, String(value)]
    configSetProc.running = true
  }

  // The current value of a setting, as a string.
  function settingValue(key, fallback) {
    var v = root.settings ? root.settings[key] : undefined
    return (v === undefined || v === null) ? fallback : String(v)
  }

  // Mirror the query to the runtime file the bar widget watches. No-op when the
  // mirror is disabled (no private per-user directory).
  function writeQuery(text) {
    if (root.queryFile === "") return
    queryFileView.setText(text)
  }

  // Schedule a search. Rapid keystrokes restart the debounce timer so a burst of
  // typing issues one request instead of one per character; without this, a slow
  // daemon falls behind and the UI lags by seconds.
  function rebuild() {
    searchDebounce.restart()
  }

  // Send the current query to the daemon. The response arrives asynchronously
  // in handleResponse().
  function sendQuery() {
    if (!sock.connected) {
      root.results = []
      displayModel.clear()
      return
    }
    var request = JSON.stringify({ q: root.filterText, limit: root.maxResults })
    sock.write(request + "\n")
  }

  function handleResponse(line) {
    var response
    try { response = JSON.parse(line) } catch (e) { return }
    if (!response || !Array.isArray(response.results)) return

    // The daemon evicts the index when idle; a query while evicted returns
    // `state: "loading"` and starts the load. Retry shortly so the next
    // response carries results.
    root.indexLoading = response.state === "loading"
    if (root.indexLoading) {
      root.results = []
      displayModel.clear()
      reloadRetry.restart()
      return
    }

    // Bound the page. The daemon is trusted, but a spoofed socket or a
    // compromised daemon must not be able to make the shared shell allocate an
    // unbounded result set (0.1.8 security hardening).
    var results = response.results
    if (results.length > root.maxResults) results = results.slice(0, root.maxResults)

    root.results = results
    if (root.selectedIndex >= results.length)
      root.selectedIndex = Math.max(0, results.length - 1)
    if (results.length === 0) root.cursorActive = false

    root.applySort()
    root.rebuildModel()
  }

  // Re-sort the current page by the active column. The daemon returns a ranked
  // page; sorting reorders that page client-side.
  function applySort() {
    if (!root.sortKey) return
    var key = root.sortKey
    var asc = root.sortAscending
    root.results.sort(function(a, b) {
      var va, vb
      if (key === "name") { va = (a.name || "").toLowerCase(); vb = (b.name || "").toLowerCase() }
      else if (key === "size") { va = Number(a.size) || 0; vb = Number(b.size) || 0 }
      else if (key === "modified") { va = Number(a.mtime) || 0; vb = Number(b.mtime) || 0 }
      else { va = (a.path || "").toLowerCase(); vb = (b.path || "").toLowerCase() }
      if (va < vb) return asc ? -1 : 1
      if (va > vb) return asc ? 1 : -1
      return 0
    })
  }

  // Click a header: same column toggles direction, a new column starts ascending.
  function toggleSort(key) {
    if (root.sortKey === key) root.sortAscending = !root.sortAscending
    else { root.sortKey = key; root.sortAscending = true }
    root.applySort()
    root.rebuildModel()
    root.selectedIndex = 0
  }

  function sortIndicator(key) {
    if (root.sortKey !== key) return ""
    return root.sortAscending ? "  ▲" : "  ▼"
  }

  function rebuildModel() {
    displayModel.clear()
    for (var i = 0; i < root.results.length; i++) {
      var r = root.results[i]
      displayModel.append({
        name: r.name || "",
        path: r.path || "",
        parentPath: root.parentPath(r.path || ""),
        sizeText: root.formatSize(r.size, r.is_dir === true),
        modifiedText: root.formatModified(r.mtime),
        isDir: r.is_dir === true
      })
    }
  }

  function select(delta) {
    if (root.results.length === 0) return
    root.cursorActive = true
    root.selectedIndex = (root.selectedIndex + delta + root.results.length) % root.results.length
    resultList.positionViewAtIndex(root.selectedIndex, ListView.Contain)
  }

  function activate() {
    if (root.results.length === 0) return
    // With no explicit cursor, Enter opens the top result (Everything behavior).
    var index = root.cursorActive ? root.selectedIndex : 0
    if (index < 0 || index >= root.results.length) return
    var item = root.results[index]
    // Only ever hand an absolute path to xdg-open. The index holds absolute
    // paths, but a spoofed socket could return a URL or an option-like string
    // ("--help"), which xdg-open would treat as a URL or a flag.
    if (typeof item.path !== "string" || item.path.charAt(0) !== "/") return
    root.dismiss()
    Quickshell.execDetached(["xdg-open", item.path])
  }

  // ---------------------------------------------------------------- formatting

  function parentPath(path) {
    var i = path.lastIndexOf("/")
    if (i <= 0) return "/"
    return path.substring(0, i)
  }

  function formatSize(bytes, isDir) {
    if (isDir) return ""
    var n = Number(bytes) || 0
    if (n < 1024) return n + " B"
    var units = ["KB", "MB", "GB", "TB", "PB"]
    var value = n / 1024
    var i = 0
    while (value >= 1024 && i < units.length - 1) { value /= 1024; i++ }
    return (value >= 10 ? Math.round(value) : value.toFixed(1)) + " " + units[i]
  }

  function formatModified(mtime) {
    var t = Number(mtime) || 0
    if (t <= 0) return ""
    var now = Math.floor(Date.now() / 1000)
    var diff = now - t
    if (diff < 60) return "just now"
    if (diff < 3600) return Math.floor(diff / 60) + "m ago"
    if (diff < 86400) return Math.floor(diff / 3600) + "h ago"
    if (diff < 7 * 86400) return Math.floor(diff / 86400) + "d ago"
    var d = new Date(t * 1000)
    function pad(v) { return v < 10 ? "0" + v : "" + v }
    return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate())
  }

  ListModel { id: displayModel }

  FileView {
    id: queryFileView
    path: root.queryFile
    printErrors: false
  }

  // Coalesces a burst of keystrokes into a single search request. 40 ms is below
  // the threshold of perception but long enough to absorb fast typing.
  Timer {
    id: searchDebounce
    interval: 40
    repeat: false
    onTriggered: root.sendQuery()
  }

  // Retries a query while the daemon is reloading an evicted index. The daemon
  // materializes the owned index in the background (~1-2 s at 1.38 M records),
  // so the retry is spaced to avoid a burst of requests while it runs.
  Timer {
    id: reloadRetry
    interval: 500
    repeat: false
    onTriggered: root.sendQuery()
  }

  // Probe which daemon socket exists before the Socket binds to a path. Runs
  // once at startup so the path is correct before the first open(), and again
  // on every open() while disconnected so a daemon that started later is found.
  Process {
    id: socketProbe
    running: true
    // Prefer the system socket, then the per-user socket. A socket is only
    // usable if it exists and is writable (connect(2) needs write permission),
    // so an inaccessible system socket falls through to the user socket. The
    // user socket path is passed as $1 to avoid quoting it into the script.
    command: ["sh", "-c",
      "sys=/run/omavrything/omavrything.sock; usr=$1; " +
      "if [ -S \"$sys\" ] && [ -w \"$sys\" ]; then echo system; " +
      "elif [ -S \"$usr\" ] && [ -w \"$usr\" ]; then echo user; " +
      "else echo none; fi",
      "probe", root.runtimeDir + "/omavrything.sock"]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var which = text.trim()
        if (which === "system") root.socketPath = "/run/omavrything/omavrything.sock"
        else if (which === "user") root.socketPath = root.runtimeDir + "/omavrything.sock"
        else { root.socketPath = ""; root.daemonAvailable = false }
        if (root.pendingOpen) {
          root.pendingOpen = false
          if (which === "none") {
            root.opened = false
          } else {
            root.opened = true
            root.rebuild()
            Qt.callLater(root.focusInput)
          }
        }
      }
    }
  }

  Socket {
    id: sock
    path: root.socketPath
    connected: root.opened
    parser: SplitParser {
      onRead: function(line) { root.handleResponse(line) }
    }
    onConnectionStateChanged: {
      root.daemonAvailable = sock.connected
      if (sock.connected && root.opened) root.rebuild()
    }
  }

  // Read the daemon configuration for the settings menu.
  Process {
    id: configGetProc
    command: [root.configCommand, "config", "get", "--json"]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: root.applyConfig(text)
    }
    onExited: function(exitCode) {
      if (exitCode !== 0) root.settingsError = "Daemon unavailable"
    }
  }

  // Write one setting back to the daemon.
  Process {
    id: configSetProc
    stdout: StdioCollector { waitForEnd: true }
    onExited: function(exitCode) {
      root.settingsBusy = false
      if (exitCode !== 0) root.settingsError = "Could not apply setting"
    }
  }

  // Read the detected volumes and their statuses for the settings panel.
  Process {
    id: volumesGetProc
    command: [root.configCommand, "volumes", "--json"]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: root.applyVolumes(text)
    }
    onExited: function(exitCode) {
      if (exitCode !== 0) root.volumesError = "Could not read volumes"
    }
  }

  // Mount and index one volume.
  Process {
    id: volumeMountProc
    stdout: StdioCollector { waitForEnd: true }
    onExited: function(exitCode) {
      root.volumesBusy = false
      if (exitCode !== 0) root.volumesError = "Could not mount volume"
      else root.refreshVolumes()
    }
  }

  // Unmount one volume and drop its index entries.
  Process {
    id: volumeUnmountProc
    stdout: StdioCollector { waitForEnd: true }
    onExited: function(exitCode) {
      root.volumesBusy = false
      if (exitCode !== 0) root.volumesError = "Could not unmount volume"
      else root.refreshVolumes()
    }
  }

  // Poll while the settings panel is open so "indexing" transitions to
  // "indexed" without the user reopening the panel.
  Timer {
    id: volumesTimer
    interval: 2000
    repeat: true
    running: root.settingsOpen
    onTriggered: root.refreshVolumes()
  }

  PanelWindow {
    id: panel
    visible: root.opened
    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    WlrLayershell.namespace: "omavrything"
    WlrLayershell.layer: WlrLayer.Overlay
    WlrLayershell.keyboardFocus: root.opened ? WlrKeyboardFocus.Exclusive : WlrKeyboardFocus.None
    exclusionMode: ExclusionMode.Ignore

    MouseArea { anchors.fill: parent; onClicked: root.dismiss() }

    BorderSurface {
      id: card
      width: Math.min(root.maxWidth, Math.round(panel.width * root.widthFraction))
      height: Math.min(root.maxHeight, Math.round(panel.height * root.heightFraction))
      radius: root.cornerRadius
      color: Util.alpha(root.background, root.backgroundOpacity)
      borderSpec: root.borderSpec
      padding: root.contentMargin
      anchors.centerIn: parent

      MouseArea { anchors.fill: parent; onClicked: {} }

      Column {
        anchors.fill: parent
        anchors.topMargin: card.contentTopInset
        anchors.rightMargin: card.contentRightInset
        anchors.bottomMargin: card.contentBottomInset
        anchors.leftMargin: card.contentLeftInset
        spacing: root.contentSpacing

        // ---------------------------------------------------------- input row
        Item {
          width: parent.width
          height: root.inputHeight

          Text {
            id: searchGlyph
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            text: "󰍉"
            color: root.foreground
            opacity: 0.7
            font.family: root.fontFamily
            font.pixelSize: Style.font.heading
          }

          // Settings button (top-right). The settings menu itself is planned for
          // a later milestone; the button is a placeholder for now.
          Item {
            id: settingsButton
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            width: Style.space(28)
            height: Style.space(28)

            Text {
              anchors.centerIn: parent
              text: "󰒓"
              color: root.foreground
              opacity: settingsMouse.containsMouse ? 0.95 : 0.6
              font.family: root.fontFamily
              font.pixelSize: Style.font.heading
            }

            MouseArea {
              id: settingsMouse
              anchors.fill: parent
              hoverEnabled: true
              cursorShape: Qt.PointingHandCursor
              onClicked: root.openSettings()
            }
          }

          // The search input is a custom, non-native field: a focusable Item
          // that renders the query with a Text element and edits it from
          // Keys.onPressed. It deliberately does NOT use a TextInput/TextEdit.
          //
          // On Qt 6.12 a native editable TextInput connects to
          // QClipboard::dataChanged and synchronously reads the foreign text
          // over the Wayland transfer with no aggregate byte limit and no total
          // deadline, so a streaming clipboard owner could stall or exhaust the
          // shared shell with no paste action and no field focus. Gating the
          // Loader on `opened` only removed the input while the window was
          // hidden; the class remained while the overlay was open. A custom
          // input has no clipboard connection at all, so the class cannot
          // recur. The Loader still gates creation on the open state so the
          // field only exists while the user is actually searching.
          Loader {
            id: inputLoader
            anchors.left: searchGlyph.right
            anchors.leftMargin: Style.space(10)
            anchors.right: settingsButton.left
            anchors.rightMargin: Style.space(10)
            anchors.top: parent.top
            anchors.bottom: parent.bottom
            active: root.opened
            sourceComponent: Item {
              id: input
              anchors.fill: parent
              focus: true

              // Blink the caret while the field has focus.
              property bool caretOn: true
              onActiveFocusChanged: if (activeFocus) caretOn = true
              Timer {
                interval: 500
                repeat: true
                running: input.activeFocus
                onTriggered: input.caretOn = !input.caretOn
              }

              // Append a printable character and mirror the query to the bar.
              function appendText(ch) {
                if (!ch) return
                root.filterText += ch
                root.selectedIndex = 0
                root.writeQuery(root.filterText)
                root.rebuild()
              }

              // Remove the last character and mirror the query to the bar.
              function backspace() {
                if (root.filterText.length === 0) return
                root.filterText = root.filterText.substring(0, root.filterText.length - 1)
                root.selectedIndex = 0
                root.writeQuery(root.filterText)
                root.rebuild()
              }

              Text {
                id: inputText
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                width: Math.max(0, parent.width - cursor.width)
                text: root.filterText
                // The query is user-typed, but it is still rendered as plain
                // text: Text defaults to AutoText, which would hand a query
                // that "looks like" rich text to Qt's rich-text engine.
                textFormat: Text.PlainText
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.heading
                elide: Text.ElideRight
                clip: true
              }

              // Caret at the end of the query. The custom field has no in-text
              // cursor movement; the query is edited at its end.
              Rectangle {
                id: cursor
                x: Math.min(inputText.contentWidth, inputText.width)
                anchors.verticalCenter: parent.verticalCenter
                width: Math.max(1, Style.space(2))
                height: inputText.height
                color: root.foreground
                visible: input.activeFocus
                opacity: input.caretOn ? 1 : 0
              }

              Text {
                anchors.fill: parent
                text: "Search files…"
                visible: root.filterText.length === 0
                color: root.foreground
                opacity: 0.45
                font.family: root.fontFamily
                font.pixelSize: Style.font.heading
                verticalAlignment: Text.AlignVCenter
              }

              Keys.onPressed: function(event) {
                if (event.key === Qt.Key_Escape) { root.dismiss(); event.accepted = true }
                else if (event.key === Qt.Key_Up) { root.select(-1); event.accepted = true }
                else if (event.key === Qt.Key_Down) { root.select(1); event.accepted = true }
                else if (event.key === Qt.Key_PageUp) { root.select(-10); event.accepted = true }
                else if (event.key === Qt.Key_PageDown) { root.select(10); event.accepted = true }
                else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) { root.activate(); event.accepted = true }
                else if (event.key === Qt.Key_Backspace || event.key === Qt.Key_Delete) { input.backspace(); event.accepted = true }
                else if (event.text.length > 0 && event.text.charCodeAt(0) >= 0x20) { input.appendText(event.text); event.accepted = true }
              }
            }
          }
        }

        // --------------------------------------------------------- header row
        Item {
          width: parent.width
          height: root.headerHeight

          Row {
            anchors.fill: parent
            spacing: root.columnGap

            // Each header is clickable: clicking sorts by that column, and
            // clicking again toggles ascending/descending. The active column
            // shows an arrow and is drawn brighter.
            Item {
              width: root.nameWidth
              height: parent.height
              Text {
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "Name" + root.sortIndicator("name")
                color: root.foreground
                opacity: root.sortKey === "name" ? 0.85 : 0.5
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                elide: Text.ElideRight
              }
              MouseArea {
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.toggleSort("name")
              }
            }
            Item {
              width: root.sizeWidth
              height: parent.height
              Text {
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                text: "Size" + root.sortIndicator("size")
                color: root.foreground
                opacity: root.sortKey === "size" ? 0.85 : 0.5
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                horizontalAlignment: Text.AlignRight
              }
              MouseArea {
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.toggleSort("size")
              }
            }
            Item {
              width: root.modifiedWidth
              height: parent.height
              Text {
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                text: "Modified" + root.sortIndicator("modified")
                color: root.foreground
                opacity: root.sortKey === "modified" ? 0.85 : 0.5
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                horizontalAlignment: Text.AlignRight
              }
              MouseArea {
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.toggleSort("modified")
              }
            }
            Item {
              width: root.pathWidth
              height: parent.height
              Text {
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "Path" + root.sortIndicator("path")
                color: root.foreground
                opacity: root.sortKey === "path" ? 0.85 : 0.5
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                elide: Text.ElideRight
              }
              MouseArea {
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.toggleSort("path")
              }
            }
          }
        }

        // ------------------------------------------------------------ results
        Item {
          width: parent.width
          height: parent.height - root.inputHeight - root.headerHeight - root.contentSpacing * 2

          ListView {
            id: resultList
            anchors.fill: parent
            visible: root.results.length > 0 && !root.settingsOpen
            model: displayModel
            clip: true
            spacing: 0
            boundsBehavior: Flickable.StopAtBounds

            delegate: Rectangle {
              id: rowDelegate
              required property int index
              required property string name
              required property string path
              required property string parentPath
              required property string sizeText
              required property string modifiedText
              required property bool isDir
              readonly property bool hasCursor: root.cursorActive && index === root.selectedIndex

              width: ListView.view.width
              height: root.rowHeight
              radius: root.cornerRadius
              color: hasCursor ? root.selectedBackground : "transparent"

              Row {
                anchors.fill: parent
                anchors.leftMargin: Style.space(6)
                anchors.rightMargin: Style.space(6)
                spacing: root.columnGap

                // Every cell renders index-derived text. File names and parent
                // paths are attacker-controlled: any local user can create a
                // directory whose name is HTML. Text defaults to AutoText,
                // which hands a string that "looks like" rich text to Qt's
                // rich-text engine; an <img src="..."> in a directory name
                // would then load and decode an attacker-chosen local file
                // (PNG/JPG/SVG/PDF/...) — or fetch a remote URL — during a
                // search, a memory-exhaustion vector in the shared shell.
                // PlainText disables rich-text parsing entirely, so no
                // resource is ever loaded. Do not remove these.
                Text {
                  width: root.nameWidth
                  anchors.verticalCenter: parent.verticalCenter
                  text: rowDelegate.name
                  textFormat: Text.PlainText
                  color: rowDelegate.hasCursor ? root.selectedText : root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  elide: Text.ElideRight
                }
                Text {
                  width: root.sizeWidth
                  anchors.verticalCenter: parent.verticalCenter
                  text: rowDelegate.sizeText
                  textFormat: Text.PlainText
                  color: rowDelegate.hasCursor ? root.selectedText : root.foreground
                  opacity: 0.75
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  horizontalAlignment: Text.AlignRight
                }
                Text {
                  width: root.modifiedWidth
                  anchors.verticalCenter: parent.verticalCenter
                  text: rowDelegate.modifiedText
                  textFormat: Text.PlainText
                  color: rowDelegate.hasCursor ? root.selectedText : root.foreground
                  opacity: 0.75
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  horizontalAlignment: Text.AlignRight
                }
                Text {
                  width: root.pathWidth
                  anchors.verticalCenter: parent.verticalCenter
                  text: rowDelegate.parentPath
                  textFormat: Text.PlainText
                  color: rowDelegate.hasCursor ? root.selectedText : root.foreground
                  opacity: 0.55
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  elide: Text.ElideLeft
                }
              }

              MouseArea {
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onContainsMouseChanged: if (containsMouse) { root.cursorActive = true; root.selectedIndex = index }
                onClicked: { root.cursorActive = true; root.selectedIndex = index; root.activate() }
              }
            }
          }

          Text {
            anchors.centerIn: parent
            visible: root.results.length === 0 && !root.settingsOpen
            text: !root.daemonAvailable
              ? "Search daemon unavailable"
              : (root.indexLoading
                 ? "Reloading index…"
                 : (root.filterText ? "No matches for “" + root.filterText + "”" : "Type to search files"))
            // Echoes the user's query; render as plain text so a query that
            // looks like rich text is never parsed by the rich-text engine.
            textFormat: Text.PlainText
            color: root.foreground
            opacity: 0.6
            font.family: root.fontFamily
            font.pixelSize: Style.font.body
          }

          // ---------------------------------------------------- settings menu
          Flickable {
            id: settingsPanel
            anchors.fill: parent
            visible: root.settingsOpen
            contentWidth: width
            contentHeight: settingsColumn.implicitHeight
            clip: true
            boundsBehavior: Flickable.StopAtBounds

            Column {
              id: settingsColumn
              width: parent.width
              spacing: root.contentSpacing

            // Header: title + close button.
            Item {
              width: parent.width
              height: root.headerHeight

              Text {
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "Settings"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                font.bold: true
              }

              Item {
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                width: Style.space(24)
                height: Style.space(24)

                Text {
                  anchors.centerIn: parent
                  text: "󰅖"
                  color: root.foreground
                  opacity: closeMouse.containsMouse ? 0.95 : 0.6
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                }

                MouseArea {
                  id: closeMouse
                  anchors.fill: parent
                  hoverEnabled: true
                  cursorShape: Qt.PointingHandCursor
                  onClicked: root.closeSettings()
                }
              }
            }

            // Opacity (0.3–1.0): dragging right makes the tile more opaque.
            Item {
              width: parent.width
              height: root.rowHeight

              Text {
                width: root.nameWidth
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "Opacity"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
              }

              PanelSlider {
                anchors.left: parent.left
                anchors.leftMargin: root.nameWidth + root.columnGap
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                minimum: 0.3
                maximum: 1.0
                step: 0.05
                value: root.backgroundOpacity
                trackColor: Util.alpha(root.foreground, 0.2)
                fillColor: root.foreground
                knobColor: root.foreground
                onMoved: function(v) { root.backgroundOpacity = v }
                onReleased: function(v) {
                  root.backgroundOpacity = v
                  root.setSetting("background_opacity", v.toFixed(2))
                }
              }
            }

            // Snapshot interval.
            Item {
              width: parent.width
              height: root.rowHeight

              Text {
                width: root.nameWidth
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "Snapshot interval"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
              }

              Dropdown {
                anchors.left: parent.left
                anchors.leftMargin: root.nameWidth + root.columnGap
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                showLabel: false
                value: root.settingValue("snapshot_interval", "300")
                options: root.snapshotOptions
                foreground: root.foreground
                background: root.background
                popupBorder: root.border
                accent: Color.accent
                fontFamily: root.fontFamily
                onChanged: function(v) { root.setSetting("snapshot_interval", v) }
              }
            }

            // Eviction cadence: the idle period after which the index is evicted
            // to its snapshot image (10-60 s in 10 s steps; default 20 s).
            Item {
              width: parent.width
              height: root.rowHeight

              Text {
                width: root.nameWidth
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "Index eviction timer"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
              }

              Dropdown {
                anchors.left: parent.left
                anchors.leftMargin: root.nameWidth + root.columnGap
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                showLabel: false
                value: root.settingValue("evict_idle_secs", "20")
                options: root.evictionOptions
                foreground: root.foreground
                background: root.background
                popupBorder: root.border
                accent: Color.accent
                fontFamily: root.fontFamily
                onChanged: function(v) { root.setSetting("evict_idle_secs", v) }
              }
            }

            // Log level.
            Item {
              width: parent.width
              height: root.rowHeight

              Text {
                width: root.nameWidth
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "Log level"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
              }

              Dropdown {
                anchors.left: parent.left
                anchors.leftMargin: root.nameWidth + root.columnGap
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                showLabel: false
                value: root.settingValue("log_level", "info")
                options: ["error", "warn", "info", "debug", "trace"]
                foreground: root.foreground
                background: root.background
                popupBorder: root.border
                accent: Color.accent
                fontFamily: root.fontFamily
                onChanged: function(v) { root.setSetting("log_level", v) }
              }
            }

            // NTFS support. Sits directly above the Volumes section because it
            // gates whether NTFS volumes are detected and listed.
            Item {
              width: parent.width
              height: root.rowHeight

              Text {
                width: root.nameWidth
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "NTFS support"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
              }

              ToggleSwitch {
                anchors.left: parent.left
                anchors.leftMargin: root.nameWidth + root.columnGap
                anchors.verticalCenter: parent.verticalCenter
                checked: root.settingValue("ntfs", "true") === "true"
                foreground: root.foreground
                accent: Color.accent
                onToggled: root.setSetting("ntfs", !checked)
              }
            }

            // Breathing room between the NTFS toggle and the volume table.
            Item {
              width: parent.width
              height: root.contentSpacing * 2
            }

            // Volumes: every detected block filesystem, its indexing status,
            // and mount/unmount actions. NTFS rows disappear when the toggle
            // above is off (the daemon filters them out).
            Text {
              width: parent.width
              text: "Volumes"
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              font.bold: true
            }

            // Column headers.
            Item {
              width: parent.width
              height: root.headerHeight

              Text {
                width: root.volMountWidth
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "Mount point"
                color: root.foreground
                opacity: 0.6
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }

              Text {
                anchors.left: parent.left
                anchors.leftMargin: root.volMountWidth + root.columnGap
                width: root.volFsWidth
                anchors.verticalCenter: parent.verticalCenter
                text: "Type"
                color: root.foreground
                opacity: 0.6
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }

              Text {
                anchors.left: parent.left
                anchors.leftMargin: root.volMountWidth + root.volFsWidth + root.columnGap * 2
                width: root.volLabelWidth
                anchors.verticalCenter: parent.verticalCenter
                text: "Label"
                color: root.foreground
                opacity: 0.6
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }

              Text {
                anchors.left: parent.left
                anchors.leftMargin: root.volMountWidth + root.volFsWidth + root.volLabelWidth + root.columnGap * 3
                anchors.right: parent.right
                anchors.rightMargin: root.volButtonSize * 2 + root.columnGap * 2
                anchors.verticalCenter: parent.verticalCenter
                text: "Status"
                color: root.foreground
                opacity: 0.6
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }
            }

            Text {
              width: parent.width
              visible: root.volumes.length === 0
              text: "No volumes detected"
              color: root.foreground
              opacity: 0.6
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
            }

            Repeater {
              model: root.volumes

              delegate: Item {
                width: parent.width
                height: root.rowHeight

                // Mount point (the device path when the volume is unmounted).
                Text {
                  width: root.volMountWidth
                  anchors.left: parent.left
                  anchors.verticalCenter: parent.verticalCenter
                  text: modelData.mount_point && modelData.mount_point !== ""
                    ? modelData.mount_point
                    : modelData.device
                  elide: Text.ElideRight
                  color: root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                }

                // Filesystem type.
                Text {
                  anchors.left: parent.left
                  anchors.leftMargin: root.volMountWidth + root.columnGap
                  width: root.volFsWidth
                  anchors.verticalCenter: parent.verticalCenter
                  text: modelData.fstype
                  elide: Text.ElideRight
                  color: root.foreground
                  opacity: 0.8
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                }

                // Label (empty when the volume has none).
                Text {
                  anchors.left: parent.left
                  anchors.leftMargin: root.volMountWidth + root.volFsWidth + root.columnGap * 2
                  width: root.volLabelWidth
                  anchors.verticalCenter: parent.verticalCenter
                  text: modelData.label || ""
                  elide: Text.ElideRight
                  color: root.foreground
                  opacity: 0.8
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                }

                // Status.
                Text {
                  anchors.left: parent.left
                  anchors.leftMargin: root.volMountWidth + root.volFsWidth + root.volLabelWidth + root.columnGap * 3
                  anchors.right: mountButton.left
                  anchors.rightMargin: root.columnGap
                  anchors.verticalCenter: parent.verticalCenter
                  text: modelData.status
                  elide: Text.ElideRight
                  color: modelData.status === "indexing" ? Color.accent : root.foreground
                  opacity: modelData.status === "unmounted" ? 0.6 : 0.9
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                }

                // Mount button.
                Item {
                  id: mountButton
                  anchors.right: unmountButton.left
                  anchors.rightMargin: root.columnGap
                  anchors.verticalCenter: parent.verticalCenter
                  width: root.volButtonSize
                  height: root.volButtonSize

                  Text {
                    anchors.centerIn: parent
                    text: "󰋊"
                    color: root.foreground
                    opacity: mountMouse.enabled
                      ? (mountMouse.containsMouse ? 0.95 : 0.7)
                      : 0.3
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.body
                  }

                  MouseArea {
                    id: mountMouse
                    anchors.fill: parent
                    hoverEnabled: true
                    enabled: modelData.status === "unmounted" && !root.volumesBusy
                    cursorShape: enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
                    onClicked: root.mountVolume(modelData.device)
                  }
                }

                // Unmount button.
                Item {
                  id: unmountButton
                  anchors.right: parent.right
                  anchors.verticalCenter: parent.verticalCenter
                  width: root.volButtonSize
                  height: root.volButtonSize

                  Text {
                    anchors.centerIn: parent
                    text: "󰅖"
                    color: root.foreground
                    opacity: unmountMouse.enabled
                      ? (unmountMouse.containsMouse ? 0.95 : 0.7)
                      : 0.3
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.body
                  }

                  MouseArea {
                    id: unmountMouse
                    anchors.fill: parent
                    hoverEnabled: true
                    enabled: modelData.mount_point && modelData.mount_point !== "" && !root.volumesBusy
                    cursorShape: enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
                    onClicked: root.unmountVolume(modelData.device)
                  }
                }
              }
            }

            Text {
              width: parent.width
              visible: root.volumesError !== ""
              text: root.volumesError
              color: Color.accent
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }

            Text {
              width: parent.width
              visible: root.settingsError !== ""
              text: root.settingsError
              color: Color.accent
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }
            }
          }
        }
      }
    }
  }
}
