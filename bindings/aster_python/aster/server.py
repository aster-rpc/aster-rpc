"""
aster.server — Aster RPC server implementation.

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

from aster_python.aster.codec import ForyCodec
from aster_python.aster.framing import HEADER, TRAILER, COMPRESSED, write_frame, read_frame
from aster_python.aster.protocol import StreamHeader, RpcStatus
from aster_python.aster.status import StatusCode, RpcError
from aster_python.aster.types import SerializationMode
from aster_python.aster.transport.base import BidiChannel, TransportError
from aster_python.aster.service import ServiceRegistry, ServiceInfo, MethodInfo

if TYPE_CHECKING:
    import aster_python

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
    connection: "aster_python.IrohConnection"
    server: "Server"
    connection_task: asyncio.Task | None = None
    stream_tasks: set[asyncio.Task] = field(default_factory=set)
    draining: bool = False
    _closed: bool = field(default=False, repr=False)


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
        endpoint: "aster_python.IrohEndpoint",
        services: list[type] | ServiceRegistry | None = None,
        codec: ForyCodec | None = None,
        interceptors: list[Any] | None = None,
        max_concurrent_streams: int | None = None,
        registry: ServiceRegistry | None = None,
    ) -> None:
        """Initialize the server.

        Args:
            endpoint: The Iroh endpoint to accept connections on.
            services: Service classes (decorated with @service) or a ServiceRegistry.
            codec: The ForyCodec for serialization. Defaults to XLANG mode.
            interceptors: List of interceptor instances to apply to all calls.
            max_concurrent_streams: Maximum concurrent streams per connection.
            registry: Optional ServiceRegistry. If not provided, creates one from services.
        """
        self._endpoint = endpoint
        self._codec = codec or ForyCodec(mode=SerializationMode.XLANG)
        self._interceptors = list(interceptors) if interceptors else []
        self._max_concurrent_streams = max_concurrent_streams

        # Set up service registry
        if registry is not None:
            self._registry = registry
        elif isinstance(services, ServiceRegistry):
            self._registry = services
        elif services:
            self._registry = ServiceRegistry()
            for svc in services:
                self._registry.register(svc)
        else:
            self._registry = ServiceRegistry()

        # Connection tracking
        self._connections: set[ConnectionContext] = set()
        self._connections_lock = asyncio.Lock()
        
        # Server state
        self._serving = False
        self._serve_task: asyncio.Task | None = None
        self._shutdown_event = asyncio.Event()

    @property
    def registry(self) -> ServiceRegistry:
        """The service registry used by this server."""
        return self._registry

    @property
    def endpoint(self) -> "aster_python.IrohEndpoint":
        """The Iroh endpoint this server is bound to."""
        return self._endpoint

    async def serve(self) -> None:
        """Start the server and accept connections.

        This method runs until shutdown is requested via drain() or close().
        """
        if self._serving:
            raise ServerError("server is already serving")

        self._serving = True
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
                    if self._serving:
                        logger.error("Error accepting connection: %s", e)
                    continue

        finally:
            self._serving = False

    async def _handle_connection(self, ctx: ConnectionContext) -> None:
        """Handle a single client connection.

        Accepts bidirectional streams and dispatches them to handlers.
        """
        logger.debug("Connection opened from %s", ctx.connection.remote_endpoint_id())

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
                    if not ctx.draining:
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

            # Cancel all stream tasks
            for task in ctx.stream_tasks:
                if not task.done():
                    task.cancel()
                    try:
                        await task
                    except asyncio.CancelledError:
                        pass

            logger.debug("Connection closed")

    async def _handle_stream(self, ctx: ConnectionContext, send: Any, recv: Any) -> None:
        """Handle a single RPC stream.

        Reads the StreamHeader, dispatches to the appropriate handler,
        and manages the stream lifecycle.
        """
        try:
            # Read the StreamHeader (first frame with HEADER flag)
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

            # Decode the StreamHeader
            try:
                header = self._codec.decode(payload, StreamHeader)
            except Exception as e:
                logger.error("Failed to decode StreamHeader: %s", e)
                await self._write_error_trailer(
                    send, StatusCode.INTERNAL, "Invalid StreamHeader"
                )
                return

            # Validate header
            if not header.service or not header.method:
                await self._write_error_trailer(
                    send, StatusCode.INVALID_ARGUMENT, "Missing service or method name"
                )
                return

            # Look up the service
            service_info = self._registry.lookup(header.service, header.version)
            if service_info is None:
                await self._write_error_trailer(
                    send, StatusCode.NOT_FOUND,
                    f"Service '{header.service}' v{header.version} not found"
                )
                return

            # Look up the method
            method_info = service_info.get_method(header.method)
            if method_info is None:
                await self._write_error_trailer(
                    send, StatusCode.UNIMPLEMENTED,
                    f"Method '{header.service}.{header.method}' not implemented"
                )
                return

            # Validate serialization mode
            if header.serialization_mode not in [m.value for m in service_info.serialization_modes]:
                await self._write_error_trailer(
                    send, StatusCode.INVALID_ARGUMENT,
                    f"Unsupported serialization mode: {header.serialization_mode}"
                )
                return

            # Get the handler instance and method
            handler = self._get_handler_for_service(service_info)
            handler_method = getattr(handler, header.method, None)
            if handler_method is None:
                await self._write_error_trailer(
                    send, StatusCode.INTERNAL, "Handler method not found"
                )
                return

            # Dispatch based on RPC pattern
            pattern = method_info.pattern
            
            if pattern == "unary":
                await self._handle_unary(send, recv, header, handler_method, method_info)
            elif pattern == "server_stream":
                await self._handle_server_stream(send, recv, header, handler_method, method_info)
            elif pattern == "client_stream":
                await self._handle_client_stream(send, recv, header, handler_method, method_info)
            elif pattern == "bidi_stream":
                await self._handle_bidi_stream(send, recv, header, handler_method, method_info)
            else:
                await self._write_error_trailer(
                    send, StatusCode.INTERNAL, f"Unknown RPC pattern: {pattern}"
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

    def _get_handler_for_service(self, service_info: ServiceInfo) -> Any:
        """Get a handler instance for a service.

        For session-scoped services, this would create a new instance per stream.
        For shared services, we need to track registered instances.
        """
        # For now, we assume service classes are registered with instances
        # In a full implementation, we'd track instances separately
        # This is a placeholder that looks up from the registry
        return None

    async def _handle_unary(
        self,
        send: Any,
        recv: Any,
        header: StreamHeader,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> None:
        """Handle a unary RPC call."""
        try:
            # Read the request frame
            frame = await read_frame(recv)
            if frame is None:
                await self._write_error_trailer(send, StatusCode.UNAVAILABLE, "Stream ended")
                return

            payload, flags = frame
            if flags & TRAILER:
                await self._write_error_trailer(send, StatusCode.UNAVAILABLE, "Unexpected trailer")
                return

            # Decode request
            compressed = bool(flags & COMPRESSED)
            request = self._codec.decode_compressed(payload, compressed, method_info.request_type)

            # Invoke handler
            response = handler_method(request)
            if asyncio.iscoroutine(response):
                response = await response

            # Encode and write response
            response_payload, response_compressed = self._codec.encode_compressed(response)
            response_flags = COMPRESSED if response_compressed else 0
            await write_frame(send, response_payload, response_flags)

            # Write trailer
            await self._write_ok_trailer(send)

            await send.finish()

        except asyncio.CancelledError:
            raise
        except RpcError:
            raise  # Already handled at upper level
        except Exception as e:
            logger.error("Unary handler error: %s", e)
            await self._write_error_trailer(send, StatusCode.UNKNOWN, str(e))

    async def _handle_server_stream(
        self,
        send: Any,
        recv: Any,
        header: StreamHeader,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> None:
        """Handle a server-streaming RPC call."""
        try:
            # Read the request frame
            frame = await read_frame(recv)
            if frame is None:
                await self._write_error_trailer(send, StatusCode.UNAVAILABLE, "Stream ended")
                return

            payload, flags = frame
            if flags & TRAILER:
                await self._write_error_trailer(send, StatusCode.UNAVAILABLE, "Unexpected trailer")
                return

            # Decode request
            compressed = bool(flags & COMPRESSED)
            request = self._codec.decode_compressed(payload, compressed, method_info.request_type)

            # Invoke handler (async generator)
            response_iter = handler_method(request)
            if asyncio.iscoroutine(response_iter):
                response_iter = await response_iter

            # Stream responses
            async for response in response_iter:
                response_payload, response_compressed = self._codec.encode_compressed(response)
                response_flags = COMPRESSED if response_compressed else 0
                await write_frame(send, response_payload, response_flags)

            # Write trailer
            await self._write_ok_trailer(send)
            await send.finish()

        except asyncio.CancelledError:
            raise
        except RpcError:
            raise
        except Exception as e:
            logger.error("Server stream handler error: %s", e)
            await self._write_error_trailer(send, StatusCode.UNKNOWN, str(e))

    async def _handle_client_stream(
        self,
        send: Any,
        recv: Any,
        header: StreamHeader,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> None:
        """Handle a client-streaming RPC call."""
        try:
            # Collect all request frames until trailer or stream end
            requests: list[Any] = []
            
            while True:
                try:
                    frame = await read_frame(recv)
                except Exception:
                    break

                if frame is None:
                    break

                payload, flags = frame
                if flags & TRAILER:
                    break

                compressed = bool(flags & COMPRESSED)
                request = self._codec.decode_compressed(payload, compressed, method_info.request_type)
                requests.append(request)

            # Invoke handler with collected requests
            if asyncio.iscoroutinefunction(handler_method):
                response = await handler_method(requests)
            else:
                response = handler_method(requests)

            # Encode and write response
            response_payload, response_compressed = self._codec.encode_compressed(response)
            response_flags = COMPRESSED if response_compressed else 0
            await write_frame(send, response_payload, response_flags)

            # Write trailer
            await self._write_ok_trailer(send)
            await send.finish()

        except asyncio.CancelledError:
            raise
        except RpcError:
            raise
        except Exception as e:
            logger.error("Client stream handler error: %s", e)
            await self._write_error_trailer(send, StatusCode.UNKNOWN, str(e))

    async def _handle_bidi_stream(
        self,
        send: Any,
        recv: Any,
        header: StreamHeader,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> None:
        """Handle a bidirectional-streaming RPC call."""
        try:
            # Invoke handler (async generator)
            response_iter = handler_method(None)  # Bidi handlers don't receive requests directly
            if asyncio.iscoroutine(response_iter):
                response_iter = await response_iter

            # Start reader task
            reader_task = asyncio.create_task(
                self._bidi_reader(send, recv, response_iter)
            )

            # Stream responses from handler
            async for response in response_iter:
                response_payload, response_compressed = self._codec.encode_compressed(response)
                response_flags = COMPRESSED if response_compressed else 0
                await write_frame(send, response_payload, response_flags)

            # Write trailer
            await self._write_ok_trailer(send)
            await send.finish()

            # Wait for reader to finish
            try:
                await reader_task
            except asyncio.CancelledError:
                reader_task.cancel()
                raise

        except asyncio.CancelledError:
            raise
        except RpcError:
            raise
        except Exception as e:
            logger.error("Bidi stream handler error: %s", e)
            await self._write_error_trailer(send, StatusCode.UNKNOWN, str(e))

    async def _bidi_reader(
        self,
        send: Any,
        recv: Any,
        response_iter: AsyncIterator,
    ) -> None:
        """Read frames from bidi stream (for future client-sent messages)."""
        try:
            while True:
                frame = await read_frame(recv)
                if frame is None:
                    break

                payload, flags = frame
                if flags & TRAILER:
                    break

                # For now, we don't process client messages in the handler
                # This would be used for full duplex where handler receives messages
                compressed = bool(flags & COMPRESSED)
                # Could pass this to response_iter if supported

        except asyncio.CancelledError:
            raise
        except Exception as e:
            logger.debug("Bidi reader error: %s", e)

    async def _write_ok_trailer(self, send: Any) -> None:
        """Write an OK status trailer."""
        status = RpcStatus(code=StatusCode.OK, message="")
        payload = self._codec.encode(status)
        await write_frame(send, payload, flags=TRAILER)

    async def _write_error_trailer(
        self, send: Any, code: StatusCode, message: str
    ) -> None:
        """Write an error status trailer."""
        try:
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
            for ctx in self._connections:
                for task in ctx.stream_tasks:
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

        self._shutdown_event.set()
        logger.info("Server closed")

    async def wait_until_stopped(self) -> None:
        """Wait until the server is stopped."""
        await self._shutdown_event.wait()
