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

    async def connect(self) -> None:
        """Connect to the peer via consumer admission, then fetch contract manifests."""
        from aster import AsterClient

        self._aster_client = AsterClient(
            endpoint_addr=self._peer_addr,
            enrollment_credential_file=self._rcan_path,
        )
        await self._aster_client.connect()
        self._services = list(self._aster_client.services)

        # Try to fetch contract manifests from the registry/blobs
        await self._fetch_manifests()

    async def _fetch_manifests(self) -> None:
        """Fetch manifest.json for each service from the blob store.

        The registry doc (accessed via registry_ticket) maps contract_ids
        to ArtifactRefs which point to blob collections containing manifest.json.
        If registry is unavailable, manifests will be empty (basic service info only).
        """
        import json as _json
        import logging

        logger = logging.getLogger(__name__)
        ticket = self._aster_client.registry_ticket if self._aster_client else ""

        if not ticket or not self._aster_client._node:
            logger.debug("No registry ticket or node — skipping manifest fetch")
            return

        try:
            from aster import blobs_client, docs_client
            from aster.contract.publication import fetch_from_collection
            from aster.registry.keys import contract_key

            bc = blobs_client(self._aster_client._node)
            dc = docs_client(self._aster_client._node)

            # Join the registry doc (read-only) and wait for initial sync
            doc, event_receiver = await dc.join_and_subscribe(ticket)

            # Wait for sync_finished event (the doc pulling entries from producer)
            import asyncio as _asyncio
            try:
                deadline = _asyncio.get_event_loop().time() + 5.0
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
                            # Sync done — give blob downloads a moment
                            await _asyncio.sleep(0.3)
                            break
                    except _asyncio.TimeoutError:
                        # Check if entries are already available
                        test_entries = await doc.query_key_prefix(b"contracts/")
                        if test_entries:
                            logger.debug("Entries found before sync_finished")
                            break
            except Exception as exc:
                logger.debug("Sync wait interrupted: %s", exc)

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
                    collection_hash = artifact.get("collection_hash", "")

                    if not collection_hash:
                        continue

                    # Download the entire collection via the collection ticket.
                    # This fetches the index blob + all referenced entry blobs.
                    blob_ticket = artifact.get("ticket")
                    if blob_ticket:
                        try:
                            await bc.download_blob(blob_ticket)
                            logger.debug("Downloaded collection via ticket")
                        except Exception as dl_exc:
                            logger.debug("Collection download failed: %s", dl_exc)

                    # Fetch manifest.json from the blob collection
                    manifest_bytes = await fetch_from_collection(
                        bc, collection_hash, "manifest.json"
                    )
                    if manifest_bytes:
                        manifest = _json.loads(manifest_bytes)
                        self._manifests[svc.name] = manifest
                        logger.debug(
                            "Fetched manifest for %s: %d methods",
                            svc.name,
                            len(manifest.get("methods", [])),
                        )
                except Exception as exc:
                    logger.debug("Failed to fetch manifest for %s: %s", svc.name, exc)

        except Exception as exc:
            logger.debug("Registry manifest fetch failed: %s", exc)

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

    async def list_blobs(self) -> list[dict[str, Any]]:
        """List blobs on the local node."""
        if self._aster_client and self._aster_client._ep:
            try:
                from aster import blobs_client
                bc = blobs_client(self._aster_client._ep)
                tags = await bc.tag_list() if hasattr(bc, "tag_list") else []
                return [{"hash": t.hash, "size": 0, "tag": t.name} for t in tags]
            except Exception:
                pass
        return []

    async def read_blob(self, blob_hash: str) -> bytes:
        """Read blob content by hash."""
        if self._aster_client and self._aster_client._ep:
            from aster import blobs_client
            bc = blobs_client(self._aster_client._ep)
            return await bc.read_to_bytes(blob_hash)
        raise RuntimeError("blob reading not available")

    async def _get_transport(self, service_name: str) -> Any:
        """Get or create an IrohTransport for a service."""
        if service_name in self._transports:
            return self._transports[service_name]

        from aster import IrohTransport, RPC_ALPN

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
        transport = IrohTransport(conn)
        self._transports[service_name] = transport
        return transport

    async def invoke(
        self, service: str, method: str, payload: dict[str, Any]
    ) -> Any:
        """Invoke a unary RPC.

        Note: For typed invocation, the request type must be importable.
        For raw dict payloads, we pass them directly — the transport will
        attempt to serialize via Fory. This works if the server accepts
        dict-like payloads or if a codec adapter is registered.
        """
        transport = await self._get_transport(service)
        return await transport.unary(service, method, payload)

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

    async def close(self) -> None:
        """Close the connection and clean up."""
        self._transports.clear()
        self._rpc_conns.clear()
        if self._aster_client:
            try:
                await self._aster_client.close()
            except Exception:
                pass


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
            {"hash": "abc123def456789012345678", "size": 1258291},
            {"hash": "deadbeef0123456789abcdef", "size": 340},
            {"hash": "cafebabe9876543210fedcba", "size": 52428800},
        ]

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
            blobs_node.loaded = True
        except Exception:
            blobs_node.loaded = True

    return svc_count, blob_count


# ── Shell REPL ────────────────────────────────────────────────────────────────

async def _run_shell(
    connection: Any,
    peer_name: str,
    raw: bool = False,
) -> None:
    """Run the interactive shell REPL."""
    console = Console()
    display = Display(console=console, raw=raw)

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

    if is_first_time() and not raw:
        guide = GuideManager(display, tour=DEFAULT_TOUR)
        guide.fire("connected")
    else:
        guide = GuideManager(display)  # empty tour, no-op
        guide.disable()

    # Shell state
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
) -> None:
    """Launch the interactive shell.

    Args:
        peer_addr: Address of the peer to connect to.
        rcan_path: Path to RCAN credential file.
        demo: If True, use demo connection (no real peer).
    """
    if demo or peer_addr is None:
        connection = DemoConnection()
        peer_name = "demo"
        await connection.connect()
    else:
        connection = PeerConnection(peer_addr=peer_addr, rcan_path=rcan_path)
        await connection.connect()
        # Extract short peer name from address
        peer_name = peer_addr[:16] + "..." if len(peer_addr) > 16 else peer_addr

    try:
        await _run_shell(connection, peer_name)
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
        "--json",
        action="store_true",
        dest="raw_json",
        help="Output raw JSON (for piping)",
    )


def run_shell_command(args: argparse.Namespace) -> int:
    """Execute the ``aster shell`` command."""
    demo = args.demo or args.peer is None

    try:
        asyncio.run(launch_shell(
            peer_addr=args.peer,
            rcan_path=args.rcan,
            demo=demo,
        ))
    except KeyboardInterrupt:
        pass

    return 0
