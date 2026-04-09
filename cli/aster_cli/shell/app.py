"""
aster_cli.shell.app — Shell REPL entry point and CLI wiring.

Usage::

    aster shell <peer-addr> [--rcan <path>]

Connects to a peer and launches an interactive shell with filesystem-like
navigation, service discovery, dynamic RPC invocation, and smart autocomplete.
"""

from __future__ import annotations

import argparse
import asyncio
import copy
import sys
from pathlib import Path
from typing import Any

from prompt_toolkit import PromptSession
from prompt_toolkit.formatted_text import HTML
from prompt_toolkit.history import FileHistory
from prompt_toolkit.styles import Style
from rich.console import Console

from aster_cli.shell.completer import ShellCompleter
from aster_cli.shell.display import Display
from aster_cli.shell.plugin import CommandContext, get_command
from aster_cli.shell.vfs import (
    NodeKind,
    build_root,
    build_directory_root,
    ensure_loaded,
    resolve_path,
)

# Import commands to trigger @register decorators
import aster_cli.shell.commands  # noqa: F401


# ── Prompt styling ────────────────────────────────────────────────────────────

SHELL_STYLE = Style.from_dict({
    "peer": "#78A6FF bold",     # Signal Blue
    "colon": "#666666",
    "path": "#61D6C2",          # Relay Teal
    "dollar": "#E6C06B",        # Trust Gold
    "": "#D4D4D4",              # default text
})


def _make_prompt(peer_name: str, cwd: str) -> HTML:
    """Build the styled prompt string."""
    return HTML(
        f"<peer>{peer_name}</peer>"
        f"<colon>:</colon>"
        f"<path>{cwd}</path>"
        f"<dollar>$ </dollar>"
    )


# ── Helpers ───────────────────────────────────────────────────────────────────


def _node_addr_to_b64(addr: Any) -> str:
    """Serialize a NodeAddr to a base64 string for _coerce_node_addr."""
    import base64
    return base64.b64encode(addr.to_bytes()).decode("ascii")


def _load_root_secret_key(config: Any) -> bytes | None:
    """Load the root private key for the shell's node identity.

    Checks (in order):
    1. OS keyring (via active profile from ``~/.aster/config.toml``)
    2. ``config.root_pubkey_file`` (explicit config)
    3. ``~/.aster/root.key`` (default file location)

    Returns 32-byte secret key or None if unavailable.
    """
    import json as _json
    import os

    # 1) Try keyring — scoped by active profile
    try:
        from aster_cli.credentials import get_root_privkey, has_keyring
        if has_keyring():
            profile = _get_active_profile()
            if profile:
                hex_key = get_root_privkey(profile)
                if hex_key:
                    return bytes.fromhex(hex_key)
    except Exception:
        pass

    # 2) Try file-based locations
    candidates = []
    if config.root_pubkey_file:
        candidates.append(config.root_pubkey_file)
    candidates.append("~/.aster/root.key")

    for path in candidates:
        expanded = os.path.expanduser(path)
        if not os.path.exists(expanded):
            continue
        try:
            with open(expanded) as f:
                content = f.read().strip()
            if content.startswith("{"):
                d = _json.loads(content)
                if "private_key" in d:
                    return bytes.fromhex(d["private_key"])
        except (ValueError, KeyError, _json.JSONDecodeError, OSError):
            continue
    return None


def _get_active_profile() -> str | None:
    """Read the active profile name from ``~/.aster/config.toml``."""
    import os

    config_path = os.path.expanduser("~/.aster/config.toml")
    if not os.path.exists(config_path):
        return None
    try:
        if sys.version_info >= (3, 11):
            import tomllib
        else:
            import tomli as tomllib  # type: ignore[no-redef]
        with open(config_path, "rb") as f:
            data = tomllib.load(f)
        return data.get("active_profile")
    except Exception:
        return None


def _resolve_peer_arg(peer_arg: str) -> tuple[str, str]:
    """Resolve a peer argument to (endpoint_addr, friendly_name).

    If ``peer_arg`` matches a peer name in ``.aster-identity``, returns
    that peer's endpoint_id and the name.  Otherwise returns the raw
    value as-is with a truncated display name.

    Raises ``SystemExit`` with a helpful message if it looks like a name
    but can't be resolved.
    """
    import os

    # Handle compact aster1... ticket format
    if peer_arg.startswith("aster1"):
        try:
            from aster import AsterTicket, NodeAddr
            ticket = AsterTicket.from_string(peer_arg)
            na = NodeAddr(
                endpoint_id=ticket.endpoint_id,
                direct_addresses=ticket.direct_addrs,
            )
            addr_b64 = _node_addr_to_b64(na)
            display = f"{ticket.endpoint_id[:8]}... (ticket)"
            return addr_b64, display
        except Exception as exc:
            print(f"error: invalid aster ticket: {exc}", file=sys.stderr)
            sys.exit(1)

    # Check if it looks like a name (short, no base64/hex, no special chars)
    looks_like_name = (
        len(peer_arg) < 64
        and "=" not in peer_arg
        and "\n" not in peer_arg
        and not all(c in "0123456789abcdef" for c in peer_arg.lower())
    )

    if looks_like_name:
        # Try loading .aster-identity
        identity_path = os.path.join(os.getcwd(), ".aster-identity")
        if os.path.exists(identity_path):
            try:
                from aster_cli.identity import load_identity
                identity = load_identity(identity_path)
                known_names = []
                for peer in identity.get("peers", []):
                    name = peer.get("name")
                    if name:
                        known_names.append(name)
                    if name == peer_arg:
                        eid = peer.get("endpoint_id")
                        if eid:
                            # Return raw endpoint ID — AsterClient handles
                            # NodeAddr construction (avoids loading native module here)
                            return eid, peer_arg
                        print(
                            f"error: peer '{peer_arg}' found in .aster-identity "
                            f"but has no endpoint_id",
                            file=sys.stderr,
                        )
                        sys.exit(1)
                # Name not found — show available names
                if known_names:
                    names_str = ", ".join(known_names)
                    print(
                        f"error: peer '{peer_arg}' not found in .aster-identity\n"
                        f"  known peers: {names_str}\n"
                        f"  or pass an aster1... ticket / base64 NodeAddr / hex EndpointId directly",
                        file=sys.stderr,
                    )
                else:
                    print(
                        f"error: peer '{peer_arg}' not found — "
                        f".aster-identity has no peer entries\n"
                        f"  pass a base64 NodeAddr / hex EndpointId directly",
                        file=sys.stderr,
                    )
                sys.exit(1)
            except SystemExit:
                raise
            except Exception:
                pass
        else:
            # No identity file and it looks like a name
            print(
                f"error: '{peer_arg}' looks like a peer name but no "
                f".aster-identity file found in {os.getcwd()}\n"
                f"  pass an aster1... ticket / base64 NodeAddr / hex EndpointId directly, or\n"
                f"  run 'aster enroll node' to create an identity file",
                file=sys.stderr,
            )
            sys.exit(1)

    # Also try reverse lookup: even if it's a raw addr, find the name
    friendly = _lookup_peer_name(peer_arg)
    if friendly:
        return peer_arg, friendly

    # Fallback: truncated display
    display = peer_arg[:16] + "…" if len(peer_arg) > 16 else peer_arg
    return peer_arg, display


