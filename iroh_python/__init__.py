"""
iroh_python - Python bindings for the Iroh P2P networking library.

This package provides Python access to Iroh's peer-to-peer networking
capabilities, including QUIC connections, content-addressed blob storage,
collaborative CRDT documents, and topic-based gossip messaging.
"""

# Import native extension module
try:
    from iroh_python._iroh_python import (
        # Exception
        IrohError,
        # Core node
        IrohNode,
        # Blobs
        BlobsClient,
        blobs_client,
        # Docs
        DocsClient,
        DocHandle,
        docs_client,
        # Gossip
        GossipClient,
        GossipTopicHandle,
        gossip_client,
        # Net / QUIC
        NetClient,
        IrohConnection,
        IrohSendStream,
        IrohRecvStream,
        net_client,
        create_endpoint,
    )
except ImportError as e:
    # Provide helpful error if native module not built
    raise ImportError(
        "Could not import native extension module. "
        "Please build the extension with 'maturin develop' first."
    ) from e

__version__ = "0.1.0"

__all__ = [
    # Exception
    "IrohError",
    # Core node
    "IrohNode",
    # Blobs
    "BlobsClient",
    "blobs_client",
    # Docs
    "DocsClient",
    "DocHandle",
    "docs_client",
    # Gossip
    "GossipClient",
    "GossipTopicHandle",
    "gossip_client",
    # Net / QUIC
    "NetClient",
    "IrohConnection",
    "IrohSendStream",
    "IrohRecvStream",
    "net_client",
    "create_endpoint",
]