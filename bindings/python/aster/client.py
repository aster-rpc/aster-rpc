"""aster.client -- Aster RPC client stub generation.

Spec reference: §8.2 (Client API), §8.3 (Local client)

This module provides client stub generation for Aster RPC services:
- ``create_client``: remote client over an Iroh connection or arbitrary transport
- ``create_local_client``: in-process client using ``LocalTransport``
"""

from __future__ import annotations

import inspect
import time
from typing import TYPE_CHECKING, Any, AsyncIterator

from aster.codec import ForyCodec, ForyConfig
from aster.interceptors.base import (
    CallContext,
    apply_error_interceptors,
    apply_request_interceptors,
    apply_response_interceptors,
    build_call_context,
    normalize_error,
)
from aster.interceptors.circuit_breaker import CircuitBreakerInterceptor
from aster.interceptors.deadline import DeadlineInterceptor
from aster.interceptors.retry import RetryInterceptor
from aster.status import RpcError
from aster.rpc_types import RpcScope, SerializationMode
from aster.transport.base import Transport, BidiChannel
from aster.service import ServiceInfo, MethodInfo, ServiceRegistry

if TYPE_CHECKING:
    import aster


# ── Client Errors ──────────────────────────────────────────────────────────────


class ClientError(Exception):
    """Base class for client errors."""
    pass


class ClientTimeoutError(ClientError):
    """Raised when a call exceeds its deadline."""
    pass


# ── Client Stub Base ─────────────────────────────────────────────────────────


