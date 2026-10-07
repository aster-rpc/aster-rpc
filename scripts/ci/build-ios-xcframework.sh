#!/usr/bin/env bash
# XCFrameworks consume static archives; desktop FFI retains rlib/cdylib.
set -euo pipefail

: "${CARGO_TARGET_DIR:?CARGO_TARGET_DIR must identify the job target directory}"
output="${1:-AsterTransport.xcframework}"
targets=(aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios)
for target in "${targets[@]}"; do
  cargo rustc --locked --release --lib -p aster_transport_ffi \
    --target "$target" --crate-type staticlib
  archive="$CARGO_TARGET_DIR/$target/release/libaster_transport_ffi.a"
  test -s "$archive" || { echo "::error::missing iOS static archive: $archive"; exit 1; }
done

mkdir -p "$CARGO_TARGET_DIR/ios-sim-fat"
lipo -create \
  "$CARGO_TARGET_DIR/aarch64-apple-ios-sim/release/libaster_transport_ffi.a" \
  "$CARGO_TARGET_DIR/x86_64-apple-ios/release/libaster_transport_ffi.a" \
  -output "$CARGO_TARGET_DIR/ios-sim-fat/libaster_transport_ffi.a"
xcodebuild -create-xcframework \
  -library "$CARGO_TARGET_DIR/aarch64-apple-ios/release/libaster_transport_ffi.a" \
  -library "$CARGO_TARGET_DIR/ios-sim-fat/libaster_transport_ffi.a" \
  -output "$output"
test -s "$output/Info.plist" || { echo "::error::XCFramework metadata was not produced: $output"; exit 1; }
