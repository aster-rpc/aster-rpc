#!/usr/bin/env bash
# build.sh — Build the Python extension and regenerate type stubs.
#
# Usage: ./scripts/build.sh
#
# Wraps `maturin develop` and regenerates bindings/python/aster/_aster.pyi
# so IntelliJ / mypy see the current native module surface.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

MANIFEST="bindings/python/rust/Cargo.toml"
STUB="bindings/python/aster/_aster.pyi"

WHEEL_DIR="bindings/python/target/wheels"

# Pin aster-rpc fork transitive deps (hickory-proto / hickory-net → beta.1)
# before preserving the versioned files. If the pins need a legitimate lockfile
# refresh, that refresh should survive the temporary build-version stamping.
./scripts/pin-fork-deps.sh

# Wheels need the exact derived version in Python metadata, Cargo package
# metadata, and the native binary. Preserve the caller's working tree while
# temporarily stamping all tracked version files.
BUILD_STATE="$(mktemp -d)"
VERSIONED_BUILD_FILES=(
  pyproject.toml
  Cargo.lock
  core/Cargo.toml
  ffi/Cargo.toml
  aster/Cargo.toml
  aster-expose/Cargo.toml
  aster-macros/Cargo.toml
  aster-transport-salvo/Cargo.toml
  bindings/python/rust/Cargo.toml
  bindings/typescript/native/Cargo.toml
)
for relative in "${VERSIONED_BUILD_FILES[@]}"; do
  mkdir -p "$BUILD_STATE/$(dirname "$relative")"
  cp "$relative" "$BUILD_STATE/$relative"
done
if [[ -f BUILD_NUMBER ]]; then
  cp BUILD_NUMBER "$BUILD_STATE/BUILD_NUMBER"
  HAD_BUILD_NUMBER=true
else
  HAD_BUILD_NUMBER=false
fi
restore_build_state() {
  for relative in "${VERSIONED_BUILD_FILES[@]}"; do
    cp "$BUILD_STATE/$relative" "$relative"
  done
  if [[ "$HAD_BUILD_NUMBER" == true ]]; then
    cp "$BUILD_STATE/BUILD_NUMBER" BUILD_NUMBER
  else
    rm -f BUILD_NUMBER
  fi
  rm -rf "$BUILD_STATE"
}
trap restore_build_state EXIT

ASTER_BUILD_VERSION="$(./scripts/release/version.sh)"
export ASTER_BUILD_VERSION
./scripts/release/prepare-build-tree.sh "$ASTER_BUILD_VERSION" >/dev/null
echo "Building Aster $ASTER_BUILD_VERSION"

# Build wheel, then install it — single cargo compilation
uv run --frozen --no-sync maturin build -m "$MANIFEST" --out "$WHEEL_DIR" "$@"
WHEEL="$(ls -t "$WHEEL_DIR"/aster_rpc-*.whl | head -n 1)"
uv pip install --offline "$WHEEL" --force-reinstall --no-deps

echo "✓ Wheel(s) in $WHEEL_DIR"

# The repo's Python tests import the source-tree package. `maturin build` only
# writes the extension into the wheel, so refresh the in-tree extension before
# importing `aster._aster` for stub generation.
uv run --frozen --no-sync python - "$WHEEL" <<'PY'
import pathlib
import sys
import zipfile

wheel = pathlib.Path(sys.argv[1])
out_dir = pathlib.Path("bindings/python/aster")
with zipfile.ZipFile(wheel) as zf:
    candidates = [
        name for name in zf.namelist()
        if name.startswith("aster/_aster.")
        and (name.endswith(".so") or name.endswith(".pyd") or name.endswith(".dll"))
    ]
    if len(candidates) != 1:
        raise SystemExit(f"expected one native extension in {wheel}, found {candidates}")
    member = candidates[0]
    target = out_dir / pathlib.Path(member).name
    target.write_bytes(zf.read(member))
    target.chmod(0o755)
    print(f"✓ Refreshed {target} from {wheel.name}")
PY

# Regenerate native-module type stub from the live compiled module.
uv run --frozen --no-sync python -c "
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
