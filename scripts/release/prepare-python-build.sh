#!/usr/bin/env bash
# Stamp the derived version into Python package metadata for this build tree.
# CI workspaces are disposable. Local callers should restore pyproject.toml
# after building if they do not want a tracked-file modification.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
version="${1:-$("$repo/scripts/release/version.sh")}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "error: '$version' is not a release version" >&2
  exit 2
}

count="${ASTER_BUILD_NUMBER:-$(git -C "$repo" rev-list --count HEAD)}"
printf '%s\n' "$count" > "$repo/BUILD_NUMBER"

python3 - "$repo/pyproject.toml" "$version" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
version = sys.argv[2]
text = path.read_text()
updated, replacements = re.subn(
    r'(?m)^version\s*=\s*"[^"]+"$',
    f'version = "{version}"',
    text,
    count=1,
)
if replacements != 1:
    raise SystemExit(f"expected one project version in {path}, found {replacements}")
path.write_text(updated)
PY

echo "$version"