def _lookup_peer_name(addr: str) -> str | None:
    """Try to find a friendly name for an address from .aster-identity."""
    import os

    identity_path = os.path.join(os.getcwd(), ".aster-identity")
    if not os.path.exists(identity_path):
        return None
    try:
        from aster_cli.identity import load_identity
        identity = load_identity(identity_path)
        for peer in identity.get("peers", []):
            eid = peer.get("endpoint_id", "")
            # Check if the addr contains this endpoint_id (NodeAddr encodes it)
            if eid and eid in addr:
                return peer.get("name")
    except Exception:
        pass
    return None


# ── Connection adapter ────────────────────────────────────────────────────────

class PeerConnection:
    """Adapter wrapping a real Aster peer connection for shell use.

    Handles:
      - Consumer admission handshake (trusted or open-gate)
      - Service discovery from admission response
      - Blob operations via BlobsClient
      - RPC invocation via IrohTransport
      - All four streaming patterns
    """

    def __init__(self, peer_addr: str, rcan_path: str | None = None) -> None:
        self._peer_addr = peer_addr
        self._rcan_path = rcan_path
        self._aster_client: Any = None
        self._ep: Any = None
        self._blobs: Any = None
        self._services: list[Any] = []  # ServiceSummary objects
        self._manifests: dict[str, dict[str, Any]] = {}  # service name → manifest dict
        self._rpc_conns: dict[str, Any] = {}  # channel addr → IrohConnection
        self._transports: dict[str, Any] = {}  # service name → IrohTransport
        self._type_factory: Any = None  # DynamicTypeFactory for typeless invocation
        self._registry_doc: Any = None  # DocHandle for the registry doc
        self._registry_event_rx: Any = None  # DocEventReceiver for live events
        self._artifact_refs: dict[str, dict[str, Any]] = {}  # contract_id → ArtifactRef dict

    async def connect(self) -> None:
        """Connect to the peer via consumer admission, then fetch contract manifests."""
        from aster import AsterClient
        from aster.config import AsterConfig

        # The shell needs its own node identity — not the service's.
        # Use the root private key (if available) as the shell's secret_key.
        # This gives the shell the root owner's identity, which services
        # in the mesh will recognise as the trust anchor — free auth.
        # Falls back to ephemeral if no root key is configured.
        config = AsterConfig.from_env()
        config.storage_path = None
        # Set to a non-existent path to prevent CWD .aster-identity fallback
        # (identity_file=None still searches CWD)
        config.identity_file = "/dev/null/.aster-identity"

        root_secret = _load_root_secret_key(config)
        config.secret_key = root_secret  # None → ephemeral, which is fine

        self._aster_client = AsterClient(
            config=config,
            endpoint_addr=self._peer_addr,
            enrollment_credential_file=self._rcan_path,
        )
        await self._aster_client.connect()
        self._services = list(self._aster_client.services)

        # Fetch manifests in the background — the shell is usable immediately
        # with basic service info from the admission response. Manifests add
        # rich metadata (field types, descriptions) and are merged in when ready.
        self._manifest_task = asyncio.create_task(self._fetch_manifests_background())

    async def _fetch_manifests_background(self) -> None:
        """Fetch manifests and synthesize types, logging errors instead of raising."""
        try:
            await self._fetch_manifests()
            self._synthesize_types()
        except Exception:
            import logging
            logging.getLogger(__name__).debug(
                "Background manifest fetch failed", exc_info=True,
            )

    async def _fetch_manifests(self) -> None:
        """Fetch manifest.json for each service from the blob store.

        The registry doc (accessed via registry_namespace) maps contract_ids
        to ArtifactRefs which point to blob collections containing manifest.json.
        If registry is unavailable, manifests will be empty (basic service info only).
        """
        import json as _json
        import logging

        logger = logging.getLogger(__name__)
        namespace = self._aster_client.registry_namespace if self._aster_client else ""

        if not namespace or not self._aster_client._node:
            logger.debug("No registry namespace or node — skipping manifest fetch")
            return

        try:
            from aster import blobs_client, docs_client
            from aster.contract.publication import fetch_from_collection
            from aster.registry.keys import contract_key

            bc = blobs_client(self._aster_client._node)
            dc = docs_client(self._aster_client._node)

            # Join the registry doc (read-only) and wait for initial sync
            remote_node_id = self._get_remote_node_id() or ""
            doc, event_receiver = await dc.join_and_subscribe_namespace(
                namespace, remote_node_id
            )
            self._registry_doc = doc
            self._registry_event_rx = event_receiver

            # Wait for sync with retry — the doc needs to pull entries from
            # the producer. We try up to 3 times with increasing timeouts.
            import asyncio as _asyncio

            sync_done = False
            for attempt in range(3):
                timeout = 3.0 * (attempt + 1)  # 3s, 6s, 9s
                try:
                    deadline = _asyncio.get_event_loop().time() + timeout
                    while _asyncio.get_event_loop().time() < deadline:
                        remaining = deadline - _asyncio.get_event_loop().time()
                        if remaining <= 0:
                            break
                        try:
                            event = await _asyncio.wait_for(
                                event_receiver.recv(), timeout=min(remaining, 1.0)
                            )
                            kind = event.kind if hasattr(event, "kind") else str(event)
                            logger.debug("Registry doc event: %s", kind)
                            if kind == "sync_finished":
                                await _asyncio.sleep(0.3)
                                sync_done = True
                                break
                        except _asyncio.TimeoutError:
                            test_entries = await doc.query_key_prefix(b"contracts/")
                            if test_entries:
                                logger.debug("Entries found before sync_finished")
                                sync_done = True
                                break
                except Exception as exc:
                    logger.debug("Sync wait attempt %d interrupted: %s", attempt + 1, exc)

                if sync_done:
                    break
                logger.debug("Sync attempt %d timed out, retrying...", attempt + 1)

            for svc in self._services:
                try:
                    # Read ArtifactRef from registry doc by key
                    key = contract_key(svc.contract_id)
                    entries = await doc.query_key_exact(key)
                    if not entries:
                        logger.debug("No ArtifactRef found for %s", svc.name)
                        continue

                    # Read the ArtifactRef JSON from the doc entry content
                    entry = entries[0]
                    content = await doc.read_entry_content(entry.content_hash)
                    artifact = _json.loads(content)
                    self._artifact_refs[svc.contract_id] = artifact
                    collection_hash = artifact.get("collection_hash", "")

                    if not collection_hash:
                        logger.debug("No collection_hash for %s", svc.name)
                        continue

                    # Download collection by hash from the remote peer.
                    # We already know the remote node_id from the connection.
                    remote_node_id = self._get_remote_node_id()
                    if remote_node_id:
                        try:
                            files = await bc.download_collection_hash(
                                collection_hash, remote_node_id
                            )
                            total_size = sum(len(data) for _, data in files)
                            artifact["size"] = total_size
                            for name, data in files:
                                if name == "manifest.json":
                                    manifest = _json.loads(data)
                                    self._manifests[svc.name] = manifest
                                    logger.debug(
                                        "Fetched manifest for %s: %d methods",
                                        svc.name,
                                        len(manifest.get("methods", [])),
                                    )
                                    break
                        except Exception as dl_exc:
                            logger.debug("Collection download failed for %s: %s", svc.name, dl_exc)
                    else:
                        # Fallback: try reading from local store
                        manifest_bytes = await fetch_from_collection(
                            bc, collection_hash, "manifest.json"
                        )
                        if manifest_bytes:
                            manifest = _json.loads(manifest_bytes)
                            self._manifests[svc.name] = manifest
                except Exception as exc:
                    logger.debug("Failed to fetch manifest for %s: %s", svc.name, exc)

        except Exception as exc:
            logger.debug("Registry manifest fetch failed: %s", exc)

    def _get_remote_node_id(self) -> str | None:
        """Get the remote peer's endpoint ID (node_id hex)."""
        try:
            from aster.high_level import _coerce_node_addr
            addr = _coerce_node_addr(self._aster_client._endpoint_addr_in)
            return addr.endpoint_id or None
        except Exception:
            return None

    async def list_services(self) -> list[dict[str, Any]]:
        """List services from admission, enriched with manifest data."""
        results = []
        for svc in self._services:
            manifest = self._manifests.get(svc.name, {})
            svc_dict: dict[str, Any] = {
                "name": svc.name,
                "version": svc.version,
                "contract_id": svc.contract_id,
                "scoped": manifest.get("scoped", "shared"),
                "channels": svc.channels if hasattr(svc, "channels") else {},
            }

            # Include methods from manifest if available
            if "methods" in manifest:
                svc_dict["methods"] = manifest["methods"]

            results.append(svc_dict)
        return results

    async def get_contract(self, service_name: str) -> dict[str, Any] | None:
        """Get contract details for a service.

        Returns manifest data if available, otherwise basic admission info.
        """
        for svc in self._services:
            if svc.name == service_name:
                manifest = self._manifests.get(service_name, {})
                return {
                    "name": svc.name,
                    "version": svc.version,
                    "contract_id": svc.contract_id,
                    "methods": manifest.get("methods", []),
                    "types": [
                        {"name": m.get("request_type", "?"), "hash": ""}
                        for m in manifest.get("methods", [])
                        if m.get("request_type")
                    ],
                }
        return None

    def get_manifests(self) -> dict[str, dict[str, Any]]:
        """Get all fetched manifests: service_name -> manifest dict."""
        return dict(self._manifests)

    def get_peer_display(self) -> str:
        """Get the peer's display name (handle or endpoint_id prefix)."""
        try:
            from aster.high_level import _coerce_node_addr
            addr = _coerce_node_addr(self._aster_client._endpoint_addr_in)
            return addr.endpoint_id[:8] if addr.endpoint_id else "unknown"
        except Exception:
            return "unknown"

    async def list_blobs(self) -> list[dict[str, Any]]:
        """List blobs: tags from the local store + collection entries from manifests."""
        results: list[dict[str, Any]] = []
        if not self._aster_client or not self._aster_client._node:
            return results

        from aster import blobs_client
        bc = blobs_client(self._aster_client._node)

        # 1) Named tags (GC-protected blobs)
        try:
            tags = await bc.tag_list()
            for t in tags:
                results.append({
                    "hash": t.hash,
                    "size": 0,
                    "tag": t.name,
                    "source": "tag",
                })
        except Exception:
            pass

        # 2) Collection entries from artifact refs (contract blobs)
        seen = {r["hash"] for r in results}
        for contract_id, artifact in self._artifact_refs.items():
            coll_hash = artifact.get("collection_hash", "")
            if not coll_hash or coll_hash in seen:
                continue
            # Find service name + version for this contract
            svc = next(
                (s for s in self._services if s.contract_id == contract_id),
                None,
            )
            svc_name = svc.name if svc else contract_id[:12]
            svc_ver = svc.version if svc else 1
            results.append({
                "hash": coll_hash,
                "size": artifact.get("size", 0),
                "tag": f"{svc_name}.v{svc_ver}",
                "source": "collection",
                "is_collection": True,
            })
            seen.add(coll_hash)

        return results

    async def list_collection_entries(self, collection_hash: str) -> list[dict[str, Any]]:
        """List entries inside a HashSeq collection."""
        if not self._aster_client or not self._aster_client._node:
            return []
        from aster import blobs_client
        bc = blobs_client(self._aster_client._node)
        try:
            entries = await bc.list_collection(collection_hash)
            return [
                {"name": name, "hash": h, "size": size}
                for name, h, size in entries
            ]
        except Exception:
            return []

    async def read_blob(self, blob_hash: str) -> bytes:
        """Read blob content by hash."""
        if self._aster_client and self._aster_client._node:
            from aster import blobs_client
            bc = blobs_client(self._aster_client._node)
            return await bc.read_to_bytes(blob_hash)
        raise RuntimeError("blob reading not available")

    def _synthesize_types(self) -> None:
        """Create dynamic dataclasses from manifest method schemas.

        These are wire-compatible with the producer's types — same
        @wire_type tag, same field names. Enables invocation without
        having the producer's Python types locally installed.
        """
        from aster.dynamic import DynamicTypeFactory
        import logging

        logger = logging.getLogger(__name__)
        self._type_factory = DynamicTypeFactory()

        for svc_name, manifest in self._manifests.items():
            methods = manifest.get("methods", [])
            self._type_factory.register_from_manifest(methods)

        if self._type_factory.type_count > 0:
            logger.debug(
                "Synthesized %d dynamic types from manifests",
                self._type_factory.type_count,
            )

    async def _get_transport(self, service_name: str) -> Any:
        """Get or create an IrohTransport for a service."""
        if service_name in self._transports:
            return self._transports[service_name]

        from aster import IrohTransport, ForyCodec
        from aster.types import SerializationMode

        # Find the service summary
        summary = None
        for svc in self._services:
            if svc.name == service_name:
                summary = svc
                break
        if summary is None:
            raise RuntimeError(f"service {service_name!r} not found on this peer")

        # Get RPC connection for the service's channel
        channel_addr = None
        for _name, addr in summary.channels.items():
            channel_addr = addr
            break
        if channel_addr is None:
            raise RuntimeError(f"service {service_name!r} has no RPC channel")

        if channel_addr not in self._rpc_conns:
            conn = await self._aster_client._rpc_conn_for(channel_addr)
            self._rpc_conns[channel_addr] = conn

        conn = self._rpc_conns[channel_addr]

        # Build a codec with synthesized types if available
        codec = None
        if self._type_factory and self._type_factory.type_count > 0:
            try:
                codec = ForyCodec(
                    mode=SerializationMode.XLANG,
                    types=self._type_factory.get_all_types(),
                )
            except Exception as exc:
                import logging
                logging.getLogger(__name__).debug("Dynamic codec failed: %s", exc)

        transport = IrohTransport(conn, codec=codec)
        self._transports[service_name] = transport
        return transport

    def _get_method_meta(self, service: str, method: str) -> dict[str, Any] | None:
        """Look up method metadata from the manifest."""
        manifest = self._manifests.get(service, {})
        for m in manifest.get("methods", []):
            if m["name"] == method:
                return m
        return None

    async def invoke(
        self, service: str, method: str, payload: dict[str, Any]
    ) -> Any:
        """Invoke a unary RPC.

        If dynamic types are available (from manifest), builds a typed
        request from the dict payload. Otherwise passes the dict directly.
        """
        transport = await self._get_transport(service)

        # Try to build a typed request from the dynamic type factory
        request: Any = payload
        if self._type_factory:
            meta = self._get_method_meta(service, method)
            if meta and meta.get("request_wire_tag"):
                try:
                    request = self._type_factory.build_request(meta, payload)
                except Exception:
                    pass  # fall back to dict

        return await transport.unary(service, method, request)

    async def server_stream(
        self, service: str, method: str, payload: dict[str, Any]
    ) -> Any:
        """Start a server-streaming RPC."""
        transport = await self._get_transport(service)
        return transport.server_stream(service, method, payload)

    async def client_stream(
        self, service: str, method: str, values: list[Any]
    ) -> Any:
        """Send a client-streaming RPC."""
        transport = await self._get_transport(service)

        async def _iter():
            for v in values:
                yield v

        return await transport.client_stream(service, method, _iter())

    def bidi_stream(
        self, service: str, method: str, values: Any
    ) -> Any:
        """Start a bidi-streaming RPC.

        Note: bidi_stream returns a BidiChannel, not an async iterator.
        The invoker handles the read/write loop.
        """
        # We need async transport setup, so return a coroutine wrapper
        async def _start():
            transport = await self._get_transport(service)
            return transport.bidi_stream(service, method)
        return _start()

    async def list_doc_entries(self) -> list[dict[str, Any]]:
        """List all entries in the registry doc."""
        if not self._registry_doc:
            return []
        try:
            entries = await self._registry_doc.query_key_prefix(b"")
            results = []
            for e in entries:
                key_str = e.key.decode("utf-8", errors="replace")
                results.append({
                    "key": key_str,
                    "author": e.author_id[:12] + "…",
                    "hash": e.content_hash[:16] + "…",
                    "size": e.content_len,
                    "timestamp": e.timestamp,
                })
            return results
        except Exception:
            return []

    async def read_doc_entry(self, key: str) -> bytes | None:
        """Read content of a registry doc entry by key."""
        if not self._registry_doc:
            return None
        try:
            entries = await self._registry_doc.query_key_exact(key.encode("utf-8"))
            if not entries:
                return None
            return await self._registry_doc.read_entry_content(entries[0].content_hash)
        except Exception:
            return None

    async def subscribe_gossip(self) -> Any:
        """Subscribe to the producer mesh gossip topic.

        The gossip topic is returned by the producer during consumer
        admission — but only when the connecting node is the root key
        holder.  Returns a GossipTopicHandle or raises.
        """
        if not self._aster_client or not self._aster_client._node:
            raise RuntimeError("not connected")

        topic_hex = self._aster_client.gossip_topic
        if not topic_hex:
            raise RuntimeError(
                "gossip topic not available — the producer only shares it "
                "with the root node. Connect with the root key to access gossip."
            )

        topic_bytes = bytes.fromhex(topic_hex)

        from aster import gossip_client

        # Need bootstrap peers — the producer we connected to
        bootstrap = []
        if self._aster_client._endpoint_addr_in:
            from aster.high_level import _coerce_node_addr
            addr = _coerce_node_addr(self._aster_client._endpoint_addr_in)
            if addr.endpoint_id:
                bootstrap.append(addr.endpoint_id)

        gc = gossip_client(self._aster_client._node)
        return await gc.subscribe(list(topic_bytes), bootstrap)

    async def close(self) -> None:
        """Close the connection and clean up.

        Fire-and-forget — the shell doesn't need to wait for graceful
        QUIC teardown. The OS reclaims sockets on process exit anyway.
        """
        # Cancel background manifest fetch if still running
        if hasattr(self, "_manifest_task") and not self._manifest_task.done():
            self._manifest_task.cancel()

        self._transports.clear()
        self._rpc_conns.clear()

        # Don't await graceful shutdown — just let the process exit.
        # AsterClient.close() does node.shutdown() which waits for
        # iroh protocols to drain, but the shell doesn't need that.


