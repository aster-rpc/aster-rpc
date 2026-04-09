"""
aster.transport.iroh -- Iroh-based remote transport.

Spec reference: §8.3.1 (Transport protocol)

This module implements the Transport interface using Iroh QUIC streams.
Each RPC call opens a bidirectional stream, sends the StreamHeader frame,
then performs the appropriate read/write sequence for the RPC pattern.
"""

from __future__ import annotations

import asyncio
import uuid
from typing import TYPE_CHECKING, Any, AsyncIterator

import aster
from aster.codec import ForyCodec, ForyConfig
from aster.framing import (
    HEADER,
    TRAILER,
    COMPRESSED,
    ROW_SCHEMA,
    write_frame,
    read_frame,
)
from aster.protocol import StreamHeader, RpcStatus
from aster.status import StatusCode, RpcError
from aster.types import SerializationMode
from aster.transport.base import (
    Transport,
    BidiChannel,
    TransportError,
    ConnectionLostError,
)

if TYPE_CHECKING:
    import aster

import logging

logger = logging.getLogger(__name__)

# Keywords in exception messages that indicate a QUIC stream/connection reset.
_RESET_KEYWORDS = (
    "reset",
    "stream reset",
    "connection reset",
    "RESET_STREAM",
    "connection lost",
    "connection closed",
)


def _map_transport_exception(exc: Exception) -> Exception:
    """Map QUIC/Iroh transport exceptions to appropriate RpcError per spec S6.7.

    QUIC RESET_STREAM and connection-level resets are mapped to
    ``StatusCode.UNAVAILABLE`` so callers see a retriable error.
    """
    if isinstance(exc, RpcError):
        return exc
    msg = str(exc).lower()
    if any(kw in msg for kw in _RESET_KEYWORDS) or isinstance(exc, (ConnectionError, ConnectionLostError)):
        return RpcError(StatusCode.UNAVAILABLE, f"stream reset by peer: {exc}")
    return exc


# ── Helper functions ─────────────────────────────────────────────────────────


def _build_metadata(
    metadata: dict[str, str] | None,
) -> tuple[list[str], list[str]]:
    """Convert metadata dict to parallel key/value lists."""
    if not metadata:
        return [], []
    keys = list(metadata.keys())
    values = [metadata[k] for k in keys]
    return keys, values


def _extract_metadata(
    keys: list[str], values: list[str]
) -> dict[str, str]:
    """Convert parallel key/value lists to dict."""
    if not keys:
        return {}
    return dict(zip(keys, values))


async def _read_trailer(
    recv: "aster.IrohRecvStream",
) -> tuple[int, str]:
    """Read a TRAILER frame and extract status."""
    frame = await read_frame(recv)
    if frame is None:
        raise ConnectionLostError("stream ended before trailer")
    payload, flags = frame
    if not (flags & TRAILER):
        raise TransportError(f"expected TRAILER frame, got flags={flags:#x}")
    
    # Decode the RpcStatus
    codec = ForyCodec(mode=SerializationMode.XLANG)
    status = codec.decode(payload, RpcStatus)

    from aster.limits import validate_status_message
    return status.code, validate_status_message(status.message)


# ── IrohTransport ───────────────────────────────────────────────────────────


