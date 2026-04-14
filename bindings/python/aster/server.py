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


# ── Per-connection session state (spec §6 / §7.5) ────────────────────────────


@dataclass
class _ConnectionState:
    """Per-connection session tracking for the reactor dispatch path.

    Mirrors Java `AsterServer.ConnectionState`. Holds the active session key
    set, the monotonic graveyard counter, and the per-connection cap. Mutated
    from the single-threaded reactor dispatch loop -- no locks required.
    """
    max_sessions: int
    active_sessions: set[tuple[int, type]] = field(default_factory=set)
    last_opened_session_id: int = 0


class _SessionNotFound(Exception):
    """sessionId <= last_opened_session_id and not in the active map."""


class _SessionLimitExceeded(Exception):
    """Opening this session would exceed max_sessions_per_connection."""


class _SessionScopeMismatch(Exception):
    """SHARED call carried a sessionId, or SESSION call arrived without one."""


# ── Reactor I/O adapters (spec Sec. 8 streaming dispatch) ────────────────────
# These let the reactor dispatch path reuse `_handle_server_stream`,
# `_handle_client_stream`, `_handle_bidi_stream` without duplicating their
# ~300 lines of interceptor / deadline / error-trailer logic. The adapters
# look enough like a real QUIC bi-stream for `framing.write_frame` /
# `framing.read_frame` to drive them.


class _ReactorSendAdapter:
    """SendStream-shaped wrapper over `ReactorResponseSender`.

    ``framing.write_frame`` serializes its payload as
    ``[4B LE len][1B flags][payload]`` and then calls ``write_all(data)``.
    This adapter inspects the flags byte and routes to the streaming or
    terminal reactor-sender API. After a trailer is written, further
    ``write_all`` calls are silently dropped (matches QUIC semantics once
    the stream's send side is finished).
    """

    def __init__(self, sender: Any) -> None:
        self._sender = sender
        self._closed = False

    async def write_all(self, data: bytes) -> None:
        if self._closed or len(data) < 5:
            return
        flags = data[4]
        if flags & TRAILER:
            self._sender.send_trailer(bytes(data))
            self._closed = True
        else:
            self._sender.send_frame(bytes(data))

    async def finish(self) -> None:
        # Reactor auto-finishes the QUIC stream after the trailer frame
        # is written. Nothing to do here.
        return None


class _ReactorRecvAdapter:
    """RecvStream-shaped wrapper over `ReactorRequestReceiver`.

    The reactor delivers frames as ``(payload, flags)`` tuples. This
    adapter re-frames them into the on-wire byte layout and serves them
    through ``read_exact(n)`` so ``framing.read_frame`` can drive dispatch.
    The first request frame arrives inline on the ``ReactorEvent``; callers
    pass it as ``first_frame_bytes`` so it shows up as the first frame on
    the adapter.
    """

    def __init__(
        self,
        receiver: Any,
        first_frame_bytes: bytes | None = None,
    ) -> None:
        self._receiver = receiver
        self._buffer = bytearray(first_frame_bytes or b"")
        self._eof = False

    async def read_exact(self, n: int) -> bytes:
        import struct as _struct
        while len(self._buffer) < n:
            if self._eof or self._receiver is None:
                raise EOFError("reactor request stream ended")
            frame = await self._receiver.recv()
            if frame is None:
                self._eof = True
                if len(self._buffer) < n:
                    raise EOFError("reactor request stream ended")
                break
            payload, flags = frame
            self._buffer.extend(_struct.pack("<I", len(payload) + 1))
            self._buffer.append(flags)
            self._buffer.extend(payload)
        out = bytes(self._buffer[:n])
        del self._buffer[:n]
        return out


