import QtQuick
import Quickshell
import Quickshell.Io

// Headless service that owns the plugin's persistent IPC target.
//
// A service is loaded at startup, so the target exists before the overlay is
// ever summoned. `focus` summons the overlay, whose open() focuses the input.
// The target and the summoned id are both the manifest plugin id.
Item {
  id: root

  property string omarchyPath: Quickshell.env("OMARCHY_PATH")
  property var shell: null
  property var manifest: null

  IpcHandler {
    target: "tech.alatcerdas.omavrything"

    function focus(): string {
      if (!root.shell) return "unknown"
      return root.shell.summon("tech.alatcerdas.omavrything", "{}") ? "ok" : "unknown"
    }

    function ping(): string {
      return "ok"
    }
  }
}
