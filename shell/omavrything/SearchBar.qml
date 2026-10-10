import QtQuick
import qs.Commons
import qs.Ui

// Persistent search bar in the omarchy-shell bar (center section, right of the
// clock anchor).
//
// The bar surface is a layer-shell PanelWindow with keyboardFocus None, so a
// widget inside it cannot receive keystrokes. This widget is therefore a
// *visual mirror* of the search bar: it shows the current query (or a
// placeholder) and summons the focusable overlay, which hosts the real input
// and the results window. See docs/omarchy-shell-integration.md.
BarWidget {
  id: root
  moduleName: "omavrything"

  // Mirrors the overlay's query so the bar reflects what is being searched.
  property string query: ""

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.query ? ("󰍉  " + root.query) : "󰍉  Search files…"
    tooltipText: "Search files (Super+Shift+Space)"
    horizontalMargin: 10
    onPressed: function(button) {
      if (!root.bar) return
      root.bar.run("omarchy-shell omavrything focus")
    }
  }
}