class ServiceClient:
    """Base class for generated service clients.

    Subclasses are typically generated dynamically by create_client() or
    create_local_client(), but can also be subclassed directly for testing.
    """

    def __init__(
        self,
        transport: Transport,
        service_info: ServiceInfo,
        codec: ForyCodec,
        interceptors: list[Any] | None = None,
    ) -> None:
        self._transport = transport
        self._service_info = service_info
        self._codec = codec
        self._interceptors = list(interceptors) if interceptors else []

    async def close(self) -> None:
        """Close the underlying transport if it exposes resources."""
        await self._transport.close()

    @property
    def service_name(self) -> str:
        """The service name."""
        return self._service_info.name

    @property
    def service_version(self) -> int:
        """The service version."""
        return self._service_info.version

    # Default set of modes the client can handle.  Covers all standard modes.
    _CLIENT_SUPPORTED_MODES: set[SerializationMode] = {
        SerializationMode.XLANG,
        SerializationMode.NATIVE,
    }

    def _get_deadline(self, timeout: float | None) -> int:
        """Convert a timeout to deadline_epoch_ms."""
        if timeout is None:
            return 0
        return int((time.time() + timeout) * 1000)

    def _resolve_serialization_mode(
        self,
        method_info: MethodInfo,
        override: SerializationMode | None,
    ) -> int:
        """Resolve the serialization mode for a call per spec S6.2.1.

        Priority:
        1. Explicit per-call override
        2. Method-level annotation
        3. Negotiated from the producer's preference list
        """
        if override is not None:
            return override.value
        if method_info.serialization is not None:
            return method_info.serialization.value
        negotiated = _negotiate_serialization_mode(
            self._service_info.serialization_modes,
            self._CLIENT_SUPPORTED_MODES,
        )
        return negotiated.value

    def _build_context(
        self,
        method_info: MethodInfo,
        *,
        metadata: dict[str, str] | None,
        deadline_epoch_ms: int,
    ) -> CallContext:
        is_streaming = method_info.pattern != RpcPattern.UNARY
        return build_call_context(
            service=self._service_info.name,
            method=method_info.name,
            metadata=metadata,
            deadline_epoch_ms=deadline_epoch_ms,
            is_streaming=is_streaming,
            pattern=method_info.pattern,
            idempotent=method_info.idempotent,
        )

    def _matching_interceptors(self, interceptor_type: type[Any]) -> list[Any]:
        return [i for i in self._interceptors if isinstance(i, interceptor_type)]

    def _deadline_timeout(self, ctx: CallContext) -> float | None:
        for interceptor in self._matching_interceptors(DeadlineInterceptor):
            timeout = interceptor.timeout_seconds(ctx)
            if timeout is not None:
                return timeout
        return None

    async def _run_call_with_interceptors(
        self,
        ctx: CallContext,
        request: Any,
        invoke: Any,
    ) -> Any:
        retry_interceptors = self._matching_interceptors(RetryInterceptor)
        breaker_interceptors = self._matching_interceptors(CircuitBreakerInterceptor)
        max_attempts = 1
        for retry in retry_interceptors:
            max_attempts = max(max_attempts, retry.policy.max_attempts)

        last_error: RpcError | None = None

        for attempt in range(1, max_attempts + 1):
            ctx.attempt = attempt
            current_request = request
            for breaker in breaker_interceptors:
                breaker.before_call(ctx)

            try:
                current_request = await apply_request_interceptors(self._interceptors, ctx, current_request)
                timeout = self._deadline_timeout(ctx)
                if timeout is not None:
                    response = await self._invoke_with_timeout(invoke(current_request), timeout)
                else:
                    response = await invoke(current_request)
                response = await apply_response_interceptors(self._interceptors, ctx, response)
                for breaker in breaker_interceptors:
                    breaker.record_success()
                return response
            except Exception as exc:
                error = normalize_error(exc)
                for breaker in breaker_interceptors:
                    breaker.record_failure(error)
                maybe_error = await apply_error_interceptors(self._interceptors, ctx, error)
                if maybe_error is None:
                    return None
                last_error = maybe_error
                should_retry = False
                retry_delay = 0.0
                for retry in retry_interceptors:
                    if retry.should_retry(ctx, maybe_error) and attempt < retry.policy.max_attempts:
                        should_retry = True
                        retry_delay = max(retry_delay, retry.backoff_seconds(attempt))
                if not should_retry:
                    raise maybe_error
                await self._sleep_with_deadline(retry_delay, ctx)

        if last_error is not None:
            raise last_error
        raise RpcError.from_status(13, "call failed")

    async def _invoke_with_timeout(self, awaitable: Any, timeout: float) -> Any:
        async with timeouts(timeout):
            return await awaitable

    async def _sleep_with_deadline(self, delay: float, ctx: CallContext) -> None:
        timeout = self._deadline_timeout(ctx)
        if timeout is None:
            await time_sleep(delay)
            return
        if timeout <= 0:
            raise RpcError.from_status(4, "deadline exceeded")
        async with timeouts(timeout):
            await time_sleep(delay)

    async def _call_unary(
        self,
        method_info: MethodInfo,
        request: Any,
        metadata: dict[str, str] | None = None,
        timeout: float | None = None,
        serialization_override: SerializationMode | None = None,
    ) -> Any:
        """Make a unary RPC call."""
        deadline = self._get_deadline(timeout)
        serialization_mode = self._resolve_serialization_mode(method_info, serialization_override)

        ctx = self._build_context(method_info, metadata=metadata, deadline_epoch_ms=deadline)

        async def invoke(current_request: Any) -> Any:
            return await self._transport.unary(
                service=self._service_info.name,
                method=method_info.name,
                request=current_request,
                metadata=ctx.metadata,
                deadline_epoch_ms=deadline,
                serialization_mode=serialization_mode,

            )

        return await self._run_call_with_interceptors(ctx, request, invoke)

    def _call_server_stream(
        self,
        method_info: MethodInfo,
        request: Any,
        metadata: dict[str, str] | None = None,
        timeout: float | None = None,
        serialization_override: SerializationMode | None = None,
    ) -> AsyncIterator[Any]:
        """Make a server-streaming RPC call."""
        deadline = self._get_deadline(timeout)
        serialization_mode = self._resolve_serialization_mode(method_info, serialization_override)


        ctx = self._build_context(method_info, metadata=metadata, deadline_epoch_ms=deadline)

        async def iterator() -> AsyncIterator[Any]:
            current_request = await apply_request_interceptors(self._interceptors, ctx, request)
            source = self._transport.server_stream(
                service=self._service_info.name,
                method=method_info.name,
                request=current_request,
                metadata=ctx.metadata,
                deadline_epoch_ms=deadline,
                serialization_mode=serialization_mode,

            )
            try:
                async for item in source:
                    yield await apply_response_interceptors(self._interceptors, ctx, item)
            except Exception as exc:
                error = normalize_error(exc)
                maybe_error = await apply_error_interceptors(self._interceptors, ctx, error)
                if maybe_error is not None:
                    raise maybe_error

        return iterator()

    async def _call_client_stream(
        self,
        method_info: MethodInfo,
        requests: AsyncIterator[Any],
        metadata: dict[str, str] | None = None,
        timeout: float | None = None,
        serialization_override: SerializationMode | None = None,
    ) -> Any:
        """Make a client-streaming RPC call."""
        deadline = self._get_deadline(timeout)
        serialization_mode = self._resolve_serialization_mode(method_info, serialization_override)


        ctx = self._build_context(method_info, metadata=metadata, deadline_epoch_ms=deadline)

        async def wrapped_requests() -> AsyncIterator[Any]:
            async for item in requests:
                yield await apply_request_interceptors(self._interceptors, ctx, item)

        async def invoke(_: Any) -> Any:
            return await self._transport.client_stream(
                service=self._service_info.name,
                method=method_info.name,
                requests=wrapped_requests(),
                metadata=ctx.metadata,
                deadline_epoch_ms=deadline,
                serialization_mode=serialization_mode,

            )

        return await self._run_call_with_interceptors(ctx, None, invoke)

    def _call_bidi_stream(
        self,
        method_info: MethodInfo,
        metadata: dict[str, str] | None = None,
        timeout: float | None = None,
        serialization_override: SerializationMode | None = None,
    ) -> BidiChannel:
        """Make a bidirectional-streaming RPC call."""
        deadline = self._get_deadline(timeout)
        serialization_mode = self._resolve_serialization_mode(method_info, serialization_override)


        ctx = self._build_context(method_info, metadata=metadata, deadline_epoch_ms=deadline)
        channel = self._transport.bidi_stream(
            service=self._service_info.name,
            method=method_info.name,
            metadata=ctx.metadata,
            deadline_epoch_ms=deadline,
            serialization_mode=serialization_mode,
        )
        if not self._interceptors:
            return channel
        return InterceptedBidiChannel(channel, self._interceptors, ctx)


