"""
aster.server -- Aster RPC server implementation.

Spec reference: §8.1 (Server API), §8.2 (Server accept loop)

This module implements the server-side of Aster RPC, including:
- Connection accept loop with per-connection task spawning
- Stream dispatch based on StreamHeader routing
- Handler execution for all RPC patterns
- Graceful shutdown with drain support
- Error handling with RpcStatus trailers
"""

from __future__ import annotations

import asyncio
import inspect
import logging
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, AsyncIterator

from aster.codec import ForyCodec, ForyConfig
from aster.framing import HEADER, TRAILER, COMPRESSED, CANCEL, ROW_SCHEMA, write_frame, read_frame
from aster.interceptors.base import (
    apply_error_interceptors,
    apply_request_interceptors,
    apply_response_interceptors,
    build_call_context,
    normalize_error,
)
from aster.json_codec import json_decode, json_encode, safe_decompress
from aster.limits import (
    LimitExceeded,
    MAX_METADATA_ENTRIES,
    validate_metadata,
)
from aster.logging import request_context
from aster.protocol import StreamHeader, RpcStatus
from aster.status import StatusCode, RpcError
from aster.rpc_types import RpcScope, SerializationMode
from aster.service import ServiceRegistry, ServiceInfo, MethodInfo

if TYPE_CHECKING:
    import aster
    from aster.peer_store import PeerAttributeStore

logger = logging.getLogger(__name__)


# ── Server Errors ────────────────────────────────────────────────────────────


class ServerError(Exception):
    """Base class for server errors."""
    pass


class ServiceNotFoundError(ServerError):
    """Raised when the requested service is not registered."""
    pass


class MethodNotFoundError(ServerError):
    """Raised when the requested method is not found in the service."""
    pass


class SerializationModeError(ServerError):
    """Raised when the requested serialization mode is not supported."""
    pass


# ── Connection Handler ────────────────────────────────────────────────────────


@dataclass
class ConnectionContext:
    """Context for a single client connection."""
    connection: "aster.IrohConnection"
    server: "Server"
    connection_task: asyncio.Task | None = None
    stream_tasks: set[asyncio.Task] = field(default_factory=set)
    draining: bool = False
    _closed: bool = field(default=False, repr=False)

    __hash__ = object.__hash__


# ── Server ──────────────────────────────────────────────────────────────────


