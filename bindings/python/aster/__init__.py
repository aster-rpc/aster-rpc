"""
aster -- Aster RPC framework + Iroh P2P networking bindings for Python.

This package provides:

  1. Low-level Iroh bindings: QUIC endpoints, content-addressed blobs,
     CRDT docs, gossip -- all exposed as async Python APIs.

  2. The Aster RPC framework: contract-first RPC with Fory xlang
     serialization, Enrollment-based admission configured through
     AsterConfig, and a service registry.

Common entry points (all importable from the top-level ``aster`` package):

    from aster import Node, AsterConfig, SecretKey
    from aster import Server, create_client, service, rpc

    async with await Node.start(AsterConfig(storage_path="/var/lib/app")) as node:
        ...

Sub-modules are also available directly::

    from aster.server import Server
    from aster.client import create_client
"""

# ── uvloop auto-install ──────────────────────────────────────────────────────
# uvloop is a libuv-backed asyncio replacement that's ~20-25% faster on
# the Aster RPC hot path (per call: open_bi + write/read frames + finish,
# many event-loop yields). Auto-install when available; opt out with
# ASTER_NO_UVLOOP=1. Only takes effect if no event loop has been created
# yet -- safe to no-op if the user already has a loop running.
def _install_uvloop_if_available() -> None:
    import os
    if os.environ.get("ASTER_NO_UVLOOP") == "1":
        return
    try:
        import asyncio
        import uvloop
    except ImportError:
        return
    try:
        # Only install if no event loop policy has been explicitly set
        # away from the default. asyncio.get_event_loop_policy() returns
        # an instance of the default policy class until something changes it.
        current = asyncio.get_event_loop_policy()
        if isinstance(current, uvloop.EventLoopPolicy):
            return
        asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())
    except Exception:
        pass

_install_uvloop_if_available()

# ── Native bindings (iroh transport layer) ───────────────────────────────────

try:
    from ._aster import (
        VERSION,
        # Exception
        IrohError,
        BlobNotFound,
        DocNotFound,
        ConnectionError,
        TicketError,
        # Core node + typed ids
        Node,
        SecretKey,
        PublicKey,
        NodeId,
        Hash,
        # Blobs
        BlobsClient,
        BlobCustody,
        BlobFormat,
        FetchStrategy,
        FetchReport,
        ProviderOutcome,
        BlobTransferStats,
        BlobTransferCounts,
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
        DownloadPolicy,
        ShareMode,
        LocalAccess,
        AuthorityFingerprint,
        AuthorityReport,
        RetireOutcome,
        docs_client,
        # Gossip
        GossipClient,
        GossipTopicHandle,
        gossip_client,
        # Net / QUIC
        NodeAddr,
        EndpointConfig,
        ConnectionInfo,
        TransportSnapshot,
        PathInfo,
        RemoteInfo,
        NetClient,
        IrohConnection,
        IrohSendStream,
        IrohRecvStream,
        net_client,
        create_endpoint,
        create_endpoint_with_config,
        # Gate 0 (connection-level admission, Rust)
        TrustMode,
        GatePolicy,
        PeerAdmission,
        PeerAdmissionStore,
        Admission,
        HandshakeRequest,
        ConnectRequest,
        ALPN_PRODUCER_ADMISSION,
        ALPN_DELEGATED_ADMISSION,
        # Enrollment (driven by AsterServer / AsterClient from AsterConfig)
        ENROLLMENT_ALPN,
        Enrollment,
        EnrollmentPlan,
        # Ticket
        AsterTicket,
        # Trust config primitives
        NamespaceCapability,
        HpkeEnvelope,
        namespace_secret_id,
        hpke_envelope_alg,
        hpke_generate_keypair,
        hpke_public_key_from_private,
        hpke_seal,
        hpke_open,
        # HPKE keys derived from node identities
        hpke_x25519_secret_from_identity,
        hpke_x25519_public_from_identity,
        hpke_x25519_public_from_node_id,
    )
except ImportError as e:
    raise ImportError(
        "Could not import native extension module. "
        "Please build the extension with 'maturin develop' first."
    ) from e

# Former name of DownloadPolicy.
DocDownloadPolicy = DownloadPolicy

