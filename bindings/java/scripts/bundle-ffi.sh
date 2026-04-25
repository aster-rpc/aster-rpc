#!/usr/bin/env bash
#
# Build aster_transport_ffi for the host platform and copy the resulting
# library into aster-runtime/src/main/resources/native/<os>-<arch>/.
#
# Used by build-java.yml to populate the per-platform classifier jar
# (`mvn package -Dnative.classifier=<os>-<arch>` picks up the lib from
# this resources directory).
#
# Local dev: running this script is optional. IrohLibrary's loader
# walks up to find target/{release,debug}/lib... in the workspace, so
# `cargo build -p aster_transport_ffi` is enough for `mvn test` to
# work without staging anything into resources.
#
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
runtime_root="$repo_root/bindings/java/aster-runtime"

uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
  Linux*)    os=linux;   ext=so    ;;
  Darwin*)   os=macos;   ext=dylib ;;
  MINGW*|MSYS*|CYGWIN*) os=windows; ext=dll ;;
  *) echo "bundle-ffi.sh: unsupported uname -s: $uname_s" >&2; exit 1 ;;
esac

case "$uname_m" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "bundle-ffi.sh: unsupported uname -m: $uname_m" >&2; exit 1 ;;
esac

if [[ "$os" == "windows" ]]; then
  lib_basename="aster_transport_ffi.${ext}"
else
  lib_basename="libaster_transport_ffi.${ext}"
fi

src="$repo_root/target/release/$lib_basename"

if [[ ! -f "$src" ]]; then
  echo "bundle-ffi.sh: building aster_transport_ffi (release) ..."
  (cd "$repo_root" && cargo build -p aster_transport_ffi --release)
fi

if [[ ! -f "$src" ]]; then
  echo "bundle-ffi.sh: expected $src after build, but it does not exist" >&2
  exit 1
fi

dest_dir="$runtime_root/src/main/resources/native/${os}-${arch}"
mkdir -p "$dest_dir"
cp -f "$src" "$dest_dir/$lib_basename"

echo "bundle-ffi.sh: copied $src -> $dest_dir/$lib_basename"
