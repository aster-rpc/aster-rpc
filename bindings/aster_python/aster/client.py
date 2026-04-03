"""
aster.client — Aster RPC client stub generation.

Spec reference: §8.2 (Client API), §8.3 (Local client)

This module provides client stub generation for Aster RPC services:
- create_client: Remote client over Iroh connection
- create_local_client: In-process client using LocalTransport
"""

from __future__ import annotations

import asyncio
import inspect
import time
import uuid
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, AsyncIterator, Callable

from aster_python.aster.codec import ForyCodec, ForyConfig
from aster_python.aster.protocol import StreamHeader, RpcStatus
from aster_python.aster.status import StatusCode, RpcError
from aster_python.aster.types import SerializationMode
from aster_python.aster.transport.base import Transport, BidiChannel
from aster_python.aster.service import ServiceInfo, MethodInfo, ServiceRegistry

if TYPE_CHECKING:
    import aster_python


# ── Client Errors ──────────────────────────────────────────────────────────────


class ClientError(Exception):
    """Base class for client errors."""
    pass


class ClientTimeoutError(ClientError):
    """Raised when a call exceeds its deadline."""
    pass


# ── Helper Functions ──────────────────────────────────────────────────────────


def _build_metadata(
    metadata: dict[str, str] | None,
) -> tuple[list[str], list[str]]:
    """Convert metadata dict to parallel key/value lists."""
    if not metadata:
        return [], []
    keys = list(metadata.keys())
    values = [metadata[k] for k in keys]
    return keys, values


# ── Client Stub Base ─────────────────────────────────────────────────────────


@dataclass
class MethodStub:
    """Metadata for a single method stub."""
    service_info: ServiceInfo
    method_info: MethodInfo


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

    @property
    def service_name(self) -> str:
        """The service name."""
        return self._service_info.name

    @property
    def service_version(self) -> int:
        """The service version."""
        return self._service_info.version

    def _get_deadline(self, timeout: float | None) -> int:
        """Convert a timeout to deadline_epoch_ms."""
        if timeout is None:
            return 0
        return int((time.time() + timeout) * 1000)

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
        serialization_mode = (
            serialization_override.value
            if serialization_override
            else method_info.serialization.value
            if method_info.serialization
            else self._service_info.serialization_modes[0].value
        )

        return await self._transport.unary(
            service=self._service_info.name,
            method=method_info.name,
            request=request,
            metadata=metadata,
            deadline_epoch_ms=deadline,
            serialization_mode=serialization_mode,
        )

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
        serialization_mode = (
            serialization_override.value
            if serialization_override
            else method_info.serialization.value
            if method_info.serialization
            else self._service_info.serialization_modes[0].value
        )

        return self._transport.server_stream(
            service=self._service_info.name,
            method=method_info.name,
            request=request,
            metadata=metadata,
            deadline_epoch_ms=deadline,
            serialization_mode=serialization_mode,
        )

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
        serialization_mode = (
            serialization_override.value
            if serialization_override
            else method_info.serialization.value
            if method_info.serialization
            else self._service_info.serialization_modes[0].value
        )

        return await self._transport.client_stream(
            service=self._service_info.name,
            method=method_info.name,
            requests=requests,
            metadata=metadata,
            deadline_epoch_ms=deadline,
            serialization_mode=serialization_mode,
        )

    def _call_bidi_stream(
        self,
        method_info: MethodInfo,
        metadata: dict[str, str] | None = None,
        timeout: float | None = None,
        serialization_override: SerializationMode | None = None,
    ) -> BidiChannel:
        """Make a bidirectional-streaming RPC call."""
        deadline = self._get_deadline(timeout)
        serialization_mode = (
            serialization_override.value
            if serialization_override
            else method_info.serialization.value
            if method_info.serialization
            else self._service_info.serialization_modes[0].value
        )

        return self._transport.bidi_stream(
            service=self._service_info.name,
            method=method_info.name,
            metadata=metadata,
            deadline_epoch_ms=deadline,
            serialization_mode=serialization_mode,
        )


# ── Dynamic Client Generation ─────────────────────────────────────────────────


def create_client(
    service_class: type,
    connection: "aster_python.IrohConnection",
    codec: ForyCodec | None = None,
    fory_config: ForyConfig | None = None,
    interceptors: list[Any] | None = None,
    registry: ServiceRegistry | None = None,
) -> ServiceClient:
    """Create a typed client stub for a remote service.

    Args:
        service_class: A class decorated with @service.
        connection: The Iroh connection to use for RPC calls.
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
    from aster_python.aster.decorators import _SERVICE_INFO_ATTR
    from aster_python.aster.transport.iroh import IrohTransport

    # Get service info
    service_info: ServiceInfo | None = getattr(service_class, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        raise ClientError(
            f"Class {service_class.__name__} is not decorated with @service"
        )

    # Create transport and codec
    if codec is None:
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=list(service_info.methods.values()) if service_info.methods else None,
            fory_config=fory_config,
        )

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
    from aster_python.aster.decorators import _SERVICE_INFO_ATTR, _METHOD_INFO_ATTR, RpcPattern
    from aster_python.aster.transport.local import LocalTransport

    # Get service info
    service_info: ServiceInfo | None = getattr(service_class, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        raise ClientError(
            f"Class {service_class.__name__} is not decorated with @service"
        )

    # Extract request/response types from all methods
    # Handle both actual types and string forward references
    request_response_types: set[type] = set()
    for method_info in service_info.methods.values():
        if method_info.request_type:
            if isinstance(method_info.request_type, type):
                request_response_types.add(method_info.request_type)
        if method_info.response_type:
            if isinstance(method_info.response_type, type):
                request_response_types.add(method_info.response_type)

    # If some types are string forward references (common when response types are
    # defined after the service class), scan the service class module for all
    # types with @fory_tag
    if len(request_response_types) < len(service_info.methods) * 2:
        module = getattr(service_class, '__module__', None)
        if module:
            import sys
            mod = sys.modules.get(module)
            if mod:
                # Find all classes with __fory_tag__ in the module
                for name, obj in inspect.getmembers(mod, inspect.isclass):
                    if hasattr(obj, '__fory_tag__'):
                        request_response_types.add(obj)

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
        interceptors=interceptors,
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
