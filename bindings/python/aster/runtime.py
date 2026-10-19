"""
aster.runtime -- Declarative ``AsterServer`` / ``AsterClient`` wrappers.

Thin composition over the low-level primitives (:class:`aster.Server`,
:func:`aster.client.create_client`) for a one-line, declarative
producer/consumer experience, configured by :class:`aster.AsterConfig`.

The ``AsterServer`` starts a single :class:`aster.Node` serving blobs + docs +
gossip + aster RPC on one endpoint, one node ID. Admission is configuration:
with ``[enrollment]`` configured (see :class:`aster.EnrollmentConfig`) the
server admits only nodes holding a credential from the configured root, each
peer's verified claims become the roles ``requires=`` checks, and revocation,
renewal and policy synchronization run in Rust (``aster::binding_support::
enrollment``). Without it the node is open.

Example (producer)::

    async with AsterServer(services=[HelloService()], config=AsterConfig.from_env()) as srv:
        print(srv.address)
        await srv.serve()  # blocks until cancelled

Example (consumer)::

    async with AsterClient(config=AsterConfig.from_env(), address=addr) as c:
        hello = await c.client(HelloService)
        print((await hello.say_hello(HelloRequest(name="World"))).message)
"""
from __future__ import annotations

import asyncio
import base64
import dataclasses
import inspect
import logging
import os
import time
import warnings

# Hot-path aliases for the proxy client
_dataclasses_asdict = dataclasses.asdict
_dataclasses_is_dataclass = dataclasses.is_dataclass


def _is_dataclass_instance(obj: Any) -> bool:
    """Fast check: True if obj is a dataclass instance (not the class itself)."""
    return _dataclasses_is_dataclass(obj) and not isinstance(obj, type)
from pathlib import Path
from typing import Any, Iterable

logger = logging.getLogger(__name__)

from . import (
    GatePolicy,
    Node,
    NodeAddr,
    TrustMode,
    net_client,
    blobs_client,
    docs_client,
    gossip_client, AsterConfig,
)
from .client import CallOptions, ServiceClient, create_client
from .transport.iroh import IrohTransport
from .contract.identity import contract_id_from_service
from .registry.models import ServiceSummary
from .rpc_types import RpcScope, SerializationMode
from .server import Server
from .peer_events import PeerEvents, PeerSnapshot
from .http import HTTPListener
from ._aster import ENROLLMENT_ALPN, DispatchServer, DrainReport, EnrollmentPlan

__all__ = ["AsterServer", "AsterClient", "AdmissionDeniedError", "RPC_ALPN"]

RPC_ALPN: bytes = b"aster/1"

# `aster.ops.NodeInfo` attributes through which a Python server advertises
# its services and registry namespace to clients (`AsterClient.services`).
_SERVICES_ATTRIBUTE = "aster.services"
_REGISTRY_ATTRIBUTE = "aster.registry_namespace"


# ── Errors ───────────────────────────────────────────────────────────────────


class AdmissionDeniedError(PermissionError):
    """Raised by :meth:`AsterClient.connect` when the server does not admit
    this client.

    With a credential (enrollment configured), the enrollment handshake was
    refused; ``__cause__`` carries the Rust error. Without one, the server
    refused the connection, which is what a server with enrollment does to a
    node that holds no credential.
    """

    def __init__(
        self,
        *,
        had_credential: bool,
        credential_file: str | None,
        our_endpoint_id: str,
        server_address: str,
        detail: str = "",
    ) -> None:
        self.had_credential = had_credential
        self.credential_file = credential_file
        self.our_endpoint_id = our_endpoint_id
        self.server_address = server_address
        self.detail = detail
        super().__init__(self.format_hint())

    def format_hint(self) -> str:
        """Return a multi-line actionable hint suitable for CLI output."""
        short_id = (self.our_endpoint_id[:16] + "...") if self.our_endpoint_id else "<unknown>"
        if not self.had_credential:
            detail = f"  detail: {self.detail}\n" if self.detail else ""
            return (
                "connection refused -- the server did not admit this node (it requires\n"
                "an enrollment credential) or could not be reached.\n"
                f"{detail}"
                "  - Ask the operator for a credential issued to this node:\n"
                f"      aster enroll node --subject {self.our_endpoint_id or '<node id>'} "
                "--audience <fleet> --out node.cred\n"
                "  - Then set ASTER_ENROLLMENT_CREDENTIAL=node.cred (or [trust]\n"
                "    enrollment_credential), ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_AUDIENCE\n"
                "    and ASTER_ENROLLMENT_FLEET_NAMESPACE, and keep the node key the\n"
                "    credential names (ASTER_SECRET_KEY_FILE)."
            )
        cred_label = self.credential_file or "<inline credential>"
        detail = f"\n  detail:     {self.detail}" if self.detail else ""
        return (
            f"enrollment refused -- the server did not admit this node.\n"
            f"  credential: {cred_label}\n"
            f"  your node:  {short_id}{detail}\n"
            "  Common causes:\n"
            "    1. The credential expired, or the root revoked this node.\n"
            "    2. The credential was issued to a DIFFERENT node key: keep the key\n"
            "       it names (ASTER_SECRET_KEY_FILE) or have it re-issued with\n"
            f"       aster enroll node --subject {self.our_endpoint_id or '<node id>'}.\n"
            "    3. The server trusts a different root, or another audience.\n"
            "    4. The server's admission policy ([enrollment] policy) excludes it."
        )


# ── AsterServer ──────────────────────────────────────────────────────────────


