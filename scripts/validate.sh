#!/usr/bin/env bash
# validate.sh — Run the same checks as CI locally to catch issues before pushing.
# Usage: ./scripts/validate.sh
#
# Mirrors the jobs in .github/workflows/ci.yml:
#   1. cargo fmt --check
#   2. cargo clippy -- -D warnings
#   3. uv run maturin develop (build the extension)
#   4. uv run pytest tests/python/

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
BOLD='\033[1m'
RESET='\033[0m'

pass() { echo -e "${GREEN}✓ $1${RESET}"; }
fail() { echo -e "${RED}✗ $1${RESET}"; exit 1; }
step() { echo -e "\n${BOLD}── $1 ──${RESET}"; }

# ── Optional local compiler cache (sccache) ─────────────────────────
if command -v sccache &>/dev/null; then
    export RUSTC_WRAPPER=sccache
    pass "Using sccache (RUSTC_WRAPPER=sccache)"
    # Reset stats for this run so end-of-run output is easier to read.
    sccache --zero-stats >/dev/null 2>&1 || true
else
    echo "  ⚠ sccache not found — running without compiler artifact cache."
    echo "  Install with: brew install sccache"
fi

# ── 1. Rust formatting ─────────────────────────────────────────────
step "cargo fmt --check"
if cargo fmt --manifest-path bindings/python/rust/Cargo.toml --check; then
    pass "Formatting OK"
else
    fail "Formatting issues found. Run: cargo fmt --manifest-path bindings/python/rust/Cargo.toml"
fi

# ── 2. Clippy (treats warnings as errors, same as CI) ──────────────
step "cargo clippy -- -D warnings"
if cargo clippy --manifest-path bindings/python/rust/Cargo.toml -- -D warnings; then
    pass "Clippy OK"
else
    fail "Clippy found errors. Fix the issues above."
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

if command -v sccache &>/dev/null; then
    step "sccache stats"
    sccache --show-stats || true
fi

echo -e "\n${GREEN}${BOLD}All checks passed!${RESET}"