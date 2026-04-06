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
import time
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

        # Resolve root key: inline > config file > ephemeral.
        priv, pub = config.resolve_root_key()
        self._root_pubkey = root_pubkey if root_pubkey is not None else pub
        self._root_privkey = priv

        if (not self._allow_all_consumers or not self._allow_all_producers) and self._root_pubkey is None:
            raise ValueError(
                "root_pubkey is required when admission is enabled "
                "(allow_all_consumers=False or allow_all_producers=False). "
                "Set ASTER_ROOT_KEY_FILE or pass root_pubkey= explicitly."
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

        # Determine which aster ALPNs to register on the Router.
        aster_alpns: list[bytes] = [RPC_ALPN]
        gate0_needed = False
        if not self._allow_all_consumers:
            aster_alpns.append(ALPN_CONSUMER_ADMISSION)
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

        # Server borrows a NetClient view of the node. AsterServer owns the
        # node lifecycle, so Server must NOT close the endpoint on its own.
        self._server = Server(
            net_client(self._node),
            services=self._services_in,
            codec=self._codec,
            interceptors=self._interceptors,
            owns_endpoint=False,
        )

        self._started = True

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
                elif alpn == ALPN_CONSUMER_ADMISSION and not self._allow_all_consumers:
                    assert self._root_pubkey is not None
                    asyncio.create_task(
                        handle_consumer_admission_connection(
                            conn,
                            root_pubkey=self._root_pubkey,
                            hook=self._hook,
                            nonce_store=self._nonce_store,
                            services_getter=lambda: services_snapshot,
                            registry_ticket_getter=lambda: self._registry_ticket,
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
# TODO: upgrade to IrohNode.memory_with_alpns() when client-side blobs/docs/gossip
# is needed (service discovery).


class AsterClient:
    """High-level, declarative consumer.

    Wraps credential minting, the consumer admission handshake, and RPC
    client construction behind one async context manager.

    ``admission_addr`` may be a :class:`NodeAddr`, a base64 ``NodeAddr``
    string (as printed by :class:`AsterServer`), or raw ``NodeAddr.to_bytes()``
    bytes.
    """

    def __init__(
        self,
        *,
        root_pubkey: bytes,
        admission_addr: NodeAddr | str | bytes,
        root_privkey: bytes | None = None,
        credential: ConsumerEnrollmentCredential | None = None,
        credential_attributes: dict[str, str] | None = None,
        credential_ttl_seconds: int = 3600,
        endpoint_config: EndpointConfig | None = None,
        channel_name: str = "rpc",
    ) -> None:
        if credential is None and root_privkey is None:
            raise ValueError(
                "AsterClient requires either root_privkey (to mint a credential) "
                "or a pre-built credential"
            )

        self._root_pubkey = root_pubkey
        self._root_privkey = root_privkey
        self._credential = credential
        self._credential_attributes = credential_attributes or {"aster.role": "consumer"}
        self._credential_ttl = credential_ttl_seconds
        self._admission_addr_in = admission_addr
        self._endpoint_config_template = endpoint_config
        self._channel_name = channel_name

        self._ep: Any | None = None
        self._services: list[ServiceSummary] = []
        self._rpc_conns: dict[str, Any] = {}
        self._clients: list[ServiceClient] = []
        self._connected: bool = False
        self._closed: bool = False

    async def connect(self) -> None:
        """Create endpoint, run admission handshake, store services. Idempotent."""
        if self._connected:
            return

        ep_config = _clone_config_with_alpns(
            self._endpoint_config_template, [ALPN_CONSUMER_ADMISSION, RPC_ALPN]
        )
        self._ep = await create_endpoint_with_config(ep_config)

        cred = self._credential
        if cred is None:
            assert self._root_privkey is not None
            cred = ConsumerEnrollmentCredential(
                credential_type="policy",
                root_pubkey=self._root_pubkey,
                expires_at=int(time.time()) + self._credential_ttl,
                attributes=dict(self._credential_attributes),
            )
            cred.signature = sign_credential(cred, self._root_privkey)

        admission_node_addr = _coerce_node_addr(self._admission_addr_in)
        conn = await self._ep.connect_node_addr(admission_node_addr, ALPN_CONSUMER_ADMISSION)
        send, recv = await conn.open_bi()
        req = ConsumerAdmissionRequest(credential_json=consumer_cred_to_json(cred))
        await send.write_all(req.to_json().encode())
        await send.finish()
        raw = await recv.read_to_end(64 * 1024)
        resp = ConsumerAdmissionResponse.from_json(raw)
        if not resp.admitted:
            raise PermissionError("consumer admission denied")

        self._services = list(resp.services)
        self._connected = True

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

        if self._ep is not None:
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


def _coerce_node_addr(addr: NodeAddr | str | bytes) -> NodeAddr:
    if isinstance(addr, NodeAddr):
        return addr
    if isinstance(addr, str):
        return NodeAddr.from_bytes(base64.b64decode(addr))
    if isinstance(addr, (bytes, bytearray)):
        return NodeAddr.from_bytes(bytes(addr))
    raise TypeError(f"unsupported admission_addr type: {type(addr).__name__}")
