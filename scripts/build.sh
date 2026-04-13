#!/usr/bin/env bash
# build.sh — Build the Python extension and regenerate type stubs.
#
# Usage: ./scripts/build.sh
#
# Wraps `maturin develop` and regenerates bindings/python/aster/_aster.pyi
# so IntelliJ / mypy see the current native module surface.

set -euo pipefail

MANIFEST="bindings/python/rust/Cargo.toml"
STUB="bindings/python/aster/_aster.pyi"

WHEEL_DIR="bindings/python/target/wheels"

# Pin aster-rpc fork transitive deps (hickory-proto / hickory-net → beta.1)
# before invoking cargo. No-op if already pinned.
./scripts/pin-fork-deps.sh

# Build wheel, then install it — single cargo compilation
uv run maturin build -m "$MANIFEST" --out "$WHEEL_DIR" "$@"
uv pip install "$WHEEL_DIR"/aster_rpc-*.whl --force-reinstall --no-deps

echo "✓ Wheel(s) in $WHEEL_DIR"

# Regenerate native-module type stub from the live compiled module.
uv run python -c "
import aster._aster as m
lines = [
    '\"\"\"Auto-generated type stubs for the native _aster extension module.\"\"\"',
    '',
    'from typing import Any, Coroutine, Optional',
    '',
]
for name in sorted(dir(m)):
    if name.startswith('_'):
        continue
    obj = getattr(m, name)
    if isinstance(obj, type):
        lines.append(f'class {name}: ...')
    elif callable(obj):
        lines.append(f'def {name}(*args: Any, **kwargs: Any) -> Any: ...')
    else:
        lines.append(f'{name}: Any')
print('\n'.join(lines))
" > "$STUB"

echo "✓ Regenerated $STUB ($(wc -l < "$STUB" | tr -d ' ') lines)"
