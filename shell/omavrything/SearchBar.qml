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

  readonly property string queryFile: (Quickshell.env("XDG_RUNTIME_DIR") || "/tmp") + "/omavrything-query"

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

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.query ? ("󰍉  " + root.query) : "󰍉  Search files…"
    tooltipText: "Search files (Super+Alt+Space)"
    horizontalMargin: 10
    onPressed: function(button) {
      if (!root.bar) return
      root.bar.run("omarchy-shell omavrything focus")
    }
  }
}