class AsterServer:
    """High-level, declarative producer.

    Starts a single :class:`aster.Node` that serves blobs + docs + gossip
    (iroh built-in protocols) alongside aster RPC (``aster/1``) -- all on
    **one endpoint, one node ID**.

    Admission comes from the configuration. With enrollment configured
    (``[enrollment]`` / ``ASTER_ENROLLMENT_*`` and a credential; see
    :class:`aster.EnrollmentConfig`), Enrollment owns Gate 0: only nodes
    holding a valid credential from ``[trust] root_pubkey`` connect, each
    peer's verified claims become the roles ``requires=`` checks (evaluated
    per call), a root revocation refuses the next call and closes the peer's
    connections, and the server's own credential renews. Any missing or
    malformed enrollment setting fails :meth:`start` with ``ValueError``.
    Without enrollment the node is open; ``gate=`` attaches a custom
    :class:`aster.GatePolicy` instead.
    """

    def __init__(
        self,
        services: list,
        *,
        config: "AsterConfig | None" = None,
        peer: str | None = None,
        identity: str | None = None,
        # Inline overrides (take priority over config):
        root_pubkey: bytes | None = None,
        # Internal wiring:
        channel_name: str = "rpc",
        codec: Any | None = None,
        interceptors: list[Any] | None = None,
        gate: GatePolicy | None = None,
        registry_namespace: str = "",
        max_sessions_per_connection: int | None = None,
        handler_timeout: float | None = None,
        builtin_topology: bool = False,
        topology_requires: Any = None,
        accept_on_behalf_of: bool | None = None,
        audit_outbox: Any = None,
    ) -> None:
        """Create an Aster RPC server.

        .. note:: **Interceptors are not wired by default.** Deadlines
           (caller deadline, method ``timeout``, ``handler_timeout``) and
           ``requires=`` (Gate 3) are enforced by the Rust dispatcher. The
           other interceptors -- rate limiting, auth, circuit breaking,
           metrics, audit logging, retry hints -- are active only when you
           pass them via the ``interceptors`` parameter; for production use,
           wire at least ``RateLimitInterceptor``. All interceptors live in
           ``aster.interceptors``.

        Args:
            services: List of ``@service``-decorated class instances to serve.
                At least one is required.
            config: Optional :class:`AsterConfig` for trust, enrollment,
                storage and networking settings. If omitted, settings are
                loaded from environment variables (``AsterConfig.from_env()``).
                With ``storage_path`` set the node is persistent; its key is
                kept in ``secret_key_file`` (default
                ``<storage_path>/node.key``).
            peer: Optional peer name for this server (used in identity file
                resolution).
            identity: Path to ``.aster-identity`` file (default: auto-detected
                from CWD). Overrides ``config.identity_file``.
            root_pubkey: 32-byte ed25519 root public key; overrides
                ``config.root_pubkey``.
            gate: Optional :class:`aster.GatePolicy` deciding Gate 0 on a
                server without enrollment (enrollment owns Gate 0; passing
                both raises ``ValueError`` at start).
            handler_timeout: Cap on any handler, in seconds (default 300;
                ``<= 0`` = no cap). Enforced in Rust together with the
                caller's deadline and each method's ``timeout``.
            builtin_topology: Explicit opt-in to the native ``aster.net.Topology``
                RPC service. Requires ``config.enable_monitoring=True``.
            topology_requires: Native role/``any_of``/``all_of`` Gate-3 requirement
                for the opt-in topology baseline service.
            accept_on_behalf_of: Overrides ``config.identity_accept_on_behalf_of``
                (gateway identity: calls an edge holding ``aster.gateway``
                makes on behalf of a user; see :mod:`aster.identity`).
                ``[identity]`` and ``[remote_config]`` (``aster.ops.NodeConfig``,
                served behind ``aster.config-admin``) come from ``config``.
            audit_outbox: An :class:`aster.AuditOutbox` recording every
                ``aster.ops.NodeConfig`` ``set`` / ``clear``.

        Example::

            @service(name="MyService", version=1)
            class MyService:
                @rpc(requires=role("ops.status"))
                async def hello(self, req: HelloRequest) -> HelloResponse:
                    return HelloResponse(message=f"Hello {req.name}")

            async with AsterServer(services=[MyService()], config=AsterConfig.from_env()) as srv:
                print(srv.address)
                await srv.serve()
        """
        if not services:
            raise ValueError("AsterServer requires at least one service")

        # Auto-load config from env if none provided.
        from .config import AsterConfig
        if config is None:
            config = AsterConfig.from_env()
        if identity is not None:
            config = config.replace(identity_file=identity)
        if root_pubkey is not None:
            config = config.replace(root_pubkey=root_pubkey)

        # Load the node key from an identity file if present (.aster-identity).
        self._peer_name = peer
        secret_key_from_identity, _peer_entry = config.load_identity(
            peer_name=peer, role="producer"
        )
        config = _with_node_identity(config, secret_key_from_identity)
        self._config = config

        self._services_in: list = list(services)
        self._channel_name = channel_name
        self._codec = codec
        # Deadlines are enforced by the Rust dispatcher (design sec. 2.4), so
        # there is no server-side deadline interceptor by default.
        self._interceptors = list(interceptors) if interceptors is not None else []

        # Gate 0: Enrollment when configured (installed at start), else an
        # application-supplied GatePolicy, else open.
        self._gate: GatePolicy | None = gate
        self._enrollment: Any | None = None
        self._registry_namespace = registry_namespace
        self._max_sessions_per_connection = max_sessions_per_connection
        self._handler_timeout = handler_timeout
        self._builtin_topology = builtin_topology
        self._topology_requires = topology_requires
        self._accept_on_behalf_of = accept_on_behalf_of
        self._audit_outbox = audit_outbox

        # Populated by start()
        self._started: bool = False
        self._node: Node | None = None
        self._service_summaries: list[ServiceSummary] = []
        self._server: Server | None = None
        # Lazy caches for .blobs / .docs / .gossip
        self._blobs: Any | None = None
        self._docs: Any | None = None
        self._gossip: Any | None = None

        # Populated by serve()
        self._serve_task: asyncio.Task | None = None
        self._subtasks: list[asyncio.Task] = []
        self._health: Any | None = None  # aster.HealthServer (Rust ops listener)
        self._closed: bool = False

        # Producer service tokens for @aster endpoint registration.
        self._producer_tokens: dict[str, dict] = {}  # service_name -> token dict
        self._load_producer_tokens()

        # Delegated admission (aster.admission ALPN, @aster-issued tokens)
        # is not served by the Python binding. Warn if the identity file
        # asks for it so the gap is visible rather than silently ignored.
        self._warn_if_delegation_configured()

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
        # Rust log records reach `logging`; exporters only if OTEL_* asks.
        from aster import observability
        observability.ensure_started()

        node_config = self._config.replace(
            alpns=(RPC_ALPN,) + tuple(a for a in self._config.alpns if a != RPC_ALPN),
        )
        # Enrollment from `[enrollment]`: validated in full (in Rust) before
        # the node starts, so a misconfiguration fails here, closed.
        plan = EnrollmentPlan.from_config(node_config, side="server")
        # `[identity]` / `[remote_config]` validated as a whole too (trusted
        # issuers and the publisher need Enrollment).
        DispatchServer.check_identity(node_config, plan is not None)
        if plan is not None:
            if self._gate is not None:
                raise ValueError(
                    "enrollment owns Gate 0: remove gate= or the [enrollment] configuration"
                )
            self._node = await Node.start(plan.config, alpns=[ENROLLMENT_ALPN])
            try:
                self._enrollment = plan.install(self._node)
            except BaseException:
                await self._node.close()
                self._node = None
                raise
        else:
            # Gate 0 runs in Rust on the node's admission hooks, failing
            # closed when protected.
            if self._gate is not None and self._gate.trust_mode == TrustMode.PROTECTED:
                node_config = node_config.replace(hook_failure_mode="fail_closed")
            self._node = await Node.start(node_config, gate=self._gate)
        await self._start_health()
        logger.debug(
            "Producer node: %s",
            f"persistent at {node_config.storage_path}"
            if node_config.storage_path else "in-memory",
        )
        addr_b64 = base64.b64encode(
            self._node.node_addr_info().to_bytes()
        ).decode()

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
                    pattern=info.scoped or "shared",
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
                    continue  # Service not in manifest -- skip
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

        # ── Gate 3 & default-deny warning ────────────────────────────────
        #
        # Gate 3 (`requires=`) is enforced by the Rust dispatcher against the
        # attributes the gate recorded for the caller, so no Python
        # capability interceptor is wired.

        # S12.2: default-deny startup warning.
        # When Gate 0 is open (no enrollment, no gate), any service without
        # explicit authorization is wide open.  Emit a warning so the
        # developer knows.
        if self._is_open:
            for svc in self._services_in:
                svc_cls = svc if inspect.isclass(svc) else type(svc)
                info = getattr(svc_cls, "__aster_service_info__", None)
                if info is None:
                    continue
                if info.public:
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
                            f"and Gate 0 is open (no [enrollment] configured). "
                            f"All consumers can call this service without "
                            f"authentication. Configure enrollment and add "
                            f"@service(requires=...) for production use.",
                            UserWarning,
                            stacklevel=2,
                        )

        # Server borrows a NetClient view of the node. AsterServer owns the
        # node lifecycle, so Server must NOT close the endpoint on its own.
        server_kwargs: dict[str, Any] = dict(
            services=self._services_in,
            codec=self._codec,
            interceptors=self._interceptors,
            owns_endpoint=False,
            gate=self._gate,
            enrollment=self._enrollment,
            node_attributes=self._discovery_attributes(),
            node=self._node,
            on_non_rpc=self._on_non_rpc,
            handler_timeout=self._handler_timeout,
            builtin_topology=self._builtin_topology,
            topology_requires=self._topology_requires,
            identity=node_config,
            accept_on_behalf_of=self._accept_on_behalf_of,
            audit_outbox=self._audit_outbox,
        )
        if self._max_sessions_per_connection is not None:
            server_kwargs["max_sessions_per_connection"] = self._max_sessions_per_connection
        self._server = Server(net_client(self._node), **server_kwargs)

        if self._enrollment is not None:
            # Serve (the enrollment ALPN included), then hold policy: sync
            # the fleet namespace from the replicas, or import it for the
            # root to publish into. Then renew this server's credential.
            self._server.start()
            try:
                await self._enrollment.attach_policy(self._node)
                # Then the issuer-key publisher (to the policy replicas) and
                # the issuer-key watcher, as the Rust AsterServer orders it.
                await self._server._dispatcher.attach_identity()
            except BaseException:
                self._server._dispatcher.close()
                self._enrollment.close()
                await self._node.close()
                raise
            self._enrollment.start_renewal()

        self._print_banner()
        self._started = True

        # Always log startup info (visible even when stderr is not a TTY)
        service_names = ", ".join(s.name for s in self._service_summaries)
        mode = self._mode
        logger.info("server starting runtime=python services=[%s] mode=%s", service_names, mode)

    @property
    def _is_open(self) -> bool:
        """Gate 0 admits every peer: no enrollment and no protecting gate."""
        if self._enrollment is not None:
            return False
        return self._gate is None or self._gate.trust_mode != TrustMode.PROTECTED

    @property
    def _mode(self) -> str:
        if self._enrollment is not None:
            return "enrollment"
        return "open" if self._is_open else "gate"

    def _discovery_attributes(self) -> list[tuple[str, str]]:
        """``aster.ops.NodeInfo`` attributes advertising this server's
        services and registry namespace (read by ``AsterClient``)."""
        import json as _json

        services = [s.to_json_dict() for s in self._service_summaries]
        out = [(_SERVICES_ATTRIBUTE, _json.dumps(services, separators=(",", ":")))]
        if self._registry_namespace:
            out.append((_REGISTRY_ATTRIBUTE, self._registry_namespace))
        return out

    def _print_banner(self) -> None:
        """Print the startup banner with service info."""
        import sys
        import os

        # Only print banner when stderr is a terminal (not in tests/pipes)
        if not sys.stderr.isatty():
            return

        C = "\033[36m"   # cyan
        B = "\033[1m"    # bold
        D = "\033[2m"    # dim
        G = "\033[32m"   # green
        Y = "\033[33m"   # yellow
        W = "\033[37m"   # white
        R = "\033[0m"    # reset
        w = sys.stderr.write

        # ── Banner ────────────────────────────────────────────────────────
        w(f"\n{C}{B}")
        w(f"        _    ____ _____ _____ ____\n")
        w(f"       / \\  / ___|_   _| ____|  _ \\\n")
        w(f"      / _ \\ \\___ \\ | | |  _| | |_) |\n")
        w(f"     / ___ \\ ___) || | | |___|  _ <\n")
        w(f"    /_/   \\_\\____/ |_| |_____|_| \\_\\\n")
        w(f"{R}\n")
        w(f"    {D}RPC after hostnames.{R}\n\n")

        # ── Services table ────────────────────────────────────────────────
        if self._service_summaries:
            # Find max name length for alignment
            max_name = max(len(s.name) for s in self._service_summaries)
            for s in self._service_summaries:
                name_pad = s.name.ljust(max_name)
                w(f"    {G}●{R} {B}{name_pad}{R}  {D}v{s.version}{R}  {D}{s.contract_id}{R}\n")
            w("\n")

        # ── Endpoint ─────────────────────────────────────────────────────
        compact = None
        endpoint_id_full = None
        if self._node:
            try:
                from . import AsterTicket
                addr_info = self._node.node_addr_info()
                endpoint_id_full = addr_info.endpoint_id
                t = AsterTicket(
                    endpoint_id=addr_info.endpoint_id,
                    direct_addrs=addr_info.direct_addresses or [],
                )
                compact = t.to_string()
            except Exception:
                pass

        if endpoint_id_full:
            short = endpoint_id_full[:16] + "…"
            w(f"    {D}node id:{R}   {W}{short}{R}  {D}(this node's keypair fingerprint){R}\n")
        if compact:
            w(f"    {D}endpoint:{R}  {compact}\n")

        # ── Mode ──────────────────────────────────────────────────────────
        mode_parts = []
        if self._is_open:
            mode_parts.append(f"{Y}open{R}")
        else:
            mode_parts.append(f"{G}{self._mode}{R}")
        if self._registry_namespace:
            mode_parts.append(f"{G}registry{R}")
        w(f"    {D}mode:{R}      {' '.join(mode_parts)}\n")

        # ── Logging ───────────────────────────────────────────────────────
        log_format = os.environ.get("ASTER_LOG_FORMAT", "text")
        log_level = os.environ.get("ASTER_LOG_LEVEL", "info")
        w(f"    {D}log:{R}       ASTER_LOG_FORMAT={W}{log_format}{R}  ASTER_LOG_LEVEL={W}{log_level}{R}\n")

        # ── Versions ──────────────────────────────────────────────────────
        try:
            from importlib.metadata import version as _pkg_version
            aster_ver = _pkg_version("aster-rpc")
        except Exception:
            aster_ver = "?"

        # Read iroh version from the native module
        iroh_ver = "0.97"  # pinned in Cargo.toml

        w(f"    {D}runtime:{R}   aster-rpc {aster_ver} (python)  iroh {iroh_ver}\n")

        # ── Copyright ─────────────────────────────────────────────────────
        w(f"\n    {D}Copyright \u00a9 2026 Emrul Islam. All rights reserved.{R}\n\n")

    async def _publish_contracts(self) -> None:
        """Create a registry doc and publish each service's contract collection.

        After publication, ``self._registry_namespace`` is set to the 64-char
        hex namespace_id, advertised to clients through ``aster.ops.NodeInfo``.
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

                from aster._aster import blake3_hex as _blake3_hex
                contract_id = _blake3_hex(contract_bytes)

                # Build collection with full method schemas
                entries = build_collection(contract, type_defs, service_info=info)

                # Upload to blob store as a native HashSeq collection.
                # GC protection is handled automatically by the HashSeq tag.
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
                    from .registry.keys import manifest_key as _mk
                    await registry_doc.set_bytes(
                        author_id, _mk(contract_id), manifest_data
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

            # share() enables the sync engine on the server side so consumers
            # can replicate this doc.  We only need the namespace_id on the wire.
            await registry_doc.share_with_addr("read")
            self._registry_namespace = registry_doc.doc_id()
            logger.debug(
                "Registry doc ready -- namespace: %s", self._registry_namespace[:16]
            )

        except Exception as exc:
            # Publication failure is non-fatal -- the server still works,
            # consumers just won't get rich contract metadata
            logger.warning("Contract publication failed (non-fatal): %s", exc)

    async def start_http(
        self, *, authenticator, certificate_pem: bytes, private_key_pem: bytes,
        bind_addr: str = "127.0.0.1:0", webtransport: bool = False,
        max_pending_auth: int = 256, max_auth_payload_bytes: int = 65536,
        upcall_runtime=None,
    ) -> HTTPListener:
        """Start an owned authenticated PEM listener over registered services.

        See :mod:`aster.http` for configuration and authentication types.
        The server must have completed start(); this also starts serving RPCs.
        """
        if not self._started or self._closed:
            raise RuntimeError("start_http requires a started, open AsterServer")
        self.serve()
        await asyncio.sleep(0)  # Let the existing dispatch pump record its task.
        assert self._server is not None
        return await self._server.start_http(
            authenticator=authenticator, certificate_pem=certificate_pem,
            private_key_pem=private_key_pem, bind_addr=bind_addr,
            webtransport=webtransport, max_pending_auth=max_pending_auth,
            max_auth_payload_bytes=max_auth_payload_bytes, upcall_runtime=upcall_runtime,
        )

    async def start_public_http(
        self, *, certificate_pem: bytes, private_key_pem: bytes,
        bind_addr: str = "127.0.0.1:0", webtransport: bool = False,
    ) -> HTTPListener:
        """Start explicitly anonymous RPCs; attributes are empty and sessions rejected."""
        if not self._started or self._closed:
            raise RuntimeError("start_public_http requires a started, open AsterServer")
        self.serve()
        await asyncio.sleep(0)
        assert self._server is not None
        return await self._server.start_public_http(
            certificate_pem=certificate_pem, private_key_pem=private_key_pem,
            bind_addr=bind_addr, webtransport=webtransport,
        )

    def serve(self) -> asyncio.Task:
        """Spawn the accept loop; return an aggregate task. (Gate 0 runs in Rust.)

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

        # Rust accepts every connection on the node: `aster/1` calls are
        # dispatched to the Python handlers, other ALPNs (consumer
        # admission) arrive through `_on_non_rpc`.
        self._server.start()
        pump = self._server._serve_task
        if pump is None or pump.done():
            pump = asyncio.create_task(self._server.serve(), name="aster-dispatch")
            self._server._serve_task = pump
        subtasks.append(pump)
        if self._health is not None:
            self._health.set_registry(bool(self._service_summaries))
            self._health.set_ready(True)

        # Auto-register endpoints with @aster for published services.
        # Requires producer service tokens (from `aster publish`).
        if self._producer_tokens:
            subtasks.append(
                asyncio.create_task(
                    self._aster_registration_loop(), name="aster-registration"
                )
            )

        self._subtasks = subtasks

        async def _wait_all() -> None:
            try:
                await asyncio.gather(*subtasks, return_exceptions=True)
            except asyncio.CancelledError:
                # Graceful shutdown on Ctrl+C / task cancellation
                logger.info("Server shutting down...")
                for t in subtasks:
                    t.cancel()
                await asyncio.gather(*subtasks, return_exceptions=True)

        self._serve_task = asyncio.create_task(_wait_all(), name="aster-server-serve")
        return self._serve_task

    def _on_non_rpc(self, alpn: bytes, conn: Any) -> None:
        """A connection on an ALPN nothing routes, handed over by the Rust
        acceptor (the enrollment ALPN is served in Rust)."""
        try:
            conn.close(0, b"unexpected alpn")
        except Exception:  # noqa: BLE001
            pass

    async def drain(self, grace_period: float = 10.0) -> "DrainReport | None":
        """Take the server out of rotation gracefully, then close it (Rust
        ``aster::AsterServer::drain``; semantics in ``aster::rpc::drain``).

        Sets the ops listener not ready, then refuses new connections (closed
        after the handshake with :data:`DRAIN_CLOSE_CODE`) and new calls
        (``UNAVAILABLE`` with the ``aster.draining`` detail, which a client
        sees as :attr:`RpcError.is_draining`); calls on a session that already
        exists still run. Each connection is closed once idle; after
        ``grace_period`` seconds the rest are closed, which cancels the calls
        still running (handlers see a peer cancel). Returns the
        :class:`DrainReport` (``None`` when the server never started).

        A second call while a drain runs moves its deadline earlier when its
        own grace ends sooner and returns the same report. Once the drain
        ends the server is closed (:meth:`close`: node, HTTP listeners, ops
        listener), as in Rust, Go, .NET, Java and TypeScript; a drain after
        that raises ``RuntimeError``. Fits Kubernetes ``preStop`` hooks and
        SIGTERM handling.

        Args:
            grace_period: Seconds to let in-flight calls finish.
        """
        if self._closed:
            raise RuntimeError("server is closed")
        logger.info("Draining server (grace_period=%.1fs)...", grace_period)
        if self._health is not None:
            self._health.set_ready(False)  # probes stop routing first
        report = None
        try:
            if self._server is not None:
                report = await self._server.drain(grace_period)
            logger.info("Drain complete: %r", report)
        finally:
            await self.close()
        return report

    def peer_events(self) -> "tuple[list[PeerSnapshot], PeerEvents]":
        """The peers connected now and a stream of every connect and
        disconnect after them, taken together so a peer connecting during the
        call is in the snapshot or in the stream, never both and never neither
        (Rust ``AsterServer::peer_events``).

        Events are per peer: :class:`PeerConnected` on a peer's first admitted
        ``aster/1`` connection (past the node's gates, so a peer enrollment or
        a drain refuses never appears), :class:`PeerDisconnected` (with a
        ``reason``) when its last one ends. Whether the peer serves what you
        need is answered by reading its ``aster.services`` NodeInfo attribute
        on ``PeerConnected``. A consumer that falls behind the bounded queue
        gets :class:`PeerLagged`; its recovery is to call this again.
        Close the returned :class:`PeerEvents` when done.

        Raises:
            RuntimeError: the server is closed or not started.
        """
        if self._closed or self._server is None:
            raise RuntimeError("server is closed")
        return self._server.peer_events()

    @property
    def is_draining(self) -> bool:
        """Whether :meth:`drain` has started."""
        dispatcher = self._server._dispatcher if self._server is not None else None
        return dispatcher is not None and dispatcher.is_draining

    async def close(self) -> None:
        """Cancel serve loops and close the node. Safe to call multiple times."""
        if self._closed:
            return
        # Set before the first await so a concurrent close (two drains ending
        # together) does not run the teardown twice.
        self._closed = True
        if self._server is not None:
            await self._server._close_http()

        if self._health is not None:
            self._health.set_ready(False)  # not ready first, then drain
        if self._server is not None and self._server._dispatcher is not None:
            self._server._dispatcher.close()
        for t in self._subtasks:
            t.cancel()
        if self._serve_task is not None:
            self._serve_task.cancel()
            try:
                await self._serve_task
            except (asyncio.CancelledError, Exception):
                pass
        if self._enrollment is not None:
            self._enrollment.close()

        # Close the node -- this triggers router.shutdown() which closes all
        # protocol handlers (including aster queue handlers) and the endpoint.
        if self._node is not None:
            try:
                await self._node.close()
            except Exception:
                pass
        if self._health is not None:
            try:
                await self._health.stop()
            except Exception:  # noqa: BLE001
                pass

    async def _start_health(self) -> None:
        """Start the Rust ops listener (`/healthz`, `/readyz`, `/metrics`) when
        the config enables it (``health_port`` / ``health_enabled`` /
        ``ASTER_HEALTH_*`` / ``[health]``)."""
        cfg = self._config
        enabled = cfg.health_enabled if cfg.health_enabled is not None else cfg.health_port != 0
        if not enabled:
            return
        from aster import HealthServer

        self._health = await HealthServer.start(
            self._node,
            cfg.health_host,
            cfg.health_port,
            peer_label_cap=cfg.metrics_peer_label_cap if cfg.metrics_peer_labels else None,
        )
        self._health.set_services(len(getattr(self, "_services_in", ()) or ()))
        logger.info("ops endpoints listening on %s", self._health.addr)

    @property
    def health(self) -> "Any | None":
        """The :class:`aster.HealthServer` started from ``[health]`` config, or ``None``."""
        return self._health

    # ── @aster endpoint registration ──────────────────────────────────────

    def _load_producer_tokens(self) -> None:
        """Load producer service tokens from .aster-identity [published_services.*]."""
        _, peer_entry = self._config.load_identity(
            peer_name=self._peer_name, role="producer"
        )
        if not peer_entry:
            return

        # The identity loader returns the raw peer dict. Published services
        # are stored as [published_services.<ServiceName>] sections in the
        # .aster-identity TOML file.
        published = peer_entry.get("published_services", {})
        if not isinstance(published, dict):
            return

        for svc_name, token_data in published.items():
            if isinstance(token_data, dict) and token_data.get("producer_token"):
                self._producer_tokens[svc_name] = token_data
                logger.debug("Loaded producer token for %s", svc_name)

    def _warn_if_delegation_configured(self) -> None:
        """Warn when published services carry an @aster delegation anchor.

        Delegated admission (``aster.admission`` ALPN, Aster-trust-spec §3a)
        is NOT implemented in the Python binding: the ALPN is not registered
        and no handler runs, so consumers holding @aster enrollment tokens
        cannot be admitted by a Python producer. Use Enrollment
        (``[enrollment]``) instead.
        """
        _, peer_entry = self._config.load_identity(
            peer_name=self._peer_name, role="producer"
        )
        if not peer_entry:
            return
        published = peer_entry.get("published_services", {})
        if not isinstance(published, dict):
            return
        delegated = sorted(
            name
            for name, pub in published.items()
            if isinstance(pub, dict)
            and pub.get("aster_root_pubkey")
            and pub.get("contract_id")
        )
        if delegated:
            logger.warning(
                "Delegated admission (aster.admission) is not supported by the "
                "Python binding; @aster enrollment tokens for %s will not be "
                "accepted. Admit consumers with Enrollment ([enrollment]).",
                ", ".join(delegated),
            )

    async def _aster_registration_loop(self) -> None:
        """Background loop: register endpoints with @aster for published services.

        Connects to @aster, registers each service's endpoint, then
        re-registers periodically before TTL expiry.
        """
        import json as _json

        # Wait a moment for the server to be fully ready
        await asyncio.sleep(2)

        ttl = 300  # 5 minutes
        interval = ttl * 0.75  # re-register at 75% of TTL

        while not self._closed:
            try:
                await self._register_endpoints_with_aster(ttl)
            except asyncio.CancelledError:
                return
            except Exception as exc:
                logger.warning("@aster registration failed: %s", exc)

            # Wait before re-registering
            try:
                await asyncio.sleep(interval)
            except asyncio.CancelledError:
                return

    async def _register_endpoints_with_aster(self, ttl: int) -> None:
        """One-shot registration of all published service endpoints."""
        if not self._node or not self._producer_tokens:
            return

        # Build our endpoint info
        addr_info = self._node.node_addr_info()
        node_id = addr_info.endpoint_id
        relay = addr_info.relay_url or ""
        direct_addrs = addr_info.direct_addresses or []

        # Resolve @aster address from identity file or profile config.
        # The token itself contains the root_pubkey which can be used
        # to discover @aster via DNS TXT record in production.
        aster_addr = self._resolve_aster_address()
        if not aster_addr:
            logger.debug("No @aster address configured -- skipping registration")
            return

        aster_client = AsterClient(address=aster_addr)

        try:
            await aster_client.connect()
        except Exception as exc:
            logger.debug("Could not connect to @aster: %s", exc)
            return

        try:
            # For each published service with a token, call register_endpoint
            for svc_name, token in self._producer_tokens.items():
                try:
                    # Use the dynamic invoke path -- we don't have generated
                    # types for @aster's PublicationService
                    import json as _json
                    request = {
                        "producer_token": _json.dumps(token),
                        "node_id": node_id,
                        "relay": relay,
                        "direct_addrs": direct_addrs,
                        "ttl": ttl,
                    }

                    # Invoke register_endpoint on PublicationService
                    conn = await aster_client._rpc_conn_for(aster_client._server_key)
                    from .transport.iroh import IrohTransport
                    transport = IrohTransport(conn, codec=self._codec)
                    resp = await transport.unary(
                        "PublicationService", "register_endpoint", request
                    )
                    logger.info(
                        "Registered endpoint with @aster: %s (%s)",
                        svc_name, node_id[:12],
                    )
                except Exception as exc:
                    logger.warning(
                        "Failed to register %s with @aster: %s",
                        svc_name, exc,
                    )
        finally:
            await aster_client.close()

    def _resolve_aster_address(self) -> str | None:
        """Resolve the @aster service address for endpoint registration.

        Checks (in order):
        1. ASTER_SERVICE_ADDRESS env var
        2. aster_service.address in the identity file's peer entry
        3. DNS TXT record on aster.site (future)
        """
        # Env var override
        addr = os.environ.get("ASTER_SERVICE_ADDRESS", "")
        if addr:
            return addr

        # Identity file -- the peer entry may have aster_service config
        _, peer_entry = self._config.load_identity(
            peer_name=self._peer_name, role="producer"
        )
        if peer_entry:
            addr = peer_entry.get("aster_service", "")
            if addr:
                return addr

        return None

    def _install_signal_handlers(self, grace_period: float = 10.0) -> None:
        """Install SIGTERM/SIGINT handlers for graceful shutdown.

        Call after ``serve()`` to enable k8s-compatible shutdown:

        - SIGTERM: drain → close (graceful)
        - SIGINT (Ctrl+C): drain → close (graceful)
        - Second SIGINT: immediate exit

        Usage::

            async with AsterServer(services=[...]) as srv:
                srv.install_signal_handlers()
                await srv.serve()
        """
        import signal

        loop = asyncio.get_event_loop()
        shutdown_count = 0

        def _handle_signal(sig: int, frame: Any) -> None:
            nonlocal shutdown_count
            shutdown_count += 1
            if shutdown_count > 1:
                logger.warning("Forced exit (second signal)")
                sys.exit(1)
            logger.info("Received %s -- draining...", signal.Signals(sig).name)
            loop.create_task(self._graceful_shutdown(grace_period))

        signal.signal(signal.SIGTERM, _handle_signal)
        signal.signal(signal.SIGINT, _handle_signal)

    async def _graceful_shutdown(self, grace_period: float) -> None:
        """Internal: drain then close."""
        try:
            await self.drain(grace_period)
        finally:
            await self.close()

    async def __aenter__(self) -> "AsterServer":
        await self.start()
        self.serve()
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.close()

    # ── Properties ───────────────────────────────────────────────────────────

    @property
    def address(self) -> str:
        """Connection address for this server (``aster1...`` ticket).

        Pass this to ``AsterClient(address=...)`` or ``aster shell``
        to connect.  Includes relay address (if available) and direct
        addresses for LAN connectivity.
        """
        self._require_started()
        assert self._node is not None
        from . import AsterTicket
        addr_info = self._node.node_addr_info()

        # Resolve relay URL to IP:port for the ticket
        relay_addr = None
        if addr_info.relay_url:
            relay_addr = _resolve_relay_addr(addr_info.relay_url)

        t = AsterTicket(
            endpoint_id=addr_info.endpoint_id,
            relay_addr=relay_addr,
            direct_addrs=addr_info.direct_addresses or [],
        )
        return t.to_string()

    @property
    def endpoint_id(self) -> str:
        """Hex endpoint ID of this server's node."""
        self._require_started()
        assert self._node is not None
        return self._node.node_id()

    # ── Back-compat aliases ──────────────────────────────────────────────

    @property
    def _ticket(self) -> str:
        """Alias for :attr:`address` (internal back-compat)."""
        return self.address

    def debug_connection_snapshot(self) -> dict[int, dict[str, int]]:
        """**TEST-ONLY**. Snapshot of per-connection session state for
        tier-2 chaos test assertions. Maps ``connection_id`` to a dict
        with keys ``active_session_count`` and ``last_opened_session_id``.
        Production code MUST NOT read this -- it exists so tests can
        verify reap semantics (connection entries drop on close) and
        session accounting without reaching into private fields.

        Mirrors TypeScript ``AsterServer2.debugConnectionSnapshot``
        and Java ``AsterServer.debugConnectionSnapshot``.
        """
        if self._server is None:
            return {}
        if self._server._dispatcher is None:
            return {}
        # Session state lives in Rust; the dispatcher mirrors it from the
        # session lifecycle events.
        return self._server._dispatcher.connection_snapshot()

    # Back-compat alias -- used in tests
    @property
    def endpoint_addr_b64(self) -> str:
        self._require_started()
        assert self._node is not None
        return base64.b64encode(self._node.node_addr_info().to_bytes()).decode()

    @property
    def _rpc_addr_b64(self) -> str:
        return self.endpoint_addr_b64

    @property
    def services(self) -> list[ServiceSummary]:
        """List of services hosted by this server."""
        self._require_started()
        return list(self._service_summaries)

    @property
    def root_pubkey(self) -> bytes | None:
        """The 32-byte ed25519 root public key from the configuration, or ``None``."""
        return self._config.resolve_root_pubkey()

    @property
    def enrollment(self) -> Any | None:
        """The ``aster.Enrollment`` installed from ``[enrollment]`` (``None``
        when enrollment is off): ``credential``, ``is_renewing``, and
        ``issue_for(subject, attributes=...)`` when the server runs a
        delegated issuer."""
        return self._enrollment

    def node_properties(self) -> Any:
        """This server's ``aster.ops.NodeConfig`` properties now
        (:class:`aster.identity.NodeProperties`; Rust
        ``AsterServer::node_config().properties()``). Empty before the
        server serves, when never set, or with ``remote_config_enabled``
        off."""
        from .identity import EMPTY_NODE_PROPERTIES

        self._require_started()
        dispatcher = self._server._dispatcher if self._server is not None else None
        if dispatcher is None:
            return EMPTY_NODE_PROPERTIES
        return dispatcher._node_properties()

    def _dispatch_server(self) -> Any:
        self._require_started()
        dispatcher = self._server._dispatcher if self._server is not None else None
        native = dispatcher.server if dispatcher is not None else None
        if native is None:
            raise RuntimeError("AsterServer is not serving")
        return native

    def restrict_peer_roles(self, peer: str, roles: "list[str]") -> None:
        """Narrow an enrolled peer's roles at runtime.

        The credential sets the maximum; runtime settings can only restrict.
        From the peer's next call (calls in flight finish) its effective
        roles are its credential roles intersected with ``roles``; a role the
        credential lacks is never granted, and ``[]`` leaves it none. The
        restriction replaces an earlier one, survives renewals of the same
        credential subject, and ends with the peer's enrollment session
        (revocation, eviction, expiry).

        Args:
            peer: The peer's node id (64 hex).
            roles: The roles the peer keeps.

        Raises:
            LookupError: ``peer`` has no live enrollment session.
        """
        self._dispatch_server().restrict_peer_roles(peer, list(roles))

    def clear_peer_restriction(self, peer: str) -> bool:
        """Remove ``peer``'s role restriction; its credential roles apply
        again from its next call. Returns whether one was set."""
        return self._dispatch_server().clear_peer_restriction(peer)

    # ── Iroh protocol clients (lazy) ─────────────────────────────────────────

    @property
    def gate(self) -> GatePolicy | None:
        """The application-supplied Gate 0 policy (``aster.GatePolicy``), or
        ``None`` (open, or Enrollment owns Gate 0).

        ``gate.admit(peer)`` / ``gate.revoke(peer)`` change who may connect;
        changes apply to new connections.
        """
        return self._gate

    @property
    def node(self) -> Node:
        """The underlying :class:`aster.Node` (escape hatch for direct access)."""
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
    def _rpc_endpoint(self) -> Any:
        return self.endpoint

    def _require_started(self) -> None:
        if not self._started:
            raise RuntimeError("AsterServer not started; call start() first")


