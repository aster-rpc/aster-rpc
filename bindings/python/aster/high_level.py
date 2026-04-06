"""
aster.high_level — Declarative ``AsterServer`` / ``AsterClient`` wrappers.

Thin composition over the existing low-level primitives
(:class:`aster.Server`, :func:`aster.trust.consumer.serve_consumer_admission`,
:func:`aster.client.create_client`) to give application code a one-line,
declarative producer/consumer experience.

The ``AsterServer`` builds a single ``IrohNode`` serving blobs + docs +
gossip + aster RPC + admission ALPNs on one endpoint, one node ID. Gate 0
(connection-level admission hook, trust spec §3.3) is automatically wired
when any admission flag is active.

Example (producer)::

    async with AsterServer(services=[HelloService()], root_pubkey=pub) as srv:
        print(srv.endpoint_addr_b64)
        await srv.serve()  # blocks until cancelled

Example (consumer)::

    async with AsterClient(
        root_pubkey=pub, root_privkey=priv, admission_addr=addr_b64,
    ) as c:
        hello = await c.client(HelloService)
        print((await hello.say_hello(HelloRequest(name="World"))).message)
"""
from __future__ import annotations

import asyncio
import base64
import inspect
import logging
import os
import time
import warnings
from typing import Any, Iterable

logger = logging.getLogger(__name__)

from . import (
    IrohNode,
    EndpointConfig,
    NodeAddr,
    create_endpoint_with_config,
    net_client,
    blobs_client,
    docs_client,
    gossip_client, AsterConfig,
)
from .client import ServiceClient, create_client
from .contract.identity import contract_id_from_service
from .registry.models import ServiceSummary
from .server import Server
from .trust.bootstrap import (
    handle_producer_admission_connection,
    make_ephemeral_mesh_state,
)
from .trust.consumer import (
    ConsumerAdmissionRequest,
    ConsumerAdmissionResponse,
    consumer_cred_to_json,
    handle_consumer_admission_connection,
)
from .trust.credentials import ConsumerEnrollmentCredential
from .trust.hooks import (
    ALPN_CONSUMER_ADMISSION,
    ALPN_PRODUCER_ADMISSION,
    MeshEndpointHook,
)
from .trust.mesh import ClockDriftConfig, MeshState
from .trust.nonces import InMemoryNonceStore
from .trust.signing import sign_credential

__all__ = ["AsterServer", "AsterClient", "RPC_ALPN"]

RPC_ALPN: bytes = b"aster/1"


# ── AsterServer ──────────────────────────────────────────────────────────────


