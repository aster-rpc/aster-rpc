"""
aster.transport.local — In-process transport using asyncio.Queue.

Spec reference: §8.3.2 (LocalTransport interceptors), §8.3.3 (wire-compatible mode)

This module implements the Transport interface for in-process communication
using asyncio.Queue. It supports:

- Full interceptor chain execution (required per spec §8.3.2)
- wire_compatible mode for Fory round-trip testing (§8.3.3)
- All four RPC patterns: unary, server_stream, client_stream, bidi_stream
"""

from __future__ import annotations

import asyncio
import uuid
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, AsyncIterator, Callable

from aster_python.aster.codec import ForyCodec, ForyConfig
from aster_python.aster.framing import HEADER, TRAILER, COMPRESSED, write_frame, read_frame
from aster_python.aster.protocol import StreamHeader, RpcStatus
from aster_python.aster.status import StatusCode, RpcError
from aster_python.aster.types import SerializationMode
from aster_python.aster.transport.base import (
    Transport,
    BidiChannel,
    TransportError,
    ConnectionLostError,
)

if TYPE_CHECKING:
    from aster_python.aster.interceptors.base import Interceptor


# ── Minimal CallContext for Phase 3 (replaced by full implementation in Phase 7) ─────


@dataclass
class CallContext:
    """Call context for interceptor chain.

    This is a minimal implementation for Phase 3. The full implementation
    with more fields will be added in Phase 7 (Interceptors).
    """
    service: str
    method: str
    call_id: str
    session_id: str | None
    peer: str | None
    metadata: dict[str, str]
    deadline: float | None
    is_streaming: bool


# ── Call request/response types for internal queuing ─────────────────────────


@dataclass
class CallRequest:
    """Internal representation of an RPC call request for LocalTransport."""
    service: str
    method: str
    request: Any
    metadata: dict[str, str] | None
    deadline_epoch_ms: int
    serialization_mode: int
    contract_id: str
    response_queue: asyncio.Queue[Any]
    error_queue: asyncio.Queue[Exception]
    trailer_queue: asyncio.Queue[tuple[int, str]]


@dataclass
class LocalBidiMessage:
    """Internal message type for local bidirectional channels."""
    is_send: bool
    data: Any = None
    is_close: bool = False


# ── LocalBidiChannel ────────────────────────────────────────────────────────


class LocalBidiChannel(BidiChannel):
    """BidiChannel implementation for LocalTransport.

    Uses asyncio.Queue for send/receive within the same process.
    Supports the async context manager protocol for convenient resource management.
    """

    def __init__(
        self,
        service: str,
        method: str,
        metadata: dict[str, str] | None,
        deadline_epoch_ms: int,
        serialization_mode: int,
        contract_id: str,
        send_queue: asyncio.Queue[LocalBidiMessage],
        recv_queue: asyncio.Queue[LocalBidiMessage],
        trailer_queue: asyncio.Queue[tuple[int, str]],
        codec: ForyCodec,
    ) -> None:
        self._service = service
        self._method = method
        self._metadata = metadata
        self._deadline_epoch_ms = deadline_epoch_ms
        self._serialization_mode = serialization_mode
        self._contract_id = contract_id
        self._send_queue = send_queue
        self._recv_queue = recv_queue
        self._trailer_queue = trailer_queue
        self._codec = codec
        self._closed = False
        self._entered = False
        self._trailer_read = False
        self._last_trailer: tuple[int, str] | None = None

    async def __aenter__(self) -> "LocalBidiChannel":
        """Enter the async context manager."""
        self._entered = True
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb) -> None:
        """Exit the async context manager, closing the channel."""
        await self.close()

    async def send(self, msg: Any) -> None:
        """Send a message on the channel."""
        if self._closed:
            raise TransportError("channel is closed")
        await self._send_queue.put(LocalBidiMessage(is_send=True, data=msg))

    async def recv(self) -> Any:
        """Receive the next message from the channel."""
        msg = await self._recv_queue.get()
        
        if msg.is_close:
            raise ConnectionLostError("channel closed by peer")
        
        if isinstance(msg.data, tuple) and len(msg.data) == 2:
            # It's a trailer
            code, message = msg.data
            self._last_trailer = (code, message)
            self._trailer_read = True
            if code != StatusCode.OK:
                raise RpcError(StatusCode(code), message)
            raise ConnectionLostError("stream ended after trailer")
        
        return msg.data

    async def close(self) -> None:
        """Close the sending side of the channel."""
        if self._closed:
            return
        self._closed = True
        await self._send_queue.put(LocalBidiMessage(is_send=False, is_close=True))

    async def wait_for_trailer(self) -> tuple[int, str]:
        """Wait for the trailing status frame."""
        if self._last_trailer is not None:
            return self._last_trailer
        
        msg = await self._trailer_queue.get()
        self._last_trailer = msg
        return msg


