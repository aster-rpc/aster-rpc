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

uv run maturin develop -m "$MANIFEST" "$@"

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
