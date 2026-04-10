"""
Nemeses -- fault injectors that wrap byte-queue streams.

Each nemesis intercepts reads/writes on the in-process pipes used by
create_local_session. The server writes clean data; the nemesis corrupts
it before the client sees it (simulating network-level faults).
"""

from __future__ import annotations

import asyncio
import functools
from typing import Any


class NemesisBase:
    name: str = "base"

    async def before_read(self, n: int) -> None:
        pass

    def after_read(self, data: bytes) -> bytes:
        return data

    async def before_write(self, data: bytes) -> bytes:
        return data

    def setup(self, service_class: type) -> type:
        return service_class


class NemesisRecvStream:
    """Wraps a recv stream and intercepts reads via a nemesis."""

    def __init__(self, inner: Any, nemesis: NemesisBase) -> None:
        self._inner = inner
        self._nemesis = nemesis

    async def read_exact(self, n: int) -> bytes:
        await self._nemesis.before_read(n)
        data = await self._inner.read_exact(n)
        return self._nemesis.after_read(data)


class NemesisSendStream:
    """Wraps a send stream and intercepts writes via a nemesis."""

    def __init__(self, inner: Any, nemesis: NemesisBase) -> None:
        self._inner = inner
        self._nemesis = nemesis

    async def write_all(self, data: bytes) -> None:
        data = await self._nemesis.before_write(data)
        await self._inner.write_all(data)

    async def finish(self) -> None:
        await self._inner.finish()


# -- Concrete nemeses ---------------------------------------------------------


class NoFault(NemesisBase):
    """Control -- no fault injection."""
    name = "no_fault"


class KillRecvStream(NemesisBase):
    """After N read_exact calls, raise ConnectionError (simulates EOF)."""
    name = "kill_recv"

    def __init__(self, kill_after_reads: int = 8) -> None:
        self._kill_after = kill_after_reads
        self._read_count = 0

    async def before_read(self, n: int) -> None:
        self._read_count += 1
        if self._read_count > self._kill_after:
            raise ConnectionError("stream killed by nemesis")


class CorruptFrame(NemesisBase):
    """After N read_exact calls, flip bits in the returned data."""
    name = "corrupt_frame"

    def __init__(self, corrupt_after_reads: int = 10) -> None:
        self._corrupt_after = corrupt_after_reads
        self._read_count = 0
        self._corrupted = False

    def after_read(self, data: bytes) -> bytes:
        self._read_count += 1
        if self._read_count == self._corrupt_after and not self._corrupted and len(data) > 0:
            self._corrupted = True
            corrupted = bytearray(data)
            corrupted[0] ^= 0xFF
            return bytes(corrupted)
        return data


class SlowHandler(NemesisBase):
    """Monkey-patch service handlers to sleep before executing."""
    name = "slow_handler"

    def __init__(self, delay_s: float = 5.0) -> None:
        self._delay = delay_s

    def setup(self, service_class: type) -> type:
        from aster.decorators import _SERVICE_INFO_ATTR

        info = getattr(service_class, _SERVICE_INFO_ATTR, None)
        if info is None:
            return service_class

        delay = self._delay

        class SlowService(service_class):
            pass

        # Patch each handler with a sleep
        for method_name in info.methods:
            original = getattr(service_class, method_name, None)
            if original is None:
                continue

            @functools.wraps(original)
            async def slow_wrapper(self_inner, *args, _orig=original, _d=delay, **kwargs):
                await asyncio.sleep(_d)
                return await _orig(self_inner, *args, **kwargs)

            setattr(SlowService, method_name, slow_wrapper)

        # Carry the service info to the subclass
        setattr(SlowService, _SERVICE_INFO_ATTR, info)
        return SlowService
