import QtQuick
import Quickshell
import Quickshell.Io

// Headless service that owns the persistent `omavrything` IPC target.
//
// A service is loaded at startup, so the target exists before the overlay is
// ever summoned. `focus` summons the overlay, whose open() focuses the input.
// The Hyprland binding calls `omarchy-shell omavrything focus`.
Item {
  id: root

  property string omarchyPath: Quickshell.env("OMARCHY_PATH")
  property var shell: null
  property var manifest: null

  IpcHandler {
    target: "omavrything"

    function focus(): string {
      if (!root.shell) return "unknown"
      return root.shell.summon("omavrything", "{}") ? "ok" : "unknown"
    }

    function ping(): string {
      return "ok"
    }
  }
}