class IrohTransport(Transport):
    """Transport implementation using Iroh QUIC streams.

    This transport opens a bidirectional QUIC stream for each RPC call,
    writes the StreamHeader as the first frame, then performs the
    appropriate read/write sequence for each RPC pattern.

    Args:
        connection: The Iroh connection to use for RPC calls.
        codec: The ForyCodec instance for serialization. If None, a
            default codec with XLANG mode is used.
    """

    def __init__(
        self,
        connection: "aster.IrohConnection",
        codec: ForyCodec | None = None,
        fory_config: ForyConfig | None = None,
    ) -> None:
        self._conn = connection
        self._codec = codec or ForyCodec(
            mode=SerializationMode.XLANG,
            fory_config=fory_config,
        )

    async def close(self) -> None:
        """Close the underlying Iroh connection."""
        self._conn.close(0, b"normal close")

    # ── Unary ───────────────────────────────────────────────────────────────

    async def unary(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_epoch_ms: int = 0,
        serialization_mode: int = 0,

    ) -> Any:
        """Perform a unary RPC call over Iroh QUIC.

        Flow:
        1. Open bidirectional stream
        2. Write StreamHeader frame (HEADER flag)
        3. Write request payload frame
        4. Read response payload frame(s)
        5. Read trailer frame (TRAILER flag)
        """
        call_id = str(uuid.uuid4())
        send, recv = await self._conn.open_bi()

        try:
            # Build and write StreamHeader
            keys, values = _build_metadata(metadata)
            header = StreamHeader(
                service=service,
                method=method,
                version=1,

                call_id=call_id,
                deadline_epoch_ms=deadline_epoch_ms,
                serialization_mode=serialization_mode,
                metadata_keys=keys,
                metadata_values=values,
            )

            header_bytes = self._codec.encode(header)
            await write_frame(send, header_bytes, flags=HEADER)

            # Encode and write request
            payload, compressed = self._codec.encode_compressed(request)
            flags = COMPRESSED if compressed else 0
            await write_frame(send, payload, flags=flags)
            await send.finish()

            # Read response frames
            response_payload = None
            while True:
                frame = await read_frame(recv)
                if frame is None:
                    raise ConnectionLostError("stream ended before response")
                payload, flags = frame
                
                if flags & TRAILER:
                    # Trailer received - decode status
                    status = self._codec.decode(payload, RpcStatus)
                    if status.code != StatusCode.OK:
                        raise RpcError(
                            StatusCode(status.code),
                            status.message,
                            dict(zip(status.detail_keys, status.detail_values)),
                        )
                    break
                
                # Data frame - should be the response
                if response_payload is not None:
                    raise TransportError("received multiple response frames for unary RPC")
                
                compressed = bool(flags & COMPRESSED)
                response_payload = self._codec.decode_compressed(
                    payload, compressed
                )

            return response_payload

        except RpcError:
            try:
                recv.stop(1)
            except Exception:
                pass
            raise
        except Exception as e:
            # Ensure stream is stopped on error
            try:
                recv.stop(1)
            except Exception:
                pass
            raise _map_transport_exception(e) from e
        finally:
            # Close send side (if not already closed by finish())
            try:
                await send.finish()
            except Exception:
                pass

    # ── Server Streaming ───────────────────────────────────────────────────

    def server_stream(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_epoch_ms: int = 0,
        serialization_mode: int = 0,

    ) -> AsyncIterator[Any]:
        """Initiate a server-streaming RPC.

        Flow:
        1. Open bidirectional stream
        2. Write StreamHeader frame (HEADER flag)
        3. Write request payload frame
        4. Read N response payload frames until trailer
        """
        return self._server_stream_impl(
            service, method, request, metadata,
            deadline_epoch_ms, serialization_mode,
        )

    async def _server_stream_impl(
        self,
        service: str,
        method: str,
        request: Any,
        metadata: dict[str, str] | None,
        deadline_epoch_ms: int,
        serialization_mode: int,
    ) -> AsyncIterator[Any]:
        call_id = str(uuid.uuid4())
        send, recv = await self._conn.open_bi()

        try:
            # Write StreamHeader
            keys, values = _build_metadata(metadata)
            header = StreamHeader(
                service=service,
                method=method,
                version=1,

                call_id=call_id,
                deadline_epoch_ms=deadline_epoch_ms,
                serialization_mode=serialization_mode,
                metadata_keys=keys,
                metadata_values=values,
            )
            header_bytes = self._codec.encode(header)
            await write_frame(send, header_bytes, flags=HEADER)

            # Write request
            payload, compressed = self._codec.encode_compressed(request)
            flags = COMPRESSED if compressed else 0
            await write_frame(send, payload, flags=flags)
            await send.finish()

            # Read response frames until trailer
            while True:
                frame = await read_frame(recv)
                if frame is None:
                    raise ConnectionLostError("stream ended before trailer")
                payload, flags = frame

                if flags & TRAILER:
                    status = self._codec.decode(payload, RpcStatus)
                    if status.code != StatusCode.OK:
                        raise RpcError(
                            StatusCode(status.code),
                            status.message,
                            dict(zip(status.detail_keys, status.detail_values)),
                        )
                    break

                # §5.5.2: Consume ROW_SCHEMA frame (first frame in ROW mode)
                if flags & ROW_SCHEMA:
                    continue

                compressed = bool(flags & COMPRESSED)
                yield self._codec.decode_compressed(payload, compressed)

        except RpcError:
            try:
                recv.stop(1)
            except Exception:
                pass
            raise
        except Exception as e:
            try:
                recv.stop(1)
            except Exception:
                pass
            raise _map_transport_exception(e) from e

    # ── Client Streaming ───────────────────────────────────────────────────

    async def client_stream(
        self,
        service: str,
        method: str,
        requests: AsyncIterator[Any],
        *,
        metadata: dict[str, str] | None = None,
        deadline_epoch_ms: int = 0,
        serialization_mode: int = 0,

    ) -> Any:
        """Perform a client-streaming RPC.

        Flow:
        1. Open bidirectional stream
        2. Write StreamHeader frame (HEADER flag)
        3. Write N request payload frames
        4. Finish stream
        5. Read response frame + trailer
        """
        call_id = str(uuid.uuid4())
        send, recv = await self._conn.open_bi()

        try:
            # Write StreamHeader
            keys, values = _build_metadata(metadata)
            header = StreamHeader(
                service=service,
                method=method,
                version=1,

                call_id=call_id,
                deadline_epoch_ms=deadline_epoch_ms,
                serialization_mode=serialization_mode,
                metadata_keys=keys,
                metadata_values=values,
            )
            header_bytes = self._codec.encode(header)
            await write_frame(send, header_bytes, flags=HEADER)

            # Stream request messages
            async for request in requests:
                payload, compressed = self._codec.encode_compressed(request)
                flags = COMPRESSED if compressed else 0
                await write_frame(send, payload, flags=flags)

            # Signal end of input
            await send.finish()

            # Read response
            response_payload = None
            while True:
                frame = await read_frame(recv)
                if frame is None:
                    raise ConnectionLostError("stream ended before response")
                payload, flags = frame

                if flags & TRAILER:
                    status = self._codec.decode(payload, RpcStatus)
                    if status.code != StatusCode.OK:
                        raise RpcError(
                            StatusCode(status.code),
                            status.message,
                            dict(zip(status.detail_keys, status.detail_values)),
                        )
                    break

                if response_payload is not None:
                    raise TransportError("received multiple response frames")
                compressed = bool(flags & COMPRESSED)
                response_payload = self._codec.decode_compressed(payload, compressed)

            return response_payload

        except RpcError:
            try:
                recv.stop(1)
            except Exception:
                pass
            raise
        except Exception as e:
            try:
                recv.stop(1)
            except Exception:
                pass
            raise _map_transport_exception(e) from e

    # ── Bidirectional Streaming ───────────────────────────────────────────

    def bidi_stream(
        self,
        service: str,
        method: str,
        *,
        metadata: dict[str, str] | None = None,
        deadline_epoch_ms: int = 0,
        serialization_mode: int = 0,

    ) -> BidiChannel:
        """Initiate a bidirectional-streaming RPC."""
        return IrohBidiChannel(
            connection=self._conn,
            codec=self._codec,
            service=service,
            method=method,
            metadata=metadata,
            deadline_epoch_ms=deadline_epoch_ms,
            serialization_mode=serialization_mode,
        )


