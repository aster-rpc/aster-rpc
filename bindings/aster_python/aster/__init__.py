"""
aster — Aster RPC framework for Python.

This package implements the Aster RPC protocol (spec v0.7.1) on top of
the existing iroh transport bindings in ``aster_python``.

Phase 1 exports: wire protocol types and framing utilities.
Phase 2 exports: Fory serialization codec.
Phase 3 exports: Transport abstraction (IrohTransport, LocalTransport).
Phase 4 exports: Service definition (decorators, service registry).
"""

from aster_python.aster.status import StatusCode, RpcError
from aster_python.aster.types import SerializationMode, RetryPolicy, ExponentialBackoff
from aster_python.aster.framing import (
    COMPRESSED,
    TRAILER,
    HEADER,
    ROW_SCHEMA,
    CALL,
    CANCEL,
    MAX_FRAME_SIZE,
    FramingError,
    write_frame,
    read_frame,
)
from aster_python.aster.protocol import StreamHeader, CallHeader, RpcStatus
from aster_python.aster.codec import (
    fory_tag,
    ForyCodec,
    ForyConfig,
    DEFAULT_COMPRESSION_THRESHOLD,
)
from aster_python.aster.transport.base import (
    Transport,
    BidiChannel,
    TransportError,
    ConnectionLostError,
)
from aster_python.aster.transport.iroh import IrohTransport
from aster_python.aster.transport.local import LocalTransport

# Phase 4: Service definition layer
from aster_python.aster.decorators import (
    service,
    rpc,
    server_stream,
    client_stream,
    bidi_stream,
    RpcPattern,
    ServiceInfo,
    MethodInfo,
)
from aster_python.aster.service import ServiceRegistry, get_default_registry, set_default_registry
from aster_python.aster.server import (
    Server,
    ServerError,
    ServiceNotFoundError,
    MethodNotFoundError,
    SerializationModeError,
)
from aster_python.aster.client import (
    ServiceClient,
    create_client,
    create_local_client,
    ClientError,
    ClientTimeoutError,
)

__all__ = [
    # status.py
    "StatusCode",
    "RpcError",
    # types.py
    "SerializationMode",
    "RetryPolicy",
    "ExponentialBackoff",
    # framing.py
    "COMPRESSED",
    "TRAILER",
    "HEADER",
    "ROW_SCHEMA",
    "CALL",
    "CANCEL",
    "MAX_FRAME_SIZE",
    "FramingError",
    "write_frame",
    "read_frame",
    # protocol.py
    "StreamHeader",
    "CallHeader",
    "RpcStatus",
    # codec.py
    "fory_tag",
    "ForyCodec",
    "ForyConfig",
    "DEFAULT_COMPRESSION_THRESHOLD",
    # transport/base.py
    "Transport",
    "BidiChannel",
    "TransportError",
    "ConnectionLostError",
    # transport/iroh.py
    "IrohTransport",
    # transport/local.py
    "LocalTransport",
    # decorators.py (Phase 4)
    "service",
    "rpc",
    "server_stream",
    "client_stream",
    "bidi_stream",
    "RpcPattern",
    "ServiceInfo",
    "MethodInfo",
    # service.py (Phase 4)
    "ServiceRegistry",
    "get_default_registry",
    "set_default_registry",
    # server.py (Phase 5)
    "Server",
    "ServerError",
    "ServiceNotFoundError",
    "MethodNotFoundError",
    "SerializationModeError",
    # client.py (Phase 6)
    "ServiceClient",
    "create_client",
    "create_local_client",
    "ClientError",
    "ClientTimeoutError",
]
