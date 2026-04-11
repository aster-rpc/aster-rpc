#!/usr/bin/env bash
# build-api-docs.sh -- Regenerate the Python and TypeScript API reference
# pages and copy them into the public docs site.
#
# Usage: ./scripts/build-api-docs.sh
#
# Pre-reqs:
#   - uv with the framework installed (run ./scripts/build.sh first)
#   - bun (for the TypeScript side)
#   - The aster-rpc/docs repo cloned at the path below (override with
#     ASTER_DOCS_REPO=/path/to/repo)
#
# Outputs:
#   - <docs>/static/api/python/    pdoc HTML for `aster.public`
#   - <docs>/static/api/typescript/ TypeDoc HTML for `@aster-rpc/aster`
#
# Both are generated from a curated public-surface module
# (`aster.public` for Python, `@aster-rpc/aster/src/public.ts` for TS)
# so the API references stay focused and aligned across languages.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOCS_REPO="${ASTER_DOCS_REPO:-$HOME/dev/aster/docs}"

if [ ! -d "$DOCS_REPO" ]; then
    echo "error: docs repo not found at $DOCS_REPO" >&2
    echo "  set ASTER_DOCS_REPO=/path/to/aster-rpc/docs" >&2
    exit 1
fi

echo "── docs repo: $DOCS_REPO"

# ── Python (pdoc) ───────────────────────────────────────────────────
echo
echo "── regenerating Python API docs (pdoc aster.public) ──"
cd "$REPO_ROOT"
uv run --quiet --with pdoc pdoc \
    --no-include-undocumented \
    -o /tmp/aster-pdoc \
    aster.public

PY_DEST="$DOCS_REPO/static/api/python"
rm -rf "$PY_DEST"
mkdir -p "$PY_DEST"
cp -R /tmp/aster-pdoc/* "$PY_DEST/"
echo "  -> $PY_DEST"

# ── TypeScript (typedoc) ────────────────────────────────────────────
echo
echo "── regenerating TypeScript API docs (typedoc @aster-rpc/aster) ──"
cd "$REPO_ROOT/bindings/typescript/packages/aster"
bun run docs

TS_DEST="$DOCS_REPO/static/api/typescript"
rm -rf "$TS_DEST"
mkdir -p "$TS_DEST"
cp -R docs-api/* "$TS_DEST/"
echo "  -> $TS_DEST"

echo
echo "✓ API docs regenerated. Don't forget to commit and push from $DOCS_REPO"
