#!/usr/bin/env bash
# Attach verified immutable files to a private Forgejo release mirror.
# Usage: attach-forgejo-release.sh <tag> <title> <file>...
# Identical existing files are skipped; conflicting bytes fail. Never deletes
# assets. ASTER_RELEASE_TOKEN stays in the environment, never process argv.
set -euo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
exec python3 "$repo/scripts/release/forgejo-release-assets.py" "$@"
