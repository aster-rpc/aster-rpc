"""
aster_python - Python bindings for the Iroh P2P networking library.

This package provides Python access to Iroh's peer-to-peer networking
capabilities, including QUIC connections, content-addressed blob storage,
collaborative CRDT documents, and topic-based gossip messaging.

Phase 2: All bindings now use aster_transport_core as the backend.
Phase 1b: Includes datagram completion, connection info, remote-info, and hooks.
"""

# Import native extension module
try:
    from ._aster_python import (
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
        # Hooks (Phase 1b)
        HookConnectInfo,
        HookHandshakeInfo,
        HookDecision,
        HookReceiver,
        HookRegistration,
        HookManager,
    )
except ImportError as e:
    # Provide helpful error if native module not built
    raise ImportError(
        "Could not import native extension module. "
        "Please build the extension with 'maturin develop' first."
    ) from e

__version__ = "0.2.0"

__all__ = [
    # Exception
    "IrohError",
    "BlobNotFound",
    "DocNotFound",
    "ConnectionError",
    "TicketError",
    # Core node
    "IrohNode",
    # Blobs
    "BlobsClient",
    "BlobStatusResult",
    "BlobObserveResult",
    "BlobLocalInfo",
    "TagInfo",
    "blobs_client",
    # Docs
    "DocsClient",
    "DocHandle",
    "DocEntry",
    "DocEvent",
    "DocEventReceiver",
    "DocDownloadPolicy",
    "docs_client",
    # Gossip
    "GossipClient",
    "GossipTopicHandle",
    "gossip_client",
    # Net / QUIC
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
    # Hooks (Phase 1b)
    "HookConnectInfo",
    "HookHandshakeInfo",
    "HookDecision",
    "HookReceiver",
    "HookRegistration",
    "HookManager",
]