# ── AsterClient ──────────────────────────────────────────────────────────────


class AsterClient:
    """High-level, declarative consumer.

    Reads configuration from :class:`AsterConfig` (env vars / TOML file) just
    like :class:`AsterServer`. Against an open server, ``AsterClient(address=
    ...)`` is enough. With enrollment configured (a credential in ``[trust]
    enrollment_credential`` / ``ASTER_ENROLLMENT_CREDENTIAL`` plus
    ``[enrollment]``), :meth:`connect` presents the credential and verifies
    the server's over the reciprocal enrollment handshake, synchronizes the
    fleet namespace the server offers (so it learns revocations of the
    server), and renews the credential from ``renew_from`` or from the server
    when the server signed it. A misconfiguration raises ``ValueError``; a
    refusal raises :class:`AdmissionDeniedError`.

    ``address`` may be an ``aster1...`` ticket (as printed by
    :class:`AsterServer`), a base64 ``NodeAddr`` string, an ``EndpointId`` hex
    string (when discovery is enabled), a :class:`NodeAddr` object, or raw
    ``NodeAddr.to_bytes()`` bytes.
    """

    def __init__(
        self,
        *,
        config: "AsterConfig | None" = None,
        peer: str | None = None,
        identity: str | None = None,
        # Connection address (aster1... ticket, base64 NodeAddr, or hex EndpointId):
        address: str | None = None,
        # Back-compat alias for address:
        endpoint_addr: NodeAddr | str | bytes | None = None,
        root_pubkey: bytes | None = None,
        enrollment_credential_file: str | None = None,
        # Internal wiring:
        channel_name: str = "rpc",
    ) -> None:
        """Create an Aster RPC client.

        Args:
            config: Optional :class:`AsterConfig`. If omitted, settings are
                loaded from environment variables.
            peer: Peer name for identity file lookup.
            identity: Path to ``.aster-identity`` file (default: auto-detected
                from CWD). Overrides ``config.identity_file``.
            address: The server's address. Accepts:
                - ``aster1...`` compact ticket (recommended)
                - Base64-encoded ``NodeAddr``
                - Hex ``EndpointId`` (requires discovery)
            endpoint_addr: Alias for *address* (back-compat).
            root_pubkey: 32-byte ed25519 root public key; overrides
                ``config.root_pubkey``.
            enrollment_credential_file: Path to this node's enrollment
                credential chain (``aster enroll node --out``); overrides
                ``config.enrollment_credential_file``. Setting it enables
                enrollment.

        Example::

            # Open server -- no credentials
            client = AsterClient(address="aster1...")
            await client.connect()

            # Enrollment -- ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_AUDIENCE,
            # ASTER_ENROLLMENT_FLEET_NAMESPACE, ASTER_ENROLLMENT_CREDENTIAL and
            # ASTER_SECRET_KEY_FILE in the environment
            client = AsterClient(config=AsterConfig.from_env(), address="aster1...")
            await client.connect()
        """
        from .config import AsterConfig

        if config is None:
            config = AsterConfig.from_env()
        if identity is not None:
            config = config.replace(identity_file=identity)
        if root_pubkey is not None:
            config = config.replace(root_pubkey=root_pubkey)
        if enrollment_credential_file is not None:
            config = config.replace(enrollment_credential_file=enrollment_credential_file)
        self._peer_name = peer

        # The node key from an identity file if present (.aster-identity).
        secret_key_from_identity, _peer_entry = config.load_identity(
            peer_name=peer, role="consumer"
        )
        config = _with_node_identity(config, secret_key_from_identity)
        self._config = config

        # Resolve endpoint address: address > endpoint_addr > config > error.
        addr = address or endpoint_addr or config.endpoint_addr
        if addr is None:
            raise ValueError(
                "AsterClient requires an endpoint address. "
                "Set ASTER_ENDPOINT_ADDR or pass address= explicitly."
            )
        self._endpoint_addr_in = addr
        self._channel_name = channel_name

        self._node: Any | None = None
        self._ep: Any | None = None
        self._enrollment: Any | None = None
        self._services: list[ServiceSummary] = []
        # Whether the server advertised its services (``aster.services``);
        # summaries resolved later from the registry do not make it so.
        self._advertised = False
        self._registry_namespace: str = ""
        self._rpc_conns: dict[str, Any] = {}
        # Serialises concurrent `_rpc_conn_for` lookups so bursts of
        # `open_session()` calls can't each race past the cache miss
        # and open their own connection (each connection has its own
        # server-side session graveyard + cap, so the race lets a
        # burst silently blow past `max_sessions_per_connection`).
        # Created on demand in `_rpc_conn_for` because `__init__`
        # may run outside an event loop.
        self._rpc_conn_lock: asyncio.Lock | None = None
        self._clients: list[ServiceClient] = []
        self._connected: bool = False
        self._closed: bool = False
        self._reconnect_attempts: int = 0
        self._max_reconnect_attempts: int = 5
        self._reconnect_base_delay: float = 1.0  # seconds
        # Registry doc joined lazily on first dynamic-proxy call; typed
        # clients don't need it. Manifests cached by (name, version) so a
        # second `proxy("X")` doesn't re-fetch.
        self._registry_doc: Any | None = None
        self._registry_event_rx: Any | None = None
        self._registry_join_lock: asyncio.Lock | None = None
        self._manifest_cache: dict[tuple[str, int], Any] = {}
        # Populated lazily by ``_ensure_type_defs`` on the first
        # ``proxy()`` call for each contract. Keyed by contract_id;
        # value is ``(by_tag, by_hash)`` of canonical TypeDef dicts
        # decoded from ``types/{hash}.bin`` collection entries. Empty
        # maps mean the producer is legacy (pre-§11.4) and callers
        # should fall back to the flat manifest path.
        self._type_def_cache: dict[
            str, tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]]]
        ] = {}

    @property
    def _server_key(self) -> str:
        """The cache key of the RPC connection to the server."""
        addr = self._endpoint_addr_in
        if isinstance(addr, NodeAddr):
            return base64.b64encode(addr.to_bytes()).decode()
        if isinstance(addr, (bytes, bytearray)):
            return base64.b64encode(bytes(addr)).decode()
        return str(addr)

    async def connect(self) -> None:
        """Start the node, enroll with the server when configured, and read
        the services the server advertises. Idempotent.

        Raises:
            ValueError: the enrollment configuration is incomplete or
                malformed (nothing falls back to an open client).
            AdmissionDeniedError: the server refused this client.
        """
        if self._connected:
            return

        # Start a full node so the consumer can join registry docs and
        # fetch blobs. With storage_path set it is persistent: the node key
        # (secret_key_file), joined docs and downloaded blobs survive
        # restarts.
        from aster import observability
        observability.ensure_started()

        target = _coerce_node_addr(self._endpoint_addr_in)
        # Enrollment from `[enrollment]`: validated in Rust before the node
        # starts (a client may not run an issuer; state defaults to a
        # temporary directory removed when the client closes).
        plan = EnrollmentPlan.from_config(self._config, side="client")
        if plan is not None:
            self._node = await Node.start(plan.config, alpns=[ENROLLMENT_ALPN])
        else:
            self._node = await Node.start(self._config)
        if self._config.storage_path:
            logger.debug("Consumer node: persistent at %s", self._config.storage_path)
        else:
            logger.debug("Consumer node: in-memory (set ASTER_STORAGE_PATH for persistence)")
        self._ep = net_client(self._node)

        try:
            if plan is not None:
                self._enrollment = plan.install(self._node)
                try:
                    await self._enrollment.enroll_with(self._node, target)
                except Exception as exc:  # noqa: BLE001
                    raise self._denied(True, str(exc)) from exc
                self._enrollment.start_renewal(target)
            await self._discover()
        except BaseException:
            await self._shutdown_node()
            raise
        self._connected = True
        self._reconnect_attempts = 0

    def _denied(self, had_credential: bool, detail: str = "") -> AdmissionDeniedError:
        our_endpoint_id = ""
        try:
            if self._node is not None:
                our_endpoint_id = self._node.node_addr_info().endpoint_id
        except Exception:  # noqa: BLE001
            pass
        return AdmissionDeniedError(
            had_credential=had_credential,
            credential_file=self._config.enrollment_credential_file,
            our_endpoint_id=our_endpoint_id,
            server_address=str(self._endpoint_addr_in),
            detail=detail,
        )

    async def _discover(self) -> None:
        """Open the RPC connection and read the services the server
        advertises through ``aster.ops.NodeInfo`` (a server that does not
        advertise them still serves typed clients)."""
        import json as _json
        from .status import RpcError
        from .transport.iroh import rpc_client

        enrolled = self._enrollment is not None
        try:
            conn = await self._rpc_conn_for(self._server_key)
        except Exception as exc:  # noqa: BLE001
            if enrolled:
                raise
            raise self._denied(False, str(exc)) from exc
        try:
            info = await asyncio.wait_for(rpc_client(conn).describe_node(), 15)
        except RpcError as exc:
            logger.debug("server does not describe its services: %s", exc)
            info = {"attributes": {}}
        except Exception as exc:  # noqa: BLE001
            self._rpc_conns.pop(self._server_key, None)
            if enrolled:
                raise
            raise self._denied(False, str(exc)) from exc

        attributes = info.get("attributes") or {}
        services: list[ServiceSummary] = []
        raw = attributes.get(_SERVICES_ATTRIBUTE)
        self._advertised = bool(raw)
        if raw:
            try:
                from .limits import MAX_ADVERTISED_SERVICES

                for d in _json.loads(raw)[:MAX_ADVERTISED_SERVICES]:
                    summary = ServiceSummary.from_json_dict(d)
                    # Every channel is this server's RPC connection.
                    summary.channels = {
                        name: self._server_key for name in (summary.channels or {self._channel_name: ""})
                    }
                    services.append(summary)
            except (ValueError, KeyError, TypeError) as exc:
                logger.warning("ignoring malformed %s attribute: %s", _SERVICES_ATTRIBUTE, exc)
        self._services = services
        self._registry_namespace = attributes.get(_REGISTRY_ATTRIBUTE) or ""
        logger.info(
            "Connected -- services: %s, registry_namespace: %s, enrollment: %s",
            [s.name for s in self._services],
            bool(self._registry_namespace),
            enrolled,
        )

    async def reconnect(self) -> None:
        """Reconnect after a connection drop.

        Closes stale connections, opens a new RPC connection and rereads the
        services list. Uses exponential backoff on repeated failures.
        """
        self._rpc_conns.clear()
        self._clients.clear()
        self._connected = False

        for attempt in range(self._max_reconnect_attempts):
            try:
                delay = self._reconnect_base_delay * (2 ** attempt)
                if attempt > 0:
                    logger.info(
                        "Reconnect attempt %d/%d (delay %.1fs)",
                        attempt + 1, self._max_reconnect_attempts, delay,
                    )
                    await asyncio.sleep(delay)

                await self._discover()
                self._connected = True
                self._reconnect_attempts = 0
                logger.info("Reconnected successfully")
                return

            except Exception as exc:
                logger.warning("Reconnect attempt %d failed: %s", attempt + 1, exc)

        raise ConnectionError(
            f"Failed to reconnect after {self._max_reconnect_attempts} attempts"
        )

    async def _rpc_conn_for(self, rpc_addr_b64: str) -> Any:
        # Fast path: cached connection, no lock needed.
        if rpc_addr_b64 in self._rpc_conns:
            return self._rpc_conns[rpc_addr_b64]
        # Slow path: serialise concurrent cache misses so a burst of
        # callers opens at most ONE underlying QUIC connection per
        # rpc_addr. Without the lock, N concurrent `open_session`s
        # would each race through `connect_node_addr` and each get a
        # distinct connection, which silently defeats the server-side
        # `max_sessions_per_connection` cap (each connection has its
        # own per-connection session state).
        if self._rpc_conn_lock is None:
            self._rpc_conn_lock = asyncio.Lock()
        async with self._rpc_conn_lock:
            # Re-check under the lock -- a concurrent caller may have
            # populated the cache while we were waiting.
            if rpc_addr_b64 in self._rpc_conns:
                return self._rpc_conns[rpc_addr_b64]
            assert self._ep is not None
            rpc_addr = _coerce_node_addr(rpc_addr_b64)
            conn = await self._ep.connect_node_addr(rpc_addr, RPC_ALPN)
            self._rpc_conns[rpc_addr_b64] = conn
            return conn

    def _next_session_id(self, rpc_addr_b64: str) -> int:
        """A fresh sessionId (never 0, which is the SHARED pool) for the
        connection cached under `rpc_addr_b64`, from the connection's shared
        counter in core: Python and Rust clients on one connection never
        collide, and a replaced connection (e.g. after `reconnect`) starts
        over."""
        conn = self._rpc_conns.get(rpc_addr_b64)
        if conn is None:
            raise RuntimeError(f"no RPC connection for {rpc_addr_b64[:16]!r}")
        from aster.transport.iroh import next_session_id
        return next_session_id(conn)

    async def _ensure_registry_doc(self) -> Any | None:
        """Join the producer's registry doc lazily. Only called from dynamic
        proxy / session paths; typed clients don't need manifests at runtime.
        Returns the joined doc handle, or ``None`` if the producer didn't
        share a registry namespace (e.g. a local-transport test fixture).
        """
        if self._registry_doc is not None:
            return self._registry_doc
        if not self._registry_namespace or self._node is None:
            return None
        if self._registry_join_lock is None:
            self._registry_join_lock = asyncio.Lock()
        async with self._registry_join_lock:
            if self._registry_doc is not None:
                return self._registry_doc
            remote_node_id = ""
            try:
                addr = _coerce_node_addr(self._endpoint_addr_in)
                remote_node_id = addr.endpoint_id or ""
            except Exception:
                return None
            if not remote_node_id:
                return None
            dc = docs_client(self._node)
            doc, receiver = await dc.join_and_subscribe_namespace(
                self._registry_namespace, remote_node_id
            )
            # Best-effort sync wait. Short timeout because the producer
            # writes the entries synchronously on startup so the peer sync
            # is usually immediate.
            await _wait_for_initial_sync(doc, receiver, timeout=6.0)
            self._registry_doc = doc
            self._registry_event_rx = receiver
            return doc

    async def _ensure_manifest(
        self, service_name: str, version: int, summary: "ServiceSummary | None" = None
    ) -> Any | None:
        """Fetch and cache the contract manifest for (name, version). Uses the
        ``manifests/{contract_id}/`` fast-path shortcut when available, falls
        back to the slow blob-collection path. Returns ``None`` if the
        producer didn't publish a manifest (e.g. an older producer).
        """
        key = (service_name, version)
        cached = self._manifest_cache.get(key)
        if cached is not None:
            return cached

        if summary is None:
            for s in self._services:
                if s.name == service_name and s.version == version:
                    summary = s
                    break
        if summary is None or not getattr(summary, "contract_id", ""):
            return None

        doc = await self._ensure_registry_doc()
        if doc is None:
            return None

        import json as _json
        from .contract.manifest import ContractManifest
        from .registry.keys import contract_key, manifest_key

        # Fast path: inline manifest shortcut.
        manifest: ContractManifest | None = None
        try:
            shortcut = await doc.query_key_exact(manifest_key(summary.contract_id))
            if shortcut:
                manifest_bytes = await doc.read_entry_content(shortcut[0].content_hash)
                manifest = ContractManifest.from_json(manifest_bytes.decode())
        except Exception as exc:
            logger.debug("manifest shortcut read failed for %s: %s", service_name, exc)

        # Slow path: ArtifactRef → blob collection → manifest.json.
        if manifest is None:
            try:
                from .contract.publication import fetch_from_collection

                entries = await doc.query_key_exact(contract_key(summary.contract_id))
                if entries:
                    content = await doc.read_entry_content(entries[0].content_hash)
                    artifact = _json.loads(content)
                    collection_hash = artifact.get("collection_hash", "")
                    if collection_hash:
                        bc = blobs_client(self._node)
                        addr = _coerce_node_addr(self._endpoint_addr_in)
                        remote_node_id = addr.endpoint_id or ""
                        if remote_node_id:
                            try:
                                await bc.download_collection_hash(
                                    collection_hash, remote_node_id
                                )
                            except Exception:
                                pass
                        manifest_bytes = await fetch_from_collection(
                            bc, collection_hash, "manifest.json"
                        )
                        if manifest_bytes:
                            manifest = ContractManifest.from_json(
                                manifest_bytes.decode()
                            )
            except Exception as exc:
                logger.debug("manifest blob fetch failed for %s: %s", service_name, exc)

        if manifest is not None:
            self._manifest_cache[key] = manifest
        return manifest

    async def _ensure_type_defs(
        self, contract_id: str
    ) -> tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]]]:
        """Fetch ``types/{hash}.bin`` blobs for ``contract_id`` and decode
        each into the canonical TypeDef dict via the Rust-core reader
        exposed through PyO3.

        Returns ``(by_tag, by_hash)``. When the producer didn't emit
        per-type blobs (legacy publisher pre-§11.4), both maps are empty
        and callers fall back to the flat-manifest proxy path. Mirrors
        the TS ``ensureTypeDefs`` in runtime.ts.
        """
        cached = self._type_def_cache.get(contract_id)
        if cached is not None:
            return cached

        empty: tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]]] = ({}, {})

        doc = await self._ensure_registry_doc()
        if doc is None:
            return empty

        import json as _json
        from .registry.keys import contract_key

        try:
            entries = await doc.query_key_exact(contract_key(contract_id))
            if not entries:
                return empty
            content = await doc.read_entry_content(entries[0].content_hash)
            artifact = _json.loads(content)
            collection_hash = artifact.get("collection_hash", "")
            if not collection_hash:
                return empty

            bc = blobs_client(self._node)

            # Pull the collection + child blobs into the local store so
            # subsequent list/read succeed. Mirrors the same flow used
            # by ``_ensure_manifest`` for the manifest.json fetch.
            addr = _coerce_node_addr(self._endpoint_addr_in)
            remote_node_id = addr.endpoint_id or ""
            if remote_node_id:
                try:
                    await bc.download_collection_hash(collection_hash, remote_node_id)
                except Exception:  # noqa: BLE001
                    pass

            entries_list = await bc.list_collection(collection_hash)
            from aster._aster import contract as _contract_native

            by_tag: dict[str, dict[str, Any]] = {}
            by_hash: dict[str, dict[str, Any]] = {}
            import re as _re

            type_entry = _re.compile(r"^types/([0-9a-f]{64})\.bin$")
            for name, entry_hash, _size in entries_list:
                m = type_entry.match(name)
                if not m:
                    continue
                try:
                    raw = await bc.read_to_bytes(entry_hash)
                    td_json = _contract_native.canonical_bytes_to_json("TypeDef", bytes(raw))
                    td = _json.loads(td_json)
                except Exception as exc:  # noqa: BLE001
                    logger.debug(
                        "type-def decode failed for %s: %s", m.group(1)[:12], exc
                    )
                    continue
                hash_hex = m.group(1)
                by_hash[hash_hex] = td
                by_tag[f"{td.get('package', '')}/{td.get('name', '')}"] = td

            out = (by_tag, by_hash)
            self._type_def_cache[contract_id] = out
            return out
        except Exception as exc:  # noqa: BLE001
            logger.debug("_ensure_type_defs failed for %s: %s", contract_id[:12], exc)
            return empty

    async def _build_dynamic_codec(
        self, summary: "ServiceSummary"
    ) -> tuple[Any, Any | None, dict[str, dict[str, Any]]]:
        """Build a codec suitable for dynamic proxy / session against
        ``summary``. Prefers Fory XLANG with manifest-synthesised types
        so requests ride the same wire the typed client uses.

        Returns ``(codec, factory, method_map)`` where ``method_map`` is
        ``{method_name: method_descriptor_dict}`` so callers can build
        typed request instances from dicts before encoding.

        Raises:
            NotImplementedError: no usable XLANG manifest. The former JSON
                (mode 3) fallback is retired; manifest-less dynamic calls
                wait for Rust-core dynamic Fory (plan D5).
        """
        modes = list(getattr(summary, "serialization_modes", None) or [])
        server_supports_xlang = not modes or "xlang" in modes

        if server_supports_xlang:
            manifest = await self._ensure_manifest(summary.name, summary.version)
            if manifest is not None and manifest.methods:
                try:
                    from .dynamic import DynamicTypeFactory
                    from .codec import ForyCodec

                    factory = DynamicTypeFactory()

                    # Hybrid path mirroring TS: prefer the canonical
                    # TypeDef graph (types/{hash}.bin blobs) when the
                    # publisher emitted it. The full tree including
                    # hex-hash refs lets us reconstruct every struct
                    # with the same Fory hash the producer computed.
                    # Fall back to the flat manifest for any root tag
                    # the TypeDef graph didn't resolve (legacy
                    # publishers, or methods without inferable wire
                    # tags).
                    contract_id = getattr(summary, "contract_id", "") or ""
                    resolved: set[str] = set()
                    if contract_id:
                        by_tag, by_hash = await self._ensure_type_defs(contract_id)
                        if by_tag:
                            roots: list[str] = []
                            for m in manifest.methods:
                                req = m.get("request_wire_tag", "")
                                res = m.get("response_wire_tag", "")
                                if req:
                                    roots.append(req)
                                if res:
                                    roots.append(res)
                            resolved = factory.register_from_type_defs(
                                roots, by_tag, by_hash
                            )

                    manifest_fallback = [
                        m
                        for m in manifest.methods
                        if not (
                            (not m.get("request_wire_tag") or m["request_wire_tag"] in resolved)
                            and (not m.get("response_wire_tag") or m["response_wire_tag"] in resolved)
                        )
                    ]
                    if manifest_fallback:
                        factory.register_from_manifest(manifest_fallback)

                    codec = ForyCodec(
                        mode=SerializationMode.XLANG,
                        types=factory.get_all_types(),
                    )
                    method_map = {m.get("name"): m for m in manifest.methods if m.get("name")}
                    return codec, factory, method_map
                except Exception as exc:
                    raise NotImplementedError(
                        f"dynamic proxy for {summary.name!r}: could not build a "
                        f"Fory codec from the published manifest ({exc}); JSON "
                        f"mode 3 is retired and Rust-core dynamic Fory is "
                        f"pending (dynamic Fory pending, see plan D5)"
                    ) from exc

        raise NotImplementedError(
            f"dynamic proxy for {summary.name!r} needs the producer's Fory XLANG "
            f"manifest; JSON mode 3 is retired (dynamic Fory pending, see plan D5)"
        )

    async def _resolve_service(
        self,
        service_cls: type,
        channel: str | None,
    ) -> tuple[Any, str, Any]:
        """Resolve `(connection, rpc_addr_key, service_info)` for a
        service class. Shared between `client()` and `open_session()`.
        Raises the same errors as `client()` on missing service /
        channel.
        """
        if not self._connected:
            raise RuntimeError("AsterClient not connected; call connect() first")
        info = getattr(service_cls, "__aster_service_info__", None)
        if info is None:
            raise TypeError(
                f"{service_cls!r} is not @service-decorated "
                f"(missing __aster_service_info__)"
            )
        rpc_addr_key = self._rpc_key_for(info.name, info.version, channel)
        conn = await self._rpc_conn_for(rpc_addr_key)
        return conn, rpc_addr_key, info

    def _rpc_key_for(self, name: str, version: int, channel: str | None) -> str:
        """The RPC connection key for a typed client of ``(name, version)``.
        A server that advertises its services must list it; one that does not
        (another binding, or a Rust ``AsterServer``) is called directly, even
        after some of its services were resolved from its registry."""
        if not self._advertised:
            return self._server_key
        summary: ServiceSummary | None = None
        for s in self._services:
            if s.name == name and s.version == version:
                summary = s
                break
        if summary is None:
            raise LookupError(
                f"service {name!r} v{version} not offered by producer "
                f"(got: {[(s.name, s.version) for s in self._services]})"
            )
        channel_key = channel or self._channel_name
        if channel_key not in summary.channels:
            raise LookupError(
                f"service {name!r} has no channel {channel_key!r} "
                f"(available: {list(summary.channels)})"
            )
        return summary.channels[channel_key]

    async def open_session(
        self,
        *,
        service_cls: type | None = None,
        channel: str | None = None,
    ) -> "ClientSession":
        """Open a new client-bound session against the peer's RPC
        connection. Allocates a fresh monotonic `sessionId` (per spec
        §6) on the underlying `(peer, connection)`; every RPC made via
        `ClientSession.client(...)` threads that id into its
        `StreamHeader` so the server routes them into the same
        session-scoped service instance.

        `service_cls` picks which of the producer's announced RPC
        addresses to bind to. If omitted, the first advertised RPC
        channel on any service is used -- fine when the producer only
        serves one channel but explicit is clearer.
        """
        if not self._connected:
            raise RuntimeError("AsterClient not connected; call connect() first")

        if service_cls is not None:
            conn, rpc_addr_key, _ = await self._resolve_service(service_cls, channel)
        else:
            # The server's RPC connection.
            rpc_addr_key = self._server_key
            conn = await self._rpc_conn_for(rpc_addr_key)

        session_id = self._next_session_id(rpc_addr_key)
        return ClientSession(
            parent=self,
            connection=conn,
            rpc_addr_key=rpc_addr_key,
            session_id=session_id,
        )

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

        rpc_addr_key = self._rpc_key_for(info.name, info.version, channel)
        conn = await self._rpc_conn_for(rpc_addr_key)
        interceptors = self._identity_interceptors() + list(interceptors or [])

        # Session-scoped services route via a ClientSession allocated on
        # the fly. Every call threads the same monotonic sessionId into
        # its StreamHeader, so the server lands all calls in the same
        # session-instance (spec Sec. 6 / 7.5). Callers who want explicit
        # session lifecycle should use AsterClient.open_session() instead.
        if info.scoped == RpcScope.SESSION:
            session_id = self._next_session_id(rpc_addr_key)
            session = ClientSession(
                parent=self,
                connection=conn,
                rpc_addr_key=rpc_addr_key,
                session_id=session_id,
            )
            client = await session.client(
                service_cls,
                codec=codec,
                interceptors=interceptors,
            )
        else:
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
        await self._shutdown_node()

    async def _shutdown_node(self) -> None:
        if self._enrollment is not None:
            self._enrollment.close()
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
    def registry_namespace(self) -> str:
        """64-char hex namespace_id for the registry doc.

        Empty string if the server advertised no registry doc.
        """
        return self._registry_namespace

    @property
    def enrollment(self) -> Any | None:
        """The ``aster.Enrollment`` installed from ``[enrollment]``, or
        ``None`` when enrollment is off (``credential``, ``is_renewing``)."""
        return self._enrollment

    def _identity_interceptors(self) -> list[Any]:
        """With ``[identity.publisher]``: hand each call's on-behalf-of token
        to the publisher (a provider's new key is published at once)."""
        if self._enrollment is None or not self._enrollment.has_publisher:
            return []
        from .identity import _PublisherNotice

        return [_PublisherNotice(self._enrollment)]

    def mint_user_token(
        self, sub: str, roles: list[str], amr: list[str], ttl: float
    ) -> str:
        """Mint a short-lived token for user ``sub`` with ``roles``, signed in
        by ``amr`` (at least one, e.g. ``aster.identity.AMR_WEBAUTHN``),
        valid for ``ttl`` seconds (at most 300), signed with this node's key
        (Rust ``AsterClient::user_token_issuer`` +
        ``Issuer::mint_user_token``). The roles must be within the issuer
        delegation (``aster.identity.issuable_roles``) in the credential held
        now; anything else raises ``ValueError``. Attach it with
        ``CallOptions(on_behalf_of=token)``; this node needs
        ``aster.gateway``. Raises ``RuntimeError`` without enrollment."""
        if self._enrollment is None:
            raise RuntimeError(
                "mint_user_token needs enrollment: the token's roles are bounded by "
                "the delegation in this node's credential"
            )
        return self._enrollment.mint_user_token(sub, list(roles), list(amr), float(ttl))

    async def node_config(self) -> Any:
        """An :class:`aster.identity.NodeConfigClient` for the server this
        client connected to (``aster.ops.NodeConfig``; needs
        ``aster.config-admin``)."""
        if not self._connected:
            raise RuntimeError("AsterClient not connected; call connect() first")
        from .identity import NodeConfigClient

        return NodeConfigClient(await self._rpc_conn_for(self._server_key))

    @property
    def is_renewing(self) -> bool:
        """Whether this client is renewing its credential."""
        return self._enrollment is not None and self._enrollment.is_renewing

    @property
    def open_gate(self) -> bool:
        """True when this client connected without enrollment (the server
        admits any node). Clients such as the shell use it to suppress
        identity banners that are irrelevant on an open server."""
        return self._connected and self._enrollment is None

    async def proxy(self, service_name: str) -> "ProxyClient":
        """Create a dynamic proxy client for a shared (stream-per-call) service.

        On first call per service, fetches the published contract manifest
        from the producer's registry doc, synthesises request/response types
        via :class:`DynamicTypeFactory`, and builds a Fory XLANG codec that
        speaks the same wire as the typed client. Subsequent calls to
        ``proxy(...)`` for the same service reuse the cached manifest.

        No local type definitions needed -- call methods with dicts and
        receive dicts back::

            mc = await client.proxy("MissionControl")
            result = await mc.getStatus({"agent_id": "edge-1"})
            print(result["status"])

        For session-scoped services, use :meth:`session` instead.

        Args:
            service_name: The service name (e.g., ``"MissionControl"``).

        Returns:
            A :class:`ProxyClient` with method stubs for each RPC method.

        Raises:
            TypeError: If the service is session-scoped (use ``session()``).
        """
        if not self._connected:
            raise RuntimeError("AsterClient not connected; call connect() first")

        summary = await self._resolve_named_service(service_name)

        if getattr(summary, "pattern", "shared") == "session":
            raise TypeError(
                f"'{service_name}' is session-scoped. "
                f"Use 'await client.session(\"{service_name}\")' instead of "
                f"'await client.proxy(\"{service_name}\")'."
            )

        codec, factory, method_map = await self._build_dynamic_codec(summary)
        return ProxyClient(
            service_name=service_name,
            aster_client=self,
            codec=codec,
            factory=factory,
            method_map=method_map,
        )

    async def session(self, service_name: str) -> "SessionProxyClient":
        """Create a dynamic proxy client for a session-scoped service.

        Opens a single bidirectional QUIC stream and multiplexes calls
        over it. Maintains a lock to ensure one call in flight at a time
        (spec requirement). Call methods with dicts, receive dicts::

            agent = await client.session("AgentSession")
            result = await agent.register({"agent_id": "edge-1"})
            print(result["assignment"])
            await agent.close()

        For shared (stream-per-call) services, use :meth:`proxy` instead.

        Args:
            service_name: The service name (e.g., ``"AgentSession"``).

        Returns:
            A session proxy with method stubs. Must be closed when done.
        """
        if not self._connected:
            raise RuntimeError("AsterClient not connected; call connect() first")

        summary = await self._resolve_named_service(service_name)

        channel_key = self._channel_name
        if channel_key not in summary.channels:
            channel_key = next(iter(summary.channels), self._channel_name)

        conn = await self._rpc_conn_for(summary.channels.get(channel_key, ""))

        codec, factory, method_map = await self._build_dynamic_codec(summary)
        session_id = self._next_session_id(summary.channels.get(channel_key, ""))
        session_client = SessionProxyClient(
            aster_client=self,
            connection=conn,
            service_name=service_name,
            session_id=session_id,
            codec=codec,
            factory=factory,
            method_map=method_map,
        )
        self._clients.append(session_client)
        return session_client

    async def _resolve_named_service(self, service_name: str) -> "ServiceSummary":
        """A service by name: from the ``aster.services`` NodeInfo attribute a
        Python server advertises, else from the registry doc named by the
        ``aster.registry_namespace`` attribute (every binding's server
        publishes that one), else ``ValueError``."""
        for s in self._services:
            if s.name == service_name:
                return s
        summary = await self._summary_from_registry(service_name)
        if summary is not None:
            self._services.append(summary)
            return summary
        return self._find_service(service_name)

    async def _summary_from_registry(self, service_name: str) -> "ServiceSummary | None":
        """Build a summary from the registry doc's
        ``services/{name}/versions/v{n}/`` pointer (highest version) and the
        published manifest."""
        from .registry.keys import parse_version_key, version_prefix

        doc = await self._ensure_registry_doc()
        if doc is None:
            return None
        prefix = version_prefix(service_name)
        try:
            entries = await doc.query_key_prefix(prefix)
        except Exception as exc:  # noqa: BLE001
            logger.debug("registry lookup for %s failed: %s", service_name, exc)
            return None
        best: tuple[int, str] | None = None
        for entry in entries:
            parsed = parse_version_key(bytes(entry.key))
            if parsed is None or parsed[0] != service_name:
                continue
            version = parsed[1]
            if best is None or version > best[0]:
                cid = bytes(await doc.read_entry_content(entry.content_hash)).decode().strip()
                best = (version, cid)
        if best is None:
            return None
        summary = ServiceSummary(
            name=service_name,
            version=best[0],
            contract_id=best[1],
            channels={self._channel_name: self._server_key},
        )
        manifest = await self._ensure_manifest(service_name, best[0], summary)
        if manifest is not None:
            summary.pattern = getattr(manifest, "scoped", "shared") or "shared"
            summary.serialization_modes = list(getattr(manifest, "serialization_modes", []) or [])
        return summary

    def _find_service(self, service_name: str) -> "ServiceSummary":
        """Look up a service by name in the services the server advertises."""
        for s in self._services:
            if s.name == service_name:
                return s
        available = [s.name for s in self._services]
        raise ValueError(
            f"Service '{service_name}' not found. "
            f"Available: {available}"
        )


