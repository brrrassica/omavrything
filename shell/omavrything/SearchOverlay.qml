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

  readonly property string runtimeDir: Quickshell.env("XDG_RUNTIME_DIR") || "/tmp"
  readonly property string socketPath: runtimeDir + "/omavrything.sock"
  readonly property string queryFile: runtimeDir + "/omavrything-query"

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

  // Plugin lifecycle hooks, called by the shell host.
  function open(payloadJson) {
    root.opened = true
    root.filterText = ""
    root.selectedIndex = 0
    root.cursorActive = false
    root.results = []
    displayModel.clear()
    root.writeQuery("")
    root.rebuild()
    Qt.callLater(function() { input.forceActiveFocus() })
  }

  function close() {
    root.opened = false
  }

  function dismiss() {
    root.opened = false
    root.writeQuery("")
    if (root.shell && typeof root.shell.hide === "function")
      root.shell.hide((root.manifest && root.manifest.id) || "omavrything")
  }

  // Placeholder for the settings menu (planned for a later milestone). The gear
  // button in the top-right corner calls this.
  function openSettings() {
    console.log("omavrything: settings requested (not implemented yet)")
  }

  // Mirror the query to the runtime file the bar widget watches.
  function writeQuery(text) {
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

    root.results = response.results
    if (root.selectedIndex >= response.results.length)
      root.selectedIndex = Math.max(0, response.results.length - 1)
    if (response.results.length === 0) root.cursorActive = false

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

          TextInput {
            id: input
            anchors.left: searchGlyph.right
            anchors.leftMargin: Style.space(10)
            anchors.right: settingsButton.left
            anchors.rightMargin: Style.space(10)
            anchors.top: parent.top
            anchors.bottom: parent.bottom
            focus: true
            text: root.filterText
            color: root.foreground
            font.family: root.fontFamily
            font.pixelSize: Style.font.heading
            verticalAlignment: TextInput.AlignVCenter
            clip: true
            selectByMouse: true
            onTextChanged: {
              if (root.filterText !== text) {
                root.filterText = text
                root.selectedIndex = 0
                root.writeQuery(text)
                root.rebuild()
              }
            }
            Keys.onPressed: function(event) {
              if (event.key === Qt.Key_Escape) { root.dismiss(); event.accepted = true }
              else if (event.key === Qt.Key_Up) { root.select(-1); event.accepted = true }
              else if (event.key === Qt.Key_Down) { root.select(1); event.accepted = true }
              else if (event.key === Qt.Key_PageUp) { root.select(-10); event.accepted = true }
              else if (event.key === Qt.Key_PageDown) { root.select(10); event.accepted = true }
              else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) { root.activate(); event.accepted = true }
            }

            Text {
              anchors.fill: parent
              text: "Search files…"
              visible: input.text.length === 0
              color: root.foreground
              opacity: 0.45
              font: input.font
              verticalAlignment: Text.AlignVCenter
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
            visible: root.results.length > 0
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

                Text {
                  width: root.nameWidth
                  anchors.verticalCenter: parent.verticalCenter
                  text: rowDelegate.name
                  color: rowDelegate.hasCursor ? root.selectedText : root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  elide: Text.ElideRight
                }
                Text {
                  width: root.sizeWidth
                  anchors.verticalCenter: parent.verticalCenter
                  text: rowDelegate.sizeText
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
            visible: root.results.length === 0
            text: !root.daemonAvailable
              ? "Search daemon unavailable"
              : (root.filterText ? "No matches for “" + root.filterText + "”" : "Type to search files")
            color: root.foreground
            opacity: 0.6
            font.family: root.fontFamily
            font.pixelSize: Style.font.body
          }
        }
      }
    }
  }
}