from .config import (
    AsterConfig, EnrollmentConfig, EnrollmentLimits, IdentityPublisherConfig,
    RoleMappingConfig, TrustedIssuer,
)
from . import identity
from .identity import (
    ForwardedIdentity, IdentityOverride, NodeConfigClient, NodeProperties, bearer_token,
)
from .provisioning import (
    ProvisioningError, GenesisVoter, GenesisSpec, JoinPermitSpec,
    VerifiedGenesis, VerifiedJoinPermit, canonical_genesis, canonical_join_permit,
    genesis_signing_bytes, join_signing_bytes, genesis_hash,
    signed_document_from_parts, canonical_candidate_ticket,
    verify_genesis, verify_join_permit_for_start,
)
from .audit_outbox import (
    AuditDestination, AuditOutboxConfig, AuditEvent, AuditOutbox,
    AuditOutboxError, AuditOutboxStatus, AuditDeliveryError,
    AuditDispositionReceipt, AuditQuarantineExport, AuditAcknowledgement,
)

# -- Sealed grants, leases, topology (plan P7) --
from .grants import (
    GRANT_AAD_DOMAIN,
    GRANT_RECORD_VERSION,
    GrantContext,
    GrantOutcome,
    GrantRecord,
    PolicyDoc,
    check_grant_role,
    grant_aad,
    grant_key,
    open_grant,
    open_grant_with_key,
    open_namespace_grant,
    reconcile_grant,
    reconcile_namespace_grant,
    seal_grant,
    seal_grant_to_key,
    seal_namespace_grant,
)
from .lease import (
    AcquireOutcome,
    Fence,
    FenceRejectedError,
    FencedResource,
    GrantorSerializer,
    Lease,
    LeaseAuthorizer,
    LeaseGrantorServer,
    LeaseOp,
    LeaseOptions,
    LeaseRowState,
    LeaseSerializer,
    LeaseSnapshot,
    LeaseTransition,
    MemorySerializer,
    TransitionOutcome,
    check_fence,
    lease_transition,
)
from .topology import (
    AdmittedFilter,
    ClusterView,
    PeerBlobStats,
    PeerView,
    QuicBytes,
    QuicTraffic,
    Topology,
    TopologySnapshot,
    TopologyTiming,
    TopologyView,
)

# -- Upcalls: Python-implemented Rust extension points (plan R6) --
from .upcalls import (
    AdmissionHandler,
    FenceOutcomeUnknownError,
    UpcallError,
    UpcallPolicy,
    UpcallRuntime,
)
from .expose import (
    EdgeRouter,
    Expose,
    ExposeAuthorizer,
    HttpBodyError,
    HttpBodyStream,
    HttpHandler,
    HttpRelayBody,
    HttpRequest,
    HttpResponse,
    HttpUpload,
    RoutePolicy,
    RouteRegistration,
    RouteSpec,
    open_relay,
    relay_http,
    relay_http_stream,
    relay_http_streaming,
    request_route,
)

# -- Coordination client (plan P9; the rest lives in aster.coordination) --
from .coordination import (
    CoordinationAwaitingAckError,
    CoordinationClient,
    CoordinationClosedError,
    CoordinationError,
    CoordinationFence,
    CoordinationInvalidArgumentError,
    CoordinationJournalFatalError,
    CoordinationJournalLockedError,
    CoordinationLease,
    CoordinationLeaseOutcome,
    CoordinationReadClient,
    CoordinationRejectedError,
    CoordinationRights,
    CoordinationRpcError,
    CoordinationUnavailableError,
    CoordinationUnresolvedError,
    PlacementManager,
    ResolvedMutation,
    fence_of,
    new_operation_id,
    placement_plan,
)

# ── Tunneling - handler-side ticket issuance (see aster.tunnel) ──────────────
from .tunnel import Tcp, Udp, HttpProxy, TunnelHandle, TunnelTarget

# ── Aster RPC framework ───────────────────────────────────────────────────────

