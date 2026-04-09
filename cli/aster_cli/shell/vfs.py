"""
aster_cli.shell.vfs -- Virtual filesystem for navigating a peer.

The VFS presents a peer's resources as a navigable tree::

    /
    ├── blobs/       → list and inspect content-addressed blobs
    ├── services/    → list services, inspect contracts, invoke methods
    │   └── <Name>/ → methods of a specific service
    └── gossip/      → (future) gossip topics

Nodes are lazily populated from the live connection.
"""

from __future__ import annotations

import contextlib
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
    COLLECTION = "collection"  # HashSeq collection (browsable like a dir)
    DOCS = "docs"
    DOC = "doc"
    DOC_ENTRY = "doc_entry"
    # Directory hierarchy (aster.site)
    ASTER = "aster"       # /aster -- the directory root
    HANDLE = "handle"     # /aster/<handle> -- a user/org
    README = "readme"     # /aster/<handle>/README.md


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
    root.add_child(VfsNode(name="docs", kind=NodeKind.DOCS, path="/docs"))
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
        # Connection may not support list_services yet -- use empty
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
        methods = contract.get("methods", []) if isinstance(contract, dict) else getattr(contract, "methods", [])
        if methods:
            for method in methods:
                m_name = method.get("name", str(method)) if isinstance(method, dict) else str(method)
                method_node = VfsNode(
                    name=m_name,
                    kind=NodeKind.METHOD,
                    path=f"{node.path}/{m_name}",
                    metadata=method if isinstance(method, dict) else {"name": m_name},
                    loaded=True,
                )
                node.add_child(method_node)
            node.loaded = True
        # If methods is empty, don't mark as loaded -- manifest may still be fetching
    except Exception:
        pass


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
            tag = blob.get("tag", "")
            is_collection = blob.get("is_collection", False)
            # Use tag as display name if available, otherwise short hash
            display = tag if tag else (hash_str[:12] + "…" if len(hash_str) > 12 else hash_str)
            kind = NodeKind.COLLECTION if is_collection else NodeKind.BLOB
            blob_node = VfsNode(
                name=display,
                kind=kind,
                path=f"/blobs/{display}",
                metadata=blob if isinstance(blob, dict) else {"hash": hash_str},
                loaded=not is_collection,  # collections need lazy loading
            )
            node.add_child(blob_node)
    except Exception:
        pass

    # Only mark loaded if children were found; otherwise retry later
    # after background manifest fetch populates artifact_refs.
    if node.children:
        node.loaded = True


async def populate_collection(node: VfsNode, connection: Any) -> None:
    """Populate a collection node with its HashSeq entries."""
    if node.loaded:
        return

    coll_hash = node.metadata.get("hash", "")
    if not coll_hash:
        node.loaded = True
        return

    try:
        entries = await connection.list_collection_entries(coll_hash)
        for entry in entries:
            name = entry.get("name", "?")
            entry_node = VfsNode(
                name=name,
                kind=NodeKind.BLOB,
                path=f"{node.path}/{name}",
                metadata=entry,
                loaded=True,
            )
            node.add_child(entry_node)
    except Exception:
        pass

    node.loaded = True


async def populate_docs(node: VfsNode, connection: Any) -> None:
    """Populate /docs with registry doc entries.

    Args:
        node: The /docs VfsNode.
        connection: The active connection to query.
    """
    if node.loaded:
        return

    try:
        entries = await connection.list_doc_entries()
        for entry in entries:
            key = entry.get("key", "?")
            entry_node = VfsNode(
                name=key,
                kind=NodeKind.DOC_ENTRY,
                path=f"/docs/{key}",
                metadata=entry,
                loaded=True,
            )
            node.add_child(entry_node)
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
    elif node.kind == NodeKind.COLLECTION:
        await populate_collection(node, connection)
    elif node.kind == NodeKind.DOCS:
        await populate_docs(node, connection)
    elif node.kind == NodeKind.ASTER:
        await populate_handles(node, connection)
    elif node.kind == NodeKind.HANDLE:
        await populate_handle_services(node, connection)
    else:
        node.loaded = True


# ── Directory VFS (aster.site) ──────────────────────────────────────────────


def build_directory_root() -> VfsNode:
    """Build the VFS root for directory mode (/aster/ hierarchy)."""
    root = VfsNode(name="/", kind=NodeKind.ROOT, path="/")
    aster = VfsNode(name="aster", kind=NodeKind.ASTER, path="/aster")
    root.add_child(aster)
    root.loaded = True
    return root


async def populate_handles(node: VfsNode, connection: Any) -> None:
    """Populate /aster with handles from the directory."""
    if node.loaded:
        return

    try:
        handles = await connection.list_handles()
        for h in handles:
            name = h.get("handle") or h.get("pubkey_hash", "???")
            handle_node = VfsNode(
                name=name,
                kind=NodeKind.HANDLE,
                path=f"/aster/{name}",
                metadata=h,
            )
            node.add_child(handle_node)
    except Exception:
        pass

    node.loaded = True


async def populate_handle_services(node: VfsNode, connection: Any) -> None:
    """Populate /aster/<handle> with README + services."""
    if node.loaded:
        return

    handle_name = node.name

    try:
        info = await connection.get_handle_info(handle_name)

        # Add README.md
        readme_text = info.get("readme", "")
        if readme_text:
            readme_node = VfsNode(
                name="README.md",
                kind=NodeKind.README,
                path=f"{node.path}/README.md",
                metadata={"content": readme_text},
                loaded=True,
            )
            node.add_child(readme_node)

        # Add services
        services = info.get("services", [])
        for svc in services:
            svc_name = svc.get("name", "???")
            published = svc.get("published", False)

            if published:
                display_name = svc_name
            else:
                # Unpublished: show hash + friendly name
                short_hash = svc.get("contract_hash", "??????")[:10]
                display_name = f"{short_hash}... ({svc_name})"

            svc_node = VfsNode(
                name=display_name,
                kind=NodeKind.SERVICE,
                path=f"{node.path}/{display_name}",
                metadata=svc,
            )

            # Pre-populate methods if available
            methods = svc.get("methods", [])
            for m in methods:
                m_name = m.get("name", str(m)) if isinstance(m, dict) else str(m)
                m_node = VfsNode(
                    name=m_name,
                    kind=NodeKind.METHOD,
                    path=f"{svc_node.path}/{m_name}",
                    metadata=m if isinstance(m, dict) else {"name": m_name},
                    loaded=True,
                )
                svc_node.add_child(m_node)
            svc_node.loaded = True

            node.add_child(svc_node)
    except Exception:
        pass

    node.loaded = True


async def ensure_directory_handle(root: VfsNode, handle_name: str, connection: Any) -> VfsNode | None:
    """Create a /aster/@handle node on demand for direct navigation."""
    aster_node = root.child("aster")
    if aster_node is None:
        return None

    existing = aster_node.child(handle_name)
    if existing is not None:
        return existing

    with contextlib.suppress(Exception):
        info = await connection.get_handle_info(handle_name)
        if info.get("services") or info.get("readme", "") or handle_name.startswith("@"):
            handle_node = VfsNode(
                name=handle_name,
                kind=NodeKind.HANDLE,
                path=f"/aster/{handle_name}",
                metadata={"handle": handle_name, "registered": True},
            )
            aster_node.add_child(handle_node)
            return handle_node
    return None