class AsterServer:
    """High-level, declarative producer.

    Builds a single :class:`IrohNode` that serves blobs + docs + gossip
    (iroh built-in protocols) alongside aster RPC (``aster/1``) and any
    enabled admission ALPNs — all on **one endpoint, one node ID**.

    When any admission gate is active (``allow_all_consumers=False`` or
    ``allow_all_producers=False``), the node is built with
    ``enable_hooks=True`` and a background task runs the Gate 0
    connection-level hook loop (``MeshEndpointHook.run_hook_loop``), which
    gates *all* protocols (blobs, docs, gossip, aster/1, admission) at the
    QUIC handshake layer.
    """

    def __init__(
        self,
        services: list,
        *,
        config: "AsterConfig | None" = None,
        peer: str | None = None,
        # Inline overrides (take priority over config):
        root_pubkey: bytes | None = None,
        allow_all_consumers: bool | None = None,
        allow_all_producers: bool | None = None,
        endpoint_config: EndpointConfig | None = None,
        # Internal wiring:
        channel_name: str = "rpc",
        codec: Any | None = None,
        interceptors: list[Any] | None = None,
        hook: MeshEndpointHook | None = None,
        nonce_store: Any | None = None,
        registry_ticket: str = "",
        mesh_state: MeshState | None = None,
        clock_drift_config: ClockDriftConfig | None = None,
        persist_mesh_state: bool = False,
    ) -> None:
        if not services:
            raise ValueError("AsterServer requires at least one service")

        # Auto-load config from env if none provided.
        from .config import AsterConfig
        if config is None:
            config = AsterConfig.from_env()
        self._config = config

        # Inline overrides win over config.
        self._allow_all_consumers = (
            allow_all_consumers if allow_all_consumers is not None
            else config.allow_all_consumers
        )
        self._allow_all_producers = (
            allow_all_producers if allow_all_producers is not None
            else config.allow_all_producers
        )

        # Load identity file if present (.aster-identity).
        self._peer_name = peer
        secret_key_from_identity, peer_entry = config.load_identity(
            peer_name=peer, role="producer"
        )
        if peer_entry and not root_pubkey:
            # Root pubkey comes from the credential in the identity file.
            root_pubkey = bytes.fromhex(peer_entry["root_pubkey"])
        if secret_key_from_identity and not config.secret_key:
            import base64 as _b64
            config.secret_key = secret_key_from_identity

        # Resolve root public key: inline > identity file > config file > ephemeral.
        # The root private key is NEVER on a running node (trust spec §1.1).
        pub = config.resolve_root_pubkey()
        self._root_pubkey = root_pubkey if root_pubkey is not None else pub

        # Dev mode: if using an ephemeral root key (no explicit pubkey file),
        # auto-open the consumer gate so the quickstart works without
        # credential files. In production (explicit root_pubkey_file),
        # the default allow_all_consumers=False requires credentials.
        if (
            config._ephemeral_privkey is not None
            and allow_all_consumers is None
            and config.root_pubkey_file is None
        ):
            self._allow_all_consumers = True
            logger.info(
                "Dev mode: allow_all_consumers=True (ephemeral root key). "
                "Set ASTER_ROOT_PUBKEY_FILE for production admission."
            )

        if (not self._allow_all_consumers or not self._allow_all_producers) and self._root_pubkey is None:
            raise ValueError(
                "root_pubkey is required when admission is enabled "
                "(allow_all_consumers=False or allow_all_producers=False). "
                "Set ASTER_ROOT_PUBKEY_FILE or pass root_pubkey= explicitly."
            )

        self._services_in: list = list(services)
        self._endpoint_config_template = endpoint_config or config.to_endpoint_config()
        self._channel_name = channel_name
        self._codec = codec
        self._interceptors = list(interceptors) if interceptors else []
        self._hook = hook
        self._nonce_store = nonce_store
        self._registry_ticket = registry_ticket
        self._mesh_state = mesh_state
        self._clock_drift_config = clock_drift_config
        self._persist_mesh_state = persist_mesh_state

        # Populated by start()
        self._started: bool = False
        self._node: IrohNode | None = None
        self._service_summaries: list[ServiceSummary] = []
        self._server: Server | None = None
        # Lazy caches for .blobs / .docs / .gossip
        self._blobs: Any | None = None
        self._docs: Any | None = None
        self._gossip: Any | None = None

        # Populated by serve()
        self._serve_task: asyncio.Task | None = None
        self._subtasks: list[asyncio.Task] = []
        self._closed: bool = False

    # ── Lifecycle ────────────────────────────────────────────────────────────

    async def start(self) -> None:
        """Create the unified node and compute ``ServiceSummary`` list. Idempotent."""
        if self._started:
            return

        # Configure structured logging from config (idempotent)
        from aster.logging import configure_logging
        configure_logging(
            format=self._config.log_format,
            level=self._config.log_level,
            mask=self._config.log_mask,
        )

        # Determine which aster ALPNs to register on the Router.
        # Consumer admission is ALWAYS registered — even in open-gate mode
        # the consumer uses it to discover services.
        aster_alpns: list[bytes] = [RPC_ALPN, ALPN_CONSUMER_ADMISSION]
        gate0_needed = False
        if not self._allow_all_consumers:
            if self._hook is None:
                self._hook = MeshEndpointHook()
            if self._nonce_store is None:
                self._nonce_store = InMemoryNonceStore()
            gate0_needed = True
        if not self._allow_all_producers:
            aster_alpns.append(ALPN_PRODUCER_ADMISSION)
            if self._hook is None:
                self._hook = MeshEndpointHook()
            gate0_needed = True

        # Build EndpointConfig so hooks (Gate 0) are installed when needed.
        ep_cfg = _build_node_endpoint_config(
            self._endpoint_config_template, enable_hooks=gate0_needed
        )

        self._node = await IrohNode.memory_with_alpns(aster_alpns, ep_cfg)
        addr_b64 = base64.b64encode(
            self._node.node_addr_info().to_bytes()
        ).decode()

        # Auto-create ephemeral MeshState when producer admission is enabled.
        if not self._allow_all_producers and self._mesh_state is None:
            assert self._root_pubkey is not None
            self._mesh_state = make_ephemeral_mesh_state(self._root_pubkey)

        # Build ServiceSummary list with per-spec contract_id.
        summaries: list[ServiceSummary] = []
        for svc in self._services_in:
            svc_cls = svc if inspect.isclass(svc) else type(svc)
            info = getattr(svc_cls, "__aster_service_info__", None)
            if info is None:
                raise TypeError(
                    f"{svc_cls!r} is not @service-decorated "
                    f"(missing __aster_service_info__)"
                )
            cid = contract_id_from_service(svc_cls)
            summaries.append(
                ServiceSummary(
                    name=info.name,
                    version=info.version,
                    contract_id=cid,
                    channels={self._channel_name: addr_b64},
                )
            )
        self._service_summaries = summaries

        # ── C11.4.3: Startup publication verification ────────────────────
        # If .aster/manifest.json exists, verify that each service's live
        # contract_id matches the committed manifest. Fatal on mismatch.
        manifest_path = os.path.join(os.getcwd(), ".aster", "manifest.json")
        if os.path.isfile(manifest_path):
            import json as _json
            from .contract.manifest import ContractManifest

            try:
                with open(manifest_path, encoding="utf-8") as f:
                    manifest_data = _json.load(f)
            except Exception as exc:
                raise RuntimeError(
                    f"Failed to read manifest at {manifest_path}: {exc}"
                ) from exc

            # Support both single manifest and list of manifests
            manifests_list = manifest_data if isinstance(manifest_data, list) else [manifest_data]
            manifest_by_key: dict[tuple[str, int], ContractManifest] = {}
            for md in manifests_list:
                m = ContractManifest(**md)
                manifest_by_key[(m.service, m.version)] = m

            for summary in summaries:
                key = (summary.name, summary.version)
                manifest = manifest_by_key.get(key)
                if manifest is None:
                    continue  # Service not in manifest — skip
                if summary.contract_id != manifest.contract_id:
                    raise RuntimeError(
                        f"Contract identity mismatch for {summary.name!r} "
                        f"v{summary.version}:\n"
                        f"  Expected (manifest): {manifest.contract_id}\n"
                        f"  Actual (live):       {summary.contract_id}\n"
                        f"  Manifest: {manifest_path}\n"
                        f"  -> The service interface has changed without "
                        f"updating the manifest.\n"
                        f"     Rerun `aster contract gen` and commit the "
                        f"updated manifest."
                    )
            logger.debug("Manifest verification passed for %d service(s)", len(manifest_by_key))

        # ── Contract publication to registry doc ─────────────────────────
        # Create a registry doc, publish contract collections (with manifest
        # including method schemas), and generate a read-only share ticket
        # so consumers can discover full contract metadata.
        await self._publish_contracts()

        # ── Gate 3: capability interceptor & default-deny warning ────────
        #
        # Build a service_name -> ServiceInfo map for the CapabilityInterceptor
        # and check whether any service has authorization configured.
        from .interceptors.capability import CapabilityInterceptor

        svc_map: dict[str, Any] = {}
        any_has_requires = False
        for svc in self._services_in:
            svc_cls = svc if inspect.isclass(svc) else type(svc)
            info = getattr(svc_cls, "__aster_service_info__", None)
            if info is not None:
                svc_map[info.name] = info
                if info.requires is not None:
                    any_has_requires = True
                for mi in info.methods.values():
                    if mi.requires is not None:
                        any_has_requires = True

        # Auto-wire the CapabilityInterceptor when trust is configured
        # (Gate 0 enabled) or when any service declares requires.
        has_auth_interceptor = any(
            isinstance(i, CapabilityInterceptor) for i in self._interceptors
        )
        if (not self._allow_all_consumers or any_has_requires) and not has_auth_interceptor:
            # Prepend so capability is checked before application interceptors.
            self._interceptors.insert(0, CapabilityInterceptor(svc_map))

        # S12.2: default-deny startup warning.
        # When Gate 0 is disabled (allow_all_consumers=True), any service
        # without explicit authorization is wide open.  Emit a warning so
        # the developer knows.
        if self._allow_all_consumers:
            for svc in self._services_in:
                svc_cls = svc if inspect.isclass(svc) else type(svc)
                info = getattr(svc_cls, "__aster_service_info__", None)
                if info is None:
                    continue
                svc_has_auth = info.requires is not None
                if not svc_has_auth:
                    svc_has_auth = any(
                        mi.requires is not None for mi in info.methods.values()
                    )
                if not svc_has_auth:
                    # Also check if user explicitly added an auth interceptor.
                    from .interceptors.auth import AuthInterceptor
                    has_explicit_auth = any(
                        isinstance(i, AuthInterceptor) for i in self._interceptors
                    )
                    if not has_explicit_auth:
                        warnings.warn(
                            f"Service '{info.name}' has no authorization configured "
                            f"and Gate 0 is disabled (allow_all_consumers=True). "
                            f"All consumers can call this service without "
                            f"authentication. Add @service(requires=...) or "
                            f"configure an auth interceptor for production use.",
                            UserWarning,
                            stacklevel=2,
                        )

        # Server borrows a NetClient view of the node. AsterServer owns the
        # node lifecycle, so Server must NOT close the endpoint on its own.
        self._server = Server(
            net_client(self._node),
            services=self._services_in,
            codec=self._codec,
            interceptors=self._interceptors,
            owns_endpoint=False,
        )

        self._print_banner()
        self._started = True

    def _print_banner(self) -> None:
        """Print the startup banner with service info."""
        import sys

        # Only print banner when stderr is a terminal (not in tests/pipes)
        if not sys.stderr.isatty():
            return

        C = "\033[36m"   # cyan
        B = "\033[1m"    # bold
        D = "\033[2m"    # dim
        G = "\033[32m"   # green
        Y = "\033[33m"   # yellow
        R = "\033[0m"    # reset

        banner = f"""{C}
     ╭──────────────────────────────────────────╮
     │{B}           _    ____ _____ _____ ____     {R}{C}│
     │{B}          / \\  / ___|_   _| ____|  _ \\    {R}{C}│
     │{B}         / _ \\ \\___ \\ | | |  _| | |_) |   {R}{C}│
     │{B}        / ___ \\ ___) || | | |___|  _ <    {R}{C}│
     │{B}       /_/   \\_\\____/ |_| |_____|_| \\_\\   {R}{C}│
     │{D}          RPC after hostnames.            {R}{C}│
     ╰──────────────────────────────────────────╯{R}
"""
        sys.stderr.write(banner)

        # Service info
        for s in self._service_summaries:
            sys.stderr.write(f"  {G}●{R} {B}{s.name}{R} v{s.version}  {D}contract:{R} {s.contract_id[:16]}…\n")

        # Endpoint
        addr = base64.b64encode(
            self._node.node_addr_info().to_bytes()
        ).decode() if self._node else "?"
        sys.stderr.write(f"  {D}endpoint:{R}  {addr[:48]}…\n")

        # Mode
        mode_parts = []
        if self._allow_all_consumers:
            mode_parts.append(f"{Y}open-gate{R}")
        else:
            mode_parts.append(f"{G}trusted{R}")
        if self._registry_ticket:
            mode_parts.append(f"{G}registry{R}")
        sys.stderr.write(f"  {D}mode:{R}      {' '.join(mode_parts)}\n")
        sys.stderr.write(f"  {D}log:{R}       ASTER_LOG_FORMAT=json|text  ASTER_LOG_LEVEL=debug|info|warn\n")
        sys.stderr.write("\n")

    async def _publish_contracts(self) -> None:
        """Create a registry doc and publish each service's contract collection.

        After publication, ``self._registry_ticket`` is set to the read-only
        share ticket so consumer admission can return it.
        """
        assert self._node is not None

        try:
            dc = docs_client(self._node)
            bc = blobs_client(self._node)

            # Create the registry doc (producer owns the write capability)
            registry_doc = await dc.create()
            author_id = await dc.create_author()

            for svc in self._services_in:
                svc_cls = svc if inspect.isclass(svc) else type(svc)
                info = getattr(svc_cls, "__aster_service_info__", None)
                if info is None:
                    continue

                # Build the type graph and contract
                from .contract.identity import (
                    ServiceContract,
                    build_type_graph,
                    canonical_xlang_bytes,
                    resolve_with_cycles,
                    compute_type_hash,
                )
                from .contract.publication import build_collection, upload_collection
                from .registry.keys import contract_key, version_key
                from .registry.models import ArtifactRef

                # Collect root types
                root_types: list[type] = []
                for mi in info.methods.values():
                    if mi.request_type is not None:
                        root_types.append(mi.request_type)
                    if mi.response_type is not None:
                        root_types.append(mi.response_type)

                type_graph = build_type_graph(root_types)
                type_defs = resolve_with_cycles(type_graph)

                # Compute type hashes
                type_hashes: dict[str, bytes] = {}
                for fqn, td in type_defs.items():
                    td_bytes = canonical_xlang_bytes(td)
                    type_hashes[fqn] = compute_type_hash(td_bytes)

                # Build ServiceContract and canonical bytes
                contract = ServiceContract.from_service_info(info, type_hashes)
                contract_bytes = canonical_xlang_bytes(contract)

                import blake3 as _blake3
                contract_id = _blake3.blake3(contract_bytes).hexdigest()

                # Build collection with full method schemas
                entries = build_collection(contract, type_defs, service_info=info)

                # Upload to blob store
                collection_hash = await upload_collection(bc, entries)

                # Create a collection ticket so consumers can download all
                # collection blobs (index + entries) in one transfer
                blob_ticket = bc.create_collection_ticket(collection_hash)

                # Write ArtifactRef to registry doc
                import time as _time
                ref = ArtifactRef(
                    contract_id=contract_id,
                    collection_hash=collection_hash,
                    ticket=blob_ticket,
                    published_by=author_id,
                    published_at_epoch_ms=int(_time.time() * 1000),
                    collection_format="index",
                )
                await registry_doc.set_bytes(
                    author_id,
                    contract_key(contract_id),
                    ref.to_json().encode(),
                )

                # Also write the manifest JSON directly to the registry doc
                # at a well-known key. This avoids the blob download round-trip
                # for consumers that only need method schemas (like the shell).
                manifest_data = None
                for ename, edata in entries:
                    if ename == "manifest.json":
                        manifest_data = edata
                        break
                if manifest_data:
                    from .registry.keys import version_key as _vk
                    manifest_key = f"manifests/{contract_id}".encode()
                    await registry_doc.set_bytes(
                        author_id, manifest_key, manifest_data
                    )

                # Version pointer
                await registry_doc.set_bytes(
                    author_id,
                    version_key(info.name, info.version),
                    contract_id.encode(),
                )

                logger.debug(
                    "Published contract %s for %s v%d (collection=%s)",
                    contract_id[:12],
                    info.name,
                    info.version,
                    collection_hash[:12],
                )

            # Generate read-only share ticket for consumers
            self._registry_ticket = await registry_doc.share_with_addr("read")
            logger.info(
                "Registry doc ready — ticket length: %d", len(self._registry_ticket)
            )

        except Exception as exc:
            # Publication failure is non-fatal — the server still works,
            # consumers just won't get rich contract metadata
            logger.warning("Contract publication failed (non-fatal): %s", exc)

    def serve(self) -> asyncio.Task:
        """Spawn the accept loop (+ Gate 0 hook loop); return an aggregate task.

        ``await server.serve()`` blocks until cancellation. The second call
        returns the same task (idempotent).
        """
        if self._serve_task is not None:
            return self._serve_task
        if not self._started:
            raise RuntimeError("AsterServer.serve() called before start()")
        assert self._server is not None
        assert self._node is not None

        subtasks: list[asyncio.Task] = []

        # Gate 0 hook loop: drain the after-handshake channel, apply the
        # MeshEndpointHook allowlist for every connection. before_connect is
        # auto-accepted inside NodeHookReceiver (the peer's endpoint ID
        # isn't authenticated at that stage).
        if self._hook is not None and self._node.has_hooks():
            self._hook_loop_task = asyncio.create_task(
                self._run_gate0(), name="aster-gate0"
            )
            subtasks.append(self._hook_loop_task)

        subtasks.append(
            asyncio.create_task(self._accept_loop(), name="aster-accept")
        )
        self._subtasks = subtasks

        async def _wait_all() -> None:
            await asyncio.gather(*subtasks, return_exceptions=True)

        self._serve_task = asyncio.create_task(_wait_all(), name="aster-server-serve")
        return self._serve_task

    async def _run_gate0(self) -> None:
        """Take the hook receiver from the node and run the hook loop."""
        assert self._node is not None
        assert self._hook is not None
        receiver = await self._node.take_hook_receiver()
        if receiver is None:
            logger.warning("AsterServer: hooks enabled but no receiver available")
            return
        await self._hook.run_hook_loop(receiver)

    async def _accept_loop(self) -> None:
        """Pull from ``node.accept_aster()`` and dispatch per ALPN."""
        assert self._node is not None
        assert self._server is not None
        services_snapshot = list(self._service_summaries)
        try:
            while True:
                try:
                    alpn, conn = await self._node.accept_aster()
                except asyncio.CancelledError:
                    raise
                except Exception as exc:  # noqa: BLE001
                    logger.warning("AsterServer: accept_aster failed: %s", exc)
                    continue

                if alpn == RPC_ALPN:
                    asyncio.create_task(
                        self._server.handle_connection(conn),
                        name="aster-rpc-conn",
                    )
                elif alpn == ALPN_CONSUMER_ADMISSION:
                    # Always handle consumer admission — even when
                    # allow_all_consumers=True the consumer needs the
                    # services list from the admission response.
                    asyncio.create_task(
                        handle_consumer_admission_connection(
                            conn,
                            root_pubkey=self._root_pubkey or b"\x00" * 32,
                            hook=self._hook,
                            nonce_store=self._nonce_store,
                            services_getter=lambda: services_snapshot,
                            registry_ticket_getter=lambda: self._registry_ticket,
                            allow_unenrolled=self._allow_all_consumers,
                        ),
                        name="aster-consumer-admission-conn",
                    )
                elif alpn == ALPN_PRODUCER_ADMISSION and not self._allow_all_producers:
                    assert self._root_pubkey is not None
                    assert self._mesh_state is not None
                    asyncio.create_task(
                        handle_producer_admission_connection(
                            conn,
                            own_root_pubkey=self._root_pubkey,
                            own_state=self._mesh_state,
                            config=self._clock_drift_config,
                            persist_state=self._persist_mesh_state,
                        ),
                        name="aster-producer-admission-conn",
                    )
                else:
                    try:
                        conn.close(0, b"unexpected alpn")
                    except Exception:  # noqa: BLE001
                        pass
        except asyncio.CancelledError:
            pass

    async def close(self) -> None:
        """Cancel serve loops and close the node. Safe to call multiple times."""
        if self._closed:
            return
        self._closed = True

        for t in self._subtasks:
            t.cancel()
        if self._serve_task is not None:
            self._serve_task.cancel()
            try:
                await self._serve_task
            except (asyncio.CancelledError, Exception):
                pass

        # Close the node — this triggers router.shutdown() which closes all
        # protocol handlers (including aster queue handlers) and the endpoint.
        if self._node is not None:
            try:
                await self._node.close()
            except Exception:
                pass

    async def __aenter__(self) -> "AsterServer":
        await self.start()
        self.serve()
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.close()

    # ── Properties ───────────────────────────────────────────────────────────

    @property
    def endpoint_addr_b64(self) -> str:
        """Base64 ``NodeAddr`` of the shared endpoint (one node ID for
        RPC + admission + blobs + docs + gossip)."""
        self._require_started()
        assert self._node is not None
        return base64.b64encode(self._node.node_addr_info().to_bytes()).decode()

    @property
    def rpc_addr_b64(self) -> str:
        return self.endpoint_addr_b64

    @property
    def admission_addr_b64(self) -> str | None:
        if self._allow_all_consumers and self._allow_all_producers:
            return None
        return self.endpoint_addr_b64

    @property
    def consumer_admission_addr_b64(self) -> str | None:
        if self._allow_all_consumers:
            return None
        return self.endpoint_addr_b64

    @property
    def producer_admission_addr_b64(self) -> str | None:
        if self._allow_all_producers:
            return None
        return self.endpoint_addr_b64

    @property
    def services(self) -> list[ServiceSummary]:
        self._require_started()
        return list(self._service_summaries)

    @property
    def mesh_state(self) -> MeshState | None:
        return self._mesh_state

    @property
    def root_pubkey(self) -> bytes | None:
        return self._root_pubkey

    # ── Iroh protocol clients (lazy) ─────────────────────────────────────────

    @property
    def node(self) -> IrohNode:
        """The underlying ``IrohNode`` (escape hatch for direct iroh access)."""
        self._require_started()
        assert self._node is not None
        return self._node

    @property
    def blobs(self) -> Any:
        """Blobs client backed by this node."""
        self._require_started()
        if self._blobs is None:
            self._blobs = blobs_client(self._node)
        return self._blobs

    @property
    def docs(self) -> Any:
        """Docs client backed by this node."""
        self._require_started()
        if self._docs is None:
            self._docs = docs_client(self._node)
        return self._docs

    @property
    def gossip(self) -> Any:
        """Gossip client backed by this node."""
        self._require_started()
        if self._gossip is None:
            self._gossip = gossip_client(self._node)
        return self._gossip

    # Back-compat aliases
    @property
    def endpoint(self) -> Any:
        """Escape hatch: the ``NetClient`` view of this node's endpoint."""
        self._require_started()
        return net_client(self._node)

    @property
    def rpc_endpoint(self) -> Any:
        return self.endpoint

    def _require_started(self) -> None:
        if not self._started:
            raise RuntimeError("AsterServer not started; call start() first")


