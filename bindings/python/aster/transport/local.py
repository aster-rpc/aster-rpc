"""
aster.transport.local -- In-process transport using asyncio.Queue.

Spec reference: §8.3.2 (LocalTransport interceptors), §8.3.3 (wire-compatible mode)

This module implements the Transport interface for in-process communication
using asyncio.Queue. It supports:

- Full interceptor chain execution (required per spec §8.3.2)
- wire_compatible mode for Fory round-trip testing (§8.3.3)
- All four RPC patterns: unary, server_stream, client_stream, bidi_stream
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, AsyncIterator, Callable

from aster.codec import ForyCodec, ForyConfig
from aster.interceptors.base import (
    CallContext,
    apply_error_interceptors,
    apply_request_interceptors,
    apply_response_interceptors,
    build_call_context,
    normalize_error,
)
from aster.interceptors.deadline import DeadlineInterceptor
from aster.status import StatusCode, RpcError
from aster.rpc_types import SerializationMode
from aster.transport.base import (
    Transport,
    BidiChannel,
    TransportError,
    ConnectionLostError,
)

if TYPE_CHECKING:
    from aster.interceptors.base import Interceptor


# ── Call request/response types for internal queuing ─────────────────────────


@dataclass
class CallRequest:
    """Internal representation of an RPC call request for LocalTransport."""
    service: str
    method: str
    request: Any
    metadata: dict[str, str] | None
    deadline_secs: int
    serialization_mode: int
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
        deadline_secs: int,
        serialization_mode: int,

        send_queue: asyncio.Queue[LocalBidiMessage],
        recv_queue: asyncio.Queue[LocalBidiMessage],
        trailer_queue: asyncio.Queue[tuple[int, str]],
        codec: ForyCodec,
    ) -> None:
        self._service = service
        self._method = method
        self._metadata = metadata
        self._deadline_secs = deadline_secs
        self._serialization_mode = serialization_mode

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
                raise RpcError.from_status(StatusCode(code), message)
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
    - wire_compatible mode for catching missing @wire_type decorators (§8.3.3)

    Args:
        handler_registry: Callable that looks up handlers by (service, method).
            Expected signature: (service: str, method: str) -> tuple[handler, types]
            where handler is a callable and types is a list of type classes.
        codec: The ForyCodec instance for serialization.
        wire_compatible: If True, forces wire-compatible encoding even for local
            calls. This exercises the full serialization pipeline and will catch
            missing @wire_type decorators on types. Defaults to False.
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

    def _deadline_timeout(self, ctx: CallContext) -> float | None:
        for interceptor in self._interceptors:
            if isinstance(interceptor, DeadlineInterceptor):
                timeout = interceptor.timeout_seconds(ctx)
                if timeout is not None:
                    return timeout
        return None

    async def _await_with_deadline(self, ctx: CallContext, awaitable: Any) -> Any:
        timeout = self._deadline_timeout(ctx)
        if timeout is None:
            return await awaitable
        async with asyncio.timeout(timeout):
            return await awaitable

    async def _build_context(
        self,
        service: str,
        method: str,
        metadata: dict[str, str] | None,
        deadline_secs: int,
        *,
        is_streaming: bool = False,
    ) -> CallContext:
        """Build a CallContext for interceptor chain."""
        return build_call_context(
            service=service,
            method=method,
            metadata=metadata,
            deadline_secs=deadline_secs,
            is_streaming=is_streaming,
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
        deadline_secs: int = 0,
        serialization_mode: int = 0,

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
            service, method, metadata, deadline_secs
        )
        
        # Serialize request if wire_compatible
        if self._wire_compatible and types:
            self._get_serialized_bytes(request, True)
        
        # Run request through interceptor chain
        request = await apply_request_interceptors(self._interceptors, ctx, request)
        
        # Invoke handler
        try:
            response = handler(request)
            
            # Handle coroutines
            if asyncio.iscoroutine(response):
                response = await self._await_with_deadline(ctx, response)
                
        except RpcError as exc:
            maybe_error = await apply_error_interceptors(self._interceptors, ctx, exc)
            if maybe_error is not None:
                raise maybe_error
            raise
        except Exception as e:
            err = normalize_error(e)
            maybe_error = await apply_error_interceptors(self._interceptors, ctx, err)
            if maybe_error is not None:
                raise maybe_error
            raise

        # Run response through interceptor chain
        response = await apply_response_interceptors(self._interceptors, ctx, response)

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
        deadline_secs: int = 0,
        serialization_mode: int = 0,

    ) -> AsyncIterator[Any]:
        """Initiate a server-streaming RPC (in-process)."""
        return self._server_stream_impl(
            service, method, request, metadata,
            deadline_secs, serialization_mode,
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
        handler, types, pattern = self._registry(service, method)
        
        ctx = await self._build_context(
            service, method, metadata, deadline_secs, is_streaming=True
        )
        
        # Run request through interceptor chain
        request = await apply_request_interceptors(self._interceptors, ctx, request)

        try:
            response_iter = handler(request)
            
            if asyncio.iscoroutine(response_iter):
                response_iter = await self._await_with_deadline(ctx, response_iter)
            
            async for item in response_iter:
                # Run each item through interceptor chain
                for interceptor in self._interceptors:
                    item = await apply_response_interceptors(self._interceptors, ctx, item)
                
                # Serialize if wire_compatible
                if self._wire_compatible:
                    item = self._deserialize(
                        self._get_serialized_bytes(item, True),
                        type(item),
                        True
                    )
                
                yield item
                
        except Exception as e:
            err = normalize_error(e)
            maybe_error = await apply_error_interceptors(self._interceptors, ctx, err)
            if maybe_error is not None:
                raise maybe_error
            raise

    # ── Client Streaming ───────────────────────────────────────────────────

    async def client_stream(
        self,
        service: str,
        method: str,
        requests: AsyncIterator[Any],
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int = 0,

    ) -> Any:
        """Perform a client-streaming RPC (in-process)."""
        handler, types, pattern = self._registry(service, method)
        
        ctx = await self._build_context(
            service, method, metadata, deadline_secs, is_streaming=True
        )
        
        # Collect requests (could be streaming in a real network scenario,
        # but for LocalTransport we collect them first)
        collected: list[Any] = []
        async for request in requests:
            # Run each request through interceptor chain
            request = await apply_request_interceptors(self._interceptors, ctx, request)
            collected.append(request)

        try:
            response = handler(collected)
            
            if asyncio.iscoroutine(response):
                response = await self._await_with_deadline(ctx, response)
                
        except Exception as e:
            err = normalize_error(e)
            maybe_error = await apply_error_interceptors(self._interceptors, ctx, err)
            if maybe_error is not None:
                raise maybe_error
            raise

        # Run response through interceptor chain
        response = await apply_response_interceptors(self._interceptors, ctx, response)

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
        deadline_secs: int = 0,
        serialization_mode: int = 0,

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
            deadline_secs=deadline_secs,
            serialization_mode=serialization_mode,
            send_queue=send_queue,
            recv_queue=recv_queue,
            trailer_queue=trailer_queue,
            codec=self._codec,
        )

        # Start the handler task
        asyncio.create_task(
            self._bidi_handler_task(
                service, method, metadata, deadline_secs,
                send_queue, recv_queue, trailer_queue,
            )
        )

        return channel

    async def _bidi_handler_task(
        self,
        service: str,
        method: str,
        metadata: dict[str, str] | None,
        deadline_secs: int,
        send_queue: asyncio.Queue[LocalBidiMessage],
        recv_queue: asyncio.Queue[LocalBidiMessage],
        trailer_queue: asyncio.Queue[tuple[int, str]],
    ) -> None:
        """Handle bidi stream messages in a background task."""
        handler, types, pattern = self._registry(service, method)
        
        ctx = await self._build_context(
            service, method, metadata, deadline_secs, is_streaming=True
        )

        try:
            # Get the async generator handler
            response_iter = handler(None)  # Bidi handlers receive context
            
            if asyncio.iscoroutine(response_iter):
                response_iter = await self._await_with_deadline(ctx, response_iter)
            
            # Message loop -- asyncio.Queue is NOT an async iterator,
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
                    msg.data = await apply_request_interceptors(self._interceptors, ctx, msg.data)
                    
                    # Send to handler
                    try:
                        item = await response_iter.__anext__()
                        # Run response through interceptor chain
                        item = await apply_response_interceptors(self._interceptors, ctx, item)
                        
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
            err = normalize_error(e)
            await apply_error_interceptors(self._interceptors, ctx, err)
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