class ClientSession:
    """A client-bound session over a per-connection multiplexed stream pool.

    Allocated via `AsterClient.open_session(...)`. Pins a
    `(connection, sessionId)` pair; every client stub produced through
    `ClientSession.client(...)` threads the same `sessionId` into its
    outbound `StreamHeader`, so the server side routes all calls to the
    same session-scoped service instance (spec §6, §7.5).

    The session is not a live server resource -- it's a client-side
    identifier that the server lazily materializes on the first call.
    Closing the session is a client-side operation that simply stops
    using the id; the server eventually reaps session state when the
    underlying QUIC connection closes.
    """

    def __init__(
        self,
        *,
        parent: AsterClient,
        connection: Any,
        rpc_addr_key: str,
        session_id: int,
    ) -> None:
        self._parent = parent
        self._connection = connection
        self._rpc_addr_key = rpc_addr_key
        self._session_id = session_id
        self._closed = False
        self._stubs: list[ServiceClient] = []

    @classmethod
    def for_test(
        cls,
        parent: "AsterClient",
        connection: Any,
        session_id: int,
    ) -> "ClientSession":
        """**TEST-ONLY** factory. Construct a `ClientSession` against an
        explicit `session_id`, bypassing `AsterClient.open_session`'s
        monotonic allocator. Mirrors Java `ClientSession.forTest` and
        TypeScript `ClientSession.forTest`.

        Production code MUST use `AsterClient.open_session` so the
        spec Sec. 6 "first stream arrival creates the session"
        invariant holds under the client's allocation order. This
        factory exists so tier-2 chaos tests can drive the server's
        lookup-or-create / graveyard logic with adversarial session_id
        sequences (out-of-order, replayed, past-the-cap) that the
        allocator would never produce.
        """
        return cls(
            parent=parent,
            connection=connection,
            rpc_addr_key="",
            session_id=session_id,
        )

    @property
    def session_id(self) -> int:
        return self._session_id

    @property
    def connection(self) -> Any:
        return self._connection

    async def client(
        self,
        service_cls: type,
        *,
        codec: Any | None = None,
        interceptors: list[Any] | None = None,
    ) -> ServiceClient:
        """Return a typed client stub bound to this session. Each stub
        uses an `IrohTransport` whose `session_id` is set to this
        session's id, so every outbound call carries the same routing
        key into the server's per-connection session map.

        Session-scoped (`RpcScope.SESSION`) service classes are routed
        here naturally; stateless services can also be used via a
        session, which just means the server treats them as
        session-instance-bound for the duration of the connection.
        """
        if self._closed:
            raise RuntimeError("ClientSession is closed")

        info = getattr(service_cls, "__aster_service_info__", None)
        if info is None:
            raise TypeError(
                f"{service_cls!r} is not @service-decorated "
                f"(missing __aster_service_info__)"
            )

        # If no explicit codec was supplied, build a service-aware Fory XLANG codec NOW so the
        # IrohTransport we hand to `create_client` knows about the
        # service's request/response types. Without this, the
        # transport's codec defaults to a bare ForyCodec with zero
        # registered types and the first `transport.unary(...)`
        # raises `TypeUnregisteredError` at encode time. The stub's
        # own codec is built inside `create_client` with the service
        # types -- but the stub delegates wire encoding to the
        # transport's codec, so the two must agree on types.
        if codec is None:
            from aster.client import _collect_service_types
            from aster.codec import ForyCodec
            request_response_types = _collect_service_types(service_cls, info)
            codec = ForyCodec(
                mode=SerializationMode.XLANG,
                types=list(request_response_types) if request_response_types else None,
            )

        transport = IrohTransport(
            connection=self._connection,
            codec=codec,
            session_id=self._session_id,
        )
        stub = create_client(
            service_cls,
            transport=transport,
            codec=codec,
            interceptors=interceptors,
        )
        self._stubs.append(stub)
        return stub

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        for stub in self._stubs:
            try:
                await stub.close()
            except Exception:
                pass
        self._stubs.clear()

    async def __aenter__(self) -> "ClientSession":
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.close()


