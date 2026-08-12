#!/usr/bin/env bash
# Print <major.minor>.<commit-count - offset> from the committed VERSION file.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
read -r major_minor offset < <(
  grep -vE '^[[:space:]]*(#|$)' "$repo/VERSION"
)

if [[ -n "${ASTER_BUILD_NUMBER:-}" ]]; then
  count="$ASTER_BUILD_NUMBER"
elif [[ -s "$repo/BUILD_NUMBER" ]]; then
  read -r count < "$repo/BUILD_NUMBER"
else
  count="$(git -C "$repo" rev-list --count HEAD)"
fi

[[ "$major_minor" =~ ^[0-9]+\.[0-9]+$ ]] || {
  echo "error: invalid major.minor '$major_minor' in VERSION" >&2
  exit 1
}
[[ "$offset" =~ ^[0-9]+$ && "$count" =~ ^[0-9]+$ ]] || {
  echo "error: VERSION offset and build number must be integers" >&2
  exit 1
}
if (( count < offset )); then
  echo "error: commit count $count is below VERSION offset $offset" >&2
  exit 1
fi

echo "$major_minor.$((count - offset))"
