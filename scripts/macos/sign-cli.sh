#!/usr/bin/env bash
# Codesign the `aster` CLI with a stable Developer ID identity.
#
# Beyond Gatekeeper: macOS Keychain items are bound to the code signature of
# the binary that created them. cargo's default ad-hoc signature changes with
# every build, so an ad-hoc `aster` cannot reliably reach root keys stored by
# an earlier build. A stable Developer ID plus a fixed --identifier keeps the
# designated requirement constant across rebuilds and upgrades. The hardened
# runtime and secure timestamp make the binary ready for notarization.
# Adapted from portal_desktop scripts/macos/sign.sh. The CLI loads no
# third-party dylibs, so it needs no entitlements.
#
# The identity name is not a secret. Notarization credentials are not used
# here; see scripts/macos/notarize-cli.sh.
#
# Usage:
#   MACOS_SIGN_IDENTITY="Developer ID Application: <Name> (<TEAM>)" \
#     scripts/macos/sign-cli.sh [path-to-binary]   # default: target/release/aster
#
# Every binary signed by the same team shares the Keychain partition that
# holds Aster root keys (see aster-keystore), so sign only tools you would
# trust with them.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
bin="${1:-$here/../../target/release/aster}"
identity="${MACOS_SIGN_IDENTITY:?set MACOS_SIGN_IDENTITY to your Developer ID Application identity (security find-identity -v -p codesigning)}"
identifier="${MACOS_SIGN_IDENTIFIER:-site.aster.cli}"

if [[ ! -f "$bin" ]]; then
  echo "error: binary not found: $bin (build first: cargo build -p aster-cli --release)" >&2
  exit 1
fi

echo "› signing $bin"
echo "  identity:   $identity"
echo "  identifier: $identifier (stable → Keychain access survives upgrades)"
codesign --force --options runtime --timestamp \
  --identifier "$identifier" \
  --sign "$identity" \
  "$bin"

echo "› verifying"
codesign --verify --strict --verbose=2 "$bin"
sig="$(codesign -dvv "$bin" 2>&1 || true)"
grep -E "Identifier|TeamIdentifier|Authority|Timestamp|flags" <<<"$sig" || true
echo "✓ signed"
