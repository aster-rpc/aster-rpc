#!/usr/bin/env bash
# validate.sh — Run the same checks as CI locally to catch issues before pushing.
# Usage: ./scripts/validate.sh
#
# Approximates the jobs in .forgejo/workflows/linux.yml — the active CI and
# release workflow (AGENTS.md) — plus the TypeScript checks. It is not a full
# mirror: notably it does not run the core/FFI Rust test suites, only the
# feature-gated ingress proofs those suites silently skip.
#   1. cargo fmt --all --check (workspace)
#   2. cargo clippy --workspace --all-targets -- -D warnings (NAPI excluded)
#   2b. scripts/check-coordination-deps.sh (coordination feature graph)
#   2c. cargo test (core, ingress-metrics allocation proofs)
#   3. uv run maturin develop (build the extension)
#   4. uv run pytest tests/python/
#   5. cargo clippy (NAPI, lib only)
#   6. tsc --noEmit + vitest run (tests/typescript/)

set -euo pipefail

# Keep in sync with .github/workflows/ci.yml
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-1.94.1}"

RED='\033[0;31m'
GREEN='\033[0;32m'
BOLD='\033[1m'
RESET='\033[0m'

pass() { echo -e "${GREEN}✓ $1${RESET}"; }
fail() { echo -e "${RED}✗ $1${RESET}"; exit 1; }
step() { echo -e "\n${BOLD}── $1 ──${RESET}"; }

# ── Local compiler cache (Kache) ────────────────────────────────────
# Report the wrapper in effect; do not set one. A bare run picks up the
# global wrapper from ~/.cargo/config.toml, and running this script under
# scripts/with-kache pins the shared config via RUSTC_WRAPPER — overriding
# either here would defeat the launcher.
#
# This previously exported RUSTC_WRAPPER=sccache and, failing that, told you
# to `brew install sccache`. sccache was removed from this machine on
# 2026-08-15 and must not be reinstalled.
#
# Note the wrapper is a pinned absolute path, not a binary on PATH, so
# `command -v kache` is not a valid check for it.
KACHE_BIN=""
if [[ -n "${RUSTC_WRAPPER:-}" ]]; then
    KACHE_BIN="${RUSTC_WRAPPER}"
    pass "Compiler cache: RUSTC_WRAPPER=${RUSTC_WRAPPER}"
else
    cargo_cfg="${CARGO_HOME:-$HOME/.cargo}/config.toml"
    if [[ -r "$cargo_cfg" ]]; then
        KACHE_BIN=$(sed -n 's/^ *rustc-wrapper *= *"\(.*\)"/\1/p' "$cargo_cfg" | head -1)
    fi
    if [[ -n "$KACHE_BIN" && -x "$KACHE_BIN" ]]; then
        pass "Compiler cache: $(basename "$KACHE_BIN") via ${cargo_cfg/#$HOME/\~}"
    else
        KACHE_BIN=""
        echo "  ⚠ No compiler cache configured — expected kache. See the rust-dev"
        echo "    skill; do not install sccache."
    fi
fi

# ── 0. Pin fork transitive deps (hickory-proto / hickory-net) ─────
step "pin-fork-deps.sh"
if ./scripts/pin-fork-deps.sh; then
    pass "Fork deps pinned"
else
    fail "Failed to pin fork deps"
fi

# ── 1. Rust formatting (entire workspace) ─────────────────────────
step "cargo fmt --all --check"
if cargo fmt --all -- --check; then
    pass "Formatting OK"
else
    fail "Formatting issues found. Run: cargo fmt --all"
fi

# ── 2. Clippy (workspace, all targets, treats warnings as errors) ──
# NAPI is excluded here because its [lib] is `cdylib`-only — `--all-targets`
# rebuilds it as a lib-test and mis-flags `#[napi]`-exported symbols as
# dead. NAPI is linted on its own below.
step "cargo clippy --workspace --all-targets -- -D warnings"
if cargo clippy --workspace --all-targets --exclude aster-transport-napi -- -D warnings; then
    pass "Clippy OK"
else
    fail "Clippy found errors. Fix the issues above."
fi

# ── 2b. Coordination feature graph ────────────────────────────────
# A Cargo.toml edit can pull OpenRaft into the default graph, or give `aster` a
# direct redb edge, without breaking a single build. See the script for why the
# redb half is about the *direct* edge and not about redb being absent.
step "coordination dependency gate"
if ./scripts/check-coordination-deps.sh; then
    pass "Coordination feature graph OK"
else
    fail "Coordination dependency gate failed. See the output above."
fi