class InterceptedBidiChannel(BidiChannel):
    def __init__(self, inner: BidiChannel, interceptors: list[Any], ctx: CallContext) -> None:
        self._inner = inner
        self._interceptors = interceptors
        self._ctx = ctx

    async def send(self, msg: Any) -> None:
        msg = await apply_request_interceptors(self._interceptors, self._ctx, msg)
        await self._inner.send(msg)

    async def recv(self) -> Any:
        try:
            item = await self._inner.recv()
            return await apply_response_interceptors(self._interceptors, self._ctx, item)
        except Exception as exc:
            error = normalize_error(exc)
            maybe_error = await apply_error_interceptors(self._interceptors, self._ctx, error)
            if maybe_error is not None:
                raise maybe_error
            raise

    async def close(self) -> None:
        await self._inner.close()

    async def wait_for_trailer(self) -> tuple[int, str]:
        return await self._inner.wait_for_trailer()

    async def __aenter__(self) -> "InterceptedBidiChannel":
        if hasattr(self._inner, "__aenter__"):
            await self._inner.__aenter__()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb) -> None:
        if hasattr(self._inner, "__aexit__"):
            await self._inner.__aexit__(exc_type, exc_val, exc_tb)
        else:
            await self.close()


def _collect_service_types(service_class: type, service_info: ServiceInfo) -> set[type]:
    """Collect concrete message types referenced by a service definition."""
    request_response_types: set[type] = set()

    for method_info in service_info.methods.values():
        if isinstance(method_info.request_type, type):
            request_response_types.add(method_info.request_type)
        if isinstance(method_info.response_type, type):
            request_response_types.add(method_info.response_type)

    # If forward refs are present, fall back to scanning the defining module for
    # tagged classes so implicit codec creation still succeeds in common cases.
    if len(request_response_types) < len(service_info.methods) * 2:
        module = getattr(service_class, "__module__", None)
        if module:
            import sys

            mod = sys.modules.get(module)
            if mod:
                for _, obj in inspect.getmembers(mod, inspect.isclass):
                    if hasattr(obj, "__wire_type__"):
                        request_response_types.add(obj)

    return request_response_types


# ── Dynamic Client Generation ─────────────────────────────────────────────────


