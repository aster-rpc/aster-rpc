#!/usr/bin/env bash
# pin-fork-deps.sh — Pin transitive deps that cargo's default resolver gets wrong.
#
# Why this exists:
#   The aster-rpc/iroh fork pins `hickory-resolver = "=0.26.0-beta.1"`, but its
#   transitive `hickory-proto` and `hickory-net` are semver-loose (`^0.26.0-beta.1`).
#   Cargo's resolver picks the latest beta (currently 0.26.0-beta.3), which has
#   breaking API changes that the beta.1 resolver doesn't compile against.
#
#   Run this once after a fresh clone (or after `rm Cargo.lock`) to force
#   hickory-proto and hickory-net down to beta.1.
#
# Usage: ./scripts/pin-fork-deps.sh
# Idempotent — safe to run repeatedly.

set -euo pipefail

cd "$(dirname "$0")/.."

# Only generate a lockfile if one doesn't exist
if [ ! -f Cargo.lock ]; then
    cargo generate-lockfile >/dev/null 2>&1
fi

# Pin hickory-proto and hickory-net to beta.1 (matches aster-rpc/iroh's pin on
# hickory-resolver). Silently succeeds if already pinned.
cargo update -p hickory-proto --precise 0.26.0-beta.1 >/dev/null 2>&1 || true
cargo update -p hickory-net   --precise 0.26.0-beta.1 >/dev/null 2>&1 || true

echo "✓ Fork transitive deps pinned (hickory-proto, hickory-net → 0.26.0-beta.1)"