# ── Offline / demo mode ──────────────────────────────────────────────────────

class DemoConnection:
    """Offline demo connection for testing the shell without a live peer.

    Provides sample services and blobs for exploring the shell UX.
    """

    async def connect(self) -> None:
        pass

    async def list_services(self) -> list[dict[str, Any]]:
        return [
            {
                "name": "HelloWorld",
                "version": 1,
                "scoped": "shared",
                "methods": [
                    {
                        "name": "sayHello",
                        "pattern": "unary",
                        "request_type": "HelloRequest",
                        "response_type": "HelloResponse",
                        "timeout": 30.0,
                        "fields": [
                            {"name": "name", "type": "str", "required": True},
                            {"name": "greeting", "type": "str", "required": False, "default": "Hello"},
                        ],
                    },
                    {
                        "name": "streamGreetings",
                        "pattern": "server_stream",
                        "request_type": "StreamRequest",
                        "response_type": "HelloResponse",
                        "timeout": None,
                        "fields": [
                            {"name": "names", "type": "list[str]", "required": True},
                        ],
                    },
                ],
            },
            {
                "name": "FileStore",
                "version": 2,
                "scoped": "shared",
                "methods": [
                    {"name": "get", "pattern": "unary", "request_type": "GetRequest", "response_type": "FileData"},
                    {"name": "put", "pattern": "unary", "request_type": "PutRequest", "response_type": "PutResponse"},
                    {"name": "list", "pattern": "server_stream", "request_type": "ListRequest", "response_type": "FileInfo"},
                    {"name": "upload", "pattern": "client_stream", "request_type": "Chunk", "response_type": "UploadResult"},
                    {"name": "sync", "pattern": "bidi_stream", "request_type": "SyncMessage", "response_type": "SyncMessage"},
                ],
            },
            {
                "name": "Analytics",
                "version": 1,
                "scoped": "session",
                "methods": [
                    {"name": "getMetrics", "pattern": "unary", "request_type": "MetricsQuery", "response_type": "MetricsResult"},
                    {"name": "watchMetrics", "pattern": "server_stream", "request_type": "WatchRequest", "response_type": "MetricEvent"},
                    {"name": "ingest", "pattern": "client_stream", "request_type": "DataPoint", "response_type": "IngestSummary"},
                ],
            },
        ]

    async def get_contract(self, service_name: str) -> dict[str, Any] | None:
        services = await self.list_services()
        for svc in services:
            if svc["name"] == service_name:
                return {
                    "name": svc["name"],
                    "version": svc["version"],
                    "contract_id": "a1b2c3d4e5f6…",
                    "methods": svc["methods"],
                    "types": [
                        {"name": m.get("request_type", "?"), "hash": "aabbccdd…"}
                        for m in svc["methods"]
                        if m.get("request_type")
                    ],
                }
        return None

    async def list_blobs(self) -> list[dict[str, Any]]:
        return [
            {"hash": "abc123def456789012345678", "size": 0, "tag": "HelloWorld.v1", "source": "collection", "is_collection": True},
        ]

    async def list_collection_entries(self, collection_hash: str) -> list[dict[str, Any]]:
        return [
            {"name": "manifest.json", "hash": "deadbeef0123456789abcdef", "size": 340},
            {"name": "contract.bin", "hash": "cafebabe9876543210fedcba", "size": 1258291},
            {"name": "types/HelloRequest.bin", "hash": "1234567890abcdef12345678", "size": 128},
        ]

    async def list_doc_entries(self) -> list[dict[str, Any]]:
        return [
            {"key": "contracts/a1b2c3d4…", "author": "7f3e9a1b2c…", "hash": "abc123def456…", "size": 256, "timestamp": 1712567890000},
            {"key": "manifests/a1b2c3d4…", "author": "7f3e9a1b2c…", "hash": "deadbeef0123…", "size": 340, "timestamp": 1712567890000},
            {"key": "versions/HelloWorld/v1", "author": "7f3e9a1b2c…", "hash": "cafebabe9876…", "size": 64, "timestamp": 1712567890000},
        ]

    async def read_doc_entry(self, key: str) -> bytes | None:
        if "manifest" in key:
            import json as _json
            return _json.dumps({"service": "HelloWorld", "version": 1, "methods": [{"name": "sayHello"}]}, indent=2).encode()
        if "versions" in key:
            return b"a1b2c3d4e5f6"
        return b'{"contract_id":"a1b2c3d4...","collection_hash":"abc123..."}'

    async def read_blob(self, blob_hash: str) -> bytes:
        if "deadbeef" in blob_hash:
            return b"Hello from the mesh!\n\nThis is sample blob content."
        return b"(binary data placeholder -- connect to a real peer for actual content)"

    async def invoke(
        self, service: str, method: str, payload: dict[str, Any]
    ) -> Any:
        import time
        # Simulate latency
        await asyncio.sleep(0.05)

        if service == "HelloWorld" and method == "sayHello":
            name = payload.get("name", payload.get("_positional", "World"))
            greeting = payload.get("greeting", "Hello")
            return {
                "message": f"{greeting}, {name}!",
                "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
            }

        return {"status": "ok", "service": service, "method": method, "args": payload}

    async def server_stream(
        self, service: str, method: str, payload: dict[str, Any]
    ) -> Any:
        async def _gen():
            import time
            for i in range(5):
                await asyncio.sleep(0.1)
                yield {"index": i, "timestamp": time.strftime("%H:%M:%S")}
        return _gen()

    async def client_stream(
        self, service: str, method: str, values: list[Any]
    ) -> Any:
        return {"received": len(values), "status": "ok"}

    def bidi_stream(
        self, service: str, method: str, values: Any
    ) -> Any:
        async def _echo():
            async for v in values:
                yield {"echo": v}
        return _echo()

    async def close(self) -> None:
        pass


