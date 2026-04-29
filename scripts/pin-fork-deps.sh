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

# NOTE (2026-04-29, iroh 0.98 upgrade): iroh 0.98 explicitly pins hickory-proto
# and hickory-net to 0.26.0-beta.4 in its own Cargo.toml, so the prior beta.1
# downgrade is now incompatible. This script is a no-op until a new transitive
# semver mismatch surfaces.

echo "✓ Fork transitive deps OK (hickory pins handled inline by iroh 0.98)"
