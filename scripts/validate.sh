#!/usr/bin/env bash
# validate.sh — Run the same checks as CI locally to catch issues before pushing.
# Usage: ./scripts/validate.sh
#
# Mirrors the jobs in .github/workflows/ci.yml + ci-typescript.yml:
#   1. cargo fmt --all --check (workspace)
#   2. cargo clippy --workspace --all-targets -- -D warnings (NAPI excluded)
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

if command -v sccache &>/dev/null; then
    step "sccache stats"
    sccache --show-stats || true
fi

echo -e "\n${GREEN}${BOLD}All checks passed!${RESET}"