# ── AsterClient ──────────────────────────────────────────────────────────────


class AsterClient:
    """High-level, declarative consumer.

    Reads configuration from :class:`AsterConfig` (env vars / TOML file)
    just like :class:`AsterServer`.  In dev mode (no credentials, ephemeral
    producer), ``AsterClient()`` with just ``ASTER_ENDPOINT_ADDR`` set is
    enough.  In production, set ``ASTER_ENROLLMENT_CREDENTIAL`` to the
    path of a pre-signed token from the operator.

    ``endpoint_addr`` may be a base64 ``NodeAddr`` string (as printed by
    :class:`AsterServer`), an ``EndpointId`` hex string (when discovery is
    enabled), a :class:`NodeAddr` object, or raw ``NodeAddr.to_bytes()``
    bytes.
    """

    def __init__(
        self,
        *,
        config: "AsterConfig | None" = None,
        peer: str | None = None,
        # Inline overrides (take priority over config):
        endpoint_addr: NodeAddr | str | bytes | None = None,
        root_pubkey: bytes | None = None,
        enrollment_credential_file: str | None = None,
        # Internal wiring:
        channel_name: str = "rpc",
    ) -> None:
        from .config import AsterConfig

        if config is None:
            config = AsterConfig.from_env()
        self._config = config
        self._peer_name = peer

        # Load identity file if present (.aster-identity).
        secret_key_from_identity, peer_entry = config.load_identity(
            peer_name=peer, role="consumer"
        )
        if secret_key_from_identity and not config.secret_key:
            config.secret_key = secret_key_from_identity
        if peer_entry and not root_pubkey:
            root_pubkey = bytes.fromhex(peer_entry["root_pubkey"])

        # Resolve endpoint address: inline > config > error.
        addr = endpoint_addr or config.endpoint_addr
        if addr is None:
            raise ValueError(
                "AsterClient requires an endpoint address. "
                "Set ASTER_ENDPOINT_ADDR or pass endpoint_addr= explicitly."
            )
        self._endpoint_addr_in = addr

        # Root pubkey (for optional response validation).
        pub = config.resolve_root_pubkey()
        self._root_pubkey = root_pubkey if root_pubkey is not None else pub

        # Enrollment credential: identity file peer > inline > config > None.
        if peer_entry and not enrollment_credential_file:
            # The peer entry IS the credential — write it to a temp file
            # that _load_enrollment_credential can read, or inline it.
            self._inline_credential = peer_entry
            self._enrollment_credential_file = None
        else:
            self._inline_credential = None
            self._enrollment_credential_file = (
                enrollment_credential_file or config.enrollment_credential_file
            )
        self._enrollment_credential_iid = config.enrollment_credential_iid
        self._channel_name = channel_name

        self._node: Any | None = None
        self._ep: Any | None = None
        self._services: list[ServiceSummary] = []
        self._registry_ticket: str = ""
        self._rpc_conns: dict[str, Any] = {}
        self._clients: list[ServiceClient] = []
        self._connected: bool = False
        self._closed: bool = False

    async def connect(self) -> None:
        """Create endpoint, run admission if credential present, store services.

        Idempotent — second call is a no-op.
        """
        if self._connected:
            return

        # Build a full IrohNode so the consumer can join registry docs
        # and fetch blobs. Previously this was a bare NetClient endpoint,
        # but docs/blobs require a full node.
        ep_cfg = self._config.to_endpoint_config()
        ep_config = _clone_config_with_alpns(
            ep_cfg, [ALPN_CONSUMER_ADMISSION, RPC_ALPN]
        )
        self._node = await IrohNode.memory_with_alpns(
            [ALPN_CONSUMER_ADMISSION, RPC_ALPN], ep_config
        )
        self._ep = net_client(self._node)

        # Always run the admission handshake — even when the consumer gate
        # is open, the response carries the services list + registry ticket.
        await self._run_admission()
        self._connected = True

    async def _run_admission(self) -> None:
        """Connect via ``aster.consumer_admission`` to get services.

        If an enrollment credential is configured, it's presented for
        verification.  If not (dev mode / open gate), an empty credential
        is sent — the producer auto-admits when ``allow_all_consumers=True``.
        """
        assert self._ep is not None

        # Build credential from: inline peer entry > credential file > empty.
        if self._inline_credential:
            cred = _credential_from_peer_entry(self._inline_credential)
            cred_json = consumer_cred_to_json(cred)
        elif self._enrollment_credential_file:
            cred = _load_enrollment_credential(self._enrollment_credential_file)
            cred_json = consumer_cred_to_json(cred)
        else:
            # No credential — dev mode / open-gate flow.
            cred_json = ""

        iid_token = self._enrollment_credential_iid or ""

        target = _coerce_node_addr(self._endpoint_addr_in)
        conn = await self._ep.connect_node_addr(target, ALPN_CONSUMER_ADMISSION)
        send, recv = await conn.open_bi()
        req = ConsumerAdmissionRequest(
            credential_json=cred_json,
            iid_token=iid_token,
        )
        await send.write_all(req.to_json().encode())
        await send.finish()
        raw = await recv.read_to_end(64 * 1024)
        resp = ConsumerAdmissionResponse.from_json(raw)
        if not resp.admitted:
            raise PermissionError(
                "consumer admission denied — set ASTER_ENROLLMENT_CREDENTIAL "
                "to a valid enrollment token"
            )

        self._services = list(resp.services)
        self._registry_ticket = resp.registry_ticket or ""
        logger.info(
            "Admitted — services: %s, registry_ticket: %s",
            [s.name for s in self._services],
            bool(self._registry_ticket),
        )

    async def _rpc_conn_for(self, rpc_addr_b64: str) -> Any:
        if rpc_addr_b64 in self._rpc_conns:
            return self._rpc_conns[rpc_addr_b64]
        assert self._ep is not None
        rpc_addr = _coerce_node_addr(rpc_addr_b64)
        conn = await self._ep.connect_node_addr(rpc_addr, RPC_ALPN)
        self._rpc_conns[rpc_addr_b64] = conn
        return conn

    async def client(
        self,
        service_cls: type,
        *,
        channel: str | None = None,
        codec: Any | None = None,
        interceptors: list[Any] | None = None,
    ) -> ServiceClient:
        """Return an RPC client for ``service_cls``."""
        if not self._connected:
            raise RuntimeError("AsterClient not connected; call connect() first")

        info = getattr(service_cls, "__aster_service_info__", None)
        if info is None:
            raise TypeError(
                f"{service_cls!r} is not @service-decorated "
                f"(missing __aster_service_info__)"
            )

        summary: ServiceSummary | None = None
        for s in self._services:
            if s.name == info.name and s.version == info.version:
                summary = s
                break
        if summary is None:
            raise LookupError(
                f"service {info.name!r} v{info.version} not offered by producer "
                f"(got: {[(s.name, s.version) for s in self._services]})"
            )

        channel_key = channel or self._channel_name
        if channel_key not in summary.channels:
            raise LookupError(
                f"service {info.name!r} has no channel {channel_key!r} "
                f"(available: {list(summary.channels)})"
            )

        conn = await self._rpc_conn_for(summary.channels[channel_key])
        client = create_client(
            service_cls,
            connection=conn,
            codec=codec,
            interceptors=interceptors,
        )
        self._clients.append(client)
        return client

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True

        for c in self._clients:
            try:
                await c.close()
            except Exception:
                pass
        self._clients.clear()
        self._rpc_conns.clear()

        if self._node is not None:
            try:
                await self._node.shutdown()
            except Exception:
                pass
        elif self._ep is not None:
            try:
                await self._ep.close()
            except Exception:
                pass

    async def __aenter__(self) -> "AsterClient":
        await self.connect()
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.close()

    @property
    def services(self) -> list[ServiceSummary]:
        return list(self._services)

    @property
    def registry_ticket(self) -> str:
        """Read-only Iroh docs share ticket for the registry doc.

        Empty string if no registry doc was provided by the producer.
        """
        return self._registry_ticket