# ── LocalTransport ──────────────────────────────────────────────────────────


class LocalTransport(Transport):
    """In-process transport using asyncio.Queue.

    This transport dispatches RPC calls directly to service handlers within
    the same process, bypassing network I/O. It is useful for:

    - Unit testing of RPC services
    - Local development without network dependencies
    - wire_compatible mode for serialization testing

    Key features:
    - Full interceptor chain execution (not skippable per spec §8.3.2)
    - wire_compatible mode for catching missing @fory_tag decorators (§8.3.3)

    Args:
        handler_registry: Callable that looks up handlers by (service, method).
            Expected signature: (service: str, method: str) -> tuple[handler, types]
            where handler is a callable and types is a list of type classes.
        codec: The ForyCodec instance for serialization.
        wire_compatible: If True, forces wire-compatible encoding even for local
            calls. This exercises the full serialization pipeline and will catch
            missing @fory_tag decorators on types. Defaults to False.
        interceptors: List of Interceptor instances to run on every call.
    """

    def __init__(
        self,
        handler_registry: Callable[
            [str, str],
            tuple[Callable[..., Any], list[type], str],  # handler, types, pattern
        ],
        codec: ForyCodec | None = None,
        fory_config: ForyConfig | None = None,
        wire_compatible: bool = False,
        interceptors: list["Interceptor"] | None = None,
    ) -> None:
        self._registry = handler_registry
        self._codec = codec or ForyCodec(
            mode=SerializationMode.XLANG,
            fory_config=fory_config,
        )
        self._wire_compatible = wire_compatible
        self._interceptors = interceptors or []

    async def close(self) -> None:
        """LocalTransport doesn't hold network resources, nothing to close."""
        pass

    async def _build_context(
        self,
        service: str,
        method: str,
        metadata: dict[str, str] | None,
        deadline_epoch_ms: int,
    ) -> CallContext:
        """Build a CallContext for interceptor chain."""
        return CallContext(
            service=service,
            method=method,
            call_id=str(uuid.uuid4()),
            session_id=None,
            peer=None,
            metadata=metadata or {},
            deadline=deadline_epoch_ms,
            is_streaming=False,
        )

    def _get_serialized_bytes(self, obj: Any, wire_compatible: bool) -> bytes:
        """Serialize an object, optionally forcing wire-compatible mode."""
        if wire_compatible:
            # In wire_compatible mode, encode through the codec to catch
            # serialization issues (missing tags, etc.)
            return self._codec.encode(obj)
        return self._codec.encode(obj)

    def _deserialize(
        self, data: bytes, expected_type: type, wire_compatible: bool
    ) -> Any:
        """Deserialize bytes, optionally using wire-compatible mode."""
        if wire_compatible:
            return self._codec.decode(data, expected_type)
        return self._codec.decode(data, expected_type)

    # ── Unary ─────────────────────────────────────────────────────────────

    async def unary(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_epoch_ms: int = 0,
        serialization_mode: int = 0,
        contract_id: str = "",
    ) -> Any:
        """Perform a unary RPC call (in-process).

        The call flows through:
        1. Handler lookup
        2. Request encoding (if wire_compatible)
        3. Interceptor chain (on_request)
        4. Handler invocation
        5. Response decoding (if wire_compatible)
        6. Interceptor chain (on_response)
        """
        handler, types, pattern = self._registry(service, method)
        
        if pattern not in ("unary", "server_stream", "client_stream", "bidi_stream"):
            raise TransportError(f"unexpected RPC pattern: {pattern}")

        # Build context for interceptors
        ctx = await self._build_context(
            service, method, metadata, deadline_epoch_ms
        )
        
        # Serialize request if wire_compatible
        request_bytes = None
        if self._wire_compatible and types:
            request_bytes = self._get_serialized_bytes(request, True)
        
        # Run request through interceptor chain
        for interceptor in self._interceptors:
            request = await interceptor.on_request(ctx, request)
        
        # Invoke handler
        try:
            response = handler(request)
            
            # Handle coroutines
            if asyncio.iscoroutine(response):
                response = await response
                
        except RpcError:
            # Re-raise RpcError after running error interceptors
            for interceptor in reversed(self._interceptors):
                result = await interceptor.on_error(ctx, RpcError(StatusCode.UNKNOWN, "handler error"))
                if result is None:
                    break
            raise
        except Exception as e:
            err = RpcError(StatusCode.UNKNOWN, str(e))
            for interceptor in reversed(self._interceptors):
                result = await interceptor.on_error(ctx, err)
                if result is None:
                    break
            raise

        # Run response through interceptor chain
        for interceptor in self._interceptors:
            response = await interceptor.on_response(ctx, response)

        # Serialize response if wire_compatible
        if self._wire_compatible and response is not None:
            response = self._deserialize(
                self._get_serialized_bytes(response, True),
                type(response),
                True
            )

        return response

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
        contract_id: str = "",
    ) -> AsyncIterator[Any]:
        """Initiate a server-streaming RPC (in-process)."""
        return self._server_stream_impl(
            service, method, request, metadata,
            deadline_epoch_ms, serialization_mode, contract_id,
        )

    async def _server_stream_impl(
        self,
        service: str,
        method: str,
        request: Any,
        metadata: dict[str, str] | None,
        deadline_epoch_ms: int,
        serialization_mode: int,
        contract_id: str,
    ) -> AsyncIterator[Any]:
        handler, types, pattern = self._registry(service, method)
        
        ctx = await self._build_context(
            service, method, metadata, deadline_epoch_ms
        )
        
        # Run request through interceptor chain
        for interceptor in self._interceptors:
            request = await interceptor.on_request(ctx, request)

        try:
            response_iter = handler(request)
            
            if asyncio.iscoroutine(response_iter):
                response_iter = await response_iter
            
            async for item in response_iter:
                # Run each item through interceptor chain
                for interceptor in self._interceptors:
                    item = await interceptor.on_response(ctx, item)
                
                # Serialize if wire_compatible
                if self._wire_compatible:
                    item = self._deserialize(
                        self._get_serialized_bytes(item, True),
                        type(item),
                        True
                    )
                
                yield item
                
        except Exception as e:
            err = RpcError(StatusCode.UNKNOWN, str(e))
            for interceptor in reversed(self._interceptors):
                result = await interceptor.on_error(ctx, err)
                if result is None:
                    break
            raise

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
        contract_id: str = "",
    ) -> Any:
        """Perform a client-streaming RPC (in-process)."""
        handler, types, pattern = self._registry(service, method)
        
        ctx = await self._build_context(
            service, method, metadata, deadline_epoch_ms
        )
        
        # Collect requests (could be streaming in a real network scenario,
        # but for LocalTransport we collect them first)
        collected: list[Any] = []
        async for request in requests:
            # Run each request through interceptor chain
            for interceptor in self._interceptors:
                request = await interceptor.on_request(ctx, request)
            collected.append(request)

        try:
            response = handler(collected)
            
            if asyncio.iscoroutine(response):
                response = await response
                
        except Exception as e:
            err = RpcError(StatusCode.UNKNOWN, str(e))
            for interceptor in reversed(self._interceptors):
                result = await interceptor.on_error(ctx, err)
                if result is None:
                    break
            raise

        # Run response through interceptor chain
        for interceptor in self._interceptors:
            response = await interceptor.on_response(ctx, response)

        # Serialize if wire_compatible
        if self._wire_compatible and response is not None:
            response = self._deserialize(
                self._get_serialized_bytes(response, True),
                type(response),
                True
            )

        return response

    # ── Bidirectional Streaming ───────────────────────────────────────────

    def bidi_stream(
        self,
        service: str,
        method: str,
        *,
        metadata: dict[str, str] | None = None,
        deadline_epoch_ms: int = 0,
        serialization_mode: int = 0,
        contract_id: str = "",
    ) -> BidiChannel:
        """Initiate a bidirectional-streaming RPC (in-process)."""
        send_queue: asyncio.Queue[LocalBidiMessage] = asyncio.Queue()
        recv_queue: asyncio.Queue[LocalBidiMessage] = asyncio.Queue()
        trailer_queue: asyncio.Queue[tuple[int, str]] = asyncio.Queue()

        # Create the bidi channel
        channel = LocalBidiChannel(
            service=service,
            method=method,
            metadata=metadata,
            deadline_epoch_ms=deadline_epoch_ms,
            serialization_mode=serialization_mode,
            contract_id=contract_id,
            send_queue=send_queue,
            recv_queue=recv_queue,
            trailer_queue=trailer_queue,
            codec=self._codec,
        )

        # Start the handler task
        asyncio.create_task(
            self._bidi_handler_task(
                service, method, metadata, deadline_epoch_ms,
                send_queue, recv_queue, trailer_queue,
            )
        )

        return channel

    async def _bidi_handler_task(
        self,
        service: str,
        method: str,
        metadata: dict[str, str] | None,
        deadline_epoch_ms: int,
        send_queue: asyncio.Queue[LocalBidiMessage],
        recv_queue: asyncio.Queue[LocalBidiMessage],
        trailer_queue: asyncio.Queue[tuple[int, str]],
    ) -> None:
        """Handle bidi stream messages in a background task."""
        handler, types, pattern = self._registry(service, method)
        
        ctx = await self._build_context(
            service, method, metadata, deadline_epoch_ms
        )

        try:
            # Get the async generator handler
            response_iter = handler(None)  # Bidi handlers receive context
            
            if asyncio.iscoroutine(response_iter):
                response_iter = await response_iter
            
            # Message loop — asyncio.Queue is NOT an async iterator,
            # so we use get() in a loop instead.
            while True:
                try:
                    msg = await asyncio.wait_for(send_queue.get(), timeout=0.1)
                except asyncio.TimeoutError:
                    # Check if handler has more items to yield (non-blocking peek via try)
                    continue
                
                if msg.is_close:
                    break
                
                if msg.is_send:
                    # Run request through interceptor chain
                    for interceptor in self._interceptors:
                        msg.data = await interceptor.on_request(ctx, msg.data)
                    
                    # Send to handler
                    try:
                        item = await response_iter.__anext__()
                        # Run response through interceptor chain
                        for interceptor in self._interceptors:
                            item = await interceptor.on_response(ctx, item)
                        
                        # Serialize if wire_compatible
                        if self._wire_compatible:
                            item = self._deserialize(
                                self._get_serialized_bytes(item, True),
                                type(item),
                                True
                            )
                        
                        await recv_queue.put(LocalBidiMessage(is_send=True, data=item))
                    except StopAsyncIteration:
                        # Handler is done
                        break
            
            # Signal end of stream
            await recv_queue.put(LocalBidiMessage(is_send=False, is_close=True))
            await trailer_queue.put((StatusCode.OK, ""))
            
        except Exception as e:
            err = RpcError(StatusCode.UNKNOWN, str(e))
            for interceptor in reversed(self._interceptors):
                result = await interceptor.on_error(ctx, err)
                if result is None:
                    break
            await recv_queue.put(LocalBidiMessage(is_send=False, is_close=True))
            await trailer_queue.put((StatusCode.UNKNOWN, str(e)))


