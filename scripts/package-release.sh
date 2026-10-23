#!/usr/bin/env bash
# Build a distributable release archive for omavrything.
#
# Produces dist/omavrything-<version>-<arch>.zip containing:
#   bin/            the omavrythingd daemon and the omavry CLI
#   data/           systemd unit, sysuser, D-Bus policy, polkit action, config
#   shell/          the omarchy-shell plugin
#   scripts/        install and helper scripts
#   docs/           user documentation
#   README.md, LICENSE
#
# Usage: scripts/package-release.sh [--no-build] [version]
# The version defaults to the workspace version in Cargo.toml. The archive is
# what the GitHub release workflow uploads; running it locally verifies the
# release can be assembled before a tag is pushed. --no-build reuses the
# existing bin/ binaries instead of rebuilding.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

build=1
version=""
for arg in "$@"; do
  case "$arg" in
    --no-build) build=0 ;;
    *) version="$arg" ;;
  esac
done
if [[ -z "$version" ]]; then
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
fi
if [[ -z "$version" ]]; then
  echo "error: could not determine version" >&2
  exit 1
fi

arch="$(uname -m)"
name="omavrything-${version}-${arch}"
dist="$here/dist"
stage="$dist/$name"
archive="$dist/$name.zip"

echo "Packaging $name"

# Build the release binaries into bin/. --no-build reuses the existing bin/
# binaries instead of rebuilding.
if [[ "$build" == 1 ]]; then
  "$here/scripts/build.sh"
fi

rm -rf "$stage"
mkdir -p "$stage"/{bin,data,shell,scripts,docs}

# Executables, read from bin/ (the canonical release binary path).
install -m755 "$here/bin/omavrythingd" "$stage/bin/omavrythingd"
install -m755 "$here/bin/omavry" "$stage/bin/omavry"

# System integration data, preserving the source layout so the installer and the
# PKGBUILD can share it.
install -Dm644 data/systemd/omavrythingd.service "$stage/data/systemd/omavrythingd.service"
install -Dm644 data/sysusers/omavrything.conf "$stage/data/sysusers/omavrything.conf"
install -Dm644 data/tmpfiles/omavrything.conf "$stage/data/tmpfiles/omavrything.conf"
install -Dm644 data/dbus/tech.alatcerdas.omavrything.conf "$stage/data/dbus/tech.alatcerdas.omavrything.conf"
install -Dm644 data/polkit/tech.alatcerdas.omavrything.policy "$stage/data/polkit/tech.alatcerdas.omavrything.policy"
install -Dm644 data/config/config.toml "$stage/data/config/config.toml"
install -Dm644 data/man/omavrythingd.8 "$stage/data/man/omavrythingd.8"

# The omarchy-shell plugin. The manifest sits at the archive root, exactly where
# the marketplace and `omarchy plugin validate` expect it, so the extracted
# archive is itself a valid plugin folder.
install -Dm644 manifest.json "$stage/manifest.json"
install -Dm644 shell/omavrything/SearchBar.qml "$stage/shell/omavrything/SearchBar.qml"
install -Dm644 shell/omavrything/SearchOverlay.qml "$stage/shell/omavrything/SearchOverlay.qml"
install -Dm644 shell/omavrything/Service.qml "$stage/shell/omavrything/Service.qml"

# Install and helper scripts.
install -m755 scripts/install.sh "$stage/scripts/install.sh"
install -m755 scripts/uninstall.sh "$stage/scripts/uninstall.sh"
install -m755 scripts/run-daemon.sh "$stage/scripts/run-daemon.sh"
install -m755 scripts/restart-all.sh "$stage/scripts/restart-all.sh"
install -m755 scripts/doctor.sh "$stage/scripts/doctor.sh"
install -m755 scripts/benchmark.sh "$stage/scripts/benchmark.sh"

# Documentation and license.
install -m644 README.md "$stage/README.md"
install -m644 LICENSE "$stage/LICENSE"
install -m644 docs/*.md "$stage/docs/"

# A short pointer for someone who unzips the archive.
cat > "$stage/INSTALL.txt" <<'EOF'
omavrything release archive
===========================

Quick start (one command, requires root):

    sudo ./scripts/install.sh

This installs the daemon, CLI, systemd unit, D-Bus policy, polkit action, man
page, default config, and the omarchy-shell plugin; creates the omavrything
user; adds you to the omavrything group; reloads D-Bus; enables and starts the
daemon; and installs, enables, and loads the shell plugin for your user.

The widget works immediately: the installer grants your session access to the
daemon socket with an ACL, so no re-login is needed now. The group change takes
effect on your next login.

To remove everything again (one command, requires root):

    sudo ./scripts/uninstall.sh

This removes the daemon, CLI, systemd unit, D-Bus/polkit policy, dedicated
user, and the per-user shell plugin for your user.

See docs/install.md for the full setup, including the optional D-Bus control
interface and the hardened systemd service.
EOF

# Zip the staging directory. The archive contains a single top-level directory
# named after the release so it extracts cleanly.
rm -f "$archive"
if command -v zip >/dev/null 2>&1; then
  (cd "$dist" && zip -qr "$name.zip" "$name")
else
  echo "error: 'zip' is required to build the release archive" >&2
  exit 1
fi

echo "Wrote $archive"
