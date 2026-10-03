#!/usr/bin/env bash
# Fail (exit 1) if ffi/aster_ffi.h is out of date with respect to the Rust
# sources. Regenerates into a temp file and diffs against the checked-in copy.
#
#   ffi/scripts/check-header.sh
#
# Fix drift with: ffi/scripts/gen-header.sh
set -euo pipefail

ffi_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp "${TMPDIR:-/tmp}/aster_ffi.h.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

"$ffi_dir/scripts/gen-header.sh" "$tmp"

if ! diff -u "$ffi_dir/aster_ffi.h" "$tmp"; then
  echo >&2
  echo "check-header.sh: ffi/aster_ffi.h is stale. Run ffi/scripts/gen-header.sh and commit the result." >&2
  exit 1
fi
echo "check-header.sh: ffi/aster_ffi.h is up to date."