class SessionProxyClient:
    """Dict-in / dict-out dynamic proxy for a session-scoped service.

    Created via :meth:`AsterClient.session`. Works without local type
    definitions: at construction time the producer's manifest is fetched
    and request/response types are synthesised via ``DynamicTypeFactory``
    so calls ride the same Fory XLANG wire as the typed client. All calls
    on one proxy share the same monotonic ``sessionId`` so the server
    routes them to the same session instance (spec Sec. 6 / 7.5).
    """

    def __init__(
        self,
        *,
        aster_client: "AsterClient",
        connection: Any,
        service_name: str,
        session_id: int,
        codec: Any,
        factory: Any | None = None,
        method_map: dict[str, dict[str, Any]] | None = None,
    ) -> None:
        from aster.transport.iroh import IrohTransport
        self._aster_client = aster_client
        self._service_name = service_name
        self._session_id = session_id
        self._codec = codec
        self._factory = factory
        self._method_map: dict[str, dict[str, Any]] = method_map or {}
        self._closed = False
        self._transport = IrohTransport(
            connection, codec=codec, session_id=session_id,
        )

    @property
    def session_id(self) -> int:
        return self._session_id

    def _wrap_request(self, method: str, payload: Any) -> Any:
        """Wrap a dict payload into the method's synthesised request
        type so Fory encodes with the right wire tag. See
        :meth:`ProxyClient._wrap_request` for the rationale.
        """
        if self._factory is None or not isinstance(payload, dict):
            return payload
        method_meta = self._method_map.get(method)
        if not method_meta:
            return payload
        try:
            return self._factory.build_request(method_meta, payload)
        except KeyError:
            return payload

    async def call(self, method: str, request: dict | None = None) -> Any:
        """Call a unary method on this session. Returns decoded response."""
        if self._closed:
            raise RuntimeError("SessionProxyClient is closed")
        wire_payload = self._wrap_request(method, request or {})
        result = await self._transport.unary(
            self._service_name,
            method,
            wire_payload,
            serialization_mode=SerializationMode.XLANG.value,
        )
        if _is_dataclass_instance(result):
            return _dataclasses_asdict(result)
        return result

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            await self._transport.close()
        except Exception:  # noqa: BLE001
            pass

    def __getattr__(self, name: str) -> Any:
        if name.startswith("_"):
            raise AttributeError(name)

        async def method_stub(request: dict | None = None, **kwargs: Any) -> Any:
            if kwargs and request is None:
                request = kwargs
            return await self.call(name, request)

        method_stub.__name__ = name
        return method_stub


