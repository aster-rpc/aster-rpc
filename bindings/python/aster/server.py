"""
aster.server -- Aster RPC server.

Spec reference: sec. 8.1 (Server API), sec. 8.2 (Server accept loop)

Since plan P4 the server runs on ``aster::rpc`` dynamic dispatch
(``docs/_internal/bindings/dynamic-dispatch-design.md``): Rust accepts the
connections and runs every pre-dispatch step -- header decode, service and
method lookup, serialization-mode checks, the session rules, Gate 3 and the
deadline -- and :mod:`aster._dispatch` runs the Python handlers and the
interceptor chain. This module keeps the public ``Server`` API on top.
"""

from __future__ import annotations

import asyncio
import inspect
import logging
from typing import TYPE_CHECKING, Any

from aster.codec import ForyCodec, ForyConfig
from aster.limits import LimitExceeded, MAX_METADATA_ENTRIES, validate_metadata
from aster.rpc_types import RpcScope, SerializationMode
from aster.service import ServiceRegistry

if TYPE_CHECKING:
    import aster
    from aster._aster import GatePolicy

logger = logging.getLogger(__name__)


# -- Server Errors ------------------------------------------------------------


class ServerError(Exception):
    """Base class for server errors."""
    pass


class ServiceNotFoundError(ServerError):
    """Raised when the requested service is not registered."""
    pass


class MethodNotFoundError(ServerError):
    """Raised when the requested method is not found in the service."""
    pass


class SerializationModeError(ServerError):
    """Raised when the requested serialization mode is not supported."""
    pass


# -- Server ------------------------------------------------------------------