class Server:
    """Aster RPC server.

    The server binds to an Iroh endpoint and dispatches incoming RPC calls
    to registered service handlers.

    Example usage::

        server = Server(
            endpoint=endpoint,
            services=[EchoService()],
        )
        await server.serve()

    For graceful shutdown::

        await server.drain(grace_period=10.0)  # Wait for in-flight calls
        await server.close()
    """

    def __init__(
        self,
        endpoint: "aster.IrohEndpoint",
        services: list[type] | ServiceRegistry | None = None,
        codec: ForyCodec | None = None,
        fory_config: ForyConfig | None = None,
        interceptors: list[Any] | None = None,
        max_concurrent_streams: int | None = None,
        registry: ServiceRegistry | None = None,
        owns_endpoint: bool = True,
        peer_store: "PeerAttributeStore | None" = None,
    ) -> None:
        """Initialize the server.

        Args:
            endpoint: The Iroh endpoint to accept connections on.
            services: Service classes (decorated with @service) or a ServiceRegistry.
            codec: The ForyCodec for serialization. Defaults to XLANG mode.
            fory_config: Optional configuration for implicitly created codecs.
            interceptors: List of interceptor instances to apply to all calls.
            max_concurrent_streams: Maximum concurrent QUIC streams per
                connection (i.e. per client peer). Streams beyond this
                limit are rejected at the QUIC layer. ``None`` = unlimited.
            registry: Optional ServiceRegistry. If not provided, creates one from services.
            owns_endpoint: If True (default), ``close()`` also closes the endpoint.
                Set False when the endpoint is managed externally (e.g. by
                :class:`AsterServer`).
        """
        self._endpoint = endpoint
        self._owns_endpoint = owns_endpoint
        self._peer_store = peer_store
        from aster.interceptors.deadline import DeadlineInterceptor
        if interceptors is not None:
            self._interceptors = list(interceptors)
        else:
            self._interceptors = [DeadlineInterceptor()]
        self._max_concurrent_streams = max_concurrent_streams
        self._service_instances: dict[tuple[str, int], Any] = {}
        # For session-scoped services we store the class (not an instance)
        self._service_classes: dict[tuple[str, int], type] = {}

        # Set up service registry first so we can collect types for the codec.
        if registry is not None:
            self._registry = registry
        elif isinstance(services, ServiceRegistry):
            self._registry = services
        elif services:
            self._registry = ServiceRegistry()
            for svc in services:
                service_class = svc if inspect.isclass(svc) else type(svc)
                info = self._registry.register(service_class)
                # Store the class for session-scoped services
                self._service_classes[(info.name, info.version)] = service_class
                if not inspect.isclass(svc):
                    self._service_instances[(info.name, info.version)] = svc
        else:
            self._registry = ServiceRegistry()

        # Build codec after registry is populated so service types are registered.
        if codec is not None:
            self._codec = codec
        else:
            from aster.client import _collect_service_types
            all_types: list[type] = []
            for svc_info in self._registry.get_all_services():
                cls = self._service_classes.get((svc_info.name, svc_info.version))
                if cls is not None:
                    for t in _collect_service_types(cls, svc_info):
                        if t not in all_types:
                            all_types.append(t)
            self._codec = ForyCodec(
                mode=SerializationMode.XLANG,
                types=all_types if all_types else None,
                fory_config=fory_config,
            )

        # Connection tracking
        self._connections: set[ConnectionContext] = set()
        self._connections_lock = asyncio.Lock()

        # Server state
        self._serving = False
        self._serve_task: asyncio.Task | None = None
        self._shutdown_event = asyncio.Event()

        for service_info in self._registry.get_all_services():
            key = (service_info.name, service_info.version)
            # Session-scoped services don't need a pre-existing instance
            if service_info.scoped == RpcScope.SESSION:
                continue
            if key not in self._service_instances:
                raise ServerError(
                    f"No implementation instance provided for service "
                    f"{service_info.name} v{service_info.version}"
                )

    @property
    def registry(self) -> ServiceRegistry:
        """The service registry used by this server."""
        return self._registry

    @property
    def endpoint(self) -> "aster.IrohEndpoint":
        """The Iroh endpoint this server is bound to."""
        return self._endpoint

    async def serve(self) -> None:
        """Start the server and accept connections.

        This method runs until shutdown is requested via drain() or close().
        """
        if self._serving:
            raise ServerError("server is already serving")

        self._serving = True
        self._serve_task = asyncio.current_task()
        self._shutdown_event.clear()

        logger.info("Server starting on %s", self._endpoint.endpoint_id())

        try:
            while self._serving:
                try:
                    # Accept a new connection
                    incoming = await self._endpoint.accept()
                    
                    # Create connection context
                    conn_ctx = ConnectionContext(
                        connection=incoming,
                        server=self,
                    )
                    
                    async with self._connections_lock:
                        self._connections.add(conn_ctx)
                    
                    # Start connection handler task
                    task = asyncio.create_task(self._handle_connection(conn_ctx))
                    conn_ctx.connection_task = task
                    
                except Exception as e:
                    if not self._serving:
                        break
                    if self._serving:
                        logger.error("Error accepting connection: %s", e)
                    continue

        finally:
            self._serving = False
            self._serve_task = None
            self._shutdown_event.set()

    async def handle_connection(self, incoming: Any) -> None:
        """Handle a connection accepted elsewhere (e.g. a shared multi-ALPN loop).

        Sets up the :class:`ConnectionContext` and tracks it for ``drain()`` /
        ``close()``, then delegates to the same per-stream handling used by
        :meth:`serve`. Call this when :class:`AsterServer` (or any higher
        dispatcher) owns the accept loop on a shared endpoint and routes
        connections by ALPN.
        """
        ctx = ConnectionContext(connection=incoming, server=self)
        async with self._connections_lock:
            self._connections.add(ctx)
        await self._handle_connection(ctx)

    async def _handle_connection(self, ctx: ConnectionContext) -> None:
        """Handle a single client connection.

        Accepts bidirectional streams and dispatches them to handlers.
        """
        from aster.health import get_connection_metrics
        conn_metrics = get_connection_metrics()
        conn_metrics.connection_opened()
        logger.debug("Connection opened from %s", ctx.connection.remote_id())

        try:
            while not ctx.draining:
                try:
                    # Accept a bidirectional stream
                    send, recv = await ctx.connection.accept_bi()
                    
                    # Check max concurrent streams limit
                    if self._max_concurrent_streams is not None:
                        async with self._connections_lock:
                            active_streams = len([t for t in ctx.stream_tasks if not t.done()])
                        if active_streams >= self._max_concurrent_streams:
                            logger.warning("Max concurrent streams reached, rejecting")
                            recv.stop(8)  # QUIC error code for capacity error
                            continue

                    # Start stream handler task
                    task = asyncio.create_task(
                        self._handle_stream(ctx, send, recv)
                    )
                    ctx.stream_tasks.add(task)
                    task.add_done_callback(ctx.stream_tasks.discard)

                except Exception as e:
                    msg = str(e)
                    if ctx.draining or "normal close" in msg or "code 0" in msg:
                        logger.debug("Connection closed: %s", msg)
                    else:
                        logger.error("Error accepting stream: %s", e)
                    break

        except asyncio.CancelledError:
            logger.debug("Connection handler cancelled")
        except Exception as e:
            logger.error("Connection error: %s", e)
        finally:
            # Clean up connection context
            async with self._connections_lock:
                self._connections.discard(ctx)

            # Cancel all stream tasks (copy to avoid set-changed-during-iteration)
            for task in list(ctx.stream_tasks):
                if not task.done():
                    task.cancel()
                    try:
                        await task
                    except asyncio.CancelledError:
                        pass

            conn_metrics.connection_closed()
            logger.debug("Connection closed")

    async def _handle_stream(self, ctx: ConnectionContext, send: Any, recv: Any) -> None:
        """Handle a single RPC stream.


        Reads the StreamHeader, dispatches to the appropriate handler,
        and manages the stream lifecycle.
        """
        from aster.health import get_connection_metrics as _gcm
        _stream_metrics = _gcm()
        _stream_metrics.stream_opened()
        try:
            # Read the StreamHeader (first frame with HEADER flag).
            # Use read_one_frame (single FFI crossing) if available,
            # falling back to read_frame (2 crossings) for non-Iroh streams.
            if hasattr(recv, 'read_one_frame'):
                frame = await recv.read_one_frame()
            else:
                frame = await read_frame(recv)
            if frame is None:
                logger.warning("Stream ended before header")
                return

            payload, flags = frame
            if not (flags & HEADER):
                logger.warning("First frame missing HEADER flag")
                await self._write_error_trailer(
                    send, StatusCode.INTERNAL, "First frame must have HEADER flag"
                )
                return

            # Decode the StreamHeader.
            # Sniff: JSON starts with '{' (0x7B), Fory XLANG starts with
            # 0x02. Accept both for cross-language interop.
            try:
                if payload and payload[0:1] == b'{':
                    header = json_decode(payload, StreamHeader)
                else:
                    header = self._codec.decode(payload, StreamHeader)
            except Exception as e:
                logger.error("Failed to decode StreamHeader: %s", e)
                await self._write_error_trailer(
                    send, StatusCode.INTERNAL, "Invalid StreamHeader"
                )
                return

            # All early-return error trailers must respect the client's
            # requested serialization mode so the client can decode them.
            ser_mode = header.serializationMode

            # Validate header -- service name is always required; method may be ""
            # for session streams
            if not header.service:
                await self._write_error_trailer(
                    send, StatusCode.INVALID_ARGUMENT, "Missing service name",
                    serialization_mode=ser_mode,
                )
                return

            # Look up the service
            service_info = self._registry.lookup(header.service, header.version)
            if service_info is None:
                await self._write_error_trailer(
                    send, StatusCode.NOT_FOUND,
                    f"Service '{header.service}' v{header.version} not found",
                    serialization_mode=ser_mode,
                )
                return

            # ── Session discriminator check ───────────────────────────────
            is_session_stream = (header.method == "")
            is_session_service = (service_info.scoped == RpcScope.SESSION)
            if is_session_stream != is_session_service:
                peer_id = ""
                try:
                    peer_id = ctx.connection.remote_id()
                except Exception:
                    pass
                if is_session_service:
                    msg = (
                        f"'{header.service}' is session-scoped: open a session "
                        f"stream (method='') instead of calling method "
                        f"'{header.method}' directly"
                    )
                    logger.warning(
                        "scope mismatch: %s; peer=%s", msg, peer_id,
                    )
                else:
                    msg = (
                        f"'{header.service}' is shared: send a method name "
                        f"instead of opening a session stream (method='')"
                    )
                    logger.warning(
                        "scope mismatch: %s; peer=%s", msg, peer_id,
                    )
                await self._write_error_trailer(
                    send, StatusCode.FAILED_PRECONDITION, msg,
                    serialization_mode=ser_mode,
                )
                return

            if is_session_stream:
                from aster.session import SessionServer
                key = (service_info.name, service_info.version)
                svc_class = self._service_classes.get(key)
                if svc_class is None:
                    await self._write_error_trailer(
                        send, StatusCode.INTERNAL, "Session service class not found",
                        serialization_mode=ser_mode,
                    )
                    return
                all_interceptors = self._resolve_interceptors(service_info)
                peer: str | None = None
                try:
                    peer = ctx.connection.remote_id()
                except Exception:
                    peer = None
                # Respect the client's requested serialization mode.
                # If the client asked for JSON (mode 3), use the JSON codec
                # so the session frames are cross-language compatible.
                session_codec = self._codec
                if ser_mode == SerializationMode.JSON.value:
                    from aster.json_codec import JsonProxyCodec
                    session_codec = JsonProxyCodec()
                session_server = SessionServer(
                    service_class=svc_class,
                    service_info=service_info,
                    codec=session_codec,
                    interceptors=all_interceptors,
                    peer_store=self._peer_store,
                )
                await session_server.run(header, send, recv, peer=peer)
                return
            # ── End session discriminator check ───────────────────────────

            # Look up the method (only for non-session streams)
            method_info = service_info.get_method(header.method)
            if method_info is None:
                await self._write_error_trailer(
                    send, StatusCode.UNIMPLEMENTED,
                    f"Method '{header.service}.{header.method}' not implemented",
                    serialization_mode=ser_mode,
                )
                return

            # Validate serialization mode.
            # JSON (mode 3) is always accepted for cross-language interop.
            accepted_modes = [m.value for m in service_info.serialization_modes]
            accepted_modes.append(SerializationMode.JSON.value)
            if header.serializationMode not in accepted_modes:
                await self._write_error_trailer(
                    send, StatusCode.INVALID_ARGUMENT,
                    f"Unsupported serialization mode: {header.serializationMode}",
                    serialization_mode=ser_mode,
                )
                return

            # Get the handler instance and method
            handler = self._get_handler_for_service(service_info)
            handler_method = getattr(handler, header.method, None)
            if handler_method is None:
                await self._write_error_trailer(
                    send, StatusCode.INTERNAL, "Handler method not found",
                    serialization_mode=ser_mode,
                )
                return

            # Dispatch based on RPC pattern -- with logging context
            pattern = method_info.pattern
            peer_id = ""
            try:
                peer_id = ctx.connection.remote_id()
            except Exception:
                pass

            with request_context(
                request_id=header.callId or "",
                service=header.service,
                method=header.method,
                peer=peer_id,
            ):
                # Run authorization-style interceptors BEFORE pattern dispatch.
                # CallContext-only interceptors (CapabilityInterceptor, deadline,
                # metrics, rate-limit) execute here and can reject the stream
                # before any frames are read. This guarantees auth checks fire
                # on every pattern, including bidi/client streams that might
                # never produce a request frame.
                pre_call_ctx = self._build_call_context(header, method_info, ctx)
                pre_interceptors = self._resolve_interceptors(service_info)
                try:
                    await apply_request_interceptors(pre_interceptors, pre_call_ctx, None)
                except RpcError as auth_err:
                    await self._write_error_trailer(
                        send, auth_err.code, auth_err.message,
                        serialization_mode=ser_mode,
                    )
                    return

                t0 = time.monotonic()
                if pattern == "unary":
                    await self._handle_unary(ctx, send, recv, header, handler_method, method_info)
                elif pattern == "server_stream":
                    await self._handle_server_stream(ctx, send, recv, header, handler_method, method_info)
                elif pattern == "client_stream":
                    await self._handle_client_stream(ctx, send, recv, header, handler_method, method_info)
                elif pattern == "bidi_stream":
                    await self._handle_bidi_stream(ctx, send, recv, header, handler_method, method_info)
                else:
                    await self._write_error_trailer(
                        send, StatusCode.INTERNAL, f"Unknown RPC pattern: {pattern}"
                    )
                    return
                duration_ms = (time.monotonic() - t0) * 1000
                logger.debug(
                    "rpc completed",
                    extra={"duration_ms": round(duration_ms, 1), "status_code": "OK"},
                )

        except asyncio.CancelledError:
            # Stream was cancelled (e.g., deadline exceeded)
            try:
                recv.stop(1)
            except Exception:
                pass
            raise
        except RpcError as e:
            # Handler raised RpcError - write it as trailer
            await self._write_error_trailer(send, e.code, e.message)
        except Exception as e:
            logger.error("Stream handler error: %s", e)
            await self._write_error_trailer(send, StatusCode.INTERNAL, str(e))
        finally:
            _stream_metrics.stream_closed()

    def _get_handler_for_service(self, service_info: ServiceInfo) -> Any:
        """Get a handler instance for a service.

        For session-scoped services, this would create a new instance per stream.
        For shared services, we need to track registered instances.
        """
        key = (service_info.name, service_info.version)
        handler = self._service_instances.get(key)
        if handler is None:
            raise ServiceNotFoundError(
                f"No implementation registered for service {service_info.name} v{service_info.version}"
            )
        return handler

    def _resolve_interceptors(self, service_info: ServiceInfo) -> list[Any]:
        resolved = list(self._interceptors)
        for item in service_info.interceptors:
            resolved.append(item() if inspect.isclass(item) else item)
        return resolved

    def _build_call_context(
        self,
        header: StreamHeader,
        method_info: MethodInfo,
        ctx: ConnectionContext,
    ) -> Any:
        peer = None
        try:
            peer = ctx.connection.remote_id()
        except Exception:
            peer = None

        # Look up admission attributes for this peer
        attributes = {}
        if self._peer_store is not None and peer:
            attributes = self._peer_store.get_attributes(peer)

        return build_call_context(
            service=header.service,
            method=header.method,
            metadata=_validated_metadata(header.metadataKeys, header.metadataValues),
            deadline_epoch_ms=header.deadlineEpochMs,
            peer=peer,
            is_streaming=method_info.pattern != "unary",
            pattern=method_info.pattern,
            idempotent=method_info.idempotent,
            call_id=header.callId or None,
            attributes=attributes,
        )

    async def _decode_request_frame(
        self,
        recv: Any,
        expected_type: type | None,
        serialization_mode: int = 0,
    ) -> tuple[Any, int] | tuple[None, None]:
        while True:
            if hasattr(recv, 'read_one_frame'):
                frame = await recv.read_one_frame()
            else:
                frame = await read_frame(recv)
            if frame is None:
                return None, None
            payload, flags = frame
            # §5.6 / §SS5.6: CANCEL on a non-session stream is ignored
            if flags & CANCEL:
                logger.warning(
                    "CANCEL frame received on non-session stream; ignoring per spec §5.6"
                )
                continue
            compressed = bool(flags & COMPRESSED)
            if serialization_mode == SerializationMode.JSON.value:
                if compressed:
                    payload = safe_decompress(payload)
                request = json_decode(payload, expected_type)
            else:
                request = self._codec.decode_compressed(payload, compressed, expected_type)
            return request, flags

    def _encode_response(
        self, response: Any, serialization_mode: int = 0
    ) -> tuple[bytes, bool]:
        """Encode a response payload, respecting the stream's serialization mode."""
        if serialization_mode == SerializationMode.JSON.value:
            return json_encode(response), False
        return self._codec.encode_compressed(response)

    @staticmethod
    def _handler_timeout(call_ctx: Any) -> float:
        """Return handler timeout: min(remaining deadline, server max)."""
        from aster.limits import MAX_HANDLER_TIMEOUT_S
        remaining = getattr(call_ctx, "remaining_seconds", None)
        if remaining is None:
            return MAX_HANDLER_TIMEOUT_S
        return max(0.0, min(remaining, MAX_HANDLER_TIMEOUT_S))

    async def _handle_unary(
        self,
        conn_ctx: ConnectionContext,
        send: Any,
        recv: Any,
        header: StreamHeader,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> None:
        """Handle a unary RPC call."""
        call_ctx = self._build_call_context(header, method_info, conn_ctx)
        interceptors = self._resolve_interceptors(self._registry.lookup(header.service, header.version))
        try:
            # Read the request frame
            request, flags = await self._decode_request_frame(recv, method_info.request_type, header.serializationMode)
            if request is None:
                await self._write_error_trailer(send, StatusCode.UNAVAILABLE, "Stream ended")
                return
            if flags & TRAILER:
                await self._write_error_trailer(send, StatusCode.UNAVAILABLE, "Unexpected trailer")
                return

            request = await apply_request_interceptors(interceptors, call_ctx, request)

            # Invoke handler with deadline enforcement
            timeout = self._handler_timeout(call_ctx)
            try:
                response = handler_method(request)
                if asyncio.iscoroutine(response):
                    response = await asyncio.wait_for(response, timeout=timeout)
            except asyncio.TimeoutError:
                await self._write_error_trailer(send, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
                return
            response = await apply_response_interceptors(interceptors, call_ctx, response)

            # Encode response + trailer, write + finish in one FFI crossing
            import struct as _struct
            response_payload, response_compressed = self._encode_response(response, header.serializationMode)
            response_flags = COMPRESSED if response_compressed else 0

            status = RpcStatus(code=StatusCode.OK, message="")
            if header.serializationMode == SerializationMode.JSON.value:
                trailer_payload = json_encode({"code": 0, "message": "", "detailKeys": [], "detailValues": []})
            else:
                trailer_payload = self._codec.encode(status)

            response_frame = (
                _struct.pack("<I", len(response_payload) + 1)
                + bytes([response_flags])
                + response_payload
            )
            trailer_frame = (
                _struct.pack("<I", len(trailer_payload) + 1)
                + bytes([TRAILER])
                + trailer_payload
            )
            await send.write_response(response_frame, trailer_frame)

        except asyncio.CancelledError:
            raise
        except RpcError as e:
            maybe_error = await apply_error_interceptors(interceptors, call_ctx, e)
            if maybe_error is not None:
                raise maybe_error
        except Exception as e:
            logger.error("Unary handler error: %s", e)
            maybe_error = await apply_error_interceptors(interceptors, call_ctx, normalize_error(e))
            if maybe_error is not None:
                await self._write_error_trailer(send, maybe_error.code, maybe_error.message)

    async def _handle_server_stream(
        self,
        conn_ctx: ConnectionContext,
        send: Any,
        recv: Any,
        header: StreamHeader,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> None:
        """Handle a server-streaming RPC call."""
        call_ctx = self._build_call_context(header, method_info, conn_ctx)
        interceptors = self._resolve_interceptors(self._registry.lookup(header.service, header.version))
        try:
            # Read the request frame
            request, flags = await self._decode_request_frame(recv, method_info.request_type, header.serializationMode)
            if request is None:
                await self._write_error_trailer(send, StatusCode.UNAVAILABLE, "Stream ended")
                return
            if flags & TRAILER:
                await self._write_error_trailer(send, StatusCode.UNAVAILABLE, "Unexpected trailer")
                return
            request = await apply_request_interceptors(interceptors, call_ctx, request)

            # Invoke handler (async generator)
            response_iter = handler_method(request)
            if asyncio.iscoroutine(response_iter):
                response_iter = await response_iter

            # §5.5.2: ROW_SCHEMA hoisting -- send schema frame before first data frame
            row_schema_sent = False
            if header.serializationMode == SerializationMode.ROW.value:
                try:
                    schema_bytes = self._codec.encode_row_schema()
                    await write_frame(send, schema_bytes, flags=ROW_SCHEMA)
                    row_schema_sent = True
                except (ValueError, NotImplementedError):
                    pass  # Not in ROW mode or schema not available

            # Stream responses with deadline enforcement
            deadline_time = asyncio.get_event_loop().time() + self._handler_timeout(call_ctx)
            async for response in response_iter:
                if asyncio.get_event_loop().time() > deadline_time:
                    await self._write_error_trailer(send, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
                    return
                response = await apply_response_interceptors(interceptors, call_ctx, response)
                response_payload, response_compressed = self._encode_response(response, header.serializationMode)
                response_flags = COMPRESSED if response_compressed else 0
                await write_frame(send, response_payload, response_flags)

            # Write trailer
            await self._write_ok_trailer(send, header.serializationMode)
            await send.finish()

        except asyncio.CancelledError:
            raise
        except RpcError as e:
            maybe_error = await apply_error_interceptors(interceptors, call_ctx, e)
            if maybe_error is not None:
                raise maybe_error
        except Exception as e:
            logger.error("Server stream handler error: %s", e)
            maybe_error = await apply_error_interceptors(interceptors, call_ctx, normalize_error(e))
            if maybe_error is not None:
                await self._write_error_trailer(send, maybe_error.code, maybe_error.message)

    async def _handle_client_stream(
        self,
        conn_ctx: ConnectionContext,
        send: Any,
        recv: Any,
        header: StreamHeader,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> None:
        """Handle a client-streaming RPC call."""
        call_ctx = self._build_call_context(header, method_info, conn_ctx)
        interceptors = self._resolve_interceptors(self._registry.lookup(header.service, header.version))
        try:
            # Collect all request frames until trailer or stream end
            from aster.limits import MAX_CLIENT_STREAM_ITEMS
            requests: list[Any] = []

            while True:
                frame = await read_frame(recv)
                if frame is None:
                    break

                payload, flags = frame
                if flags & TRAILER:
                    try:
                        eoi_status: RpcStatus = self._codec.decode(payload) if not header.serializationMode == SerializationMode.JSON.value else json_decode(payload, RpcStatus)
                        if eoi_status.code != StatusCode.OK:
                            await self._write_error_trailer(
                                send, StatusCode.INTERNAL,
                                f"client sent non-OK EoI trailer (code={eoi_status.code})",
                                serialization_mode=header.serializationMode,
                            )
                            return
                    except Exception:
                        pass
                    break
                # §5.6 / §SS5.6: CANCEL on a non-session stream is ignored
                if flags & CANCEL:
                    logger.warning(
                        "CANCEL frame received on non-session stream; ignoring per spec §5.6"
                    )
                    continue

                if len(requests) >= MAX_CLIENT_STREAM_ITEMS:
                    await self._write_error_trailer(
                        send, StatusCode.RESOURCE_EXHAUSTED,
                        f"client stream exceeded {MAX_CLIENT_STREAM_ITEMS} items",
                        serialization_mode=header.serializationMode,
                    )
                    return

                compressed = bool(flags & COMPRESSED)
                if header.serializationMode == SerializationMode.JSON.value:
                    if compressed:
                        payload = safe_decompress(payload)
                    request = json_decode(payload, method_info.request_type)
                else:
                    request = self._codec.decode_compressed(payload, compressed, method_info.request_type)
                request = await apply_request_interceptors(interceptors, call_ctx, request)
                requests.append(request)

            async def request_iter() -> AsyncIterator[Any]:
                for item in requests:
                    yield item

            timeout = self._handler_timeout(call_ctx)
            try:
                response = handler_method(request_iter())
                if asyncio.iscoroutine(response):
                    response = await asyncio.wait_for(response, timeout=timeout)
            except asyncio.TimeoutError:
                await self._write_error_trailer(send, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
                return
            response = await apply_response_interceptors(interceptors, call_ctx, response)

            # Encode and write response
            response_payload, response_compressed = self._encode_response(response, header.serializationMode)
            response_flags = COMPRESSED if response_compressed else 0
            await write_frame(send, response_payload, response_flags)

            # Write trailer
            await self._write_ok_trailer(send, header.serializationMode)
            await send.finish()

        except asyncio.CancelledError:
            raise
        except RpcError as e:
            maybe_error = await apply_error_interceptors(interceptors, call_ctx, e)
            if maybe_error is not None:
                raise maybe_error
        except Exception as e:
            logger.error("Client stream handler error: %s", e)
            maybe_error = await apply_error_interceptors(interceptors, call_ctx, normalize_error(e))
            if maybe_error is not None:
                await self._write_error_trailer(send, maybe_error.code, maybe_error.message)

    async def _handle_bidi_stream(
        self,
        conn_ctx: ConnectionContext,
        send: Any,
        recv: Any,
        header: StreamHeader,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> None:
        """Handle a bidirectional-streaming RPC call.

        Auth interceptors already ran in handle_stream() before dispatch.
        """
        call_ctx = self._build_call_context(header, method_info, conn_ctx)
        interceptors = self._resolve_interceptors(self._registry.lookup(header.service, header.version))
        try:
            request_queue: asyncio.Queue[Any] = asyncio.Queue()
            request_done = asyncio.Event()

            async def request_iter() -> AsyncIterator[Any]:
                while True:
                    item = await request_queue.get()
                    if item is _BIDI_EOF:
                        break
                    yield item

            # Invoke handler (async generator)
            response_iter = handler_method(request_iter())
            if asyncio.iscoroutine(response_iter):
                response_iter = await response_iter

            # Start reader task
            reader_task = asyncio.create_task(
                self._bidi_reader(recv, method_info.request_type, request_queue, request_done, interceptors, call_ctx, header.serializationMode)
            )

            # §5.5.2: ROW_SCHEMA hoisting -- send schema frame before first data frame
            if header.serializationMode == SerializationMode.ROW.value:
                try:
                    schema_bytes = self._codec.encode_row_schema()
                    await write_frame(send, schema_bytes, flags=ROW_SCHEMA)
                except (ValueError, NotImplementedError):
                    pass  # Not in ROW mode or schema not available

            # Stream responses from handler with deadline enforcement
            deadline_time = asyncio.get_event_loop().time() + self._handler_timeout(call_ctx)
            async for response in response_iter:
                if asyncio.get_event_loop().time() > deadline_time:
                    reader_task.cancel()
                    try:
                        await reader_task
                    except (asyncio.CancelledError, Exception):
                        pass
                    await self._write_error_trailer(send, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
                    return
                response = await apply_response_interceptors(interceptors, call_ctx, response)
                response_payload, response_compressed = self._encode_response(response, header.serializationMode)
                response_flags = COMPRESSED if response_compressed else 0
                await write_frame(send, response_payload, response_flags)

            # Write trailer
            await self._write_ok_trailer(send, header.serializationMode)
            await send.finish()

            # Wait for reader to finish
            await reader_task

        except asyncio.CancelledError:
            raise
        except RpcError as e:
            maybe_error = await apply_error_interceptors(interceptors, call_ctx, e)
            if maybe_error is not None:
                raise maybe_error
        except Exception as e:
            logger.error("Bidi stream handler error: %s", e)
            maybe_error = await apply_error_interceptors(interceptors, call_ctx, normalize_error(e))
            if maybe_error is not None:
                await self._write_error_trailer(send, maybe_error.code, maybe_error.message)

    async def _bidi_reader(
        self,
        recv: Any,
        request_type: type | None,
        request_queue: asyncio.Queue[Any],
        request_done: asyncio.Event,
        interceptors: list[Any],
        call_ctx: Any,
        serialization_mode: int = 0,
    ) -> None:
        """Read inbound bidi request frames and feed the handler iterator."""
        try:
            while True:
                frame = await read_frame(recv)
                if frame is None:
                    break

                payload, flags = frame
                if flags & TRAILER:
                    break
                # §5.6 / §SS5.6: CANCEL on a non-session stream is ignored
                if flags & CANCEL:
                    logger.warning(
                        "CANCEL frame received on non-session stream; ignoring per spec §5.6"
                    )
                    continue

                compressed = bool(flags & COMPRESSED)
                if serialization_mode == SerializationMode.JSON.value:
                    if compressed:
                        import zstandard
                        payload = zstandard.ZstdDecompressor().decompress(payload)
                    request = json_decode(payload, request_type)
                else:
                    request = self._codec.decode_compressed(payload, compressed, request_type)
                request = await apply_request_interceptors(interceptors, call_ctx, request)
                await request_queue.put(request)

        except asyncio.CancelledError:
            raise
        except Exception as e:
            logger.debug("Bidi reader error: %s", e)
        finally:
            await request_queue.put(_BIDI_EOF)
            request_done.set()

    async def _write_ok_trailer(self, send: Any, serialization_mode: int = 0) -> None:
        """Write an OK status trailer."""
        status = RpcStatus(code=StatusCode.OK, message="")
        if serialization_mode == SerializationMode.JSON.value:
            payload = json_encode({"code": 0, "message": "", "detailKeys": [], "detailValues": []})
        else:
            payload = self._codec.encode(status)
        await write_frame(send, payload, flags=TRAILER)

    async def _write_error_trailer(
        self, send: Any, code: StatusCode, message: str,
        serialization_mode: int = 0,
    ) -> None:
        """Write an error status trailer."""
        try:
            if serialization_mode == SerializationMode.JSON.value:
                payload = json_encode({"code": code.value, "message": message, "detailKeys": [], "detailValues": []})
            else:
                status = RpcStatus(code=code, message=message)
                payload = self._codec.encode(status)
            await write_frame(send, payload, flags=TRAILER)
            await send.finish()
        except Exception as e:
            logger.error("Failed to write error trailer: %s", e)

    async def drain(self, grace_period: float = 10.0) -> None:
        """Gracefully drain the server.

        Stops accepting new connections and streams, waits for in-flight
        RPCs to complete, then cancels remaining handlers.

        Args:
            grace_period: Maximum seconds to wait for in-flight calls.
        """
        logger.info("Draining server (grace period: %.1fs)", grace_period)

        # Stop accepting new connections
        self._serving = False

        # Mark all connections as draining
        async with self._connections_lock:
            for ctx in self._connections:
                ctx.draining = True

        # Wait for in-flight calls with timeout
        deadline = time.monotonic() + grace_period

        while time.monotonic() < deadline:
            active_streams = 0
            async with self._connections_lock:
                for ctx in self._connections:
                    active_streams += len([t for t in ctx.stream_tasks if not t.done()])

            if active_streams == 0:
                break

            logger.debug("Waiting for %d active streams", active_streams)
            await asyncio.sleep(0.1)

        # Cancel remaining stream handlers
        async with self._connections_lock:
            for ctx in list(self._connections):
                for task in list(ctx.stream_tasks):
                    if not task.done():
                        task.cancel()

        logger.info("Drain complete")

    async def close(self) -> None:
        """Close the server and all connections."""
        logger.info("Closing server")

        # Cancel serve task
        if self._serve_task and not self._serve_task.done():
            self._serve_task.cancel()
            try:
                await self._serve_task
            except asyncio.CancelledError:
                pass

        # Close all connections
        async with self._connections_lock:
            for ctx in self._connections:
                if ctx.connection_task and not ctx.connection_task.done():
                    ctx.connection_task.cancel()
                try:
                    ctx.connection.close(0, b"server closed")
                except Exception:
                    pass
            self._connections.clear()

        if self._owns_endpoint:
            try:
                await self._endpoint.close()
            except Exception:
                pass

        self._shutdown_event.set()
        logger.info("Server closed")

    async def wait_until_stopped(self) -> None:
        """Wait until the server is stopped."""
        await self._shutdown_event.wait()


_BIDI_EOF = object()


def _validated_metadata(
    keys: list[str] | None, values: list[str] | None
) -> dict[str, str] | None:
    """Build metadata dict from StreamHeader lists with security validation.

    Enforces MAX_METADATA_ENTRIES and MAX_METADATA_TOTAL_BYTES from limits.py.
    """
    if not keys:
        return None


    vals = values or []
    try:
        validate_metadata(keys, vals)
    except LimitExceeded as e:
        import logging
        logging.getLogger(__name__).warning("Metadata limit exceeded: %s", e)
        # Truncate to the limit rather than rejecting the entire call
        keys = keys[:MAX_METADATA_ENTRIES]
        vals = vals[:MAX_METADATA_ENTRIES]

    return dict(zip(keys, vals))

