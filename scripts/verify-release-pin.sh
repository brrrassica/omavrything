#!/usr/bin/env bash
# Verify that a built release archive matches the digests pinned in the reviewed
# source (`scripts/daemon-checksums.txt`).
#
# This is the pre-tag gate. It is deliberately separate from the release
# workflow so the same check can run:
#
#   * locally, right after `scripts/package-release.sh`, before anything is
#     tagged (the mistake that broke the 0.2.3, 0.2.4 and 0.2.5 releases was
#     discovering a bad pin only after the tag made it irreversible);
#   * in `.github/workflows/prepare-release.yml`, on the exact release
#     container, so the pin is validated against the environment that will
#     actually build the published artifact.
#
# Usage:
#   scripts/verify-release-pin.sh [version]
#
# `version` defaults to the workspace version in `Cargo.toml`. The built
# artifacts are read from `dist/`; it does NOT build them.
#
# Exit codes:
#   0  every pinned digest matches the built artifact
#   1  a digest is missing from, or does not match, the pinned file
#   2  the built artifacts are missing (nothing to compare)
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
archive="$dist/$name.zip"
pinned="$here/scripts/daemon-checksums.txt"

if [[ ! -f "$archive" ]]; then
  echo "error: $archive does not exist" >&2
  echo "       run scripts/package-release.sh first" >&2
  exit 2
fi
if [[ ! -f "$pinned" ]]; then
  echo "error: $pinned is missing from the tree" >&2
  exit 1
fi

# The three artifacts that must be pinned. The archive entry cannot live inside
# the archive (it would be circular), but it must be in the committed file.
artifacts=(
  "$name/bin/omavrythingd"
  "$name/bin/omavry"
  "$name.zip"
)

# Look a digest up in the committed pin file.
lookup_pin() {
  local file="$1"
  awk -v f="$file" '$2 == f { print $1; found = 1 } END { exit found ? 0 : 1 }' "$pinned"
}

# The digest of the built artifact.
built_digest() {
  sha256sum "$dist/$1" | awk '{print $1}'
}

fail=0
echo "Verifying omavrything $version against $pinned"
echo
for artifact in "${artifacts[@]}"; do
  expected="$(lookup_pin "$artifact" || true)"
  if [[ -z "$expected" ]]; then
    echo "FAIL  $artifact" >&2
    echo "      no pinned digest in scripts/daemon-checksums.txt" >&2
    echo "      the version-bump PR must pin the release before tagging" >&2
    fail=1
    continue
  fi
  actual="$(built_digest "$artifact")"
  if [[ "$actual" != "$expected" ]]; then
    echo "FAIL  $artifact" >&2
    echo "      pinned $expected" >&2
    echo "      built  $actual" >&2
    fail=1
    continue
  fi
  echo "ok    $artifact  $expected"
done

echo
if [[ "$fail" -ne 0 ]]; then
  cat >&2 <<EOF
The committed pin does not describe this build. Nothing may be tagged or
published until it does.

The digests above are what the current tree produces. Re-pin by replacing the
three ${version} lines in scripts/daemon-checksums.txt with the 'built' values
and committing that in the release PR, then re-run this check.

Do NOT edit the pin after tagging and do NOT re-point an existing tag: that is
exactly what forced the 0.2.4 and 0.2.5 releases to be re-pinned out of band.
EOF
  exit 1
fi

echo "All ${version} artifacts match the committed pin."
