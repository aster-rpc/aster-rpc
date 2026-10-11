"""Raw WebTransport media transport; independent of framed Aster RPC.

Authentication runs before CONNECT upgrade. Applications retain responsibility
for capability/PoP admission before using media streams. Closing a session closes
its whole underlying QUIC connection. Keep stream owners alive after FIN until
transport_acked() or explicit reset; releasing an owner aborts its directions.
"""
from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

from .http import _AuthBinding, RPCAuthRequest, RPCAuthResponse
from .upcalls import get_runtime

__all__ = ["WebTransportListener", "WebTransportSession", "WebTransportStream",
           "WebTransportOptions", "RPCAuthRequest", "RPCAuthResponse"]

@dataclass(frozen=True)
class WebTransportOptions:
    certificate_pem: bytes
    private_key_pem: bytes = field(repr=False)
    path: str = "/wt"
    bind_addr: str = "127.0.0.1:0"
    max_sessions: int = 64
    max_streams: int = 128
    max_datagrams: int = 128
    max_datagram_bytes: int = 65536
    max_read_bytes: int = 1048576

    def _arguments(self) -> dict[str, Any]:
        values = vars(self).copy()
        for name in ("max_sessions", "max_streams", "max_datagrams", "max_datagram_bytes", "max_read_bytes"):
            value = values[name]
            if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
                raise ValueError(f"{name} must be a positive integer")
        return values

class WebTransportListener:
    def __init__(self, native, binding=None):
        self._native, self._binding = native, binding

    @classmethod
    async def start(cls, options: WebTransportOptions, authenticator) -> WebTransportListener:
        """Authenticate each CONNECT using the existing RPCAuthRequest/Response adapter."""
        from . import _aster
        if not callable(getattr(authenticator, "authenticate", authenticator)):
            raise TypeError("authenticator must be callable or implement authenticate")
        runtime = getattr(authenticator, "upcall_runtime", None) or get_runtime()
        binding = _AuthBinding(authenticator, runtime)
        try:
            native = await _aster._raw_wt_start(**options._arguments(), authenticator=binding)
        except BaseException:
            runtime.unbind(binding)
            raise
        return cls(native, binding)

    @classmethod
    async def start_public(cls, options: WebTransportOptions) -> WebTransportListener:
        """Explicitly allow anonymous CONNECTs on this exact route."""
        from . import _aster
        return cls(await _aster._raw_wt_start(**options._arguments()))

    @property
    def tcp_addr(self) -> str: return self._native.tcp_addr
    @property
    def quic_addr(self) -> str: return self._native.quic_addr

    async def accept(self) -> WebTransportSession:
        return WebTransportSession(await self._native.accept())

    async def stop(self, grace: float = 0.0) -> None:
        """Cancel admissions and sessions, then join within grace seconds and release TCP/UDP ports."""
        await self._native.stop(grace)
        if self._binding is not None:
            self._binding.upcall_runtime.unbind(self._binding)
            self._binding = None

    async def aclose(self) -> None: await self.stop()
    async def __aenter__(self): return self
    async def __aexit__(self, *exc): await self.aclose()
    def __del__(self):
        binding = getattr(self, "_binding", None)
        if binding is not None:
            try: binding.upcall_runtime.unbind(binding)
            except Exception: pass

class WebTransportSession:
    def __init__(self, native): self._native = native
    def info(self) -> dict[str, Any]: return json.loads(self._native.info())
    def stats(self) -> dict[str, int]: return json.loads(self._native.stats())
    @property
    def max_datagram_size(self) -> int | None: return self._native.max_datagram_size()
    def send_datagram(self, data: bytes) -> None: self._native.send_datagram(data)
    async def receive_datagram(self) -> bytes: return await self._native.receive_datagram()
    async def accept_bi(self) -> WebTransportStream: return WebTransportStream(await self._native.accept_bi())
    async def accept_uni(self) -> WebTransportStream: return WebTransportStream(await self._native.accept_uni())
    async def open_bi(self) -> WebTransportStream: return WebTransportStream(await self._native.open_bi())
    async def open_uni(self) -> WebTransportStream: return WebTransportStream(await self._native.open_uni())
    def close(self, code: int = 0, reason: bytes = b"") -> None:
        """Close the entire QUIC connection, waking all pending session operations."""
        self._native.close(code, reason)
    async def closed(self) -> dict[str, Any]: return json.loads(await self._native.closed())
    async def __aenter__(self): return self
    async def __aexit__(self, *exc): self.close()

class WebTransportStream:
    def __init__(self, native): self._native = native
    def info(self) -> dict[str, Any]: return json.loads(self._native.info())
    async def read(self, maximum: int = 65536) -> bytes:
        """Read up to maximum bytes; empty bytes means EOF. Cancellation preserves unread bytes."""
        return await self._native.read(maximum)
    async def write(self, data: bytes) -> None:
        """Write under backpressure. Cancellation after writing starts resets sending; a prefix may reach the peer."""
        await self._native.write(data)
    async def finish(self) -> None:
        """Send FIN; this is not delivery acknowledgment and does not release the stream."""
        await self._native.finish()
    async def reset(self, code: int = 0) -> None: await self._native.reset(code)
    async def stop(self, code: int = 0) -> None: await self._native.stop(code)
    async def set_priority(self, priority: int) -> None:
        """Signed transport priority, higher values first; independent of RPC urgency."""
        await self._native.set_priority(priority)
    async def priority(self) -> int | None: return await self._native.priority()
    async def transport_acked(self) -> bool:
        """Wait for transport acknowledgment; false means reset/loss, not application receipt."""
        return await self._native.transport_acked()
    async def aclose(self) -> None:
        info = self.info()
        if info["has_send"]: await self.reset()
        if info["has_receive"]: await self.stop()
    async def __aenter__(self): return self
    async def __aexit__(self, *exc): await self.aclose()
