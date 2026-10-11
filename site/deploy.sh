#!/usr/bin/env bash
# Build the documentation site and deploy it to a web root.
#
#   site/deploy.sh /path/to/webroot          # e.g. the pm2 docroot
#   OMAVRYTHING_WEBROOT=/srv/site site/deploy.sh
#
# The build needs the dependencies listed in site/requirements.txt. If a
# virtualenv exists at .venv-site (the path documented in the README), it is
# used; otherwise $PYTHON or python3 is.
#
# Note: this copies without --delete, so a web root that also holds unrelated
# files (such as a process-manager start script) is left alone. Stale files from
# an older build are not pruned; pass --prune if you want that.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

prune=0
dest=""
for arg in "$@"; do
  case "$arg" in
    --prune) prune=1 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) dest="$arg" ;;
  esac
done
dest="${dest:-${OMAVRYTHING_WEBROOT:-}}"

if [[ -z "$dest" ]]; then
  echo "usage: $0 [--prune] <destination-dir>" >&2
  echo "   or: OMAVRYTHING_WEBROOT=<dir> $0" >&2
  exit 2
fi

python="${PYTHON:-python3}"
if [[ -x "$here/../.venv-site/bin/python" ]]; then
  python="$here/../.venv-site/bin/python"
fi

"$python" "$here/build.py" --out "$here/dist"

mkdir -p "$dest"
if command -v rsync >/dev/null 2>&1; then
  rsync -a $([[ $prune == 1 ]] && echo --delete) "$here/dist/" "$dest/"
else
  cp -r "$here/dist/." "$dest/"
fi

echo "deployed $here/dist -> $dest"