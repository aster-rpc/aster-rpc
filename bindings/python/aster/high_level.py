"""
aster.high_level — Declarative ``AsterServer`` / ``AsterClient`` wrappers.

Thin composition over the existing low-level primitives
(:class:`aster.Server`, :func:`aster.trust.consumer.serve_consumer_admission`,
:func:`aster.create_endpoint_with_config`, :func:`aster.client.create_client`)
to give application code a one-line, declarative producer/consumer experience.

Example (producer)::

    async with AsterServer(services=[HelloService()], root_pubkey=pub) as srv:
        print(srv.admission_addr_b64, srv.rpc_addr_b64)
        await srv.serve()  # blocks until cancelled

Example (consumer)::

    async with AsterClient(
        root_pubkey=pub, root_privkey=priv, admission_addr=addr_b64,
    ) as c:
        hello = c.client(HelloService)
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

from . import create_endpoint_with_config, EndpointConfig, NodeAddr
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

    Wraps endpoint creation, ``ServiceSummary`` construction with per-spec
    ``contract_id``, consumer admission, and the low-level :class:`Server`
    behind one async context manager.

    The flag matrix:

    * ``allow_all_consumers=True`` and ``allow_all_producers=True`` →
      no admission endpoints, no :class:`MeshEndpointHook`.
    * ``allow_all_consumers=False`` → run ``aster.consumer_admission`` to
      gate consumers against ``root_pubkey``.
    * ``allow_all_producers=False`` → run ``aster.producer_admission`` to
      gate peer producers (mesh join, §2.4). If no ``mesh_state`` is
      provided, an ephemeral founding state is auto-created (random salt,
      empty accepted-producer set, no persistence).
    """

    def __init__(
        self,
        services: list,
        *,
        endpoint_config: EndpointConfig | None = None,
        root_pubkey: bytes | None = None,
        allow_all_consumers: bool = False,
        allow_all_producers: bool = True,
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
        if (not allow_all_consumers or not allow_all_producers) and root_pubkey is None:
            raise ValueError(
                "root_pubkey is required when admission is enabled "
                "(allow_all_consumers=False or allow_all_producers=False)"
            )

        self._services_in: list = list(services)
        self._endpoint_config_template = endpoint_config
        self._root_pubkey = root_pubkey
        self._allow_all_consumers = allow_all_consumers
        self._allow_all_producers = allow_all_producers
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
        self._ep: Any | None = None
        self._service_summaries: list[ServiceSummary] = []
        self._server: Server | None = None

        # Populated by serve()
        self._serve_task: asyncio.Task | None = None
        self._subtasks: list[asyncio.Task] = []
        self._closed: bool = False

    # ── Lifecycle ────────────────────────────────────────────────────────────

    async def start(self) -> None:
        """Create endpoints and compute ``ServiceSummary`` list. Idempotent."""
        if self._started:
            return

        # One endpoint serves RPC + any enabled admission ALPN; the accept
        # loop dispatches per connection by ALPN.
        alpns: list[bytes] = [RPC_ALPN]
        if not self._allow_all_consumers:
            alpns.append(ALPN_CONSUMER_ADMISSION)
            if self._hook is None:
                self._hook = MeshEndpointHook()
            if self._nonce_store is None:
                self._nonce_store = InMemoryNonceStore()
        if not self._allow_all_producers:
            alpns.append(ALPN_PRODUCER_ADMISSION)
        cfg = _clone_config_with_alpns(self._endpoint_config_template, alpns)
        self._ep = await create_endpoint_with_config(cfg)
        rpc_addr_b64 = base64.b64encode(
            self._ep.endpoint_addr_info().to_bytes()
        ).decode()

        # Auto-create an ephemeral MeshState if producer admission is enabled
        # and no caller-supplied state was given.
        if not self._allow_all_producers and self._mesh_state is None:
            assert self._root_pubkey is not None
            self._mesh_state = make_ephemeral_mesh_state(self._root_pubkey)

        # Build ServiceSummary list with per-spec contract_id for each service.
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
                    channels={self._channel_name: rpc_addr_b64},
                )
            )
        self._service_summaries = summaries

        # Construct the low-level Server. It borrows the shared endpoint —
        # we (AsterServer) drive the accept loop; Server.handle_connection()
        # is called per RPC-ALPN connection.
        self._server = Server(
            self._ep,
            services=self._services_in,
            codec=self._codec,
            interceptors=self._interceptors,
        )

        self._started = True

    def serve(self) -> asyncio.Task:
        """Spawn the RPC and admission serve loops; return the aggregate task.

        Calling ``await server.serve()`` blocks until cancellation. The second
        call returns the same task, so calling this inside a context manager
        and then awaiting its result is safe.
        """
        if self._serve_task is not None:
            return self._serve_task
        if not self._started:
            raise RuntimeError("AsterServer.serve() called before start()")
        assert self._server is not None

        subtasks: list[asyncio.Task] = [
            asyncio.create_task(self._accept_loop(), name="aster-accept"),
        ]
        self._subtasks = subtasks

        async def _wait_all() -> None:
            await asyncio.gather(*subtasks, return_exceptions=True)

        self._serve_task = asyncio.create_task(_wait_all(), name="aster-server-serve")
        return self._serve_task

    async def _accept_loop(self) -> None:
        """Single accept loop over the shared endpoint; dispatch per ALPN."""
        assert self._ep is not None
        assert self._server is not None
        services_snapshot = list(self._service_summaries)
        try:
            while True:
                try:
                    conn = await self._ep.accept()
                except asyncio.CancelledError:
                    raise
                except Exception as exc:  # noqa: BLE001
                    logger.warning("AsterServer: accept() failed: %s", exc)
                    continue

                try:
                    alpn = conn.connection_info().alpn
                    if not isinstance(alpn, bytes):
                        alpn = bytes(alpn)
                except Exception:  # noqa: BLE001
                    alpn = b""

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
                    # Unknown/disabled ALPN: close the connection.
                    try:
                        conn.close(0, b"unexpected alpn")
                    except Exception:  # noqa: BLE001
                        pass
        except asyncio.CancelledError:
            pass

    async def close(self) -> None:
        """Cancel serve loops and close endpoints. Safe to call multiple times."""
        if self._closed:
            return
        self._closed = True

        # Cancel subtasks and the aggregate.
        for t in self._subtasks:
            t.cancel()
        if self._serve_task is not None:
            self._serve_task.cancel()
            try:
                await self._serve_task
            except (asyncio.CancelledError, Exception):
                pass

        # Close the RPC server (also closes the shared endpoint per server.py).
        if self._server is not None:
            try:
                await self._server.close()
            except Exception:
                pass

    async def __aenter__(self) -> "AsterServer":
        await self.start()
        # Spawn serve loops eagerly so the caller can just `await srv.serve()`
        # or use the server without ever calling serve() explicitly (e.g., if
        # another task drives shutdown).
        self.serve()
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.close()

    # ── Properties ───────────────────────────────────────────────────────────

    @property
    def endpoint_addr_b64(self) -> str:
        """Base64 ``NodeAddr`` of the shared endpoint — serves RPC and any
        enabled admission ALPNs. Clients dial this using the ALPN they want.
        """
        self._require_started()
        assert self._ep is not None
        return base64.b64encode(self._ep.endpoint_addr_info().to_bytes()).decode()

    # Per-ALPN aliases (same base64 string; returns None when the ALPN is off).
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
    def mesh_state(self) -> MeshState | None:
        return self._mesh_state

    @property
    def services(self) -> list[ServiceSummary]:
        self._require_started()
        return list(self._service_summaries)

    @property
    def endpoint(self) -> Any:
        """Escape hatch: the shared ``NetClient`` endpoint."""
        self._require_started()
        return self._ep

    # Back-compat alias.
    @property
    def rpc_endpoint(self) -> Any:
        return self.endpoint

    @property
    def root_pubkey(self) -> bytes | None:
        return self._root_pubkey

    def _require_started(self) -> None:
        if not self._started:
            raise RuntimeError("AsterServer not started; call start() first")


