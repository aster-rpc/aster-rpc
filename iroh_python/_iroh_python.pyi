"""Type stubs for the iroh_python native extension module."""

from typing import Any, Optional, Tuple, Coroutine

class IrohError(Exception):
    """Exception raised by Iroh operations."""
    ...

class IrohNode:
    """Composite wrapper for an Iroh node with all protocols."""

    @staticmethod
    def memory() -> Coroutine[Any, Any, "IrohNode"]:
        """Create an in-memory Iroh node with all protocols."""
        ...

    def node_id(self) -> str:
        """Return this node's EndpointId as a hex string."""
        ...

    def node_addr(self) -> str:
        """Return the node's address info as a debug string."""
        ...

    def close(self) -> Coroutine[Any, Any, None]:
        """Gracefully shut down the node."""
        ...

    def shutdown(self) -> Coroutine[Any, Any, None]:
        """Alias for close() — gracefully shut down the node."""
        ...

    def add_node_addr(self, other: "IrohNode") -> None:
        """Add another node's address info for peer discovery."""
        ...

class BlobsClient:
    """Client for blob storage operations."""

    def add_bytes(self, data: bytes) -> Coroutine[Any, Any, str]:
        """Store bytes and return the BLAKE3 hash hex string."""
        ...

    def read_to_bytes(self, hash_hex: str) -> Coroutine[Any, Any, bytes]:
        """Read a blob by its BLAKE3 hash hex string."""
        ...

    def create_ticket(self, hash_hex: str) -> str:
        """Create a blob ticket string for sharing a blob."""
        ...

    def download_blob(self, ticket: str) -> Coroutine[Any, Any, bytes]:
        """Download a blob from a blob ticket string."""
        ...

class DocHandle:
    """Handle to a single document for set/get operations."""

    def set_bytes(self, author: str, key: bytes, value: bytes) -> Coroutine[Any, Any, None]:
        """Set a key-value pair in the document."""
        ...

    def get_exact(self, author: str, key: bytes) -> Coroutine[Any, Any, Optional[bytes]]:
        """Get the exact value for a given author + key."""
        ...

    def share(self, mode: str, addr_info: str) -> Coroutine[Any, Any, str]:
        """Share the document, returning a ticket string."""
        ...

class DocsClient:
    """Client for CRDT collaborative documents."""

    def create(self) -> Coroutine[Any, Any, DocHandle]:
        """Create a new document."""
        ...

    def create_author(self) -> Coroutine[Any, Any, str]:
        """Create a new author and return its ID as a hex string."""
        ...

    def join(self, ticket: str) -> Coroutine[Any, Any, DocHandle]:
        """Join a document by ticket string."""
        ...

class GossipTopicHandle:
    """Handle to a subscribed gossip topic for send/recv."""

    def broadcast(self, data: bytes) -> Coroutine[Any, Any, None]:
        """Broadcast data to all peers on this topic."""
        ...

    def recv(self) -> Coroutine[Any, Any, Tuple[str, Any]]:
        """Receive the next event. Returns (event_type, data)."""
        ...

class GossipClient:
    """Client for gossip pub-sub messaging."""

    def subscribe(
        self, topic_bytes: bytes, bootstrap: list[str]
    ) -> Coroutine[Any, Any, GossipTopicHandle]:
        """Subscribe to a gossip topic."""
        ...

class NetClient:
    """Client for QUIC networking."""

    def connect(self, node_id: str, alpn: str) -> Coroutine[Any, Any, "IrohConnection"]:
        """Connect to a remote node by its node ID and ALPN protocol."""
        ...

    def accept(self) -> Coroutine[Any, Any, "IrohConnection"]:
        """Accept an incoming connection."""
        ...

    def endpoint_id(self) -> str:
        """Return this endpoint's ID as a hex string."""
        ...

    def endpoint_addr(self) -> str:
        """Return this endpoint's address info as a debug string."""
        ...

class IrohConnection:
    """A QUIC connection."""

    def open_bi(self) -> Coroutine[Any, Any, Tuple["IrohSendStream", "IrohRecvStream"]]:
        """Open a bi-directional stream."""
        ...

    def accept_bi(self) -> Coroutine[Any, Any, Tuple["IrohSendStream", "IrohRecvStream"]]:
        """Accept an incoming bi-directional stream."""
        ...

    def send_datagram(self, data: bytes) -> Coroutine[Any, Any, None]:
        """Send a datagram."""
        ...

    def read_datagram(self) -> Coroutine[Any, Any, bytes]:
        """Read a datagram."""
        ...

    def remote_id(self) -> str:
        """Return the remote node's ID as a hex string."""
        ...

class IrohSendStream:
    """A QUIC send stream."""

    def write_all(self, data: bytes) -> Coroutine[Any, Any, None]:
        """Write all data to the stream."""
        ...

    def finish(self) -> Coroutine[Any, Any, None]:
        """Finish the stream (signal end of data)."""
        ...

class IrohRecvStream:
    """A QUIC receive stream."""

    def read_to_end(self, max_size: int) -> Coroutine[Any, Any, bytes]:
        """Read all data from the stream up to max_size bytes."""
        ...

def blobs_client(node: IrohNode) -> BlobsClient:
    """Create a BlobsClient from an IrohNode."""
    ...

def docs_client(node: IrohNode) -> DocsClient:
    """Create a DocsClient from an IrohNode."""
    ...

def gossip_client(node: IrohNode) -> GossipClient:
    """Create a GossipClient from an IrohNode."""
    ...

def net_client(node: IrohNode) -> NetClient:
    """Create a NetClient from an IrohNode."""
    ...

def create_endpoint(alpn: str) -> Coroutine[Any, Any, NetClient]:
    """Create a bare QUIC endpoint (no protocols) for custom ALPN."""
    ...