def create_client(
    service_class: type,
    connection: "aster.IrohConnection | None" = None,
    transport: Transport | None = None,
    codec: ForyCodec | None = None,
    fory_config: ForyConfig | None = None,
    interceptors: list[Any] | None = None,
    registry: ServiceRegistry | None = None,
) -> ServiceClient:
    """Create a typed client stub for a remote service.

    Args:
        service_class: A class decorated with @service.
        connection: The Iroh connection to use for RPC calls.
        transport: Optional pre-built transport implementation.
        codec: The ForyCodec for serialization. Defaults to XLANG mode.
        fory_config: Optional configuration for implicitly created codecs.
        interceptors: List of interceptor instances to apply to all calls.
        registry: Optional ServiceRegistry for looking up service info.

    Returns:
        A ServiceClient instance with typed method stubs.

    Example::

        client = create_client(EchoService, connection)
        response = await client.echo(EchoRequest(message="hello"))
    """
    from aster.decorators import _SERVICE_INFO_ATTR
    from aster.transport.iroh import IrohTransport

    # Get service info
    service_info: ServiceInfo | None = getattr(service_class, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        raise ClientError(
            f"Class {service_class.__name__} is not decorated with @service"
        )

    # Create codec if needed.
    if codec is None:
        request_response_types = _collect_service_types(service_class, service_info)
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=list(request_response_types) if request_response_types else None,
            fory_config=fory_config,
        )

    # Session-scoped services need a SessionStub via create_session (async).
    # create_client is sync -- callers using AsterClient.client() get the
    # async dispatch automatically; direct callers should use create_session
    # explicitly when working with session-scoped services.
    if service_info.scoped == RpcScope.SESSION:
        raise ClientError(
            f"{service_class.__name__} is session-scoped and cannot be created "
            f"via create_client(). Use aster.session.create_session() (async) "
            f"or AsterClient.client() which handles both cases."
        )

    if transport is None:
        if connection is None:
            raise ClientError("create_client requires either connection or transport")
        transport = IrohTransport(connection=connection, codec=codec)

    # Create client class dynamically
    client_cls = _generate_client_class(service_info)

    return client_cls(
        transport=transport,
        service_info=service_info,
        codec=codec,
        interceptors=interceptors,
    )


def create_local_client(
    service_class: type,
    implementation: Any,
    wire_compatible: bool = True,
    codec: ForyCodec | None = None,
    fory_config: ForyConfig | None = None,
    interceptors: list[Any] | None = None,
) -> ServiceClient:
    """Create a typed client stub for an in-process service.

    Uses LocalTransport internally for zero-copy in-memory calls.

    Args:
        service_class: A class decorated with @service.
        implementation: The service implementation instance.
        wire_compatible: If True, exercises full serialization pipeline.
            Defaults to True for conformance testing.
        codec: The ForyCodec for serialization.
        fory_config: Optional configuration for implicitly created codecs.
        interceptors: List of interceptor instances to apply to all calls.

    Returns:
        A ServiceClient instance with typed method stubs.

    Example::

        client = create_local_client(EchoService, EchoServiceImpl())
        response = await client.echo(EchoRequest(message="hello"))
    """
    from aster.decorators import _SERVICE_INFO_ATTR, _METHOD_INFO_ATTR
    from aster.transport.local import LocalTransport

    # Get service info
    service_info: ServiceInfo | None = getattr(service_class, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        raise ClientError(
            f"Class {service_class.__name__} is not decorated with @service"
        )

    request_response_types = _collect_service_types(service_class, service_info)

    # Create codec
    if codec is None:
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=request_response_types if request_response_types else None,
            fory_config=fory_config,
        )

    # Build handler registry for LocalTransport
    def handler_registry(service: str, method: str):
        """Look up handler from the implementation."""
        handler = getattr(implementation, method, None)
        if handler is None:
            raise ClientError(f"No handler for {service}/{method}")

        # Get method info from the service class
        method_info: MethodInfo | None = getattr(
            getattr(service_class, method, None),
            _METHOD_INFO_ATTR,
            None
        )

        pattern = "unary"
        if method_info:
            pattern = method_info.pattern

        types = []
        if method_info:
            if method_info.request_type:
                types.append(method_info.request_type)
            if method_info.response_type:
                types.append(method_info.response_type)

        return handler, types, pattern

    transport = LocalTransport(
        handler_registry=handler_registry,
        codec=codec,
        wire_compatible=wire_compatible,
        interceptors=interceptors,
    )

    # Create client class dynamically
    client_cls = _generate_client_class(service_info)

    return client_cls(
        transport=transport,
        service_info=service_info,
        codec=codec,
        interceptors=None,
    )


