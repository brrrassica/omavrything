import QtQuick
import Quickshell
import Quickshell.Io

// Headless service that owns the plugin's persistent IPC target and bootstraps
// the system daemon on first run.
//
// A service is loaded at startup, so the target exists before the overlay is
// ever summoned. `focus` summons the overlay, whose open() focuses the input.
// The target and the summoned id are both the manifest plugin id.
//
// On first start, if the system daemon is not installed, this service runs
// scripts/bootstrap.sh, which escalates to root with pkexec (the Omarchy polkit
// agent — the omarchy.polkit shell plugin — shows the authentication dialog) and
// installs the systemd daemon, the omavrything user/group, and the D-Bus/polkit
// policy. The user authenticates once; no terminal and no manual command is
// required. NTFS indexing works out of the box because the system daemon keeps
// its capabilities.
Item {
  id: root

  property string omarchyPath: Quickshell.env("OMARCHY_PATH")
  property var shell: null
  property var manifest: null

  // The plugin root. This file lives at <plugin>/shell/omavrything/Service.qml,
  // so the plugin root is two directories up. Qt.resolvedUrl(".") resolves to
  // the directory containing this file (the same idiom Omarchy's own plugins
  // use). The HOME path is a fallback for the standard `omarchy plugin add`
  // location.
  readonly property string pluginDir: {
    var u = Qt.resolvedUrl(".").toString()
    if (u.indexOf("file://") === 0) u = u.substring(7)
    return u
  }
  readonly property string homePluginDir: (Quickshell.env("HOME") || "") + "/.config/omarchy/plugins/tech.alatcerdas.omavrything/"
  readonly property string bootstrapScript: pluginDir + "../../scripts/bootstrap.sh"
  readonly property string bootstrapScriptFallback: homePluginDir + "scripts/bootstrap.sh"

  // Guards against re-prompting on every shell restart within a session.
  property bool bootstrapAttempted: false
  property bool bootstrapRunning: false
  property string bootstrapError: ""
  property string bootstrapArg: ""

  // Run the bootstrap once per shell session. bootstrap.sh itself is a no-op
  // when the daemon is already installed or a previous prompt was dismissed.
  function ensureDaemon() {
    if (root.bootstrapAttempted || root.bootstrapRunning) return
    root.bootstrapAttempted = true
    root.runBootstrap("")
  }

  // Retry the setup, ignoring a previous dismissal (the `setup` IPC).
  function retrySetup() {
    if (root.bootstrapRunning) return
    root.bootstrapAttempted = true
    root.runBootstrap("--force")
  }

  function runBootstrap(arg) {
    if (root.bootstrapRunning) return
    root.bootstrapRunning = true
    root.bootstrapError = ""
    root.bootstrapArg = arg
    bootstrapProcess.running = true
  }

  Component.onCompleted: root.ensureDaemon()

  // The escalation. pkexec shows the Omarchy polkit dialog; the installer runs
  // as root. Exit 126 means the user dismissed the prompt.
  Process {
    id: bootstrapProcess
    // Pick whichever bootstrap.sh exists: the resolved plugin dir first, then
    // the standard `omarchy plugin add` location.
    command: ["sh", "-c",
      "s=\"$1\"; [ -f \"$s\" ] || s=\"$2\"; exec sh \"$s\" \"$3\"",
      "bootstrap", root.bootstrapScript, root.bootstrapScriptFallback, root.bootstrapArg]
    stdout: StdioCollector { id: bootstrapStdout; waitForEnd: true }
    stderr: StdioCollector { id: bootstrapStderr; waitForEnd: true }
    onExited: function(exitCode) {
      root.bootstrapRunning = false
      if (exitCode === 0) {
        root.bootstrapError = ""
      } else if (exitCode === 126) {
        root.bootstrapError = "Authentication dismissed"
      } else {
        const err = String(bootstrapStderr.text || "").trim()
        root.bootstrapError = err !== "" ? err : ("Setup failed (" + exitCode + ")")
      }
    }
  }

  IpcHandler {
    target: "tech.alatcerdas.omavrything"

    function focus(): string {
      if (!root.shell) return "unknown"
      return root.shell.summon("tech.alatcerdas.omavrything", "{}") ? "ok" : "unknown"
    }

    function ping(): string {
      return "ok"
    }

    // Retry the system-daemon setup (e.g. after dismissing the polkit prompt).
    function setup(): string {
      root.retrySetup()
      return "ok"
    }
  }
}
