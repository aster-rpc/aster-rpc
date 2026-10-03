#!/usr/bin/env bash
# Build the aster_transport_ffi cdylib, compile ffi/tests/abi_contract_test.c
# against ffi/aster_ffi.h + that library, and run it.
#
#   ffi/scripts/run-c-abi-test.sh
#
# Env:
#   CARGO        cargo command to use (default: cargo), e.g. "with-kache cargo"
#   CC           C compiler (default: cc)
#   PROFILE      debug | release (default: debug)
#   SKIP_BUILD=1 reuse an existing library instead of invoking cargo
set -euo pipefail

ffi_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "$ffi_dir/.." && pwd)"
profile="${PROFILE:-debug}"
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
lib_dir="$target_dir/$profile"

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  profile_flag=()
  [[ "$profile" == "release" ]] && profile_flag=(--release)
  (cd "$repo_root" && ${CARGO:-cargo} build -p aster_transport_ffi --lib "${profile_flag[@]}")
fi

case "$(uname -s)" in
  Darwin*) lib="$lib_dir/libaster_transport_ffi.dylib" ;;
  *)       lib="$lib_dir/libaster_transport_ffi.so" ;;
esac
[[ -f "$lib" ]] || { echo "run-c-abi-test.sh: missing $lib" >&2; exit 1; }

out_dir="$(mktemp -d "${TMPDIR:-/tmp}/aster-abi-test.XXXXXX")"
trap 'rm -rf "$out_dir"' EXIT

${CC:-cc} -std=c11 -Wall -Wextra -Werror \
  -I "$ffi_dir" \
  -o "$out_dir/abi_contract_test" \
  "$ffi_dir/tests/abi_contract_test.c" \
  -L "$lib_dir" -laster_transport_ffi -Wl,-rpath,"$lib_dir"

"$out_dir/abi_contract_test"
