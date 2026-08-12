#!/usr/bin/env bash
# Stamp all dynamic Aster version metadata in a disposable build tree.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
version="${1:-$("$repo/scripts/release/version.sh")}"

"$repo/scripts/release/prepare-python-build.sh" "$version" >/dev/null
"$repo/scripts/release/prepare-cargo-build.sh" "$version" >/dev/null

echo "$version"
