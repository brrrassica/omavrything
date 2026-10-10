import Quickshell
import Quickshell.Io
import Quickshell.Wayland
import QtQuick
import qs.Commons
import qs.Ui

// Focusable search overlay: the interactive input plus the floating results
// window, anchored to the top and right-center aligned so it sits under the
// bar's search widget.
//
// This is the ADR-008 fallback: because the bar surface cannot take keyboard
// focus, the input lives here rather than in the bar widget. The overlay is a
// layer-shell PanelWindow with keyboardFocus Exclusive while open, modelled on
// the first-party menu plugin.
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

  // Result-window sizing (settable; defaults for the M1 spike).
  property int cardWidth: 520
  property int maxResults: 8
  property real anchorFraction: 0.6

  // Theme: shares the [menu] surface tokens so themes that style the menu also
  // style the search overlay.
  property color background: Color.menu.background
  property color foreground: Color.menu.text
  property color border: Color.menu.border
  property var borderSpec: Border.surfaceSpec("menu", "border", border, Math.max(1, Style.space(2)))
  property color selectedBackground: Color.menu.selectedBackground
  property color selectedText: Color.menu.selectedText
  readonly property int cornerRadius: Style.cornerRadius
  property string fontFamily: Style.font.menuFamily
  property int contentMargin: Style.spacing.panelPadding
  property int inputHeight: Math.max(Style.space(40), Style.font.heading + Style.spacing.controlPaddingY * 2)
  property int rowHeight: Math.max(Style.space(40), Style.font.body + Style.spacing.rowPaddingX * 2)
  property int rowSpacing: Style.spacing.xs
  property int contentSpacing: Style.spacing.md

  // Fake data for the M1 feasibility spike. Replaced by daemon results in M2.
  readonly property var fakeFiles: [
    { path: "/home/sam/Documents/report-2024.pdf", isDir: false },
    { path: "/home/sam/Documents/notes/meeting-notes.md", isDir: false },
    { path: "/home/sam/Downloads/omarchy-quattro.iso", isDir: false },
    { path: "/home/sam/Pictures/screenshots/2026-10-10.png", isDir: false },
    { path: "/home/sam/Projects/omavrything/Cargo.toml", isDir: false },
    { path: "/home/sam/Projects/omavrything/crates/omavrything-core/src/lib.rs", isDir: false },
    { path: "/home/sam/Music/playlist.m3u", isDir: false },
    { path: "/home/sam/Videos/demo.mp4", isDir: false },
    { path: "/etc/omavrything/config.toml", isDir: false },
    { path: "/var/log/omavrythingd.log", isDir: false },
    { path: "/home/sam/.config/omarchy/shell.json", isDir: false },
    { path: "/home/sam/Documents", isDir: true }
  ]

  // Plugin lifecycle hooks, called by the shell host.
  function open(payloadJson) {
    root.opened = true
    root.filterText = ""
    root.selectedIndex = 0
    root.cursorActive = false
    root.rebuild()
    Qt.callLater(function() { input.forceActiveFocus() })
  }

  function close() {
    root.opened = false
  }

  function dismiss() {
    root.opened = false
    if (root.shell && typeof root.shell.hide === "function")
      root.shell.hide((root.manifest && root.manifest.id) || "omavrything")
  }

  function rebuild() {
    var q = root.filterText.trim().toLowerCase()
    var terms = q.length > 0 ? q.split(/\s+/) : []
    var out = []
    for (var i = 0; i < root.fakeFiles.length; i++) {
      var f = root.fakeFiles[i]
      var hay = f.path.toLowerCase()
      var ok = true
      for (var t = 0; t < terms.length; t++) {
        if (hay.indexOf(terms[t]) < 0) { ok = false; break }
      }
      if (ok) out.push(f)
      if (out.length >= root.maxResults) break
    }
    root.results = out
    if (root.selectedIndex >= out.length) root.selectedIndex = Math.max(0, out.length - 1)
    if (out.length === 0) root.cursorActive = false
    displayModel.clear()
    for (var j = 0; j < out.length; j++) displayModel.append(out[j])
  }

  function select(delta) {
    if (root.results.length === 0) return
    root.cursorActive = true
    root.selectedIndex = (root.selectedIndex + delta + root.results.length) % root.results.length
    resultList.positionViewAtIndex(root.selectedIndex, ListView.Contain)
  }

  function activate() {
    if (!root.cursorActive || root.selectedIndex < 0 || root.selectedIndex >= root.results.length) return
    var f = root.results[root.selectedIndex]
    root.dismiss()
    Quickshell.execDetached(["xdg-open", f.path])
  }

  readonly property int listAreaHeight: {
    if (root.results.length === 0) return root.rowHeight
    var h = root.results.length * root.rowHeight + (root.results.length - 1) * root.rowSpacing
    return Math.min(h, Math.round(panel.height * 0.5))
  }
  readonly property int cardHeight: root.contentMargin * 2 + root.inputHeight + root.contentSpacing + root.listAreaHeight

  ListModel { id: displayModel }

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
      width: root.cardWidth
      height: root.cardHeight
      radius: root.cornerRadius
      color: root.background
      borderSpec: root.borderSpec
      padding: root.contentMargin
      x: Math.round(panel.width * root.anchorFraction - width / 2)
      y: Style.gapsOut

      MouseArea { anchors.fill: parent; onClicked: {} }

      Column {
        anchors.fill: parent
        anchors.topMargin: card.contentTopInset
        anchors.rightMargin: card.contentRightInset
        anchors.bottomMargin: card.contentBottomInset
        anchors.leftMargin: card.contentLeftInset
        spacing: root.contentSpacing

        Item {
          width: parent.width
          height: root.inputHeight

          TextInput {
            id: input
            anchors.fill: parent
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
                root.rebuild()
              }
            }
            Keys.onPressed: function(event) {
              if (event.key === Qt.Key_Escape) { root.dismiss(); event.accepted = true }
              else if (event.key === Qt.Key_Up) { root.select(-1); event.accepted = true }
              else if (event.key === Qt.Key_Down) { root.select(1); event.accepted = true }
              else if (event.key === Qt.Key_PageUp) { root.select(-5); event.accepted = true }
              else if (event.key === Qt.Key_PageDown) { root.select(5); event.accepted = true }
              else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) { root.activate(); event.accepted = true }
            }
          }

          Text {
            anchors.fill: parent
            text: "Search files…"
            visible: input.text.length === 0
            color: root.foreground
            opacity: 0.5
            font.family: root.fontFamily
            font.pixelSize: Style.font.heading
            verticalAlignment: Text.AlignVCenter
          }
        }

        Item {
          width: parent.width
          height: root.listAreaHeight

          ListView {
            id: resultList
            anchors.fill: parent
            visible: root.results.length > 0
            model: displayModel
            clip: true
            spacing: root.rowSpacing
            boundsBehavior: Flickable.StopAtBounds

            delegate: BorderSurface {
              required property int index
              required property string path
              required property bool isDir
              readonly property bool hasCursor: root.cursorActive && index === root.selectedIndex
              width: ListView.view.width
              height: root.rowHeight
              radius: root.cornerRadius
              color: hasCursor ? root.selectedBackground : "transparent"
              borderSpec: Border.none()

              Text {
                anchors.left: parent.left
                anchors.leftMargin: Style.space(10)
                anchors.right: parent.right
                anchors.rightMargin: Style.space(10)
                anchors.verticalCenter: parent.verticalCenter
                text: parent.path
                color: parent.hasCursor ? root.selectedText : root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
                elide: Text.ElideMiddle
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
            text: root.filterText ? "No matches for “" + root.filterText + "”" : "Type to search files"
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