# ── Directory demo mode (aster.site) ────────────────────────────────────────


class DirectoryDemoConnection:
    """Offline demo simulating the aster.site directory experience.

    Shows a browsable hierarchy of handles and published services.
    """

    # Simulated current user
    MY_HANDLE = "emrul"
    MY_PUBKEY_HASH = "a1b2c3d4e5f6"

    # Simulated directory data
    _HANDLES = [
        {"handle": "emrul", "pubkey_hash": "a1b2c3d4e5f6", "registered": True},
        {"handle": "acme-corp", "pubkey_hash": "f7e8d9c0b1a2", "registered": True},
        {"handle": "alice-dev", "pubkey_hash": "1234abcd5678", "registered": True},
        {"handle": None, "pubkey_hash": "7f3a2bc9de01", "registered": False},
        {"handle": None, "pubkey_hash": "9e8d7c6b5a43", "registered": False},
    ]

    _HANDLE_INFO: dict[str, dict[str, Any]] = {
        "emrul": {
            "readme": (
                "# emrul\n\n"
                "Building distributed systems with Aster.\n\n"
                "Services:\n"
                "- TaskManager — async task queue for AI agent workflows\n"
                "- InvoiceService — invoice lifecycle management\n"
            ),
            "services": [
                {
                    "name": "TaskManager",
                    "published": True,
                    "version": 3,
                    "scoped": "session",
                    "description": "Async task queue for AI agent workflows",
                    "endpoints": 2,
                    "methods": [
                        {
                            "name": "submitTask",
                            "pattern": "unary",
                            "request_type": "TaskRequest",
                            "response_type": "TaskHandle",
                            "timeout": 30.0,
                            "fields": [
                                {"name": "prompt", "type": "str", "required": True},
                                {"name": "priority", "type": "int", "required": False, "default": 0},
                                {"name": "tags", "type": "list[str]", "required": False},
                            ],
                        },
                        {
                            "name": "watchProgress",
                            "pattern": "server_stream",
                            "request_type": "TaskHandle",
                            "response_type": "ProgressEvent",
                            "timeout": None,
                            "fields": [
                                {"name": "task_id", "type": "str", "required": True},
                            ],
                        },
                        {
                            "name": "cancelTask",
                            "pattern": "unary",
                            "request_type": "CancelRequest",
                            "response_type": "CancelResult",
                            "timeout": 10.0,
                            "fields": [
                                {"name": "task_id", "type": "str", "required": True},
                                {"name": "reason", "type": "str", "required": False},
                            ],
                        },
                    ],
                },
                {
                    "name": "InvoiceService",
                    "published": False,
                    "version": 1,
                    "scoped": "shared",
                    "contract_hash": "b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2",
                    "description": "Invoice lifecycle management",
                    "endpoints": 0,
                    "methods": [
                        {
                            "name": "create",
                            "pattern": "unary",
                            "request_type": "InvoiceRequest",
                            "response_type": "Invoice",
                        },
                        {
                            "name": "list",
                            "pattern": "server_stream",
                            "request_type": "ListFilter",
                            "response_type": "Invoice",
                        },
                    ],
                },
            ],
        },
        "acme-corp": {
            "readme": (
                "# acme-corp\n\n"
                "Enterprise payment infrastructure.\n"
            ),
            "services": [
                {
                    "name": "PaymentGateway",
                    "published": True,
                    "version": 7,
                    "scoped": "session",
                    "description": "Process payments across multiple providers",
                    "endpoints": 5,
                    "methods": [
                        {
                            "name": "charge",
                            "pattern": "unary",
                            "request_type": "ChargeRequest",
                            "response_type": "ChargeResult",
                            "timeout": 30.0,
                            "fields": [
                                {"name": "amount_cents", "type": "int", "required": True},
                                {"name": "currency", "type": "str", "required": True},
                                {"name": "source_token", "type": "str", "required": True},
                            ],
                        },
                        {
                            "name": "refund",
                            "pattern": "unary",
                            "request_type": "RefundRequest",
                            "response_type": "RefundResult",
                            "timeout": 30.0,
                        },
                        {
                            "name": "watchSettlements",
                            "pattern": "server_stream",
                            "request_type": "SettlementFilter",
                            "response_type": "SettlementEvent",
                            "timeout": None,
                        },
                    ],
                },
                {
                    "name": "FraudDetector",
                    "published": True,
                    "version": 2,
                    "scoped": "shared",
                    "description": "Real-time transaction fraud scoring",
                    "endpoints": 3,
                    "methods": [
                        {
                            "name": "score",
                            "pattern": "unary",
                            "request_type": "Transaction",
                            "response_type": "FraudScore",
                            "timeout": 5.0,
                        },
                        {
                            "name": "trainModel",
                            "pattern": "client_stream",
                            "request_type": "TrainingExample",
                            "response_type": "TrainingResult",
                            "timeout": 300.0,
                        },
                    ],
                },
            ],
        },
        "alice-dev": {
            "readme": (
                "# alice-dev\n\n"
                "AI research tools and agent services.\n"
            ),
            "services": [
                {
                    "name": "DocumentSummarizer",
                    "published": True,
                    "version": 1,
                    "scoped": "session",
                    "description": "Summarize documents using LLM pipelines",
                    "endpoints": 1,
                    "methods": [
                        {
                            "name": "summarize",
                            "pattern": "unary",
                            "request_type": "SummarizeRequest",
                            "response_type": "Summary",
                            "timeout": 60.0,
                            "fields": [
                                {"name": "document", "type": "str", "required": True},
                                {"name": "max_length", "type": "int", "required": False, "default": 500},
                            ],
                        },
                        {
                            "name": "streamSummary",
                            "pattern": "server_stream",
                            "request_type": "SummarizeRequest",
                            "response_type": "SummaryChunk",
                            "timeout": 120.0,
                        },
                    ],
                },
            ],
        },
        "7f3a2bc9de01": {
            "readme": "",
            "services": [
                {
                    "name": "WeatherAPI",
                    "published": True,
                    "version": 1,
                    "scoped": "shared",
                    "description": "Global weather data service",
                    "endpoints": 1,
                    "methods": [
                        {
                            "name": "getCurrent",
                            "pattern": "unary",
                            "request_type": "LocationQuery",
                            "response_type": "WeatherData",
                            "timeout": 10.0,
                        },
                    ],
                },
            ],
        },
        "9e8d7c6b5a43": {
            "readme": "",
            "services": [
                {
                    "name": "EchoService",
                    "published": True,
                    "version": 1,
                    "scoped": "shared",
                    "description": "Simple echo for testing connectivity",
                    "endpoints": 1,
                    "methods": [
                        {
                            "name": "echo",
                            "pattern": "unary",
                            "request_type": "EchoRequest",
                            "response_type": "EchoResponse",
                            "timeout": 5.0,
                        },
                    ],
                },
            ],
        },
    }

    def __init__(self) -> None:
        from aster_cli.join import get_local_identity_state
        from aster_cli.profile import get_published_services

        state = get_local_identity_state()
        self.my_handle = state["handle"] or state["display_handle"].lstrip("@")
        self.my_pubkey_hash = (state["root_pubkey"] or self.MY_PUBKEY_HASH)[:12]
        self._handles = copy.deepcopy(self._HANDLES)
        self._handle_info = copy.deepcopy(self._HANDLE_INFO)

        local_services = []
        published_names = set(get_published_services(state["profile"]))
        manifest_path = Path(".aster/manifest.json")
        if manifest_path.exists():
            try:
                import json as _json

                payload = _json.loads(manifest_path.read_text())
                manifests = payload if isinstance(payload, list) else [payload]
                for manifest in manifests:
                    methods = []
                    for method in manifest.get("methods", []):
                        methods.append({
                            "name": method.get("name", "?"),
                            "pattern": method.get("pattern", "unary"),
                            "request_type": method.get("request_type", "?"),
                            "response_type": method.get("response_type", "?"),
                            "timeout": method.get("timeout"),
                            "fields": method.get("fields", []),
                        })
                    service_name = manifest.get("service", "UnknownService")
                    local_services.append({
                        "name": service_name,
                        "published": service_name in published_names,
                        "version": manifest.get("version", 1),
                        "scoped": manifest.get("scoped", "shared"),
                        "description": "Local manifest",
                        "endpoints": 1 if service_name in published_names else 0,
                        "contract_hash": manifest.get("contract_id", ""),
                        "methods": methods,
                    })
            except Exception:
                local_services = []

        info = self._handle_info.setdefault(self.my_handle, {"readme": "", "services": []})
        info["services"] = local_services + info.get("services", [])
        if not any((h.get("handle") or h.get("pubkey_hash")) == self.my_handle for h in self._handles):
            self._handles.insert(0, {
                "handle": self.my_handle,
                "pubkey_hash": self.my_pubkey_hash,
                "registered": bool(state["handle"]) and state["handle_status"] in {"pending", "verified"},
            })

    async def connect(self) -> None:
        pass

    async def list_handles(self) -> list[dict[str, Any]]:
        return [
            {
                "handle": h["handle"] or h["pubkey_hash"],
                "pubkey_hash": h["pubkey_hash"],
                "registered": h["registered"],
            }
            for h in self._handles
        ]

    async def get_handle_info(self, handle: str) -> dict[str, Any]:
        return self._handle_info.get(handle, {"readme": "", "services": []})

    # -- These support drill-down into services (reuses demo logic) --

    async def list_services(self) -> list[dict[str, Any]]:
        return []

    async def get_contract(self, service_name: str) -> dict[str, Any] | None:
        # Search all handles for the service
        for info in self._HANDLE_INFO.values():
            for svc in info.get("services", []):
                if svc["name"] == service_name:
                    return {
                        "name": svc["name"],
                        "version": svc.get("version", 1),
                        "contract_id": svc.get("contract_hash", "demo-hash")[:16] + "...",
                        "methods": svc.get("methods", []),
                        "types": [
                            {"name": m.get("request_type", "?"), "hash": "aabbccdd..."}
                            for m in svc.get("methods", [])
                            if m.get("request_type")
                        ],
                    }
        return None

    async def list_blobs(self) -> list[dict[str, Any]]:
        return []

    async def read_blob(self, blob_hash: str) -> bytes:
        return b"(directory demo -- no blob content)"

    async def invoke(
        self, service: str, method: str, payload: dict[str, Any]
    ) -> Any:
        await asyncio.sleep(0.05)
        if service == "TaskManager" and method == "submitTask":
            return {
                "task_id": "tsk_demo_001",
                "status": "queued",
                "prompt": payload.get("prompt", ""),
                "priority": payload.get("priority", 0),
            }
        return {"status": "ok", "service": service, "method": method, "args": payload}

    async def server_stream(
        self, service: str, method: str, payload: dict[str, Any]
    ) -> Any:
        async def _gen():
            import time
            for i in range(5):
                await asyncio.sleep(0.1)
                yield {"index": i, "timestamp": time.strftime("%H:%M:%S")}
        return _gen()

    async def client_stream(
        self, service: str, method: str, values: list[Any]
    ) -> Any:
        return {"received": len(values), "status": "ok"}

    def bidi_stream(self, service: str, method: str, values: Any) -> Any:
        async def _echo():
            async for v in values:
                yield {"echo": v}
        return _echo()

    async def close(self) -> None:
        pass