# ── 2c. §12.4 ingress allocation proofs ───────────────────────────
# `core/tests/ingress_metrics.rs` is `#![cfg(feature = "ingress-metrics")]`.
# Without the feature it compiles to an empty binary and reports "0 passed" —
# green, and proving nothing. These are the only tests that assert over-cap
# bytes were never *materialized*, rather than that the call was refused.
step "cargo test (core, ingress allocation proofs)"
if cargo test -p aster_transport_core --features ingress-metrics --test ingress_metrics; then
    pass "Ingress allocation proofs OK"
else
    fail "Ingress allocation proofs failed. See the output above."
fi

# ── 3. Build the Python extension via uv ───────────────────────────
step "Build extension + regenerate stubs"
if command -v uv &>/dev/null; then
    if ./scripts/build.sh; then
        pass "Build OK"
    else
        fail "build.sh failed"
    fi
else
    echo "  ⚠ uv not found — skipping build step."
    echo "  Install with: curl -LsSf https://astral.sh/uv/install.sh | sh"
fi

# ── 4. Tests via uv ────────────────────────────────────────────────
step "uv run pytest tests/python/"
if command -v uv &>/dev/null; then
    if uv run pytest tests/python/ -v --timeout=60; then
        pass "Tests OK"
    else
        fail "Tests failed"
    fi
else
    echo "  ⚠ uv not found — skipping test step."
    echo "  Install with: curl -LsSf https://astral.sh/uv/install.sh | sh"
fi

# ── 5. TypeScript: clippy (NAPI crate, lib-only) ──────────────────
# fmt is already covered by `cargo fmt --all` above; clippy on the
# napi crate stays scoped to the lib (no --all-targets) for the same
# reason it's excluded from the workspace clippy step.
step "cargo clippy (NAPI)"
if cargo clippy -p aster-transport-napi -- -D warnings; then
    pass "NAPI clippy OK"
else
    fail "NAPI clippy found errors."
fi

# ── 6. TypeScript: typecheck + tests ──────────────────────────────
if command -v bun &>/dev/null || [ -x "$HOME/.bun/bin/bun" ]; then
    BUN="${BUN:-${HOME}/.bun/bin/bun}"

    step "TypeScript typecheck (tsc --noEmit)"
    if (cd bindings/typescript/packages/aster && npx tsc --noEmit); then
        pass "TypeScript typecheck OK"
    else
        fail "TypeScript typecheck failed"
    fi

    step "TypeScript tests (vitest)"
    if (cd bindings/typescript/packages/aster && "$BUN" run vitest run); then
        pass "TypeScript tests OK"
    else
        fail "TypeScript tests failed"
    fi
else
    echo "  ⚠ bun not found — skipping TypeScript checks."
    echo "  Install with: curl -fsSL https://bun.sh/install | bash"
fi

# ── 7. Python static analysis (non-blocking) ─────────────────────
step "Python import check"
if uv run python -c "from aster import AsterServer, AsterClient, service, rpc, wire_type, any_of, all_of, SerializationMode" 2>&1; then
    pass "Import check OK"
else
    fail "Import check failed -- broken import in aster package"
fi

step "ASCII check (no Unicode dashes in Python source)"
if python3 scripts/check-ascii.py bindings/python/aster/*.py bindings/python/aster/**/*.py cli/aster_cli/*.py cli/aster_cli/**/*.py 2>/dev/null; then
    pass "ASCII check OK"
else
    echo "  ⚠ Non-ASCII characters found (see above). Fix before committing."
fi

step "Dead code check (vulture)"
if command -v vulture &>/dev/null || uv run vulture --version &>/dev/null 2>&1; then
    uv run vulture bindings/python/aster/ cli/aster_cli/ --min-confidence 80 2>&1 || true
    pass "Vulture scan complete (review any findings above)"
else
    echo "  ⚠ vulture not installed -- skipping dead code check"
fi

step "Pyright (Python type check -- warnings only)"
if command -v pyright &>/dev/null || uv run pyright --version &>/dev/null 2>&1; then
    uv run pyright bindings/python/aster/__init__.py bindings/python/aster/public.py 2>&1 || true
    pass "Pyright scan complete"
else
    echo "  ⚠ pyright not installed -- skipping type check"
fi

if [[ -n "$KACHE_BIN" ]]; then
    step "kache stats"
    # Scoped to roughly this run rather than all time, so the numbers describe
    # the validation that just ran.
    "$KACHE_BIN" stats --since 1h || true
fi

echo -e "\n${GREEN}${BOLD}All checks passed!${RESET}"