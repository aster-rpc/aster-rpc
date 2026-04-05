"""
tests/conformance/wire/conftest.py

Session-scoped conftest that generates binary wire fixture files on first run.

The fixture files are committed once generated.  Re-running this conftest
will NOT overwrite existing files, so the golden bytes remain stable across
runs.  To regenerate, delete the .bin files and re-run pytest.
"""

from __future__ import annotations

import asyncio
import io
from pathlib import Path


from aster.framing import CANCEL, TRAILER, write_frame

_FIXTURES_DIR = Path(__file__).parent


class _MemSendStream:
    def __init__(self) -> None:
        self._buf = io.BytesIO()

    async def write_all(self, data: bytes) -> None:
        self._buf.write(data)

    def getvalue(self) -> bytes:
        return self._buf.getvalue()


def _generate_fixture(filename: str, payload: bytes, flags: int) -> None:
    """Write a fixture file if it does not already exist."""
    path = _FIXTURES_DIR / filename
    if path.exists():
        return

    async def _write() -> bytes:
        send = _MemSendStream()
        await write_frame(send, payload, flags=flags)
        return send.getvalue()

    raw = asyncio.get_event_loop().run_until_complete(_write())
    path.write_bytes(raw)


def pytest_configure(config) -> None:
    """Generate binary fixtures before any tests run."""
    _generate_fixture("cancel_flags_only.bin", b"", CANCEL)
    _generate_fixture("trailer_ok.bin", b"", TRAILER)