class _ReactorConnStub:
    """Minimal IrohConnection-shaped stub for `ConnectionContext`.

    Reactor dispatch runs without a live Python ConnectionContext (the
    real connection is owned by the Rust reactor). Streaming handlers only
    need ``remote_id()`` for peer lookup in ``_build_call_context``.
    """

    def __init__(self, peer_id: str) -> None:
        self._peer = peer_id

    def remote_id(self) -> str:
        return self._peer


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
        max_sessions_per_connection: int = 1024,
        registry: ServiceRegistry | None = None,
        owns_endpoint: bool = True,
        peer_store: "PeerAttributeStore | None" = None,
        node: Any = None,
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
        self._node = node
        from aster.interceptors.deadline import DeadlineInterceptor
        if interceptors is not None:
            self._interceptors = list(interceptors)
        else:
            self._interceptors = [DeadlineInterceptor()]
        self._max_concurrent_streams = max_concurrent_streams
        self._max_sessions_per_connection = max_sessions_per_connection
        self._service_instances: dict[tuple[str, int], Any] = {}
        # For session-scoped services we store the class (not an instance)
        self._service_classes: dict[tuple[str, int], type] = {}
        # Reactor-path per-connection session state (spec §6 / §7.5).
        # Keyed by connection_id → active sessions + graveyard counter.
        self._connection_sessions: dict[int, _ConnectionState] = {}
        # Session-scoped handler instance cache for the reactor path.
        # Keyed by (connection_id, session_id, impl_class).
        self._reactor_session_instances: dict[tuple[int, int, type], Any] = {}

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

    async def serve(self, channel_capacity: int = 256) -> None:
        """Start the server. Alias for :meth:`serve_reactor`.

        The reactor is the only dispatch path post-multiplexed-streams
        (spec Sec. 6). This method exists so existing ``await server.serve()``
        call sites keep working; it requires ``node=`` on the ``Server``.
        """
        await self.serve_reactor(channel_capacity=channel_capacity)

    async def serve_reactor(self, channel_capacity: int = 256) -> None:
        """Start the server using the Rust-driven reactor.

        The reactor runs the accept loop, stream reads, and response writes
        entirely in Rust. Python only handles dispatch and handler invocation.
        The pump consumes `next_event()` so per-connection session lifecycle
        (spec §6 / §7.5) can observe ConnectionClosed events and reap state.

        Requires that the Server was constructed with a ``node`` parameter.
        """
        if self._node is None:
            raise ServerError(
                "serve_reactor() requires a node; pass node= to Server()"
            )
        if self._serving:
            raise ServerError("server is already serving")

        from aster._aster import start_reactor

        self._serving = True
        self._serve_task = asyncio.current_task()
        self._shutdown_event.clear()

        logger.info("Server starting (reactor mode) on %s", self._endpoint.endpoint_id())

        reactor = start_reactor(self._node, channel_capacity)

        try:
            while self._serving:
                event = await reactor.next_event()
                if event is None:
                    break

                if event.kind == "call":
                    response_sender = event.take_sender()
                    if response_sender is None:
                        # Shouldn't happen -- guard against double-take.
                        logger.warning("Call event missing response sender")
                        continue
                    request_receiver = event.take_request_receiver()
                    cancel_flag = event.cancel_flag
                    asyncio.create_task(
                        self._dispatch_reactor_call(
                            event.call_id,
                            event.header_payload or b"",
                            event.header_flags,
                            event.request_payload or b"",
                            event.request_flags,
                            event.peer_id,
                            event.connection_id,
                            response_sender,
                            request_receiver,
                            cancel_flag,
                        )
                    )
                elif event.kind == "connection_closed":
                    self._on_reactor_connection_closed(
                        event.connection_id, event.peer_id,
                    )
        finally:
            self._serving = False
            self._serve_task = None
            self._shutdown_event.set()

    def _on_reactor_connection_closed(
        self, connection_id: int, peer_id: str,
    ) -> None:
        """Reap per-connection session state on ConnectionClosed (spec §7.5).

        Drops the ``_ConnectionState`` and all cached session handler
        instances for this connection. Instances with an async ``close()``
        hook have it scheduled on the current loop; sync ``close()`` is
        invoked immediately.
        """
        state = self._connection_sessions.pop(connection_id, None)
        if state is None:
            return

        for session_id, impl_class in list(state.active_sessions):
            instance = self._reactor_session_instances.pop(
                (connection_id, session_id, impl_class), None,
            )
            if instance is None:
                continue
            close_hook = getattr(instance, "close", None)
            if close_hook is None:
                continue
            try:
                result = close_hook()
                if asyncio.iscoroutine(result):
                    asyncio.create_task(result)
            except Exception as e:  # noqa: BLE001
                logger.warning(
                    "Session close() raised for %s sessionId=%d: %s",
                    impl_class.__name__, session_id, e,
                )

        logger.debug(
            "Reactor connection closed: connection_id=%d peer=%s "
            "(reaped %d sessions)",
            connection_id, peer_id, len(state.active_sessions),
        )

    async def _dispatch_reactor_call(
        self,
        call_id: int,
        header_payload: bytes,
        header_flags: int,
        request_payload: bytes,
        request_flags: int,
        peer_id: str,
        connection_id: int,
        response_sender: Any,
        request_receiver: Any = None,
        cancel_flag: Any = None,
    ) -> None:
        import struct as _struct

        header: StreamHeader | None = None
        ser_mode = 0
        try:
            if not (header_flags & HEADER):
                self._reactor_error_response(
                    response_sender, StatusCode.INTERNAL,
                    "First frame must have HEADER flag",
                )
                return

            if header_payload and header_payload[0:1] == b'{':
                header = json_decode(header_payload, StreamHeader)
            else:
                header = self._codec.decode(header_payload, StreamHeader)

            ser_mode = header.serializationMode

            if not header.service:
                self._reactor_error_response(
                    response_sender, StatusCode.INVALID_ARGUMENT,
                    "Missing service name", ser_mode,
                )
                return

            service_info = self._registry.lookup(header.service, header.version)
            if service_info is None:
                self._reactor_error_response(
                    response_sender, StatusCode.NOT_FOUND,
                    f"Service '{header.service}' v{header.version} not found",
                    ser_mode,
                )
                return

            method_info = service_info.get_method(header.method)
            if method_info is None:
                self._reactor_error_response(
                    response_sender, StatusCode.UNIMPLEMENTED,
                    f"Method '{header.service}.{header.method}' not implemented",
                    ser_mode,
                )
                return

            try:
                handler = self._resolve_instance(
                    service_info, connection_id,
                    getattr(header, "sessionId", 0), peer_id,
                )
            except _SessionScopeMismatch as e:
                self._reactor_error_response(
                    response_sender, StatusCode.FAILED_PRECONDITION,
                    str(e), ser_mode,
                )
                return
            except _SessionNotFound as e:
                self._reactor_error_response(
                    response_sender, StatusCode.NOT_FOUND, str(e), ser_mode,
                )
                return
            except _SessionLimitExceeded as e:
                self._reactor_error_response(
                    response_sender, StatusCode.RESOURCE_EXHAUSTED,
                    str(e), ser_mode,
                )
                return
            handler_method = getattr(handler, header.method, None)
            if handler_method is None:
                self._reactor_error_response(
                    response_sender, StatusCode.INTERNAL,
                    "Handler method not found", ser_mode,
                )
                return

            # Validate the client's requested serialization mode. JSON is
            # always accepted for cross-language interop.
            accepted_modes = [m.value for m in service_info.serialization_modes]
            accepted_modes.append(SerializationMode.JSON.value)
            if header.serializationMode not in accepted_modes:
                self._reactor_error_response(
                    response_sender, StatusCode.INVALID_ARGUMENT,
                    f"Unsupported serialization mode: {header.serializationMode}",
                    ser_mode,
                )
                return

            # Pre-dispatch authz: run CallContext-only interceptors
            # (CapabilityInterceptor, deadline, rate-limit, metrics, audit)
            # with request=None so they can reject the call before any
            # request frames are decoded. This guarantees auth fires on
            # every pattern -- including bidi/client streams that might
            # never produce a request frame.
            pre_call_ctx = build_call_context(
                service=header.service,
                method=header.method,
                metadata=_validated_metadata(header.metadataKeys, header.metadataValues),
                deadline_secs=header.deadline,
                peer=peer_id,
                is_streaming=(method_info.pattern != "unary"),
                pattern=method_info.pattern,
                idempotent=method_info.idempotent,
                call_id=header.callId,
                attributes=(
                    self._peer_store.get_attributes(peer_id)
                    if self._peer_store and peer_id else {}
                ),
            )
            pre_interceptors = self._resolve_interceptors(service_info)
            try:
                await apply_request_interceptors(pre_interceptors, pre_call_ctx, None)
            except RpcError as auth_err:
                self._reactor_error_response(
                    response_sender, auth_err.code, auth_err.message, ser_mode,
                )
                return

            # Branch on RPC pattern. Unary runs inline on the reactor fast
            # path (sender.submit bundles response + trailer). Streaming
            # patterns build adapter objects that look like QUIC streams so
            # we can reuse the `_handle_*_stream` methods.
            pattern = method_info.pattern
            if pattern != "unary":
                with request_context(
                    request_id=str(header.callId) if header.callId else "",
                    service=header.service,
                    method=header.method,
                    peer=peer_id,
                ):
                    await self._dispatch_reactor_streaming(
                        pattern, header, service_info, method_info,
                        handler_method, peer_id, request_payload, request_flags,
                        response_sender, request_receiver,
                    )
                return

            # Decode request
            compressed = bool(request_flags & COMPRESSED)
            if ser_mode == SerializationMode.JSON.value:
                if compressed:
                    request_payload = safe_decompress(request_payload)
                request = json_decode(request_payload, method_info.request_type)
            else:
                request = self._codec.decode_compressed(
                    request_payload, compressed, method_info.request_type,
                )

            # Interceptors
            call_ctx = build_call_context(
                service=header.service,
                method=header.method,
                metadata=_validated_metadata(header.metadataKeys, header.metadataValues),
                deadline_secs=header.deadline,
                peer=peer_id,
                is_streaming=False,
                pattern="unary",
                idempotent=method_info.idempotent,
                call_id=header.callId,
                attributes=(
                    self._peer_store.get_attributes(peer_id)
                    if self._peer_store and peer_id else {}
                ),
            )
            interceptors = self._resolve_interceptors(service_info)
            request = await apply_request_interceptors(interceptors, call_ctx, request)

            # Invoke handler
            timeout = self._handler_timeout(call_ctx)
            from aster.interceptors.base import invoke_handler_with_ctx, reset_call_context
            response, _cv_token = invoke_handler_with_ctx(
                handler_method, request, call_ctx, method_info.accepts_ctx,
            )
            try:
                if asyncio.iscoroutine(response):
                    response = await asyncio.wait_for(response, timeout=timeout)
                response = await apply_response_interceptors(interceptors, call_ctx, response)
            finally:
                reset_call_context(_cv_token)

            # Encode response + trailer
            response_payload, response_compressed = self._encode_response(
                response, ser_mode,
            )
            resp_flags = COMPRESSED if response_compressed else 0

            status = RpcStatus(code=StatusCode.OK, message="")
            if ser_mode == SerializationMode.JSON.value:
                trailer_payload = json_encode({
                    "code": 0, "message": "", "detailKeys": [], "detailValues": [],
                })
            else:
                trailer_payload = self._codec.encode(status)

            response_frame = (
                _struct.pack("<I", len(response_payload) + 1)
                + bytes([resp_flags])
                + response_payload
            )
            trailer_frame = (
                _struct.pack("<I", len(trailer_payload) + 1)
                + bytes([TRAILER])
                + trailer_payload
            )

            response_sender.submit(bytes(response_frame), bytes(trailer_frame))

        except RpcError as e:
            self._reactor_error_response(
                response_sender, e.code, e.message, ser_mode,
            )
        except asyncio.TimeoutError:
            self._reactor_error_response(
                response_sender, StatusCode.DEADLINE_EXCEEDED,
                "deadline exceeded", ser_mode,
            )
        except Exception as e:
            logger.error("Reactor dispatch error: %s", e)
            self._reactor_error_response(
                response_sender, StatusCode.INTERNAL, str(e), ser_mode,
            )

    async def _dispatch_reactor_streaming(
        self,
        pattern: str,
        header: StreamHeader,
        service_info: ServiceInfo,
        method_info: MethodInfo,
        handler_method: Any,
        peer_id: str,
        first_request_payload: bytes,
        first_request_flags: int,
        response_sender: Any,
        request_receiver: Any,
    ) -> None:
        """Drive a streaming RPC through the reactor channels.

        Builds adapter objects that look like a QUIC bi-stream so the
        existing `_handle_server_stream` / `_handle_client_stream` /
        `_handle_bidi_stream` methods work unchanged. The first request
        frame is already inline on the reactor event; we reframe it into
        the recv adapter's buffer so ``framing.read_frame`` sees it as
        the first frame on the stream.
        """
        import struct as _struct

        first_frame_bytes = (
            _struct.pack("<I", len(first_request_payload) + 1)
            + bytes([first_request_flags])
            + first_request_payload
        )

        send_adapter = _ReactorSendAdapter(response_sender)
        recv_adapter = _ReactorRecvAdapter(request_receiver, first_frame_bytes)

        conn_ctx = ConnectionContext(
            connection=_ReactorConnStub(peer_id),  # type: ignore[arg-type]
            server=self,
        )

        if pattern == "server_stream":
            await self._handle_server_stream(
                conn_ctx, send_adapter, recv_adapter, header,
                handler_method, method_info,
            )
        elif pattern == "client_stream":
            await self._handle_client_stream(
                conn_ctx, send_adapter, recv_adapter, header,
                handler_method, method_info,
            )
        elif pattern == "bidi_stream":
            await self._handle_bidi_stream(
                conn_ctx, send_adapter, recv_adapter, header,
                handler_method, method_info,
            )
        else:
            # Shouldn't happen -- unary is handled inline above and these
            # are the only remaining patterns.
            self._reactor_error_response(
                response_sender, StatusCode.INTERNAL,
                f"unknown RPC pattern '{pattern}'",
                header.serializationMode,
            )

    def _reactor_error_response(
        self,
        response_sender: Any,
        code: StatusCode,
        message: str,
        serialization_mode: int = 0,
    ) -> None:
        import struct as _struct
        try:
            if serialization_mode == SerializationMode.JSON.value:
                trailer_payload = json_encode({
                    "code": code.value, "message": message,
                    "detailKeys": [], "detailValues": [],
                })
            else:
                status = RpcStatus(code=code, message=message)
                trailer_payload = self._codec.encode(status)
            trailer_frame = (
                _struct.pack("<I", len(trailer_payload) + 1)
                + bytes([TRAILER])
                + trailer_payload
            )
            response_sender.submit(b"", bytes(trailer_frame))
        except Exception as e:
            logger.error("Failed to send reactor error response: %s", e)

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

    def _resolve_instance(
        self,
        service_info: ServiceInfo,
        connection_id: int,
        session_id: int,
        peer_id: str,
    ) -> Any:
        """Spec §6 lookup-or-create for the reactor dispatch path.

        Mirrors Java ``AsterServer.resolveInstance`` post-Step-6.5:

        - SHARED service + ``sessionId == 0`` → cached shared instance.
        - SHARED service + ``sessionId != 0`` → FAILED_PRECONDITION.
        - SESSION service + ``sessionId == 0`` → FAILED_PRECONDITION.
        - SESSION service + ``sessionId > last_opened_session_id`` → create,
          subject to ``max_sessions_per_connection`` cap.
        - SESSION service + ``sessionId`` already active → reuse instance.
        - SESSION service + ``sessionId <= last_opened_session_id`` and not
          active → NOT_FOUND (graveyard).
        """
        is_session_scope = service_info.scoped == RpcScope.SESSION

        if not is_session_scope:
            if session_id != 0:
                raise _SessionScopeMismatch(
                    f"service '{service_info.name}' is SHARED but call "
                    f"carried sessionId={session_id}"
                )
            return self._get_handler_for_service(service_info)

        if session_id == 0:
            raise _SessionScopeMismatch(
                f"service '{service_info.name}' is SESSION-scoped; call "
                "must carry a non-zero sessionId"
            )

        impl_class = self._service_classes.get(
            (service_info.name, service_info.version)
        )
        if impl_class is None:
            raise ServiceNotFoundError(
                f"No implementation class registered for service "
                f"{service_info.name} v{service_info.version}"
            )

        state = self._connection_sessions.get(connection_id)
        if state is None:
            state = _ConnectionState(max_sessions=self._max_sessions_per_connection)
            self._connection_sessions[connection_id] = state

        key = (session_id, impl_class)
        if key in state.active_sessions:
            instance_key = (connection_id, session_id, impl_class)
            instance = self._reactor_session_instances.get(instance_key)
            if instance is None:
                # Shouldn't happen: active set and instance map are in lockstep.
                instance = impl_class(peer=peer_id)
                self._reactor_session_instances[instance_key] = instance
            return instance

        if session_id <= state.last_opened_session_id:
            raise _SessionNotFound(
                f"session {session_id} was previously opened on this "
                "connection and is now closed"
            )

        # Spec §7.5: cap counts active sessions only. A fresh sessionId
        # beyond the cap is rejected *without* bumping the graveyard
        # counter, so a retry with the same id surfaces RESOURCE_EXHAUSTED
        # again rather than flipping to NOT_FOUND.
        if len(state.active_sessions) >= state.max_sessions:
            raise _SessionLimitExceeded(
                f"connection has reached "
                f"max_sessions_per_connection={state.max_sessions}"
            )

        state.last_opened_session_id = session_id
        state.active_sessions.add(key)
        instance = impl_class(peer=peer_id)
        self._reactor_session_instances[(connection_id, session_id, impl_class)] = instance
        return instance

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
            deadline_secs=header.deadline,
            peer=peer,
            is_streaming=method_info.pattern != "unary",
            pattern=method_info.pattern,
            idempotent=method_info.idempotent,
            call_id=header.callId,
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

            # Invoke handler (async generator) with CallContext injection
            from aster.interceptors.base import invoke_handler_with_ctx, reset_call_context
            response_iter, _cv_token = invoke_handler_with_ctx(
                handler_method, request, call_ctx, method_info.accepts_ctx,
            )
            try:
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
            finally:
                reset_call_context(_cv_token)

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
            from aster.interceptors.base import invoke_handler_with_ctx, reset_call_context
            _cv_token = None
            try:
                try:
                    response, _cv_token = invoke_handler_with_ctx(
                        handler_method, request_iter(), call_ctx, method_info.accepts_ctx,
                    )
                    if asyncio.iscoroutine(response):
                        response = await asyncio.wait_for(response, timeout=timeout)
                except asyncio.TimeoutError:
                    await self._write_error_trailer(send, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
                    return
                response = await apply_response_interceptors(interceptors, call_ctx, response)
            finally:
                reset_call_context(_cv_token)

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

            # Invoke handler (async generator) with CallContext injection
            from aster.interceptors.base import invoke_handler_with_ctx, reset_call_context
            response_iter, _cv_token = invoke_handler_with_ctx(
                handler_method, request_iter(), call_ctx, method_info.accepts_ctx,
            )
            try:
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
            finally:
                reset_call_context(_cv_token)

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

