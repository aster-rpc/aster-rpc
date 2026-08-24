#!/usr/bin/env bash
# check-coordination-deps.sh -- the coordination feature-graph gate (Slice 0).
#
# Three properties, all easy to break with a one-line Cargo.toml edit and none
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
#   3. `windows-sys` gains no direct edge outside the coordination features. It
#      exists for §11.4's `ReplaceFileW` snapshot replacement, which only a
#      voter performs.
#
# Depth-1 output is `aster` plus its direct dependencies, which is exactly the
# set properties (2) and (3) are about.
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
# graph. `--locked` matches the rest of CI and means a stale Cargo.lock fails
# the gate rather than being quietly resolved into whatever it would become.
tree() { cargo tree --locked -p aster -e normal "$@"; }

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

# ── 3. No direct windows-sys edge outside the voter feature ─────────────────
#
# windows-sys is declared under `[target.'cfg(windows)'.dependencies]`, so on
# this Linux/macOS runner `cargo tree` prunes it from every graph. An assertion
# made without `--target` would therefore pass while proving nothing at all --
# including on the day someone moves the edge to `coordination-client`.
# Resolving explicitly for a Windows triple puts the edge in the graph being
# inspected. This resolves metadata only; the target need not be installed.
WIN_TARGET=x86_64-pc-windows-msvc
wintree() { tree --target "$WIN_TARGET" "$@"; }

out=$(wintree --depth 1)
if grep -q '\bwindows-sys\b' <<<"$out"; then
    fail "aster declares a direct windows-sys dependency in the 'default' Windows graph; it belongs to the coordination features only"
fi
pass "no direct windows-sys edge in the 'default' Windows graph"

# Both coordination graphs need the pinned edge on Windows: the voter for
# the write-through snapshot replacement, the client for the S4-D journal's
# write-through record replacement. A client without it would make the
# journal's durability claim quietly false on one platform.
for label in "coordination-client" "coordination"; do
    graph_win=$(wintree --depth 1 --no-default-features --features "$label")
    if ! grep -q 'windows-sys v0.61.2' <<<"$graph_win"; then
        fail "the '$label' Windows graph does not declare the pinned direct windows-sys v0.61.2 edge its write-through replacement needs"
    fi
    pass "direct windows-sys v0.61.2 edge present in the '$label' Windows graph"
done

# The lock is the standard library's, not a crate's. A reintroduced fs2/fs4
# would compile and pass every other gate while silently adding a supply-chain
# edge for an operation `std::fs::File::try_lock()` already provides.
for crate_name in fs2 fs4; do
    if grep -q "\b${crate_name}\b" <<<"$(tree --no-default-features --features coordination)"; then
        fail "'$crate_name' is back in the 'coordination' graph; the directory \
lock uses std::fs::File::try_lock() on the pinned toolchain"
    fi
    pass "no '$crate_name' edge in the 'coordination' graph"
done

echo -e "\n${GREEN}Coordination dependency gate passed.${RESET}"
