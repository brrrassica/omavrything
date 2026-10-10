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
# Usage: scripts/package-release.sh [version]
# The version defaults to the workspace version in Cargo.toml. The archive is
# what the GitHub release workflow uploads; running it locally verifies the
# release can be assembled before a tag is pushed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

version="${1:-}"
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

# Build the release binaries. --locked keeps the build reproducible against the
# committed Cargo.lock.
cargo build --release --locked --workspace

rm -rf "$stage"
mkdir -p "$stage"/{bin,data,shell,scripts,docs}

# Executables.
install -m755 target/release/omavrythingd "$stage/bin/omavrythingd"
install -m755 target/release/omavry "$stage/bin/omavry"

# System integration data, preserving the source layout so the installer and the
# PKGBUILD can share it.
install -Dm644 data/systemd/omavrythingd.service "$stage/data/systemd/omavrythingd.service"
install -Dm644 data/sysusers/omavrything.conf "$stage/data/sysusers/omavrything.conf"
install -Dm644 data/dbus/org.omarchy.omavrything.conf "$stage/data/dbus/org.omarchy.omavrything.conf"
install -Dm644 data/polkit/org.omarchy.omavrything.policy "$stage/data/polkit/org.omarchy.omavrything.policy"
install -Dm644 data/config/config.toml "$stage/data/config/config.toml"

# The omarchy-shell plugin.
install -Dm644 shell/omavrything/manifest.json "$stage/shell/omavrything/manifest.json"
install -Dm644 shell/omavrything/SearchBar.qml "$stage/shell/omavrything/SearchBar.qml"
install -Dm644 shell/omavrything/SearchOverlay.qml "$stage/shell/omavrything/SearchOverlay.qml"
install -Dm644 shell/omavrything/Service.qml "$stage/shell/omavrything/Service.qml"

# Install and helper scripts.
install -m755 scripts/install.sh "$stage/scripts/install.sh"
install -m755 scripts/install-plugin.sh "$stage/scripts/install-plugin.sh"
install -m755 scripts/run-daemon.sh "$stage/scripts/run-daemon.sh"
install -m755 scripts/restart-all.sh "$stage/scripts/restart-all.sh"

# Documentation and license.
install -m644 README.md "$stage/README.md"
install -m644 LICENSE "$stage/LICENSE"
install -m644 docs/*.md "$stage/docs/"

# A short pointer for someone who unzips the archive.
cat > "$stage/INSTALL.txt" <<'EOF'
omavrything release archive
===========================

Quick start (system install, requires root):

    sudo ./scripts/install.sh --system

Then enable the shell plugin for your user:

    ./scripts/install-plugin.sh
    omarchy plugin enable omavrything --section center

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