# ── AsterClient ──────────────────────────────────────────────────────────────


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
        self._rpc_conns: dict[str, Any] = {}   # rpc_addr_b64 → IrohConnection
        self._clients: list[ServiceClient] = []
        self._connected: bool = False
        self._closed: bool = False

    # ── Lifecycle ────────────────────────────────────────────────────────────

    async def connect(self) -> None:
        """Create endpoint, run admission handshake, store services. Idempotent."""
        if self._connected:
            return

        ep_config = _clone_config_with_alpns(
            self._endpoint_config_template, [ALPN_CONSUMER_ADMISSION, RPC_ALPN]
        )
        self._ep = await create_endpoint_with_config(ep_config)

        # Mint credential if not supplied.
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

        # Admission handshake.
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
        """Return an RPC client for ``service_cls``, opening a channel conn on demand."""
        if not self._connected:
            raise RuntimeError("AsterClient not connected; call connect() first")

        info = getattr(service_cls, "__aster_service_info__", None)
        if info is None:
            raise TypeError(
                f"{service_cls!r} is not @service-decorated "
                f"(missing __aster_service_info__)"
            )

        # Find matching service summary (by name + version).
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
        self._rpc_conns.clear()  # IrohConnections close with the endpoint

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

    # ── Properties ───────────────────────────────────────────────────────────

    @property
    def services(self) -> list[ServiceSummary]:
        return list(self._services)


# ── helpers ──────────────────────────────────────────────────────────────────


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
    # Start from the template's alpns, add ours.
    for a in list(template.alpns):
        if a not in seen:
            seen.add(a)
            merged.append(a)
    kwargs: dict[str, Any] = {"alpns": merged}
    # Copy over optional fields if set on the template.
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
