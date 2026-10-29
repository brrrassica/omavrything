#!/usr/bin/env bash
# Fetch the omavrythingd daemon and omavry CLI from the GitHub release that
# matches the manifest version, into a target directory.
#
# The archive is verified against a SHA-256 digest pinned in this reviewed
# source (scripts/daemon-checksums.txt) before anything is extracted: the
# binaries are about to be installed as root, so an unverified download is
# refused.
#
# The digest is deliberately NOT fetched from the release. The archive and its
# .sha256 asset live in the same GitHub release, which is mutable, so a checksum
# downloaded alongside the archive proves nothing about who produced it: an
# attacker able to replace one asset can replace the other. Pinning the digest
# in the reviewed source binds the shipped binaries to this commit.
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

# The trust anchor: the digest pinned in this reviewed source. A missing entry
# is a hard failure -- never fall back to a checksum fetched from the release.
checksums="$here/scripts/daemon-checksums.txt"
[[ -f "$checksums" ]] || {
  echo "error: $checksums is missing; refusing to install an unverified binary" >&2
  exit 1
}
expected="$(awk -v f="$name.zip" '$2 == f { print $1; exit }' "$checksums")"
if [[ -z "$expected" ]]; then
  echo "error: no pinned digest for ${name}.zip in scripts/daemon-checksums.txt" >&2
  echo "       refusing to install an unverified binary; the release may not be" >&2
  echo "       pinned yet, or this architecture (${arch}) has no release" >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "==> downloading ${zip_url}"
if ! curl -fL --retry 3 --connect-timeout 15 -o "$tmp/$name.zip" "$zip_url"; then
  echo "error: could not download the release archive for v${version} (${arch})" >&2
  echo "       build locally with scripts/build.sh, or check the release exists" >&2
  exit 1
fi

echo "==> verifying checksum against the pinned digest"
actual="$(sha256sum "$tmp/$name.zip" | awk '{print $1}')"
if [[ "$actual" != "$expected" ]]; then
  echo "error: checksum mismatch for ${name}.zip" >&2
  echo "       expected $expected (pinned in scripts/daemon-checksums.txt)" >&2
  echo "       got      $actual" >&2
  exit 1
fi

echo "==> extracting binaries"
mkdir -p "$dest"
unzip -o -j "$tmp/$name.zip" "$name/bin/omavrythingd" "$name/bin/omavry" -d "$dest" >/dev/null
chmod 755 "$dest/omavrythingd" "$dest/omavry"
echo "==> wrote $dest/omavrythingd and $dest/omavry"