# ── In-memory frame streams (for testing wire protocol) ─────────────────────


class MemSendStream:
    """In-memory async send stream for testing framing."""

    def __init__(self) -> None:
        self.buf = bytearray()
        self._finished = False

    async def write_all(self, data: bytes) -> None:
        self.buf.extend(data)

    async def finish(self) -> None:
        self._finished = True


class MemRecvStream:
    """In-memory async recv stream for testing framing."""

    def __init__(self, data: bytes) -> None:
        self._data = memoryview(data)
        self._pos = 0
        self._stopped = False
        self._stop_code: int | None = None

    async def read_exact(self, n: int) -> bytes:
        if self._stopped:
            raise EOFError("stream stopped")
        if self._pos + n > len(self._data):
            raise EOFError("end of stream")
        chunk = bytes(self._data[self._pos : self._pos + n])
        self._pos += n
        return chunk

    async def read(self, max_len: int) -> bytes | None:
        if self._stopped:
            return None
        remaining = len(self._data) - self._pos
        n = min(max_len, remaining)
        if n == 0:
            return None
        chunk = bytes(self._data[self._pos : self._pos + n])
        self._pos += n
        return chunk

    def stop(self, code: int) -> None:
        self._stopped = True
        self._stop_code = code


class LocalConnection:
    """A paired connection for LocalTransport testing.

    Provides the same interface as IrohConnection but uses queues.
    """

    def __init__(self) -> None:
        self._bi_queue: asyncio.Queue[tuple[MemSendStream, MemRecvStream]] = asyncio.Queue()
        self._closed = False

    async def open_bi(self) -> tuple[MemSendStream, MemRecvStream]:
        """Open a bidirectional stream."""
        # For local testing, we create paired send/recv streams
        # that share the same buffer
        send = MemSendStream()
        recv = MemRecvStream(b"")  # Will be updated when data is written
        return send, recv

    async def accept_bi(self) -> tuple[MemSendStream, MemRecvStream]:
        """Accept a bidirectional stream."""
        return await self._bi_queue.get()

    def close(self, code: int, reason: bytes) -> None:
        self._closed = True

    def remote_id(self) -> str:
        return "local-connection"