# ── helpers ──────────────────────────────────────────────────────────────────


def _build_node_endpoint_config(
    template: EndpointConfig | None,
    *,
    enable_hooks: bool = False,
) -> EndpointConfig | None:
    """Build an EndpointConfig for IrohNode.memory_with_alpns.

    Copies user-provided template fields and optionally force-enables hooks.
    Returns None when no template and no hooks are needed (caller passes None
    to the Rust side for the default presets::N0 path).
    """
    if template is None and not enable_hooks:
        return None
    kwargs: dict[str, Any] = {"alpns": []}  # Router sets ALPNs
    if template is not None:
        for attr in (
            "relay_mode", "secret_key", "enable_monitoring",
            "enable_hooks", "hook_timeout_ms",
        ):
            if hasattr(template, attr):
                val = getattr(template, attr)
                if val is not None:
                    kwargs[attr] = val
    if enable_hooks:
        kwargs["enable_hooks"] = True
    return EndpointConfig(**kwargs)


def _clone_config_with_alpns(
    template: EndpointConfig | None, alpns: Iterable[bytes]
) -> EndpointConfig:
    """Return a new EndpointConfig with ``alpns`` (preserving template options)."""
    merged: list[bytes] = []
    seen: set[bytes] = set()
    for a in alpns:
        if a not in seen:
            seen.add(a)
            merged.append(a)
    if template is None:
        return EndpointConfig(alpns=merged)
    for a in list(template.alpns):
        if a not in seen:
            seen.add(a)
            merged.append(a)
    kwargs: dict[str, Any] = {"alpns": merged}
    for attr in ("relay_mode", "secret_key", "enable_monitoring", "enable_hooks", "hook_timeout_ms"):
        if hasattr(template, attr):
            val = getattr(template, attr)
            if val is not None:
                kwargs[attr] = val
    return EndpointConfig(**kwargs)


