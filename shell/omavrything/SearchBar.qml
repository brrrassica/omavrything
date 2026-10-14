import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// Persistent search bar in the omarchy-shell bar (center section, right of the
// clock anchor).
//
// The bar surface is a layer-shell PanelWindow with keyboardFocus None, so a
// widget inside it cannot receive keystrokes. This widget is therefore a
// *visual mirror* of the search bar: it shows the query typed in the scratchpad
// (mirrored through a runtime file) and summons the scratchpad on click.
BarWidget {
  id: root
  moduleName: "omavrything"

  // Mirrors the scratchpad's query so the bar reacts to every keystroke.
  property string query: ""

  // The query mirror is a private channel between the overlay and the bar. It
  // must never live in a world-writable directory (see SearchOverlay.qml): a
  // predictable path under /tmp lets another local user pre-create it as a
  // symlink or feed the bar arbitrary content. Use $XDG_RUNTIME_DIR (0700) and
  // disable the mirror when it is unset rather than falling back to /tmp.
  readonly property string runtimeDir: Quickshell.env("XDG_RUNTIME_DIR") || ""
  readonly property string queryFile: runtimeDir !== "" ? runtimeDir + "/omavrything-query" : ""

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  FileView {
    id: queryFileView
    path: root.queryFile
    watchChanges: true
    printErrors: false
    onLoaded: root.query = text().trim()
    onLoadFailed: root.query = ""
    onFileChanged: reload()
  }

  // Seed the mirror file so `watchChanges` has something to watch. Without this,
  // a bar widget that starts before the overlay has ever opened watches a
  // non-existent path and may miss the file's creation. Skipped when the mirror
  // is disabled (no private per-user directory).
  Component.onCompleted: {
    if (root.queryFile !== "" && !queryFileView.exists) queryFileView.setText("")
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.query ? ("󰍉  " + root.query) : "󰍉  Search files…"
    tooltipText: "Search files (Super+Alt+Space)"
    horizontalMargin: 10
    onPressed: function(button) {
      if (!root.bar) return
      root.bar.run("omarchy-shell tech.alatcerdas.omavrything focus")
    }
  }
}
