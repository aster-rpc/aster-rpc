I investigated the `CTypeId` warning properly and confirmed it is **not caused by our Aster ROW wrapper**. Reproducing the upstream-style `pyfory.format.infer_schema(...)` + `create_row_encoder(...)` flow directly in this environment still emits the same warning, even without any Aster code involved.

What we learned:
- The warning reproduces with plain `pyfory.format.create_row_encoder(schema)`.
- We are on `pyfory 0.16.0` in a Python `3.13.5` environment.
- The installed package contains `pyfory/includes/libformat.pxd` with `cpdef enum class CTypeId`, which matches the warning text and points to a pyfory/Cython packaging/runtime issue rather than a bug in our codec logic.
- I removed the runtime suppression workaround so we are no longer hiding the problem in library code.

Current state:
- Targeted tests still pass: `114 passed`, but the ROW warning remains visible because it originates inside pyfory itself.
- So the correct answer is: **no suppression in Aster runtime**, and the next real fix should be one of:
  1. investigate whether pyfory has a newer version that fixes Python 3.13/Cython enum import behavior,
  2. pin/document a pyfory+Python combination without the warning,
  3. or open/track an upstream pyfory issue with this minimal reproduction.

A good reproduction command is:
`uv run python - <<'PY'
import warnings
from dataclasses import dataclass
import pyfory.format as fmt

@dataclass
class UpstreamLike:
    f1: str
    f2: int

with warnings.catch_warnings(record=True) as rec:
    warnings.simplefilter('always')
    schema = fmt.infer_schema(UpstreamLike)
    encoder = fmt.create_row_encoder(schema)
    row = encoder.to_row(UpstreamLike('abc', 1))
    back = encoder.from_row(row)
    print('warnings:', len(rec))
    for w in rec:
        print(type(w.message).__name__, str(w.message))
    print('back:', back)
PY`

If you want, next I can help prepare the exact upstream bug report or investigate whether a pyfory/Python version change avoids it.
