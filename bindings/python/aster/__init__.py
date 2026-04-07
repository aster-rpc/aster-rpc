"""
aster — Aster RPC framework + Iroh P2P networking bindings for Python.

This package provides:

  1. Low-level Iroh bindings: QUIC endpoints, content-addressed blobs,
     CRDT docs, gossip — all exposed as async Python APIs.

  2. The Aster RPC framework: contract-first RPC with Fory xlang
     serialization, consumer/producer admission, and a service registry.

Common entry points (all importable from the top-level ``aster`` package):

    from aster import IrohNode, create_endpoint_with_config, EndpointConfig
    from aster import Server, create_client, service, rpc

Sub-modules are also available directly::

    from aster.server import Server
    from aster.client import create_client
    from aster.trust.consumer import serve_consumer_admission
"""

# ── Native bindings (iroh transport layer) ───────────────────────────────────

try:
    from ._aster import (
        # Exception
        IrohError,
        BlobNotFound,
        DocNotFound,
        ConnectionError,
        TicketError,
        # Core node
        IrohNode,
        # Blobs
        BlobsClient,
        BlobStatusResult,
        BlobObserveResult,
        BlobLocalInfo,
        TagInfo,
        blobs_client,
        # Docs
        DocsClient,
        DocHandle,
        DocEntry,
        DocEvent,
        DocEventReceiver,
        DocDownloadPolicy,
        docs_client,
        # Gossip
        GossipClient,
        GossipTopicHandle,
        gossip_client,
        # Net / QUIC
        NodeAddr,
        EndpointConfig,
        ConnectionInfo,
        RemoteInfo,
        NetClient,
        IrohConnection,
        IrohSendStream,
        IrohRecvStream,
        net_client,
        create_endpoint,
        create_endpoint_with_config,
        # Hooks
        HookConnectInfo,
        HookHandshakeInfo,
        HookDecision,
        HookReceiver,
        HookRegistration,
        HookManager,
        NodeHookReceiver,
        NodeHookDecisionSender,
    )
except ImportError as e:
    raise ImportError(
        "Could not import native extension module. "
        "Please build the extension with 'maturin develop' first."
    ) from e

from .config import load_endpoint_config, AsterConfig

# ── Aster RPC framework ───────────────────────────────────────────────────────

from .status import StatusCode, RpcError
from .types import SerializationMode, RetryPolicy, ExponentialBackoff
from .framing import (
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
from .protocol import StreamHeader, CallHeader, RpcStatus
from .codec import (
    wire_type,
    ForyCodec,
    ForyConfig,
    DEFAULT_COMPRESSION_THRESHOLD,
)
from .transport.base import (
    Transport,
    BidiChannel,
    TransportError,
    ConnectionLostError,
)
from .transport.iroh import IrohTransport
from .transport.local import LocalTransport
from .metadata import Metadata
from .decorators import (
    service,
    rpc,
    server_stream,
    client_stream,
    bidi_stream,
    RpcPattern,
    ServiceInfo,
    MethodInfo,
)
from .service import ServiceRegistry, get_default_registry, set_default_registry
from .server import (
    Server,
    ServerError,
    ServiceNotFoundError,
    MethodNotFoundError,
    SerializationModeError,
)
from .client import (
    ServiceClient,
    create_client,
    create_local_client,
    ClientError,
    ClientTimeoutError,
)
from .high_level import AsterServer, AsterClient, RPC_ALPN
from .health import HealthServer, check_health, check_ready, metrics_snapshot
from .interceptors import (
    CallContext,
    Interceptor,
    DeadlineInterceptor,
    AuthInterceptor,
    RetryInterceptor,
    CircuitBreakerInterceptor,
    AuditLogInterceptor,
    MetricsInterceptor,
)

try:
    from importlib.metadata import version as _pkg_version
    __version__ = _pkg_version("aster-rpc")
except Exception:
    __version__ = "0.0.0-dev"

__all__ = [
    # ── Iroh native bindings ──
    "IrohError",
    "BlobNotFound",
    "DocNotFound",
    "ConnectionError",
    "TicketError",
    "IrohNode",
    "BlobsClient",
    "BlobStatusResult",
    "BlobObserveResult",
    "BlobLocalInfo",
    "TagInfo",
    "blobs_client",
    "DocsClient",
    "DocHandle",
    "DocEntry",
    "DocEvent",
    "DocEventReceiver",
    "DocDownloadPolicy",
    "docs_client",
    "GossipClient",
    "GossipTopicHandle",
    "gossip_client",
    "NodeAddr",
    "EndpointConfig",
    "ConnectionInfo",
    "RemoteInfo",
    "NetClient",
    "IrohConnection",
    "IrohSendStream",
    "IrohRecvStream",
    "net_client",
    "create_endpoint",
    "create_endpoint_with_config",
    "load_endpoint_config",
    "HookConnectInfo",
    "HookHandshakeInfo",
    "HookDecision",
    "HookReceiver",
    "HookRegistration",
    "HookManager",
    "NodeHookReceiver",
    "NodeHookDecisionSender",
    # ── Aster RPC framework ──
    "StatusCode",
    "RpcError",
    "SerializationMode",
    "RetryPolicy",
    "ExponentialBackoff",
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
    "StreamHeader",
    "CallHeader",
    "RpcStatus",
    "wire_type",
    "Metadata",
    "ForyCodec",
    "ForyConfig",
    "DEFAULT_COMPRESSION_THRESHOLD",
    "Transport",
    "BidiChannel",
    "TransportError",
    "ConnectionLostError",
    "IrohTransport",
    "LocalTransport",
    "service",
    "rpc",
    "server_stream",
    "client_stream",
    "bidi_stream",
    "RpcPattern",
    "ServiceInfo",
    "MethodInfo",
    "ServiceRegistry",
    "get_default_registry",
    "set_default_registry",
    "Server",
    "ServerError",
    "ServiceNotFoundError",
    "MethodNotFoundError",
    "SerializationModeError",
    "ServiceClient",
    "create_client",
    "create_local_client",
    "ClientError",
    "ClientTimeoutError",
    "CallContext",
    "Interceptor",
    "DeadlineInterceptor",
    "AuthInterceptor",
    "RetryInterceptor",
    "CircuitBreakerInterceptor",
    "AuditLogInterceptor",
    "MetricsInterceptor",
    # ── High-level declarative API ──
    "AsterConfig",
    "AsterServer",
    "AsterClient",
    "RPC_ALPN",
]