# ── VFS population from connection ──────────────────────────────────────────

async def _populate_from_connection(root, connection) -> tuple[int, int]:
    """Pre-populate the VFS from the connection. Returns (service_count, blob_count)."""
    services_node = root.child("services")
    blobs_node = root.child("blobs")

    svc_count = 0
    blob_count = 0

    # Populate services
    if services_node:
        try:
            summaries = await connection.list_services()
            for svc in summaries:
                name = svc["name"] if isinstance(svc, dict) else str(svc)
                from aster_cli.shell.vfs import VfsNode, NodeKind
                svc_node = VfsNode(
                    name=name,
                    kind=NodeKind.SERVICE,
                    path=f"/services/{name}",
                    metadata=svc if isinstance(svc, dict) else {"name": name},
                )

                # Populate methods if available in summary
                methods = svc.get("methods", []) if isinstance(svc, dict) else []
                for m in methods:
                    m_name = m.get("name", str(m)) if isinstance(m, dict) else str(m)
                    m_node = VfsNode(
                        name=m_name,
                        kind=NodeKind.METHOD,
                        path=f"/services/{name}/{m_name}",
                        metadata=m if isinstance(m, dict) else {"name": m_name},
                        loaded=True,
                    )
                    svc_node.add_child(m_node)
                # Only mark loaded if methods were populated;
                # otherwise let ensure_loaded retry after manifest fetch
                if methods:
                    svc_node.loaded = True

                services_node.add_child(svc_node)
                svc_count += 1
            services_node.loaded = True
        except Exception:
            services_node.loaded = True

    # Populate blobs
    if blobs_node:
        try:
            blobs = await connection.list_blobs()
            for blob in blobs:
                hash_str = blob.get("hash", str(blob)) if isinstance(blob, dict) else str(blob)
                short = hash_str[:12] + "…" if len(hash_str) > 12 else hash_str
                from aster_cli.shell.vfs import VfsNode, NodeKind
                blob_node = VfsNode(
                    name=short,
                    kind=NodeKind.BLOB,
                    path=f"/blobs/{short}",
                    metadata=blob if isinstance(blob, dict) else {"hash": hash_str},
                    loaded=True,
                )
                blobs_node.add_child(blob_node)
                blob_count += 1
            # Only mark loaded if we found blobs; otherwise let
            # ensure_loaded retry after manifest fetch populates artifact_refs
            if blob_count > 0:
                blobs_node.loaded = True
        except Exception:
            blobs_node.loaded = True

    return svc_count, blob_count