def _generate_client_class(service_info: ServiceInfo) -> type:
    """Generate a ServiceClient subclass with typed method stubs."""

    class GeneratedClient(ServiceClient):
        """Generated client for {service_info.name}."""

    # Add methods for each RPC
    for method_name, method_info in service_info.methods.items():
        _add_method_stub(GeneratedClient, method_name, method_info, service_info)

    GeneratedClient.__name__ = f"{service_info.name}Client"
    GeneratedClient.__doc__ = f"Client for {service_info.name} v{service_info.version}"

    return GeneratedClient


def _add_method_stub(
    cls: type,
    method_name: str,
    method_info: MethodInfo,
    service_info: ServiceInfo | None = None,
) -> None:
    """Add a method stub to a client class."""

    if method_info.pattern == RpcPattern.UNARY:
        async def stub(
            self: ServiceClient,
            request: Any,
            *,
            metadata: dict[str, str] | None = None,
            timeout: float | None = None,
            serialization: SerializationMode | None = None,
        ) -> Any:
            """Unary RPC call."""
            return await self._call_unary(
                method_info=method_info,
                request=request,
                metadata=metadata,
                timeout=timeout,
                serialization_override=serialization,
            )

    elif method_info.pattern == RpcPattern.SERVER_STREAM:
        def stub(
            self: ServiceClient,
            request: Any,
            *,
            metadata: dict[str, str] | None = None,
            timeout: float | None = None,
            serialization: SerializationMode | None = None,
        ) -> AsyncIterator[Any]:
            """Server-streaming RPC call."""
            return self._call_server_stream(
                method_info=method_info,
                request=request,
                metadata=metadata,
                timeout=timeout,
                serialization_override=serialization,
            )

    elif method_info.pattern == RpcPattern.CLIENT_STREAM:
        async def stub(
            self: ServiceClient,
            requests: AsyncIterator[Any],
            *,
            metadata: dict[str, str] | None = None,
            timeout: float | None = None,
            serialization: SerializationMode | None = None,
        ) -> Any:
            """Client-streaming RPC call."""
            return await self._call_client_stream(
                method_info=method_info,
                requests=requests,
                metadata=metadata,
                timeout=timeout,
                serialization_override=serialization,
            )

    elif method_info.pattern == RpcPattern.BIDI_STREAM:
        def stub(
            self: ServiceClient,
            *,
            metadata: dict[str, str] | None = None,
            timeout: float | None = None,
            serialization: SerializationMode | None = None,
        ) -> BidiChannel:
            """Bidirectional-streaming RPC call."""
            return self._call_bidi_stream(
                method_info=method_info,
                metadata=metadata,
                timeout=timeout,
                serialization_override=serialization,
            )

    else:
        raise ClientError(f"Unknown RPC pattern: {method_info.pattern}")

    # Set method metadata
    stub.__name__ = method_name
    svc_name = service_info.name if service_info else "UnknownService"
    stub.__doc__ = f"{method_info.pattern} RPC to {svc_name}.{method_name}"

    setattr(cls, method_name, stub)


# ── RpcPattern import for internal use ──────────────────────────────────────


class RpcPattern:
    """RPC pattern enumeration."""
    UNARY = "unary"
    SERVER_STREAM = "server_stream"
    CLIENT_STREAM = "client_stream"
    BIDI_STREAM = "bidi_stream"


def _negotiate_serialization_mode(
    producer_modes: list[SerializationMode],
    client_modes: set[SerializationMode],
) -> SerializationMode:
    """Select serialization mode per spec S6.2.1.

    Walks the producer's preference list and picks the first mode the client
    also supports. Falls back to the producer's first advertised mode if there
    is no overlap (server will reject with INVALID_ARGUMENT if truly
    unsupported).
    """
    for mode in producer_modes:
        if mode in client_modes:
            return mode
    # Fallback: use producer's first preference
    if producer_modes:
        return producer_modes[0]
    return SerializationMode.XLANG


def time_sleep(delay: float):
    return __import__("asyncio").sleep(delay)


def timeouts(timeout: float):
    return __import__("asyncio").timeout(timeout)
