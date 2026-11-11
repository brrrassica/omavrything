#!/usr/bin/env bash
# Merge a freshly built `dist/daemon-checksums.txt` into the committed
# `scripts/daemon-checksums.txt` **append-only**.
#
# Why this exists: `scripts/daemon-checksums.txt` is the trust anchor for the
# fetch-on-first-run bootstrap and the installer, and it is merged between
# branches like ordinary text. Hand-editing it has silently deleted released
# pins at least three times (0.2.3, 0.2.4, 0.2.5): the new entries were added on
# one branch while the other still ended at the previous version, and the release
# merge then removed them.
#
# This tool only ever ADDS entries. It never removes or rewrites an existing
# one, so it cannot cause that failure. If a *conflicting* entry for the same
# name already exists with a different digest, it refuses and tells you which
# one to delete deliberately.
#
# Usage:
#   scripts/checksums-add.sh [dist/daemon-checksums.txt]
#
# Typical release flow (on the release branch, before tagging):
#
#   scripts/package-release.sh          # builds dist/ + dist/daemon-checksums.txt
#   scripts/checksums-add.sh            # merge the new entries, append-only
#   scripts/verify-release-pin.sh       # the gate: must be green
#
# Then commit both `scripts/daemon-checksums.txt` and the source change in the
# release PR, and only then tag.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

src="${1:-dist/daemon-checksums.txt}"
dst="scripts/daemon-checksums.txt"

if [[ ! -f "$src" ]]; then
  echo "error: $src does not exist" >&2
  echo "       run scripts/package-release.sh first" >&2
  exit 2
fi
if [[ ! -f "$dst" ]]; then
  echo "error: $dst does not exist" >&2
  exit 2
fi

# Validate the whole generated file FIRST, then write. A single pass that
# appended as it went would leave a partially merged file behind when a later
# line conflicts, which is worse than not running at all: the pin file is a
# trust anchor, so it is either fully consistent or untouched.
pending=()
conflicts=0
while IFS= read -r line; do
  # Skip blanks and comments from the generated file.
  [[ -z "${line//[[:space:]]/}" || "$line" == \#* ]] && continue
  digest="${line%% *}"
  name="${line#*  }"
  # A well-formed pin line is "<64 hex>  <name>".
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || continue
  [[ -n "$name" ]] || continue

  # Already present with the same digest: nothing to do (idempotent).
  if awk -v d="$digest" -v n="$name" \
      '$1 == d && $2 == n { found = 1 } END { exit found ? 0 : 1 }' "$dst"; then
    continue
  fi

  # Same name, different digest: a genuine conflict, never auto-resolved.
  existing="$(awk -v n="$name" '$2 == n { print $1; exit }' "$dst")"
  if [[ -n "$existing" ]]; then
    echo "error: $name is already pinned with a different digest" >&2
    echo "       committed $existing" >&2
    echo "       produced  $digest" >&2
    echo "" >&2
    echo "       Append-only merge refuses to overwrite a pin. If this release" >&2
    echo "       legitimately supersedes the old one, delete the stale line for" >&2
    echo "       $name by hand and commit that deliberately, then re-run." >&2
    conflicts=1
    continue
  fi

  pending+=("$digest  $name")
done < "$src"

if [[ "$conflicts" -ne 0 ]]; then
  echo "" >&2
  echo "Aborted with $conflicts conflict(s). $dst was NOT modified." >&2
  exit 1
fi

added=0
for entry in "${pending[@]}"; do
  printf '%s\n' "$entry" >> "$dst"
  echo "added  ${entry#*  }  ${entry%% *}"
  added=$((added + 1))
done

# Pin the archive-input fingerprint alongside the artifact digests. This is what
# lets a later run detect that a file *inside* the archive changed after the pin
# was taken (a doc edit, a manifest bump) without rebuilding anything, and it is
# how the pre-tag gate and the release job prove they are looking at the same
# tree even though one runs on the release branch and the other on the merge
# commit.
inputs_pin="scripts/archive-inputs.txt"
{
  echo "# Fingerprint of every file that goes into the release archive."
  echo "# Regenerate with scripts/checksums-add.sh after scripts/package-release.sh."
  echo "# Verify with scripts/archive-inputs.sh --check."
  echo "# Format: <mode>  <sha256>  <path>"
  echo "#"
  scripts/archive-inputs.sh --list
} > "$inputs_pin"
echo "Wrote $inputs_pin (archive input fingerprint)"

echo ""
if [[ "$added" -eq 0 ]]; then
  echo "No new entries; $dst already covers everything in $src."
else
  echo "Appended $added entr(y/ies) to $dst."
fi
echo "Next: scripts/verify-release-pin.sh  (must be green before tagging)"