from .status import StatusCode, RpcError
from .rpc_types import RpcScope, SerializationMode, RetryPolicy, ExponentialBackoff
from .wire import (
    COMPRESSED,
    TRAILER,
    HEADER,
    ROW_SCHEMA,
    CALL,
    CANCEL,
    END_STREAM,
    MAX_FRAME_SIZE,
    FramingError,
    EnvelopeError,
    write_frame,
    read_frame,
    StreamHeader,
    CallHeader,
    RpcStatus,
)
from .codec import (
    wire_type,
    contract_default,
    wire_name,
    ForyCodec,
    ForyConfig,
    DEFAULT_COMPRESSION_THRESHOLD,
)
from .scalars import ULID, ScalarError, decimal
from .transport.base import (
    Transport,
    BidiChannel,
    TransportError,
    ConnectionLostError,
)
from .transport.iroh import IrohTransport
from .transport.local import LocalTransport
from .metadata import Metadata, describe, Description
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
from .webtransport import WebTransportListener, WebTransportSession, WebTransportStream, WebTransportOptions
from .http import (HTTPListener, RPCAuthRequest, RPCAuthResponse, GeneratedCertificate,
                   generate_self_signed_certificate, generate_webtransport_certificate,
                   verify_certificate_binding)
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
    CallOptions,
)
from .runtime import AsterServer, AsterClient, ClientSession, AdmissionDeniedError, RPC_ALPN
from .capabilities import any_of, all_of
from .peer_events import (
    PeerConnected,
    PeerDisconnected,
    PeerEvents,
    PeerIdentity,
    PeerLagged,
    PeerSnapshot,
)
from ._aster import HealthServer, DrainReport, DRAIN_CLOSE_CODE, DRAINING_DETAIL_KEY
from . import metrics, tracing, observability
from .interceptors import (
    CallContext,
    Interceptor,
    DeadlineInterceptor,
    AuthInterceptor,
    RetryInterceptor,
    CircuitBreakerInterceptor,
    AuditLogInterceptor,
)

# The compiled constant remains correct even when the wheel was unpacked or
# vendored without its .dist-info metadata.
__version__ = VERSION