# ── IrohBidiChannel ─────────────────────────────────────────────────────────


class IrohBidiChannel(BidiChannel):
    """BidiChannel implementation for Iroh QUIC streams.

    Manages a bidirectional stream for bidirectional or client-streaming RPCs.
    Supports the async context manager protocol for convenient resource management.
    """

    def __init__(
        self,
        connection: "aster.IrohConnection",
        codec: ForyCodec,
        service: str,
        method: str,
        metadata: dict[str, str] | None,
        deadline_epoch_ms: int,
        serialization_mode: int,
    ) -> None:
        self._conn = connection
        self._codec = codec
        self._service = service
        self._method = method
        self._metadata = metadata
        self._deadline_epoch_ms = deadline_epoch_ms
        self._serialization_mode = serialization_mode
        self._send: "aster.IrohSendStream | None" = None
        self._recv: "aster.IrohRecvStream | None" = None
        self._header_written = False
        self._closed = False
        self._trailer_read = False
        self._last_trailer: tuple[int, str] | None = None

    async def __aenter__(self) -> "IrohBidiChannel":
        """Enter the async context manager, opening the stream."""
        await self._ensure_stream()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb) -> None:
        """Exit the async context manager, closing the stream."""
        await self.close()
        if self._last_trailer is None:
            try:
                await self.wait_for_trailer()
            except Exception:
                pass  # Stream may already be closed

    async def _ensure_stream(self) -> tuple["aster.IrohSendStream", "aster.IrohRecvStream"]:
        """Lazily open the bidirectional stream and write header."""
        if self._send is not None and self._recv is not None:
            return self._send, self._recv

        call_id = str(uuid.uuid4())
        self._send, self._recv = await self._conn.open_bi()

        # Write StreamHeader
        keys, values = _build_metadata(self._metadata)
        header = StreamHeader(
            service=self._service,
            method=self._method,
            version=1,
            call_id=call_id,
            deadline_epoch_ms=self._deadline_epoch_ms,
            serialization_mode=self._serialization_mode,
            metadata_keys=keys,
            metadata_values=values,
        )
        header_bytes = self._codec.encode(header)
        await write_frame(self._send, header_bytes, flags=HEADER)
        self._header_written = True

        return self._send, self._recv

    async def send(self, msg: Any) -> None:
        """Send a message on the stream."""
        if self._closed:
            raise TransportError("channel is closed")
        
        send, _ = await self._ensure_stream()
        payload, compressed = self._codec.encode_compressed(msg)
        flags = COMPRESSED if compressed else 0
        await write_frame(send, payload, flags=flags)

    async def recv(self) -> Any:
        """Receive the next message from the stream."""
        _, recv = await self._ensure_stream()

        while True:
            frame = await read_frame(recv)
            if frame is None:
                raise ConnectionLostError("stream ended")
            
            payload, flags = frame

            if flags & TRAILER:
                self._trailer_read = True
                status = self._codec.decode(payload, RpcStatus)
                self._last_trailer = (status.code, status.message)
                if status.code != StatusCode.OK:
                    raise RpcError(
                        StatusCode(status.code),
                        status.message,
                        dict(zip(status.detail_keys, status.detail_values)),
                    )
                raise ConnectionLostError("stream ended after trailer")

            # §5.5.2: Consume ROW_SCHEMA frame (first frame in ROW mode)
            if flags & ROW_SCHEMA:
                continue

            compressed = bool(flags & COMPRESSED)
            return self._codec.decode_compressed(payload, compressed)

    async def close(self) -> None:
        """Close the sending side of the stream."""
        if self._closed:
            return
        
        self._closed = True
        
        if self._send is not None:
            try:
                await self._send.finish()
            except Exception:
                pass

    async def wait_for_trailer(self) -> tuple[int, str]:
        """Wait for the trailing status frame."""
        if self._last_trailer is not None:
            return self._last_trailer
        
        _, recv = await self._ensure_stream()

        while True:
            frame = await read_frame(recv)
            if frame is None:
                raise ConnectionLostError("stream ended before trailer")
            
            payload, flags = frame

            if flags & TRAILER:
                status = self._codec.decode(payload, RpcStatus)
                self._last_trailer = (status.code, status.message)
                return self._last_trailer
