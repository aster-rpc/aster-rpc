"""The wait budget: how long a test waits for something that must happen.

A test that waits for a reply, a delivered cancel or a member becoming ready
bounds the wait with ``WAIT_BUDGET`` (seconds) rather than a literal: a loaded
CI host (two jobs per Linux runner, one of them compiling) took over 3 s to
deliver a cancel that takes milliseconds idle. ``ASTER_TEST_TIME_SCALE``
(default 1; CI sets it) multiplies the base, and conftest.py scales the
pytest-timeout per-test limit by the same factor.

Never use it for a wait that is meant to expire ("nothing arrives within X", a
deadline under test): scaling those only slows the test down. Those keep their
literal with a ``short-wait-ok: <reason>`` comment
(scripts/ci/lint_test_waits.py). Rust, TS, Java, .NET and Go carry the same
budget under the same variable.

Importable from tests/python (pytest puts this directory on sys.path), and
from tests/chaos through its conftest.
"""

from __future__ import annotations

import os

WAIT_BUDGET_BASE = 15.0


def time_scale() -> float:
    """``ASTER_TEST_TIME_SCALE``, or 1 when unset or empty."""
    raw = os.environ.get("ASTER_TEST_TIME_SCALE", "").strip()
    if not raw:
        return 1.0
    try:
        scale = float(raw)
    except ValueError:
        scale = 0.0
    if not (scale > 0 and scale != float("inf")):
        # A typo here would otherwise silently run CI unscaled.
        raise ValueError(f"ASTER_TEST_TIME_SCALE must be a positive number, got {raw!r}")
    return scale


TIME_SCALE = time_scale()
WAIT_BUDGET = WAIT_BUDGET_BASE * TIME_SCALE