# ── Directory VFS population ─────────────────────────────────────────────────


async def _populate_directory(root, connection) -> int:
    """Pre-populate the /aster/ directory VFS. Returns handle count."""
    aster_node = root.child("aster")
    if not aster_node:
        return 0

    handle_count = 0
    try:
        from aster_cli.shell.vfs import VfsNode, NodeKind

        handles = await connection.list_handles()
        for h in handles:
            name = h.get("handle") or h.get("pubkey_hash", "???")
            handle_node = VfsNode(
                name=name,
                kind=NodeKind.HANDLE,
                path=f"/aster/{name}",
                metadata=h,
            )
            aster_node.add_child(handle_node)
            handle_count += 1

            # Pre-populate services for each handle
            info = await connection.get_handle_info(name)

            readme_text = info.get("readme", "")
            if readme_text:
                readme_node = VfsNode(
                    name="README.md",
                    kind=NodeKind.README,
                    path=f"/aster/{name}/README.md",
                    metadata={"content": readme_text},
                    loaded=True,
                )
                handle_node.add_child(readme_node)

            for svc in info.get("services", []):
                svc_name = svc.get("name", "???")
                published = svc.get("published", False)

                if published:
                    display_name = svc_name
                else:
                    short_hash = svc.get("contract_hash", "??????")[:10]
                    display_name = f"{short_hash}... ({svc_name})"

                svc_node = VfsNode(
                    name=display_name,
                    kind=NodeKind.SERVICE,
                    path=f"/aster/{name}/{display_name}",
                    metadata=svc,
                )
                for m in svc.get("methods", []):
                    m_name = m.get("name", str(m)) if isinstance(m, dict) else str(m)
                    m_node = VfsNode(
                        name=m_name,
                        kind=NodeKind.METHOD,
                        path=f"/aster/{name}/{display_name}/{m_name}",
                        metadata=m if isinstance(m, dict) else {"name": m_name},
                        loaded=True,
                    )
                    svc_node.add_child(m_node)
                svc_node.loaded = True
                handle_node.add_child(svc_node)

            handle_node.loaded = True

        aster_node.loaded = True
    except Exception:
        aster_node.loaded = True

    return handle_count


