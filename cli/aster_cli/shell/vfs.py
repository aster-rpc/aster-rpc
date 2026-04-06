"""
aster_cli.shell.vfs — Virtual filesystem for navigating a peer.

The VFS presents a peer's resources as a navigable tree::

    /
    ├── blobs/       → list and inspect content-addressed blobs
    ├── services/    → list services, inspect contracts, invoke methods
    │   └── <Name>/ → methods of a specific service
    └── gossip/      → (future) gossip topics

Nodes are lazily populated from the live connection.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any


class NodeKind(Enum):
    ROOT = "root"
    BLOBS = "blobs"
    BLOB = "blob"
    SERVICES = "services"
    SERVICE = "service"
    METHOD = "method"
    GOSSIP = "gossip"
    TOPIC = "topic"


@dataclass
class VfsNode:
    """A node in the virtual filesystem tree."""

    name: str
    kind: NodeKind
    path: str  # absolute path, e.g. "/services/HelloWorld"
    children: dict[str, VfsNode] = field(default_factory=dict)
    metadata: dict[str, Any] = field(default_factory=dict)
    loaded: bool = False  # whether children have been fetched from peer

    def child(self, name: str) -> VfsNode | None:
        """Get a child node by name (case-insensitive match)."""
        # Exact match first
        if name in self.children:
            return self.children[name]
        # Case-insensitive fallback
        lower = name.lower()
        for k, v in self.children.items():
            if k.lower() == lower:
                return v
        return None

    def add_child(self, node: VfsNode) -> None:
        """Add a child node."""
        self.children[node.name] = node

    def is_leaf(self) -> bool:
        """Whether this node has no children (is a leaf)."""
        return len(self.children) == 0 and self.loaded

    def sorted_children(self) -> list[VfsNode]:
        """Children sorted by name."""
        return sorted(self.children.values(), key=lambda n: n.name)


def build_root() -> VfsNode:
    """Build the initial VFS root with static top-level directories."""
    root = VfsNode(name="/", kind=NodeKind.ROOT, path="/")

    root.add_child(VfsNode(name="blobs", kind=NodeKind.BLOBS, path="/blobs"))
    root.add_child(VfsNode(name="services", kind=NodeKind.SERVICES, path="/services"))
    root.add_child(VfsNode(name="gossip", kind=NodeKind.GOSSIP, path="/gossip"))

    root.loaded = True
    return root


def resolve_path(root: VfsNode, cwd: str, target: str) -> tuple[VfsNode | None, str]:
    """Resolve a path (absolute or relative) to a VFS node.

    Args:
        root: The VFS root node.
        cwd: Current working directory path.
        target: The path to resolve (absolute or relative).

    Returns:
        (node, resolved_path) or (None, target) if not found.
    """
    # Build absolute path
    if target.startswith("/"):
        parts = [p for p in target.split("/") if p]
    elif target == "..":
        parts = [p for p in cwd.split("/") if p]
        if parts:
            parts.pop()
    elif target.startswith("../"):
        base = [p for p in cwd.split("/") if p]
        if base:
            base.pop()
        rest = [p for p in target[3:].split("/") if p]
        parts = base + rest
    elif target == ".":
        parts = [p for p in cwd.split("/") if p]
    else:
        base = [p for p in cwd.split("/") if p]
        extra = [p for p in target.split("/") if p]
        parts = base + extra

    # Walk the tree
    node = root
    for part in parts:
        child = node.child(part)
        if child is None:
            resolved = "/" + "/".join(parts)
            return None, resolved
        node = child

    resolved = "/" + "/".join(parts) if parts else "/"
    return node, resolved


async def populate_services(node: VfsNode, connection: Any) -> None:
    """Populate /services with service info from the peer.

    Args:
        node: The /services VfsNode.
        connection: The active connection to query.
    """
    if node.loaded:
        return

    try:
        # Query service summaries from the peer
        summaries = await connection.list_services()

        for summary in summaries:
            svc_name = summary.get("name", summary) if isinstance(summary, dict) else str(summary)
            svc_node = VfsNode(
                name=svc_name,
                kind=NodeKind.SERVICE,
                path=f"/services/{svc_name}",
                metadata=summary if isinstance(summary, dict) else {"name": svc_name},
            )
            node.add_child(svc_node)
    except Exception:
        # Connection may not support list_services yet — use empty
        pass

    node.loaded = True


async def populate_service_methods(node: VfsNode, connection: Any) -> None:
    """Populate a /services/<name> node with method info.

    Args:
        node: A service VfsNode.
        connection: The active connection to query.
    """
    if node.loaded:
        return

    try:
        contract = await connection.get_contract(node.name)
        if contract and hasattr(contract, "methods"):
            for method in contract.methods:
                m_name = method.get("name", str(method)) if isinstance(method, dict) else str(method)
                method_node = VfsNode(
                    name=m_name,
                    kind=NodeKind.METHOD,
                    path=f"{node.path}/{m_name}",
                    metadata=method if isinstance(method, dict) else {"name": m_name},
                    loaded=True,
                )
                node.add_child(method_node)
    except Exception:
        pass

    node.loaded = True


async def populate_blobs(node: VfsNode, connection: Any) -> None:
    """Populate /blobs with blob listing from the peer.

    Args:
        node: The /blobs VfsNode.
        connection: The active connection to query.
    """
    if node.loaded:
        return

    try:
        blobs = await connection.list_blobs()
        for blob in blobs:
            hash_str = blob.get("hash", str(blob)) if isinstance(blob, dict) else str(blob)
            short = hash_str[:12] + "…" if len(hash_str) > 12 else hash_str
            blob_node = VfsNode(
                name=short,
                kind=NodeKind.BLOB,
                path=f"/blobs/{short}",
                metadata=blob if isinstance(blob, dict) else {"hash": hash_str},
                loaded=True,
            )
            node.add_child(blob_node)
    except Exception:
        pass

    node.loaded = True


async def ensure_loaded(node: VfsNode, connection: Any) -> None:
    """Ensure a node's children are loaded from the peer."""
    if node.loaded:
        return

    if node.kind == NodeKind.SERVICES:
        await populate_services(node, connection)
    elif node.kind == NodeKind.SERVICE:
        await populate_service_methods(node, connection)
    elif node.kind == NodeKind.BLOBS:
        await populate_blobs(node, connection)
    else:
        node.loaded = True
