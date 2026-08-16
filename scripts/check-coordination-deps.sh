#!/usr/bin/env bash
# check-coordination-deps.sh -- the coordination feature-graph gate (Slice 0).
#
# Two properties, both easy to break with a one-line Cargo.toml edit and neither
# visible in a normal build:
#
#   1. OpenRaft is absent from the default and `coordination-client` graphs
#      entirely. It is a consensus engine; a client stub must not compile one,
#      and a consumer who never asked to host a voter must not link one.
#
#   2. `redb` gains no *direct* edge outside the `coordination` feature. Note
#      the word direct: iroh-blobs and iroh-docs already pull redb 4.1.0 and
#      3.1.3 transitively into every graph, so "redb is absent" is false and a
#      gate asserting it would be wrong. What must not appear is `aster`
#      declaring its own dependency on it.
#
# Depth-1 output is `aster` plus its direct dependencies, which is exactly the
# set property (2) is about.
#
# Usage: ./scripts/check-coordination-deps.sh

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
RESET='\033[0m'

pass() { echo -e "${GREEN}OK${RESET}  $1"; }
fail() { echo -e "${RED}FAIL${RESET} $1"; exit 1; }

# `-e normal` keeps dev-dependencies out: a dev-only OpenRaft (for the storage
# conformance suite) would be legitimate, and this gate is about the shipping
# graph.
tree() { cargo tree -p aster -e normal "$@"; }

# ── 1. OpenRaft is absent unless the voter feature is on ────────────────────

for label in "default" "coordination-client"; do
    if [[ "$label" == "default" ]]; then
        out=$(tree)
    else
        out=$(tree --no-default-features --features coordination-client)
    fi
    if grep -q '\bopenraft\b' <<<"$out"; then
        echo "$out" | grep -n 'openraft' || true
        fail "openraft is in the '$label' graph; it belongs only to 'coordination'"
    fi
    pass "no openraft in the '$label' graph"
done

voter=$(tree --no-default-features --features coordination)
if ! grep -q 'openraft v0.9.25' <<<"$voter"; then
    fail "the 'coordination' graph does not resolve openraft v0.9.25 (the pinned version)"
fi
pass "openraft v0.9.25 present in the 'coordination' graph"

# ── 2. No direct redb edge outside the voter feature ────────────────────────

for label in "default" "coordination-client"; do
    if [[ "$label" == "default" ]]; then
        out=$(tree --depth 1)
    else
        out=$(tree --depth 1 --no-default-features --features coordination-client)
    fi
    if grep -q '\bredb\b' <<<"$out"; then
        fail "aster declares a direct redb dependency in the '$label' graph"
    fi
    pass "no direct redb edge in the '$label' graph"
done

voter_direct=$(tree --depth 1 --no-default-features --features coordination)
if ! grep -q 'redb v4.1.0' <<<"$voter_direct"; then
    fail "the 'coordination' graph does not declare the pinned direct redb v4.1.0 edge"
fi
pass "direct redb v4.1.0 edge present in the 'coordination' graph"

echo -e "\n${GREEN}Coordination dependency gate passed.${RESET}"