def _resolve_relay_addr(relay_url: str) -> str | None:
    """Resolve a relay URL (e.g. ``https://relay.iroh.network``) to ``ip:port``.

    Uses stdlib DNS resolution -- no subprocess. Returns None on failure.
    """
    import socket
    from urllib.parse import urlparse

    try:
        parsed = urlparse(relay_url)
        host = parsed.hostname
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
        if not host:
            return None
        infos = socket.getaddrinfo(host, port, socket.AF_UNSPEC, socket.SOCK_STREAM)
        if infos:
            # Use first result -- (family, type, proto, canonname, sockaddr)
            addr = infos[0][4]
            return f"{addr[0]}:{addr[1]}"
    except (socket.gaierror, OSError, ValueError):
        pass
    return None


# ── helpers ──────────────────────────────────────────────────────────────────


def _with_node_identity(config: AsterConfig, identity_key: bytes | None) -> AsterConfig:
    """Fold the node key into ``config``.

    - A ``[node] secret_key`` from ``.aster-identity`` is used when the config
      has no ``secret_key`` of its own (an explicit ``secret_key_file`` must
      then hold the same key -- ``Node.start`` raises ``ValueError`` if not).
    - A persistent node (``storage_path``) without any configured key gets
      ``secret_key_file = <storage_path>/node.key`` so its identity survives
      restarts.
    """
    if identity_key and config.secret_key is None:
        config = config.replace(secret_key=identity_key)
    if (
        config.storage_path
        and config.secret_key is None
        and config.secret_key_file is None
    ):
        config = config.replace(
            secret_key_file=os.path.join(config.storage_path, "node.key")
        )
    return config