__all__ = [
    "ProvisioningError", "GenesisVoter", "GenesisSpec", "JoinPermitSpec",
    "VerifiedGenesis", "VerifiedJoinPermit", "canonical_genesis", "canonical_join_permit",
    "genesis_signing_bytes", "join_signing_bytes", "genesis_hash",
    "signed_document_from_parts", "canonical_candidate_ticket",
    "verify_genesis", "verify_join_permit_for_start",
    "WebTransportListener", "WebTransportSession", "WebTransportStream", "WebTransportOptions",
    "HTTPListener", "RPCAuthRequest", "RPCAuthResponse", "GeneratedCertificate",
    "generate_self_signed_certificate", "generate_webtransport_certificate",
    "verify_certificate_binding",
    "AuditDestination", "AuditOutboxConfig", "AuditEvent", "AuditOutbox",
    "AuditOutboxError", "AuditOutboxStatus", "AuditDeliveryError",
    "AuditDispositionReceipt", "AuditQuarantineExport", "AuditAcknowledgement",
    "VERSION",
    "__version__",
    # ── Iroh native bindings ──
    "IrohError",
    "BlobNotFound",
    "DocNotFound",
    "ConnectionError",
    "TicketError",
    "Node",
    "SecretKey",
    "PublicKey",
    "NodeId",
    "Hash",
    "BlobsClient",
    "BlobCustody",
    "BlobFormat",
    "FetchStrategy",
    "FetchReport",
    "ProviderOutcome",
    "BlobTransferStats",
    "BlobTransferCounts",
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
    "DownloadPolicy",
    "DocDownloadPolicy",
    "ShareMode",
    "LocalAccess",
    "AuthorityFingerprint",
    "AuthorityReport",
    "RetireOutcome",
    "docs_client",
    "GossipClient",
    "GossipTopicHandle",
    "gossip_client",
    "NodeAddr",
    "EndpointConfig",
    "ConnectionInfo",
    "TransportSnapshot",
    "PathInfo",
    "RemoteInfo",
    "NetClient",
    "IrohConnection",
    "IrohSendStream",
    "IrohRecvStream",
    "net_client",
    "create_endpoint",
    "create_endpoint_with_config",
    "TrustMode",
    "GatePolicy",
    "PeerAdmission",
    "PeerAdmissionStore",
    "Admission",
    "HandshakeRequest",
    "ConnectRequest",
    "ALPN_PRODUCER_ADMISSION",
    "ALPN_DELEGATED_ADMISSION",
    "ENROLLMENT_ALPN",
    "Enrollment",
    "EnrollmentPlan",
    "AsterTicket",
    "NamespaceCapability",
    "HpkeEnvelope",
    "namespace_secret_id",
    "hpke_envelope_alg",
    "hpke_generate_keypair",
    "hpke_public_key_from_private",
    "hpke_seal",
    "hpke_open",
    "hpke_x25519_secret_from_identity",
    "hpke_x25519_public_from_identity",
    "hpke_x25519_public_from_node_id",
    # -- Sealed grants, leases, topology (plan P7) --
    "GRANT_AAD_DOMAIN",
    "GRANT_RECORD_VERSION",
    "GrantContext",
    "GrantOutcome",
    "GrantRecord",
    "PolicyDoc",
    "check_grant_role",
    "grant_aad",
    "grant_key",
    "open_grant",
    "open_grant_with_key",
    "open_namespace_grant",
    "reconcile_grant",
    "reconcile_namespace_grant",
    "seal_grant",
    "seal_grant_to_key",
    "seal_namespace_grant",
    "AcquireOutcome",
    "Fence",
    "FenceRejectedError",
    "FencedResource",
    "GrantorSerializer",
    "Lease",
    "LeaseAuthorizer",
    "LeaseGrantorServer",
    "LeaseOp",
    "LeaseOptions",
    "LeaseRowState",
    "LeaseSerializer",
    "LeaseSnapshot",
    "LeaseTransition",
    "MemorySerializer",
    "TransitionOutcome",
    "check_fence",
    "lease_transition",
    "AdmittedFilter",
    "ClusterView",
    "PeerBlobStats",
    "PeerView",
    "QuicBytes",
    "QuicTraffic",
    "Topology",
    "TopologySnapshot",
    "TopologyTiming",
    "TopologyView",
    # -- Upcalls (plan R6) --
    "AdmissionHandler",
    "FenceOutcomeUnknownError",
    "UpcallError",
    "UpcallPolicy",
    "UpcallRuntime",
    "EdgeRouter",
    "Expose",
    "ExposeAuthorizer",
    "HttpBodyError",
    "HttpBodyStream",
    "HttpHandler",
    "HttpRelayBody",
    "HttpRequest",
    "HttpResponse",
    "HttpUpload",
    "RoutePolicy",
    "RouteRegistration",
    "RouteSpec",
    "open_relay",
    "relay_http",
    "relay_http_stream",
    "relay_http_streaming",
    "request_route",
    # -- Coordination client (plan P9) --
    "CoordinationAwaitingAckError",
    "CoordinationClient",
    "CoordinationClosedError",
    "CoordinationError",
    "CoordinationFence",
    "CoordinationInvalidArgumentError",
    "CoordinationJournalFatalError",
    "CoordinationJournalLockedError",
    "CoordinationLease",
    "CoordinationLeaseOutcome",
    "CoordinationReadClient",
    "CoordinationRejectedError",
    "CoordinationRights",
    "CoordinationRpcError",
    "CoordinationUnavailableError",
    "CoordinationUnresolvedError",
    "PlacementManager",
    "ResolvedMutation",
    "fence_of",
    "new_operation_id",
    "placement_plan",
    # ── Aster RPC framework ──
    "StatusCode",
    "RpcError",
    "RpcScope",
    "SerializationMode",
    "RetryPolicy",
    "ExponentialBackoff",
    "COMPRESSED",
    "TRAILER",
    "HEADER",
    "ROW_SCHEMA",
    "CALL",
    "CANCEL",
    "END_STREAM",
    "MAX_FRAME_SIZE",
    "FramingError",
    "EnvelopeError",
    "write_frame",
    "read_frame",
    "StreamHeader",
    "CallHeader",
    "RpcStatus",
    "wire_type",
    "contract_default",
    "wire_name",
    "ULID",
    "ScalarError",
    "decimal",
    "Metadata",
    "describe",
    "Description",
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
    "CallOptions",
    "CallContext",
    "Interceptor",
    "DeadlineInterceptor",
    "AuthInterceptor",
    "RetryInterceptor",
    "CircuitBreakerInterceptor",
    "AuditLogInterceptor",
    # ── Observability (Rust pipeline; see aster.observability) ──
    "HealthServer",
    "DrainReport",
    "PeerConnected",
    "PeerDisconnected",
    "PeerEvents",
    "PeerIdentity",
    "PeerLagged",
    "PeerSnapshot",
    "DRAIN_CLOSE_CODE",
    "DRAINING_DETAIL_KEY",
    "metrics",
    "tracing",
    "observability",
    # ── High-level declarative API ──
    "AsterConfig",
    "EnrollmentConfig",
    "EnrollmentLimits",
    "RoleMappingConfig",
    "TrustedIssuer",
    "IdentityPublisherConfig",
    "identity",
    "ForwardedIdentity",
    "IdentityOverride",
    "NodeConfigClient",
    "NodeProperties",
    "bearer_token",
    "AsterServer",
    "AsterClient",
    "ClientSession",
    "AdmissionDeniedError",
    "RPC_ALPN",
    # ── Tunneling ──
    "Tcp",
    "Udp",
    "HttpProxy",
    "TunnelHandle",
    "TunnelTarget",
]