def _credential_from_peer_entry(peer: dict) -> ConsumerEnrollmentCredential:
    """Build a ConsumerEnrollmentCredential from a [[peers]] entry in .aster-identity."""
    nonce_hex = peer.get("nonce")
    return ConsumerEnrollmentCredential(
        credential_type=peer.get("type", "policy"),
        root_pubkey=bytes.fromhex(peer["root_pubkey"]),
        expires_at=int(peer["expires_at"]),
        attributes=peer.get("attributes", {}),
        endpoint_id=peer.get("endpoint_id"),
        nonce=bytes.fromhex(nonce_hex) if nonce_hex else None,
        signature=bytes.fromhex(peer.get("signature", "")),
    )


def _load_enrollment_credential(path: str) -> ConsumerEnrollmentCredential:
    """Load a pre-signed ConsumerEnrollmentCredential from a JSON file.

    The JSON should have been created by ``aster authorize consumer`` (CLI)
    and contains: credential_type, root_pubkey (hex), expires_at, attributes,
    nonce (hex, OTT only), signature (hex).
    """
    import json as _json

    expanded = os.path.expanduser(path)
    with open(expanded) as f:
        d = _json.load(f)
    nonce_hex = d.get("nonce")
    cred = ConsumerEnrollmentCredential(
        credential_type=d.get("credential_type", "policy"),
        root_pubkey=bytes.fromhex(d["root_pubkey"]),
        expires_at=int(d["expires_at"]),
        attributes=d.get("attributes", {}),
        endpoint_id=d.get("endpoint_id"),
        nonce=bytes.fromhex(nonce_hex) if nonce_hex else None,
        signature=bytes.fromhex(d.get("signature", "")),
    )
    return cred


def _coerce_node_addr(addr: NodeAddr | str | bytes) -> NodeAddr:
    if isinstance(addr, NodeAddr):
        return addr
    if isinstance(addr, str):
        return NodeAddr.from_bytes(base64.b64decode(addr))
    if isinstance(addr, (bytes, bytearray)):
        return NodeAddr.from_bytes(bytes(addr))
    raise TypeError(f"unsupported admission_addr type: {type(addr).__name__}")