class Server:
    """Aster RPC server.

    Serves the registered services on a node (started with ``aster/1``).

    Example usage::

        server = Server(
            endpoint=net_client(node),
            services=[EchoService()],
            node=node,
        )
        await server.serve()

    For graceful shutdown::

        await server.drain(grace_period=10.0)  # Wait for in-flight calls
        await server.close()
    """

    def __init__(
        self,
        endpoint: "aster.NetClient | None",
        services: list[type] | ServiceRegistry | None = None,
        codec: ForyCodec | None = None,
        fory_config: ForyConfig | None = None,
        interceptors: list[Any] | None = None,
        max_concurrent_streams: int | None = None,
        max_sessions_per_connection: int = 1024,
        registry: ServiceRegistry | None = None,
        owns_endpoint: bool = True,
        gate: "GatePolicy | None" = None,
        node: Any = None,
        on_non_rpc: Any = None,
        handler_timeout: float | None = None,
    ) -> None:
        """Initialize the server.

        Args:
            endpoint: The node's ``NetClient`` (used for its id and, with
                ``owns_endpoint``, closed by :meth:`close`).
            services: ``@service`` instances (shared services) or classes
                (session-scoped services, instantiated per session as
                ``cls(peer=...)``), or a ServiceRegistry.
            codec: The ForyCodec for serialization. Defaults to XLANG mode.
            fory_config: Optional configuration for implicitly created codecs.
            interceptors: Interceptor instances applied to every call.
            max_concurrent_streams: Accepted for compatibility; unused.
            max_sessions_per_connection: Cap on active sessions per
                connection (enforced in Rust).
            registry: Optional ServiceRegistry. If not provided, creates one
                from services.
            owns_endpoint: If True (default), ``close()`` also closes the
                endpoint. Set False when the endpoint is managed externally
                (e.g. by :class:`AsterServer`).
            gate: The node's ``GatePolicy``; its admission attributes are the
                caller's ``CallContext.attributes`` and feed Gate 3.
            node: The ``aster.Node`` to serve on (started with ``aster/1``).
            on_non_rpc: ``callback(alpn, connection)`` for inbound connections
                on other ALPNs that the node accepted.
            handler_timeout: Cap on any handler, seconds (default 300 s,
                ``<= 0`` = none). Enforced in Rust together with the caller's
                deadline and the method's ``timeout``.
        """
        self._endpoint = endpoint
        self._owns_endpoint = owns_endpoint
        self._gate = gate
        self._node = node
        self._on_non_rpc = on_non_rpc
        self._handler_timeout_s = handler_timeout
        self._dispatcher: Any = None
        # Deadlines are enforced by the Rust dispatcher (design sec. 2.4), so
        # there is no server-side deadline interceptor by default.
        self._interceptors = list(interceptors) if interceptors is not None else []
        self._max_concurrent_streams = max_concurrent_streams
        self._max_sessions_per_connection = max_sessions_per_connection
        self._service_instances: dict[tuple[str, int], Any] = {}
        # For session-scoped services we store the class (not an instance)
        self._service_classes: dict[tuple[str, int], type] = {}

        # Set up service registry first so we can collect types for the codec.
        if registry is not None:
            self._registry = registry
        elif isinstance(services, ServiceRegistry):
            self._registry = services
        elif services:
            self._registry = ServiceRegistry()
            for svc in services:
                service_class = svc if inspect.isclass(svc) else type(svc)
                info = self._registry.register(service_class)
                self._service_classes[(info.name, info.version)] = service_class
                if not inspect.isclass(svc):
                    self._service_instances[(info.name, info.version)] = svc
        else:
            self._registry = ServiceRegistry()

        # Build codec after registry is populated so service types are registered.
        if codec is not None:
            self._codec = codec
        else:
            from aster.client import _collect_service_types
            all_types: list[type] = []
            for svc_info in self._registry.get_all_services():
                cls = self._service_classes.get((svc_info.name, svc_info.version))
                if cls is not None:
                    for t in _collect_service_types(cls, svc_info):
                        if t not in all_types:
                            all_types.append(t)
            self._codec = ForyCodec(
                mode=SerializationMode.XLANG,
                types=all_types if all_types else None,
                fory_config=fory_config,
            )

        # Server state
        self._serving = False
        self._serve_task: asyncio.Task | None = None
        self._shutdown_event = asyncio.Event()

        for service_info in self._registry.get_all_services():
            key = (service_info.name, service_info.version)
            # Session-scoped services don't need a pre-existing instance
            if service_info.scoped == RpcScope.SESSION:
                continue
            if key not in self._service_instances:
                raise ServerError(
                    f"No implementation instance provided for service "
                    f"{service_info.name} v{service_info.version}"
                )

    @property
    def registry(self) -> ServiceRegistry:
        """The service registry used by this server."""
        return self._registry

    @property
    def endpoint(self) -> Any:
        """The endpoint this server is bound to."""
        return self._endpoint

    def _dispatch_services(self) -> list[Any]:
        """Registered services in registry order: instances for shared
        services, classes for session-scoped ones."""
        out: list[Any] = []
        for info in self._registry.get_all_services():
            key = (info.name, info.version)
            if info.scoped == RpcScope.SESSION:
                out.append(self._service_classes[key])
            else:
                out.append(self._service_instances[key])
        return out

    def start(self) -> Any:
        """Register the services with the Rust dispatcher and start serving
        on the node. Calls queue in Rust until :meth:`serve` drains them.
        Idempotent; returns the ``Dispatcher``."""
        if self._dispatcher is not None:
            return self._dispatcher
        if self._node is None:
            raise ServerError("serve() requires a node; pass node= to Server()")
        from aster._dispatch import Dispatcher
        dispatcher = Dispatcher(
            self._dispatch_services(),
            codec=self._codec,
            interceptors=self._interceptors,
            gate=self._gate,
            on_non_rpc=self._on_non_rpc,
            max_sessions_per_connection=self._max_sessions_per_connection,
            handler_timeout=self._handler_timeout_s,
        )
        dispatcher.start(self._node)
        self._dispatcher = dispatcher
        return dispatcher

    async def serve(self, channel_capacity: int = 256) -> None:
        """Serve until cancelled (or :meth:`close`).

        Rust (``aster::rpc::Server``) accepts connections and runs every
        pre-dispatch step; this loop runs the Python handlers. Requires
        ``node=`` on the ``Server``. ``channel_capacity`` is accepted for
        compatibility.
        """
        if self._serving:
            raise ServerError("server is already serving")
        dispatcher = self.start()
        self._serving = True
        self._serve_task = asyncio.current_task()
        self._shutdown_event.clear()
        if self._endpoint is not None and hasattr(self._endpoint, "endpoint_id"):
            logger.info("Server starting on %s", self._endpoint.endpoint_id())
        try:
            await dispatcher.run()
        finally:
            self._serving = False
            self._serve_task = None
            self._shutdown_event.set()

    async def drain(self, grace_period: float = 10.0) -> None:
        """Gracefully drain the server: wait up to ``grace_period`` seconds
        for in-flight handlers, then cancel the rest."""
        logger.info("Draining server (grace period: %.1fs)", grace_period)
        if self._dispatcher is not None:
            await self._dispatcher.drain(grace_period)
        logger.info("Drain complete")

    async def close(self) -> None:
        """Stop serving and cancel in-flight handlers."""
        logger.info("Closing server")
        if self._dispatcher is not None:
            self._dispatcher.close()

        if self._serve_task and not self._serve_task.done():
            self._serve_task.cancel()
            try:
                await self._serve_task
            except asyncio.CancelledError:
                pass

        if self._owns_endpoint and self._endpoint is not None:
            try:
                await self._endpoint.close()
            except Exception:
                pass

        self._shutdown_event.set()
        logger.info("Server closed")

    async def wait_until_stopped(self) -> None:
        """Wait until the server is stopped."""
        await self._shutdown_event.wait()


def _validated_metadata(
    keys: list[str] | None, values: list[str] | None
) -> dict[str, str] | None:
    """Build a metadata dict from header lists with the binding's security
    limits (``MAX_METADATA_ENTRIES`` / ``MAX_METADATA_TOTAL_BYTES``).

    Truncates to the limit rather than rejecting the call.
    """
    if not keys:
        return None

    vals = values or []
    try:
        validate_metadata(keys, vals)
    except LimitExceeded as e:
        logger.warning("Metadata limit exceeded: %s", e)
        keys = keys[:MAX_METADATA_ENTRIES]
        vals = vals[:MAX_METADATA_ENTRIES]

    return dict(zip(keys, vals))
