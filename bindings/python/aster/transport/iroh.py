"""
aster.transport.iroh -- Iroh-based remote transport.

Spec reference: §8.3.1 (Transport protocol), Aster-multiplexed-streams.md §5/§6

Every RPC call goes through the per-connection multiplexed-stream pool
via `aster._aster.AsterCall`. The call handle owns a pooled bi-stream
for its lifetime and releases it back to the pool on success (or
discards it on error). Streams are never finished per-call; the server
reads frames in a loop (spec §6), so every request's last frame
carries `FLAG_END_STREAM` to tell the dispatcher the request phase is
done.

`session_id` selects the pool routing key: 0 = SHARED pool (stateless),
non-zero = per-session pool. `ClientSession.open` on the client allocates
a monotonic u32 per connection.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, AsyncIterator

import aster
from aster._aster import AsterCall, StreamAcquireError
from aster.codec import ForyCodec, ForyConfig
from aster.framing import (
    COMPRESSED,
    END_STREAM,
    HEADER,
    ROW_SCHEMA,
    TRAILER,
    encode_frame,
)
from aster.protocol import RpcStatus, StreamHeader
from aster.rpc_types import SerializationMode
from aster.status import RpcError, StatusCode
from aster.transport.base import (
    BidiChannel,
    ConnectionLostError,
    Transport,
    TransportError,
)

if TYPE_CHECKING:
    import aster

import logging

logger = logging.getLogger(__name__)

# `recv_frame` kind discriminator values. Keep in sync with
# `bindings/python/rust/src/call.rs`.
_RECV_OK = 0
_RECV_END_OF_STREAM = 1
_RECV_TIMEOUT = 2


def _build_metadata(
    metadata: dict[str, str] | None,
) -> tuple[list[str], list[str]]:
    if not metadata:
        return [], []
    keys = list(metadata.keys())
    values = [metadata[k] for k in keys]
    return keys, values


def _acquire_error_to_rpc_error(exc: StreamAcquireError) -> RpcError:
    reason = getattr(exc, "reason", "UNKNOWN")
    # Pool exhaustion / transport failures all map to UNAVAILABLE so
    # callers see a retriable error (spec §6.7).
    return RpcError(StatusCode.UNAVAILABLE, f"stream acquire failed: {reason}: {exc}")


class _CallDriver:
    """Per-call helper that owns an `AsterCall` handle and drives the
    multiplexed request/response loop. Always pair with `release()` on
    success or `discard()` on error -- the handle is valid only between
    `acquire()` and one of those terminators.
    """

    def __init__(self, call: AsterCall, codec: ForyCodec) -> None:
        self._call = call
        self._codec = codec
        self._terminated = False

    @classmethod
    async def acquire(
        cls,
        conn: "aster.IrohConnection",
        session_id: int,
        codec: ForyCodec,
    ) -> "_CallDriver":
        """Acquire a **unary** call handle via the per-connection pool.
        For streaming RPC patterns use `acquire_streaming` -- per spec
        §3 line 65, streaming substreams must bypass the pool so they
        don't starve unary slots on the same session (scenario §4.4).
        """
        try:
            call = await AsterCall.acquire(conn, session_id)
        except StreamAcquireError as exc:
            raise _acquire_error_to_rpc_error(exc) from exc
        return cls(call, codec)

    @classmethod
    async def acquire_streaming(
        cls,
        conn: "aster.IrohConnection",
        codec: ForyCodec,
    ) -> "_CallDriver":
        """Acquire a **streaming** call handle. Opens a dedicated
        multiplexed substream via `open_bi` that bypasses the
        per-connection pool entirely (spec §3 line 65). Used for
        server-stream, client-stream, and bidi patterns. The session
        id still ships in the `StreamHeader` the binding writes, so
        the server still routes the call to the right session
        instance -- only pool accounting is bypassed.
        """
        try:
            call = await AsterCall.acquire_streaming(conn)
        except StreamAcquireError as exc:
            raise _acquire_error_to_rpc_error(exc) from exc
        return cls(call, codec)

    async def send_header(
        self,
        *,
        service: str,
        method: str,
        deadline_secs: int,
        serialization_mode: int,
        metadata: dict[str, str] | None,
        session_id: int,
    ) -> None:
        keys, values = _build_metadata(metadata)
        header = StreamHeader(
            service=service,
            method=method,
            version=1,
            callId=0,
            deadline=deadline_secs,
            serializationMode=serialization_mode,
            metadataKeys=keys,
            metadataValues=values,
            sessionId=session_id,
        )
        header_bytes = self._codec.encode(header)
        await self._call.send_frame(encode_frame(header_bytes, HEADER))

    async def send_request(self, request: Any, *, last: bool = True) -> None:
        payload, compressed = self._codec.encode_compressed(request)
        flags = 0
        if compressed:
            flags |= COMPRESSED
        if last:
            flags |= END_STREAM
        await self._call.send_frame(encode_frame(payload, flags))

    async def send_end_stream(self) -> None:
        """Send an empty END_STREAM frame to signal no more requests on a
        bidi-streaming call whose final request was already emitted
        without the `last=True` flag."""
        await self._call.send_frame(encode_frame(b"", END_STREAM))

    async def recv_frame(self) -> tuple[bytes, int] | None:
        """Pull one frame from the recv side. Returns `(payload, flags)`
        or `None` on end-of-stream. Raises on transport errors."""
        payload, flags, kind = await self._call.recv_frame(0)
        if kind == _RECV_OK:
            return payload, flags
        if kind == _RECV_END_OF_STREAM:
            return None
        # timeout_ms=0 means block indefinitely, so this branch is
        # unreachable in practice; treat as a transport error.
        raise TransportError("unexpected recv timeout on multiplexed stream")

    def decode_response(self, payload: bytes, flags: int, response_type: Any = None) -> Any:
        compressed = bool(flags & COMPRESSED)
        return self._codec.decode_compressed(payload, compressed)

    def check_trailer(self, payload: bytes) -> None:
        """Parse a trailer payload and raise RpcError if non-OK.

        Empty payload is a clean OK trailer (core strips empty END_STREAM
        forwards on the server side; see commit `cdabc02`).
        """
        if not payload:
            return
        status = self._codec.decode(payload, RpcStatus)
        if status.code != StatusCode.OK:
            raise RpcError.from_status(
                StatusCode(status.code),
                status.message,
                dict(zip(status.detailKeys, status.detailValues)),
            )

    def release(self) -> None:
        if not self._terminated:
            self._terminated = True
            self._call.release()

    def discard(self) -> None:
        if not self._terminated:
            self._terminated = True
            self._call.discard()


class IrohTransport(Transport):
    """Transport implementation over the per-connection multiplexed-stream pool.

    Every RPC call acquires a pooled bi-stream via `AsterCall`, writes a
    `StreamHeader` as the first frame, then drives the pattern-specific
    request/response loop. Streams are returned to the pool on success
    (`release`) or discarded on error (`discard`).

    Args:
        connection: The Iroh connection to issue calls on.
        codec: Fory codec for request/response serialization. Defaults
            to an XLANG codec.
        session_id: Pool routing key. 0 = SHARED (stateless); non-zero
            selects a per-session pool keyed by this id. Normally set
            via `AsterClient.open_session` rather than directly.
    """

    def __init__(
        self,
        connection: "aster.IrohConnection",
        codec: ForyCodec | None = None,
        fory_config: ForyConfig | None = None,
        session_id: int = 0,
    ) -> None:
        self._conn = connection
        self._codec = codec or ForyCodec(
            mode=SerializationMode.XLANG,
            fory_config=fory_config,
        )
        self._session_id = session_id
        from aster.json_codec import JsonProxyCodec
        self._default_serialization_mode = (
            SerializationMode.JSON.value
            if isinstance(self._codec, JsonProxyCodec)
            else SerializationMode.XLANG.value
        )

    @property
    def session_id(self) -> int:
        return self._session_id

    async def close(self) -> None:
        self._conn.close(0, b"normal close")

    def _resolve_mode(self, override: int | None) -> int:
        return override if override is not None else self._default_serialization_mode

    # ── Unary ───────────────────────────────────────────────────────────────

    async def unary(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int | None = None,
    ) -> Any:
        driver = await _CallDriver.acquire(self._conn, self._session_id, self._codec)
        try:
            await driver.send_header(
                service=service,
                method=method,
                deadline_secs=deadline_secs,
                serialization_mode=self._resolve_mode(serialization_mode),
                metadata=metadata,
                session_id=self._session_id,
            )
            await driver.send_request(request, last=True)

            response_payload: bytes | None = None
            response_flags = 0
            while True:
                frame = await driver.recv_frame()
                if frame is None:
                    raise ConnectionLostError("stream ended before trailer")
                payload, flags = frame
                if flags & TRAILER:
                    driver.check_trailer(payload)
                    if response_payload is None:
                        raise RpcError(
                            StatusCode.INTERNAL,
                            "unary call received OK trailer with no response frame",
                        )
                    driver.release()
                    return driver.decode_response(response_payload, response_flags)
                if flags & ROW_SCHEMA:
                    continue
                if response_payload is not None:
                    raise TransportError("unary call received multiple response frames")
                response_payload = payload
                response_flags = flags
        except BaseException:
            driver.discard()
            raise

    # ── Server Streaming ───────────────────────────────────────────────────

    def server_stream(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int | None = None,
    ) -> AsyncIterator[Any]:
        return self._server_stream_impl(
            service, method, request, metadata,
            deadline_secs, self._resolve_mode(serialization_mode),
        )

    async def _server_stream_impl(
        self,
        service: str,
        method: str,
        request: Any,
        metadata: dict[str, str] | None,
        deadline_secs: int,
        serialization_mode: int,
    ) -> AsyncIterator[Any]:
        driver = await _CallDriver.acquire_streaming(self._conn, self._codec)
        released = False
        try:
            await driver.send_header(
                service=service,
                method=method,
                deadline_secs=deadline_secs,
                serialization_mode=serialization_mode,
                metadata=metadata,
                session_id=self._session_id,
            )
            await driver.send_request(request, last=True)

            while True:
                frame = await driver.recv_frame()
                if frame is None:
                    raise ConnectionLostError("stream ended before trailer")
                payload, flags = frame
                if flags & TRAILER:
                    driver.check_trailer(payload)
                    driver.release()
                    released = True
                    return
                if flags & ROW_SCHEMA:
                    continue
                yield driver.decode_response(payload, flags)
        finally:
            if not released:
                driver.discard()

    # ── Client Streaming ───────────────────────────────────────────────────

    async def client_stream(
        self,
        service: str,
        method: str,
        requests: AsyncIterator[Any],
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int | None = None,
    ) -> Any:
        driver = await _CallDriver.acquire_streaming(self._conn, self._codec)
        try:
            await driver.send_header(
                service=service,
                method=method,
                deadline_secs=deadline_secs,
                serialization_mode=self._resolve_mode(serialization_mode),
                metadata=metadata,
                session_id=self._session_id,
            )
            # Buffer all requests so we can mark the last one with
            # END_STREAM. This matches Java's `runClientStream`.
            buffered: list[Any] = [item async for item in requests]
            if not buffered:
                # Protocol requires at least one request frame to arrive
                # inline with the call (core dispatcher bootstrap).
                raise TransportError(
                    "client_stream requires at least one request frame"
                )
            for i, item in enumerate(buffered):
                await driver.send_request(item, last=(i == len(buffered) - 1))

            response_payload: bytes | None = None
            response_flags = 0
            while True:
                frame = await driver.recv_frame()
                if frame is None:
                    raise ConnectionLostError("stream ended before trailer")
                payload, flags = frame
                if flags & TRAILER:
                    driver.check_trailer(payload)
                    if response_payload is None:
                        raise RpcError(
                            StatusCode.INTERNAL,
                            "client_stream got OK trailer with no response frame",
                        )
                    driver.release()
                    return driver.decode_response(response_payload, response_flags)
                if flags & ROW_SCHEMA:
                    continue
                if response_payload is not None:
                    raise TransportError(
                        "client_stream received multiple response frames"
                    )
                response_payload = payload
                response_flags = flags
        except BaseException:
            driver.discard()
            raise

    # ── Bidirectional Streaming ───────────────────────────────────────────

    def bidi_stream(
        self,
        service: str,
        method: str,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int | None = None,
    ) -> BidiChannel:
        return IrohBidiChannel(
            connection=self._conn,
            codec=self._codec,
            service=service,
            method=method,
            metadata=metadata,
            deadline_secs=deadline_secs,
            serialization_mode=self._resolve_mode(serialization_mode),
            session_id=self._session_id,
        )


class IrohBidiChannel(BidiChannel):
    """Interleaved bidi channel over a pooled multiplexed stream.

    Request frames carry `END_STREAM` on the final frame (emitted by
    `close()`), after which the pool handle is released once the server
    writes its trailer.
    """

    def __init__(
        self,
        connection: "aster.IrohConnection",
        codec: ForyCodec,
        service: str,
        method: str,
        metadata: dict[str, str] | None,
        deadline_secs: int,
        serialization_mode: int,
        session_id: int,
    ) -> None:
        self._conn = connection
        self._codec = codec
        self._service = service
        self._method = method
        self._metadata = metadata
        self._deadline_secs = deadline_secs
        self._serialization_mode = serialization_mode
        self._session_id = session_id
        self._driver: _CallDriver | None = None
        self._sent_end_stream = False
        self._trailer_read = False
        self._last_trailer: tuple[int, str] | None = None

    async def __aenter__(self) -> "IrohBidiChannel":
        await self._ensure_driver()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb) -> None:
        await self.close()
        if self._last_trailer is None:
            try:
                await self.wait_for_trailer()
            except Exception:
                pass

    async def _ensure_driver(self) -> _CallDriver:
        if self._driver is not None:
            return self._driver
        self._driver = await _CallDriver.acquire_streaming(self._conn, self._codec)
        try:
            await self._driver.send_header(
                service=self._service,
                method=self._method,
                deadline_secs=self._deadline_secs,
                serialization_mode=self._serialization_mode,
                metadata=self._metadata,
                session_id=self._session_id,
            )
        except BaseException:
            self._driver.discard()
            self._driver = None
            raise
        return self._driver

    async def send(self, msg: Any) -> None:
        if self._sent_end_stream:
            raise TransportError("channel is closed for sending")
        driver = await self._ensure_driver()
        try:
            await driver.send_request(msg, last=False)
        except BaseException:
            driver.discard()
            self._driver = None
            raise

    async def recv(self) -> Any:
        driver = await self._ensure_driver()
        while True:
            try:
                frame = await driver.recv_frame()
            except BaseException:
                driver.discard()
                self._driver = None
                raise
            if frame is None:
                raise ConnectionLostError("stream ended")
            payload, flags = frame
            if flags & TRAILER:
                self._trailer_read = True
                if not payload:
                    self._last_trailer = (StatusCode.OK, "")
                    driver.release()
                    self._driver = None
                    return None
                status = self._codec.decode(payload, RpcStatus)
                self._last_trailer = (status.code, status.message)
                if status.code != StatusCode.OK:
                    driver.discard()
                    self._driver = None
                    raise RpcError.from_status(
                        StatusCode(status.code),
                        status.message,
                        dict(zip(status.detailKeys, status.detailValues)),
                    )
                driver.release()
                self._driver = None
                return None
            if flags & ROW_SCHEMA:
                continue
            return driver.decode_response(payload, flags)

    async def close(self) -> None:
        """Signal end-of-requests. Sends an empty `END_STREAM` frame so
        the server-side dispatcher stops reading. Idempotent."""
        if self._sent_end_stream or self._driver is None:
            self._sent_end_stream = True
            return
        self._sent_end_stream = True
        try:
            await self._driver.send_end_stream()
        except Exception:
            pass

    async def wait_for_trailer(self) -> tuple[int, str]:
        if self._last_trailer is not None:
            return self._last_trailer
        driver = await self._ensure_driver()
        while True:
            try:
                frame = await driver.recv_frame()
            except BaseException:
                driver.discard()
                self._driver = None
                raise
            if frame is None:
                raise ConnectionLostError("stream ended before trailer")
            payload, flags = frame
            if flags & TRAILER:
                if not payload:
                    self._last_trailer = (StatusCode.OK, "")
                else:
                    status = self._codec.decode(payload, RpcStatus)
                    self._last_trailer = (status.code, status.message)
                driver.release()
                self._driver = None
                return self._last_trailer
