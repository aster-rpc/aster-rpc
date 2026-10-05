"""O0 smoke test: ASTER_LOG_STDERR=1 makes importing the native module install
the Rust stderr log sink. Runs in a subprocess: the sink is once per process."""

from __future__ import annotations

import os
import subprocess
import sys


def _import_stderr(enable: bool) -> str:
    env = {k: v for k, v in os.environ.items() if k not in ("ASTER_LOG_STDERR", "ASTER_LOG")}
    if enable:
        env["ASTER_LOG_STDERR"] = "1"
    proc = subprocess.run(
        [sys.executable, "-c", "import aster"],
        env=env, capture_output=True, text=True, timeout=60, check=True,
    )
    return proc.stderr


def test_import_installs_the_stderr_sink_when_asked():
    on = _import_stderr(True)
    assert "aster::otel" in on and "stderr logging enabled" in on, on
    off = _import_stderr(False)
    assert "stderr logging enabled" not in off, off