# ── Shell REPL ────────────────────────────────────────────────────────────────

async def _run_shell(
    connection: Any,
    peer_name: str,
    raw: bool = False,
    directory_mode: bool = False,
    air_gapped: bool = False,
) -> None:
    """Run the interactive shell REPL."""
    console = Console()
    display = Display(console=console, raw=raw)
    from rich.panel import Panel
    from aster_cli.join import get_local_identity_state

    state = get_local_identity_state()

    if not raw:
        banner_lines = []
        if air_gapped:
            banner_lines.append("[bold]Air-gapped mode[/bold] [dim](@aster service disabled for this session)[/dim]")
        if not state["root_pubkey"]:
            banner_lines.append("No identity configured.")
            banner_lines.append("[dim]Run `aster keygen root` or `join <handle> <email>` to get started.[/dim]")
        elif state["handle_status"] == "pending":
            banner_lines.append(f"[bold cyan]{state['display_handle']}[/bold cyan] [dim]pending verification[/dim]")
            banner_lines.append("[dim]Run `verify <code>` or `status` to check for auto-verification.[/dim]")
        elif state["handle_status"] == "verified":
            banner_lines.append(f"[bold cyan]{state['display_handle']}[/bold cyan] [dim]verified[/dim]")
        else:
            banner_lines.append(f"[bold cyan]{state['display_handle']}[/bold cyan] [dim]not registered[/dim]")
            banner_lines.append("[dim]Run `join <handle> <email>` to register this identity.[/dim]")
        display.console.print()
        display.console.print(
            Panel("\n".join(banner_lines), border_style="blue", padding=(0, 2))
        )

    if directory_mode:
        root = build_directory_root()
        handle_count = await _populate_directory(root, connection)
        display.directory_welcome(peer_name, handle_count)
    else:
        # Build VFS
        root = build_root()

        # Pre-populate from connection
        svc_count, blob_count = await _populate_from_connection(root, connection)

        # Welcome banner
        display.welcome(peer_name, svc_count, blob_count)

    # ── Guided tour ───────────────────────────────────────────────────────
    from aster_cli.shell.guide import (
        DEFAULT_TOUR,
        GuideManager,
        Tour,
        is_first_time,
        mark_tour_complete,
    )

    if directory_mode:
        # Directory mode has its own UX flow — skip the peer tour
        guide = GuideManager(display)
        guide.disable()
    elif is_first_time() and not raw:
        guide = GuideManager(display, tour=DEFAULT_TOUR)
        guide.fire("connected")
    else:
        guide = GuideManager(display)  # empty tour, no-op
        guide.disable()

    # Shell state
    if directory_mode:
        cwd = f"/aster/{peer_name}"
    else:
        cwd = "/"
    _last_ctrl_c = 0.0  # timestamp of last Ctrl+C for double-tap exit

    # Command context (mutable — cwd updates)
    ctx = CommandContext(
        vfs_cwd=cwd,
        vfs_root=root,
        connection=connection,
        display=display,
        peer_name=peer_name,
        interactive=True,
        raw_output=raw,
        guide=guide,
    )

    # History
    history_dir = Path.home() / ".aster"
    history_dir.mkdir(exist_ok=True)
    history = FileHistory(str(history_dir / "shell_history"))

    # Completer
    completer = ShellCompleter(get_context=lambda: ctx)

    # Prompt session
    session: PromptSession = PromptSession(
        history=history,
        completer=completer,
        style=SHELL_STYLE,
        complete_while_typing=True,
    )

    # REPL loop
    while True:
        try:
            prompt = _make_prompt(peer_name, ctx.vfs_cwd)
            text = await session.prompt_async(prompt)
            text = text.strip()
            _last_ctrl_c = 0.0  # reset on successful input

            if not text:
                continue

            # Parse command
            parts = _tokenize(text)
            cmd_name = parts[0]
            cmd_args = parts[1:]

            # Handle ./methodName syntax
            if cmd_name.startswith("./"):
                method_name = cmd_name[2:]
                cmd = get_command("./")
                if cmd:
                    await cmd.execute([method_name] + cmd_args, ctx)
                else:
                    display.error(f"unknown command: {cmd_name}")
                continue

            # Handle quit/exit aliases
            if cmd_name in ("quit", "q"):
                cmd_name = "exit"

            # Look up command
            cmd = get_command(cmd_name)
            if cmd is None:
                # Try as a direct method invocation if in a service dir
                node, _ = resolve_path(root, ctx.vfs_cwd, ".")
                if node and node.kind == NodeKind.SERVICE and node.child(cmd_name):
                    # Treat as ./methodName
                    invoke_cmd = get_command("./")
                    if invoke_cmd:
                        await invoke_cmd.execute([cmd_name] + cmd_args, ctx)
                    continue

                display.error(f"unknown command: {cmd_name} (try 'help')")
                continue

            # Check if command is valid at current path
            if not cmd.is_valid_at(ctx.vfs_cwd):
                display.error(f"'{cmd_name}' is not available at {ctx.vfs_cwd}")
                continue

            # Execute
            old_cwd = ctx.vfs_cwd
            await cmd.execute(cmd_args, ctx)

            # Fire guide events
            if guide.is_active:
                guide.fire("command", cmd_name)
                if ctx.vfs_cwd != old_cwd:
                    guide.fire("cd", ctx.vfs_cwd)

        except KeyboardInterrupt:
            import time as _time
            now = _time.monotonic()
            if now - _last_ctrl_c < 1.5:
                # Double Ctrl+C → clean exit
                display.print()
                display.info("Disconnecting...")
                break
            _last_ctrl_c = now
            display.print()
            display.info("Press Ctrl+C again to exit")
            continue
        except EOFError:
            display.print()
            display.info("Disconnecting...")
            break
        except SystemExit:
            break
        except Exception as e:
            display.error(str(e))

    # Mark tour complete if it was active
    if guide.tour and not guide.tour.is_complete:
        pass  # partial tour, don't mark complete
    elif guide.tour and guide.tour.is_complete:
        try:
            mark_tour_complete()
        except Exception:
            pass  # non-critical


