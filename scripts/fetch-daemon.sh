#!/usr/bin/env bash
# Fetch the omavrythingd daemon and omavry CLI from the GitHub release that
# matches the manifest version, into a target directory.
#
# The archive is verified against the release's .sha256 asset before anything is
# extracted: the binaries are about to be installed as root, so an unverified
# download is refused.
#
# Usage: scripts/fetch-daemon.sh <dest-dir>
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="${1:-}"
[[ -n "$dest" ]] || { echo "usage: fetch-daemon.sh <dest-dir>" >&2; exit 2; }

repo="brrrassica/omavrything"
version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$here/manifest.json" | head -n1)"
[[ -n "$version" ]] || { echo "error: could not read version from manifest.json" >&2; exit 1; }
arch="$(uname -m)"
name="omavrything-${version}-${arch}"
base="https://github.com/${repo}/releases/download/v${version}"
zip_url="${base}/${name}.zip"
sha_url="${zip_url}.sha256"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "==> downloading ${zip_url}"
if ! curl -fL --retry 3 --connect-timeout 15 -o "$tmp/$name.zip" "$zip_url"; then
  echo "error: could not download the release archive for v${version} (${arch})" >&2
  echo "       build locally with scripts/build.sh, or check the release exists" >&2
  exit 1
fi

echo "==> verifying checksum"
if ! curl -fL --retry 3 --connect-timeout 15 -o "$tmp/$name.zip.sha256" "$sha_url"; then
  echo "error: no .sha256 asset for ${name}; refusing to install an unverified binary" >&2
  exit 1
fi
( cd "$tmp" && sha256sum -c "$name.zip.sha256" ) || { echo "error: checksum mismatch" >&2; exit 1; }

echo "==> extracting binaries"
mkdir -p "$dest"
unzip -o -j "$tmp/$name.zip" "$name/bin/omavrythingd" "$name/bin/omavry" -d "$dest" >/dev/null
chmod 755 "$dest/omavrythingd" "$dest/omavry"
echo "==> wrote $dest/omavrythingd and $dest/omavry"
