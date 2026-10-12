#!/usr/bin/env bash
# Qualify the `aster` CLI's root-key lifecycle in the macOS Keychain across
# an upgrade: two differently built binaries (release and debug, so their code
# hashes differ) signed with the same Developer ID and identifier must create,
# read, export, delete and re-import each other's items without a prompt.
# Deleting another build's item exercises aster-keystore's `security` fallback.
#
# Uses a throwaway Keychain service name and a temporary ASTER_HOME; it never
# touches the real `aster` service or ~/.aster. Every step has a timeout, so
# an unexpected Keychain access prompt fails the step instead of hanging.
#
# Usage: MACOS_SIGN_IDENTITY="Developer ID Application: …" scripts/macos/qualify-cli-keychain.sh
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
work="$(mktemp -d)"
service="aster-qual-$$"
export ASTER_HOME="$work/home" ASTER_KEYSTORE=os ASTER_KEYRING_SERVICE="$service"
unset ASTER_PROFILE

cleanup() {
  while security delete-generic-password -s "$service" >/dev/null 2>&1; do :; done
  rm -rf "$work"
}
trap cleanup EXIT

echo "› building and signing two builds"
cargo build -q --manifest-path "$repo/Cargo.toml" -p aster-cli --release
cargo build -q --manifest-path "$repo/Cargo.toml" -p aster-cli
cp "$repo/target/release/aster" "$work/aster-a"
cp "$repo/target/debug/aster" "$work/aster-b"
"$here/sign-cli.sh" "$work/aster-a" >/dev/null
"$here/sign-cli.sh" "$work/aster-b" >/dev/null
cdhash() { codesign -dvvv "$1" 2>&1 | awk -F= '/^CDHash=/{print $2}'; }
[[ "$(cdhash "$work/aster-a")" != "$(cdhash "$work/aster-b")" ]] || { echo "error: builds are identical" >&2; exit 1; }

failures=0
step() {
  local name="$1" expect="$2"; shift 2
  local out
  if out="$(perl -e 'alarm 30; exec @ARGV' "$@" --profile qual --json 2>&1)" && [[ "$out" == *"$expect"* ]]; then
    echo "  PASS  $name"
  else
    echo "  FAIL  $name"; echo "$out" | sed 's/^/        /'
    failures=$((failures + 1))
  fi
}

echo "› lifecycle (service $service)"
step "A generates"                   '"action":"generated"' "$work/aster-a" key root generate
step "B (upgrade) reads A's item"     '"stored":true' "$work/aster-b" key root show
step "B exports A's item"             '"file"'    "$work/aster-b" key root export --out "$work/backup.key"
step "B deletes A's item"             '"stored":false' "$work/aster-b" key root delete --yes
step "B imports the backup"           '"action":"imported"' "$work/aster-b" key root import "$work/backup.key"
step "A (downgrade) reads B's item"   '"stored":true' "$work/aster-a" key root show
step "A deletes B's item"             '"stored":false' "$work/aster-a" key root delete --yes

if ((failures)); then
  echo "✗ $failures step(s) failed"; exit 1
fi
echo "✓ Keychain lifecycle qualified across differently built, identically signed binaries"
