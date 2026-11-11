#!/usr/bin/env bash
# Verify that every version reference and release-derived value in the tree
# agrees with the version being released.
#
# This closes several failures that are invisible to a digest check because they
# are not about the built bytes at all:
#
#   * manifest.json drifted from the tag -> fetch-daemon.sh builds the download
#     URL from the manifest version and gets a 404, so the plugin bootstrap
#     fails even though the release and its pin are perfect.
#   * Cargo.toml / Cargo.lock drifted -> the shipped binary reports a different
#     version than the one users asked for; bootstrap.sh compares
#     `omavrythingd --version` against the manifest, so an upgrade silently does
#     not happen.
#   * PKGBUILD sha256sums still holds a previous release's tarball hash -> the
#     AUR package fails to build (the 0.1.0 hash lingered through 0.2.5).
#   * user-facing docs still advertise the previous version -> they are shipped
#     inside the archive.
#
# Usage:
#   scripts/verify-release-metadata.sh [version]
#
# `version` defaults to the workspace version in Cargo.toml. Pass the tag's
# version (e.g. 0.2.6) in CI.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

version="${1:-}"
if [[ -z "$version" ]]; then
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
fi
[[ -n "$version" ]] || {
  echo "error: could not determine version" >&2
  exit 1
}

fail=0
note() { printf 'ok    %s\n' "$*"; }
bad() {
  printf 'FAIL  %s\n' "$*" >&2
  fail=1
}

echo "Verifying release metadata for $version"
echo

# --- manifest.json --------------------------------------------------------
# The plugin's identity and the version fetch-daemon.sh downloads.
manifest_version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' manifest.json | head -n1)"
if [[ "$manifest_version" == "$version" ]]; then
  note "manifest.json version = $version"
else
  bad "manifest.json version is '${manifest_version:-missing}', expected $version"
fi

# --- Cargo.toml workspace version ----------------------------------------
cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
if [[ "$cargo_version" == "$version" ]]; then
  note "Cargo.toml workspace version = $version"
else
  bad "Cargo.toml workspace version is '${cargo_version:-missing}', expected $version"
fi

# --- Cargo.lock workspace packages ---------------------------------------
# A stale lock entry makes the built binary report the old version even when
# Cargo.toml was bumped.
stale_lock=""
for pkg in omavrything-cli omavrything-client omavrything-core omavrything-ntfs omavrythingd; do
  # Each [[package]] block is "name = \"...\"" then "version = \"...\"".
  # Parse per block rather than relying on field positions.
  v="$(awk -v p="$pkg" '
    /^\[\[package\]\]/ { name = ""; ver = ""; inblock = 1; next }
    inblock && /^name = /    { gsub(/"/, "", $3); name = $3; next }
    inblock && /^version = / { gsub(/"/, "", $3); ver = $3
                               if (name == p) { print ver; exit } }
  ' Cargo.lock)"
  [[ "$v" == "$version" ]] || stale_lock="$stale_lock $pkg(${v:-?})"
done
if [[ -z "$stale_lock" ]]; then
  note "Cargo.lock workspace packages = $version"
else
  bad "Cargo.lock has stale workspace versions:$stale_lock"
fi

# --- PKGBUILD -------------------------------------------------------------
pkgver="$(sed -n 's/^pkgver=\(.*\)/\1/p' packaging/PKGBUILD | head -n1)"
if [[ "$pkgver" == "$version" ]]; then
  note "packaging/PKGBUILD pkgver = $version"
else
  bad "packaging/PKGBUILD pkgver is '${pkgver:-missing}', expected $version"
fi

# The AUR source hash is over the GitHub tag tarball, which cannot exist before
# the tag, so it cannot be validated in the pre-tag gate. It CAN be validated
# once the tag exists: compare it against the real tarball digest. Post-tag the
# workflow runs this with the tarball path/URL, which catches the 0.1.0 hash
# that survived three releases.
pkgbuild_sum="$(sed -n "s/^sha256sums=('\([0-9a-f]\{64\}\)')/\1/p" packaging/PKGBUILD | head -n1)"
if [[ -z "$pkgbuild_sum" ]]; then
  bad "packaging/PKGBUILD has no single sha256sums entry"
elif [[ -n "${PKGBUILD_TARBALL:-}" ]]; then
  actual="$(sha256sum "$PKGBUILD_TARBALL" | awk '{print $1}')"
  if [[ "$pkgbuild_sum" == "$actual" ]]; then
    note "packaging/PKGBUILD sha256sums matches the tag tarball"
  else
    bad "packaging/PKGBUILD sha256sums does not match the tag tarball"
    echo "       in PKGBUILD $pkgbuild_sum" >&2
    echo "       tarball    $actual" >&2
    echo "       run 'updpkgsums' in packaging/ with the tarball present" >&2
  fi
else
  # Pre-tag: the tarball does not exist yet. Warn rather than fail, but make the
  # remaining manual step unmissable.
  printf 'note  %s\n' "packaging/PKGBUILD sha256sums not checked (no tag tarball yet; run updpkgsums after tagging)"
fi

# --- user-facing version references --------------------------------------
# These are shipped inside the archive and are what a user reads first. Kept as
# a list so adding a new one is a one-line change.
check_contains() {
  local file="$1" pattern="$2" label="$3"
  if [[ ! -f "$file" ]]; then
    bad "$label: $file is missing"
    return
  fi
  if grep -qE "$pattern" "$file"; then
    note "$label advertises $version"
  else
    bad "$label does not advertise $version ($file)"
  fi
}
check_contains README.md "Status:\*\* v${version//./\\.}" "README.md"
check_contains docs/install.md "v${version//./\\.}" "docs/install.md"
check_contains site/index.md "v${version//./\\.}" "site/index.md"
check_contains data/man/omavrythingd.8 "omavrything ${version//./\\.}" "man page"

echo
if [[ "$fail" -ne 0 ]]; then
  cat >&2 <<EOF

Release metadata does not agree with $version. A release in this state breaks
one of:
  - the plugin bootstrap (manifest version -> download URL -> 404)
  - the upgrade path (installed --version never matches the manifest)
  - the AUR build (stale source hash)
  - the first thing a user reads (docs advertise the old version)

Fix the entries above, then re-run this check. Do this BEFORE scripting the pin:
these files are inside the archive, so changing them invalidates the archive
digest too.
EOF
  exit 1
fi

echo "All release metadata agrees with $version."