def _tokenize(text: str) -> list[str]:
    """Simple shell-like tokenization respecting quotes."""
    import shlex
    try:
        return shlex.split(text)
    except ValueError:
        return text.split()


# ── Public API ────────────────────────────────────────────────────────────────

async def launch_shell(
    peer_addr: str | None = None,
    rcan_path: str | None = None,
    demo: bool = False,
    demo2: bool = False,
    air_gapped: bool = False,
    raw: bool = False,
) -> None:
    """Launch the interactive shell.

    Args:
        peer_addr: Address of the peer to connect to.  May be a base64
                   NodeAddr string, an EndpointId hex, or a peer name
                   from ``.aster-identity``.
        rcan_path: Path to RCAN credential file.
        demo: If True, use demo connection (no real peer).
        demo2: If True, use directory demo (aster.site browsing).
    """
    if demo2:
        connection = DirectoryDemoConnection()
        peer_name = connection.my_handle
        await connection.connect()
        try:
            await _run_shell(connection, peer_name, raw=raw, directory_mode=True, air_gapped=air_gapped)
        finally:
            await connection.close()
        return

    if demo or peer_addr is None:
        connection = DemoConnection()
        peer_name = "demo"
        await connection.connect()
    else:
        # Resolve peer name before heavy imports (instant — just reads .aster-identity)
        resolved_addr, friendly_name = _resolve_peer_arg(peer_addr)
        peer_name = friendly_name

        # Animate a spinner during connect so the CLI feels responsive
        connection = PeerConnection(peer_addr=resolved_addr, rcan_path=rcan_path)
        connect_task = asyncio.create_task(connection.connect())

        frames = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
        i = 0
        while not connect_task.done():
            sys.stderr.write(f"\r  {frames[i % len(frames)]} connecting to {peer_name}")
            sys.stderr.flush()
            i += 1
            try:
                await asyncio.wait_for(asyncio.shield(connect_task), timeout=0.08)
                break
            except asyncio.TimeoutError:
                continue
        sys.stderr.write("\r\033[K")

        # Re-raise if connect failed
        await connect_task

    try:
        await _run_shell(connection, peer_name, raw=raw, air_gapped=air_gapped)
    finally:
        await connection.close()


def register_shell_subparser(subparsers: argparse._SubParsersAction) -> None:
    """Register the ``aster shell`` subcommand."""
    shell_parser = subparsers.add_parser(
        "shell",
        help="Interactive shell for exploring a peer",
    )
    shell_parser.add_argument(
        "peer",
        nargs="?",
        default=None,
        help="Peer address to connect to (omit for demo mode)",
    )
    shell_parser.add_argument(
        "--rcan",
        default=None,
        metavar="PATH",
        help="Path to RCAN credential file",
    )
    shell_parser.add_argument(
        "--demo",
        action="store_true",
        help="Launch in demo mode with sample data",
    )
    shell_parser.add_argument(
        "--demo2",
        action="store_true",
        help=argparse.SUPPRESS,  # undocumented
    )
    shell_parser.add_argument(
        "--air-gapped",
        action="store_true",
        help="Disable @aster service features for this shell session",
    )
    shell_parser.add_argument(
        "--json",
        action="store_true",
        dest="raw_json",
        help="Output raw JSON (for piping)",
    )


def run_shell_command(args: argparse.Namespace) -> int:
    """Execute the ``aster shell`` command."""
    demo2 = getattr(args, "demo2", False)
    demo = args.demo or (args.peer is None and not demo2)

    try:
        asyncio.run(launch_shell(
            peer_addr=args.peer,
            rcan_path=args.rcan,
            demo=demo,
            demo2=demo2,
            air_gapped=args.air_gapped,
            raw=args.raw_json,
        ))
    except KeyboardInterrupt:
        pass
    except SystemExit:
        pass

    return 0
