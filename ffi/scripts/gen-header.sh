#!/usr/bin/env bash
# Regenerate ffi/aster_ffi.h from the Rust sources with cbindgen.
#
#   ffi/scripts/gen-header.sh [output-path]
#
# Requires cbindgen on PATH (`cargo install cbindgen --locked --version 0.29.2`).
# The checked-in header was produced with the version pinned below; other
# versions may format differently and trip check-header.sh.
set -euo pipefail

CBINDGEN_VERSION="0.29.2"
ffi_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${1:-$ffi_dir/aster_ffi.h}"

if ! command -v cbindgen >/dev/null 2>&1; then
  echo "gen-header.sh: cbindgen not found; install with: cargo install cbindgen --locked --version $CBINDGEN_VERSION" >&2
  exit 2
fi
have="$(cbindgen --version | awk '{print $2}')"
if [[ "$have" != "$CBINDGEN_VERSION" ]]; then
  echo "gen-header.sh: warning: cbindgen $have != pinned $CBINDGEN_VERSION; output may differ" >&2
fi

cd "$ffi_dir"
# -q: suppress the "Skip ... (not no_mangle)" warnings for internal statics.
cbindgen -q --config cbindgen.toml --crate aster_transport_ffi --output "$out"