def _is_completed_sync(event: Any) -> bool:
    """Whether ``event`` is a ``sync_finished`` for a round that completed.

    A failed round (refused, aborted, or an unreachable peer) also ends with
    ``sync_finished``, with ``sync_ok`` false; it means "not synced yet".
    """
    return (
        getattr(event, "kind", None) == "sync_finished"
        and getattr(event, "sync_ok", None) is True
    )


async def _wait_for_initial_sync(
    doc: Any, receiver: Any, timeout: float, prefix: bytes = b"contracts/"
) -> bool:
    """Wait up to ``timeout`` seconds for a completed sync round or for an
    entry under ``prefix``. Returns whether either happened.

    A failed sync round counts as no news: waiting continues until the
    deadline, checking for entries while the stream is quiet.
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout

    async def has_entries() -> bool:
        try:
            return bool(await doc.query_key_prefix(prefix))
        except Exception:
            return False

    while (remaining := deadline - loop.time()) > 0:
        try:
            event = await asyncio.wait_for(receiver.recv(), timeout=min(remaining, 1.0))
        except asyncio.TimeoutError:
            if await has_entries():
                return True
            continue
        if event is None:
            # The subscription ended; nothing more will arrive.
            return await has_entries()
        if _is_completed_sync(event):
            await asyncio.sleep(0.1)
            return True
        if getattr(event, "kind", None) == "sync_finished":
            logger.debug(
                "sync round with %s failed (%s): %s",
                getattr(event, "peer", "?"),
                getattr(event, "sync_origin", "?"),
                getattr(event, "sync_error", "?"),
            )
    return await has_entries()


def _coerce_node_addr(addr: NodeAddr | str | bytes) -> NodeAddr:
    if isinstance(addr, NodeAddr):
        return addr
    if isinstance(addr, str):
        # Compact aster1... ticket format
        if addr.startswith("aster1"):
            from . import AsterTicket
            ticket = AsterTicket.from_string(addr)
            return NodeAddr(
                endpoint_id=ticket.endpoint_id,
                relay_url=None,  # ticket stores resolved IP:port, not relay URL
                direct_addresses=ticket.direct_addrs,
            )
        # 64-char hex string → bare endpoint ID (32 bytes, no relay/direct addrs)
        if len(addr) == 64 and all(c in "0123456789abcdef" for c in addr.lower()):
            return NodeAddr(endpoint_id=addr)
        # Otherwise assume base64-encoded NodeAddr bytes
        return NodeAddr.from_bytes(base64.b64decode(addr))
    if isinstance(addr, (bytes, bytearray)):
        return NodeAddr.from_bytes(bytes(addr))
    raise TypeError(f"unsupported address type: {type(addr).__name__}")


# ── ProxyClient ──────────────────────────────────────────────────────────────


class ProxyClient:
    """Dynamic proxy client that invokes RPC methods without local type definitions.

    Created via ``await AsterClient.proxy("ServiceName")``. Methods are
    discovered from the service contract and accept/return dicts::

        mc = await client.proxy("MissionControl")
        result = await mc.getStatus({"agent_id": "edge-1"})
        print(result["status"])

    For streaming methods, use the explicit stream helpers::

        async for entry in mc.server_stream("tailLogs", {"level": "warn"}):
            print(entry)
    """

    def __init__(
        self,
        service_name: str,
        aster_client: AsterClient,
        codec: Any | None = None,
        factory: Any | None = None,
        method_map: dict[str, dict[str, Any]] | None = None,
    ) -> None:
        self._service_name = service_name
        self._client = aster_client
        self._transport: Any = None
        self._codec: Any = codec
        self._factory = factory
        self._method_map: dict[str, dict[str, Any]] = method_map or {}
        self._method_cache: dict[str, "_ProxyMethod"] = {}
        self._options: CallOptions = CallOptions()

    def with_options(self, options: CallOptions) -> "ProxyClient":
        """A view of this proxy whose calls use ``options`` (deadline,
        metadata, serialization mode; merged over this proxy's) on every
        pattern. Shares the connection and dynamic codec once resolved."""
        view = ProxyClient.__new__(ProxyClient)
        view.__dict__.update(self.__dict__)
        view._method_cache = {}
        view._options = self._options.merged(options)
        return view

    def _call_kwargs(self) -> dict[str, Any]:
        """The transport keyword arguments for this proxy's options."""
        o = self._options
        out: dict[str, Any] = {}
        if o.metadata:
            out["metadata"] = dict(o.metadata)
        if o.timeout is not None:
            out["deadline_secs"] = float(o.timeout)
        if o.serialization_mode is not None:
            out["serialization_mode"] = int(o.serialization_mode)
        return out

    def _wrap_request(self, method_name: str, payload: Any) -> Any:
        """If this proxy is backed by a DynamicTypeFactory + Fory codec,
        and the given ``payload`` is a plain dict whose method has a
        synthesised request type, wrap the dict into a typed instance so
        Fory emits the correct wire tag. Otherwise return ``payload``
        unchanged.
        """
        if self._factory is None or not isinstance(payload, dict):
            return payload
        method_meta = self._method_map.get(method_name)
        if not method_meta:
            return payload
        try:
            return self._factory.build_request(method_meta, payload)
        except KeyError:
            return payload

    async def _ensure_transport(self) -> None:
        if self._transport is not None:
            return

        from .transport.iroh import IrohTransport

        summary = None
        for s in self._client._services:
            if s.name == self._service_name:
                summary = s
                break
        if summary is None:
            raise RuntimeError(f"{self._service_name} not found")

        rpc_addr = summary.channels.get("rpc", "") if hasattr(summary, 'channels') and summary.channels else ""
        if not rpc_addr:
            rpc_addr = self._client._endpoint_addr_in if hasattr(self._client, '_endpoint_addr_in') else ""
        if not rpc_addr:
            raise RuntimeError(f"{self._service_name} has no rpc channel")

        conn = await self._client._rpc_conn_for(rpc_addr)

        if self._codec is None:
            self._codec, self._factory, self._method_map = (
                await self._client._build_dynamic_codec(summary)
            )
        self._transport = IrohTransport(conn, codec=self._codec)

    def __getattr__(self, method_name: str) -> "_ProxyMethod":
        if method_name.startswith("_"):
            raise AttributeError(method_name)
        # Cache _ProxyMethod instances per method name to avoid per-call
        # allocation in tight loops. Using object.__getattribute__ to avoid
        # recursing into __getattr__.
        cache = object.__getattribute__(self, "_method_cache")
        cached = cache.get(method_name)
        if cached is not None:
            return cached
        m = _ProxyMethod(self, method_name)
        cache[method_name] = m
        return m


async def _wrap_async_iter(
    proxy: "ProxyClient",
    method_name: str,
    source: Any,
) -> Any:
    """Wrap each dict emitted by ``source`` into the synthesised request
    type for ``method_name`` so Fory can encode it with the right wire
    tag. Used by client-streaming paths where the user hands us an
    async iterator of dicts.
    """
    async for item in source:
        yield proxy._wrap_request(method_name, item)


class _ProxyMethod:
    """Bound proxy method -- callable as ``await proxy.methodName(args)``."""

    def __init__(self, proxy: ProxyClient, method_name: str) -> None:
        self._proxy = proxy
        self._method_name = method_name

    async def __call__(self, payload: Any = None, **kwargs: Any) -> Any:
        """Invoke the RPC method.

        Automatically detects the pattern:
        - Pass a dict or kwargs -> unary RPC
        - Pass an async iterator/generator -> client streaming
        """
        proxy = self._proxy
        if proxy._transport is None:
            await proxy._ensure_transport()

        # Detect client streaming: payload is an async iterator
        if (
            payload is not None
            and not isinstance(payload, (dict, str, bytes))
            and hasattr(payload, "__aiter__")
        ):
            wrapped_stream = _wrap_async_iter(proxy, self._method_name, payload)
            result = await proxy._transport.client_stream(
                proxy._service_name,
                self._method_name,
                wrapped_stream,
                **proxy._call_kwargs(),
            )
            if _is_dataclass_instance(result):
                return _dataclasses_asdict(result)
            return result

        # Build payload from dict or kwargs
        if payload is None:
            payload = kwargs
        elif isinstance(payload, dict) and kwargs:
            payload = {**payload, **kwargs}

        wire_payload = proxy._wrap_request(
            self._method_name,
            payload if isinstance(payload, dict) else (payload or {}),
        )

        try:
            result = await proxy._transport.unary(
                proxy._service_name,
                self._method_name,
                wire_payload,
                **proxy._call_kwargs(),
            )
        except Exception as exc:
            # When the user calls a server-streaming method via the unary
            # ``await proxy.method(...)`` path, the transport sees a second
            # response frame and raises a low-level "multiple response
            # frames" error. Translate that into an actionable message
            # pointing at the streaming helpers, which is the only way to
            # consume server_stream / bidi_stream methods on the proxy.
            msg = str(exc)
            if "multiple response frames" in msg:
                hint = (
                    f"'{proxy._service_name}.{self._method_name}' is a streaming "
                    f"RPC and cannot be called as a unary `await proxy.{self._method_name}(...)`.\n"
                    f"  - For server-streaming methods, iterate the result of "
                    f"`proxy.{self._method_name}.stream(...)`:\n"
                    f"      async for item in proxy.{self._method_name}.stream({{...}}):\n"
                    f"          ...\n"
                    f"  - For bidi-streaming methods, use `proxy.{self._method_name}.bidi()`."
                )
                raise RuntimeError(hint) from exc
            raise

        if _is_dataclass_instance(result):
            return _dataclasses_asdict(result)
        return result

    async def stream(self, payload: dict | None = None, **kwargs: Any) -> Any:
        """Invoke as server-streaming RPC. Returns an async iterator.

        Usage::

            async for entry in mc.tailLogs.stream({"level": "warn"}):
                print(entry)
        """
        await self._proxy._ensure_transport()

        if payload is None:
            payload = kwargs
        elif isinstance(payload, dict) and kwargs:
            payload = {**payload, **kwargs}

        wire_payload = self._proxy._wrap_request(self._method_name, payload or {})

        async for item in self._proxy._transport.server_stream(
            self._proxy._service_name,
            self._method_name,
            wire_payload,
            **self._proxy._call_kwargs(),
        ):
            import dataclasses
            if dataclasses.is_dataclass(item) and not isinstance(item, type):
                yield dataclasses.asdict(item)
            else:
                yield item

    def bidi(self) -> "_ProxyBidiChannel":
        """Open a bidirectional streaming RPC. Returns a channel.

        Usage::

            channel = mc.runCommand.bidi()
            await channel.open()
            await channel.send({"command": "echo hello"})
            async for result in channel:
                print(result)
            await channel.close()
        """
        return _ProxyBidiChannel(self._proxy, self._method_name)


class _ProxyBidiChannel:
    """Bidirectional streaming channel for the proxy client."""

    def __init__(self, proxy: ProxyClient, method_name: str) -> None:
        self._proxy = proxy
        self._method_name = method_name
        self._channel: Any = None

    async def open(self) -> None:
        """Open the bidi stream."""
        await self._proxy._ensure_transport()
        self._channel = self._proxy._transport.bidi_stream(
            self._proxy._service_name,
            self._method_name,
            **self._proxy._call_kwargs(),
        )

    async def send(self, payload: dict) -> None:
        """Send a message on the bidi stream."""
        if self._channel is None:
            await self.open()
        wire_payload = self._proxy._wrap_request(self._method_name, payload)
        await self._channel.send(wire_payload)

    async def close(self) -> None:
        """Close the send side of the bidi stream."""
        if self._channel is not None:
            await self._channel.close()

    def __aiter__(self):
        return self

    async def __anext__(self) -> Any:
        if self._channel is None:
            raise StopAsyncIteration
        try:
            item = await self._channel.__anext__()
            import dataclasses
            if dataclasses.is_dataclass(item) and not isinstance(item, type):
                return dataclasses.asdict(item)
            return item
        except StopAsyncIteration:
            raise
