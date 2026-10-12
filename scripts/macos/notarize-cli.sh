#!/usr/bin/env bash
# Notarize the signed `aster` CLI.
#
# A bare Mach-O cannot be stapled (stapler supports only .app/.dmg/.pkg), so
# a zip of the Developer-ID-signed binary is notarized; Gatekeeper verifies it
# online on first run. Adapted from portal_desktop scripts/macos/notarize.sh.
#
# Credentials live in the login Keychain as a notarytool profile, so no
# password appears on a command line (where `ps` would show it). Once:
#   xcrun notarytool store-credentials aster-notary \
#     --apple-id <apple-id> --team-id <TEAM>      # prompts for the app password
# Then:
#   APPLE_TEAM_ID=<TEAM> scripts/macos/notarize-cli.sh [path-to-binary]
# NOTARY_PROFILE overrides the profile name (default: aster-notary).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="${1:-$repo/target/release/aster}"
version="$("$bin" --version | awk '{print $2}')"
arch="$(uname -m)"
zip="$repo/dist/aster-cli-$version-macos-$arch.zip"

: "${APPLE_TEAM_ID:?set APPLE_TEAM_ID to the team that signed the binary}"
profile="${NOTARY_PROFILE:-aster-notary}"

# Capture first: `codesign … | grep -q` can SIGPIPE codesign, which under
# pipefail reads as a false failure.
sig="$(codesign -dvv "$bin" 2>&1 || true)"
if [[ "$sig" != *"TeamIdentifier=$APPLE_TEAM_ID"* || "$sig" != *"runtime"* ]]; then
  echo "error: $bin is not Developer-ID signed with the hardened runtime; run scripts/macos/sign-cli.sh first" >&2
  exit 1
fi

mkdir -p "$repo/dist"
rm -f "$zip"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
cp "$bin" "$staging/aster"
( cd "$staging" && zip -q -X "$zip" aster )

echo "› submitting $zip to the Apple notary service (waits for the result)…"
xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait

echo "✓ notarized: $zip"
echo "  sha256: $(shasum -a 256 "$zip" | awk '{print $1}')"
