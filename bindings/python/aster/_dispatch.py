"""
aster._dispatch -- Python handlers on ``aster::rpc`` dynamic dispatch (plan P4).

Rust (``aster._aster.DispatchServer``) owns everything before the handler:
header decode, service/method lookup, serialization-mode checks, the session
rules (graveyard, per-connection cap), Gate 3 (``requires``), deadlines and
frame limits. It delivers calls in batches; this module maps each one to its
handler through a ``[service_idx][method_idx]`` table, builds the
``CallContext``, runs the interceptor chain and the handler, and answers
through the call's ``ServerCall`` with encoded payloads and statuses.

Design: ``docs/_internal/bindings/dynamic-dispatch-design.md`` sec. 2, sec. 4.2.
Error mapping (sec. 2.7): ``RpcError`` passes through; a request that does not
decode is ``INVALID_ARGUMENT``; any other handler exception is ``INTERNAL``;
a call whose handler never answers is ended by Rust with ``INTERNAL
"handler dropped call"`` when its ``ServerCall`` is dropped. Deadlines are
enforced by Rust, which sends ``DEADLINE_EXCEEDED`` and reports the
cancellation so the handler task is cancelled here.
"""

from __future__ import annotations

import asyncio
import inspect
import logging
import sys
import time
from typing import Any, AsyncIterator, Callable

from aster._aster import DispatchServer, ServerCall
from aster._lowering import lower_json, method_names, service_info_of
from aster.interceptors.base import (
    CallContext,
    apply_error_interceptors,
    apply_request_interceptors,
    apply_response_interceptors,
    normalize_error,
)
from aster.limits import MAX_CLIENT_STREAM_ITEMS
from aster.tracing import _CallScope
from aster.status import RpcError, StatusCode

logger = logging.getLogger(__name__)

# Event kinds delivered by ``DispatchServer`` (tuples; calls are
# ``ServerCall`` objects). Keep in sync with ``rust/src/rpc.rs``.
EV_CANCELLED = 2
EV_RESPONSE_WRITABLE = 3
EV_RESPONSE_CLOSED = 4
EV_SESSION_CLOSED = 5
EV_CONNECTION_CLOSED = 6
EV_NON_RPC = 7

# ``ServerCall.flags``.
FLAG_FIRST_IS_END = 0x1
FLAG_NEW_SESSION = 0x2
FLAG_IROH_AUTHENTICATED = 0x4

_BATCH = 64
_INTERNAL = int(StatusCode.INTERNAL)
_INVALID_ARGUMENT = int(StatusCode.INVALID_ARGUMENT)

# Run handlers eagerly (Python >= 3.12): a handler that does not suspend
# answers inside the batch loop, without a task switch.
_EAGER = sys.version_info >= (3, 12)


def _validated_metadata(md: dict[str, str]) -> dict[str, str]:
    """Header metadata with the binding's limits applied (truncated, not
    rejected, as before)."""
    from aster.server import _validated_metadata as validate

    return validate(list(md.keys()), list(md.values())) or {}


class ServerCallContext(CallContext):
    """A ``CallContext`` built from a ``ServerCall``. Fields that cost a
    crossing (metadata, attributes, the transport snapshot, the tunnel
    handle) are read on first access; assigning any of them works as on a
    plain ``CallContext``."""

    def __init__(  # noqa: D107 -- deliberately not the dataclass __init__
        self,
        call: ServerCall,
        method: "_Method",
        gate: Any,
    ) -> None:
        d = self.__dict__
        d["service"] = method.service.name
        d["method"] = method.name
        wire_id = call.wire_call_id
        d["call_id"] = str(wire_id) if wire_id else str(call.call_id)
        d["session_id"] = str(call.session_key) if call.session_key else None
        d["peer"] = call.peer
        secs = call.deadline_secs
        d["deadline"] = time.time() + secs if secs > 0 else None
        d["is_streaming"] = method.streaming
        d["pattern"] = method.pattern
        d["idempotent"] = method.idempotent
        d["attempt"] = 1
        d["_call"] = call
        d["_gate"] = gate

    def _lazy(self, key: str, make: Callable[[], Any]) -> Any:
        d = self.__dict__
        try:
            return d[key]
        except KeyError:
            v = d[key] = make()
            return v

    def _snapshot(self) -> tuple[Any, Any, Any]:
        def make() -> tuple[Any, Any, Any]:
            conn = self.__dict__["_call"].connection()
            if conn is None:
                return None, None, None
            try:
                snap = conn.transport_snapshot()
            except Exception:  # noqa: BLE001
                return None, None, None
            return snap.peer_addr, snap.relay_url, snap.rtt_micros
        return self._lazy("_snap", make)

    @property  # type: ignore[override]
    def metadata(self) -> dict[str, str]:
        call = self.__dict__["_call"]
        return self._lazy(
            "_metadata",
            lambda: _validated_metadata(call.metadata()) if call.has_metadata() else {},
        )

    @metadata.setter
    def metadata(self, value: dict[str, str]) -> None:
        self.__dict__["_metadata"] = value

    @property  # type: ignore[override]
    def attributes(self) -> dict[str, str]:
        def make() -> dict[str, str]:
            gate = self.__dict__["_gate"]
            peer = self.__dict__["peer"]
            attrs = self.__dict__["_call"].attributes()
            if gate is not None and peer:
                attrs.update(gate.attributes(peer))
            return attrs
        return self._lazy("_attributes", make)

    @attributes.setter
    def attributes(self, value: dict[str, str]) -> None:
        self.__dict__["_attributes"] = value

    @property  # type: ignore[override]
    def tunnel(self) -> Any:
        def make() -> Any:
            conn = self.__dict__["_call"].connection()
            if conn is None or not hasattr(conn, "authorize_tunnel"):
                return None
            from aster.tunnel import TunnelHandle
            return TunnelHandle(conn)
        return self._lazy("_tunnel", make)

    @tunnel.setter
    def tunnel(self, value: Any) -> None:
        self.__dict__["_tunnel"] = value

    @property  # type: ignore[override]
    def peer_addr(self) -> Any:
        return self.__dict__.get("_peer_addr", self._snapshot()[0])

    @peer_addr.setter
    def peer_addr(self, value: Any) -> None:
        self.__dict__["_peer_addr"] = value

    @property  # type: ignore[override]
    def relay_url(self) -> Any:
        return self.__dict__.get("_relay_url", self._snapshot()[1])

    @relay_url.setter
    def relay_url(self, value: Any) -> None:
        self.__dict__["_relay_url"] = value

    @property  # type: ignore[override]
    def rtt_micros(self) -> Any:
        return self.__dict__.get("_rtt_micros", self._snapshot()[2])

    @rtt_micros.setter
    def rtt_micros(self, value: Any) -> None:
        self.__dict__["_rtt_micros"] = value

    def __repr__(self) -> str:
        return (
            f"ServerCallContext(service={self.service!r}, method={self.method!r}, "
            f"call_id={self.call_id!r}, peer={self.peer!r}, pattern={self.pattern!r})"
        )


class _Service:
    __slots__ = ("name", "version", "info", "instance", "cls", "session", "interceptors", "methods", "sessions")

    def __init__(self, svc: Any, base_interceptors: list[Any]) -> None:
        info = service_info_of(svc)
        self.info = info
        self.name = info.name
        self.version = info.version
        self.session = info.scoped in ("session", "stream")
        self.cls = svc if inspect.isclass(svc) else type(svc)
        self.instance = None if inspect.isclass(svc) else svc
        if not self.session and self.instance is None:
            raise ValueError(
                f"No implementation instance provided for service {info.name} v{info.version}"
            )
        resolved = list(base_interceptors)
        for item in info.interceptors:
            resolved.append(item() if inspect.isclass(item) else item)
        self.interceptors = resolved
        # session_key -> per-session instance (session-scoped services only).
        self.sessions: dict[int, Any] = {}
        self.methods = [_Method(self, info.methods[n]) for n in method_names(info)]


class _Method:
    __slots__ = ("service", "name", "pattern", "streaming", "idempotent", "request_type", "accepts_ctx", "run")

    def __init__(self, service: _Service, mi: Any) -> None:
        self.service = service
        self.name = mi.name
        self.pattern = mi.pattern
        self.streaming = mi.pattern != "unary"
        self.idempotent = mi.idempotent
        self.request_type = mi.request_type
        self.accepts_ctx = mi.accepts_ctx
        self.run = None  # bound by the Dispatcher


class Dispatcher:
    """Serves ``services`` on a node through ``DispatchServer``.

    Args:
        services: ``@service`` instances (shared) or classes (session-scoped;
            instantiated per session as ``cls(peer=...)``).
        codec: the ``ForyCodec`` used for every payload.
        interceptors: server interceptors (service-level ones are appended).
        gate: the node's ``GatePolicy``; its admission attributes feed
            ``CallContext.attributes`` and, when any service declares
            ``requires``, Rust's Gate 3.
        on_non_rpc: ``callback(alpn, connection)`` for connections on other
            ALPNs (e.g. the frozen consumer-admission handler).
    """

    def __init__(
        self,
        services: list[Any],
        *,
        codec: Any,
        interceptors: list[Any] | None = None,
        gate: Any = None,
        on_non_rpc: Callable[[bytes, Any], None] | None = None,
        max_sessions_per_connection: int | None = None,
        handler_timeout: float | None = None,
        node_info: tuple[str, list[str], list[str]] | None = ("", [], []),
    ) -> None:
        base = list(interceptors or [])
        self._services = [_Service(s, base) for s in services]
        self._codec = codec
        self._gate = gate
        self._on_non_rpc = on_non_rpc
        self._max_sessions = max_sessions_per_connection
        self._handler_timeout = handler_timeout
        self._node_info = node_info
        runners = {
            "unary": self._unary,
            "server_stream": self._server_stream,
            "client_stream": self._client_stream,
            "bidi_stream": self._bidi_stream,
        }
        self._table: list[list[_Method]] = []
        for svc in self._services:
            for m in svc.methods:
                m.run = runners.get(m.pattern, self._unary)
            self._table.append(svc.methods)
        self._tasks: dict[int, asyncio.Task] = {}
        # Mirror of Rust's per-connection session accounting, from the
        # NEW_SESSION / SessionClosed / ConnectionClosed events (read by the
        # test-only `AsterServer.debug_connection_snapshot`):
        # connection_id -> [active session keys, last opened session id].
        self._conn_sessions: dict[int, list[Any]] = {}
        self._session_conn: dict[int, int] = {}
        self._server: DispatchServer | None = None
        self._loop: asyncio.AbstractEventLoop | None = None

    # -- lifecycle ------------------------------------------------------------

    def start(self, node: Any) -> DispatchServer:
        """Register the services on ``node`` (started with ``aster/1``) and
        start serving. Calls queue in Rust until :meth:`run` drains them."""
        if self._server is not None:
            return self._server
        any_requires = any(
            s.info.requires is not None or any(mi.requires is not None for mi in s.info.methods.values())
            for s in self._services
        )
        self._server = DispatchServer.start(
            node,
            [lower_json(s.cls) for s in self._services],
            node_info=self._node_info,
            gate=self._gate,
            gate_attributes=bool(self._gate is not None and any_requires),
            max_sessions_per_connection=self._max_sessions,
            handler_timeout=self._handler_timeout,
        )
        return self._server

    @property
    def server(self) -> DispatchServer | None:
        return self._server

    @property
    def in_flight(self) -> int:
        return len(self._tasks)

    async def run(self) -> None:
        """Drain events until the server stops (or this task is cancelled)."""
        assert self._server is not None, "start() first"
        srv = self._server
        self._loop = loop = asyncio.get_running_loop()
        on_call = self._on_call
        on_event = self._on_event
        call_type = ServerCall
        while True:
            batch = srv.poll_batch(_BATCH)
            if batch is None:
                return
            if not batch:
                batch = await srv.next_batch(_BATCH)
                if batch is None:
                    return
            for ev in batch:
                if type(ev) is call_type:
                    on_call(ev, loop)
                else:
                    on_event(ev)
            # Let the handlers just started run before taking more.
            await asyncio.sleep(0)

    def close(self) -> None:
        if self._server is not None:
            self._server.close()
        for task in list(self._tasks.values()):
            task.cancel()

    async def drain(self, grace_period: float) -> None:
        deadline = time.monotonic() + grace_period
        while self._tasks and time.monotonic() < deadline:
            await asyncio.sleep(0.05)
        for task in list(self._tasks.values()):
            task.cancel()

    # -- events ---------------------------------------------------------------

    def _on_call(self, call: ServerCall, loop: asyncio.AbstractEventLoop) -> None:
        m = self._table[call.service_idx][call.method_idx]
        coro = m.run(call, m)
        if _EAGER:
            task = asyncio.Task(coro, loop=loop, eager_start=True)
            if task.done():
                return
        else:
            task = loop.create_task(coro)
        self._tasks[call.call_id] = task

    def _on_event(self, ev: tuple) -> None:
        kind = ev[0]
        if kind == EV_CANCELLED:
            task = self._tasks.get(ev[1])
            if task is not None:
                task.cancel()
        elif kind == EV_SESSION_CLOSED:
            self._close_session(ev[1])
        elif kind == EV_NON_RPC:
            _, alpn, conn = ev
            if self._on_non_rpc is not None:
                try:
                    self._on_non_rpc(alpn, conn)
                except Exception as e:  # noqa: BLE001
                    logger.warning("non-RPC connection handler failed: %s", e)
            else:
                try:
                    conn.close(0, b"unexpected alpn")
                except Exception:  # noqa: BLE001
                    pass
        elif kind == EV_CONNECTION_CLOSED:
            self._conn_sessions.pop(ev[1], None)
            logger.debug("connection closed: connection_id=%d peer=%s", ev[1], ev[2])
        # EV_RESPONSE_WRITABLE / EV_RESPONSE_CLOSED: this binding waits with
        # `wait_writable` instead of the event-driven reserve.

    def connection_snapshot(self) -> dict[int, dict[str, int]]:
        """``{connection_id: {"active_session_count", "last_opened_session_id"}}``."""
        return {
            cid: {"active_session_count": len(st[0]), "last_opened_session_id": st[1]}
            for cid, st in self._conn_sessions.items()
        }

    def _close_session(self, session_key: int) -> None:
        cid = self._session_conn.pop(session_key, None)
        if cid is not None:
            st = self._conn_sessions.get(cid)
            if st is not None:
                st[0].discard(session_key)
        for svc in self._services:
            instance = svc.sessions.pop(session_key, None)
            if instance is None:
                continue
            close_hook = getattr(instance, "close", None)
            if close_hook is None:
                return
            try:
                result = close_hook()
                if asyncio.iscoroutine(result):
                    asyncio.ensure_future(result)
            except Exception as e:  # noqa: BLE001
                logger.warning("Session close() raised for %s: %s", svc.cls.__name__, e)
            return

    def _instance(self, call: ServerCall, svc: _Service) -> Any:
        if not svc.session:
            return svc.instance
        key = call.session_key
        inst = svc.sessions.get(key)
        if inst is None:
            # NEW_SESSION, or a key we never saw: create it.
            inst = svc.cls(peer=call.peer)
            svc.sessions[key] = inst
            cid = call.connection_id
            st = self._conn_sessions.get(cid)
            if st is None:
                st = self._conn_sessions[cid] = [set(), 0]
            st[0].add(key)
            st[1] = max(st[1], call.session_id)
            self._session_conn[key] = cid
        return inst

    # -- helpers --------------------------------------------------------------

    def _decode(self, payload: bytes, tp: Any) -> Any:
        return self._codec.decode(payload, tp)

    async def _send(self, call: ServerCall, payload: bytes) -> bool:
        """One stream frame; waits out a full queue. False if the call is gone."""
        r = call.send(payload)
        while r > 0:
            if not await call.wait_writable(r):
                return False
            r = call.send(payload)
        return r == 0

    async def _respond(self, call: ServerCall, payload: bytes) -> None:
        r = call.respond(payload)
        while r > 0:
            if not await call.wait_writable(r):
                return
            r = call.respond(payload)

    @staticmethod
    def _fail(call: ServerCall, code: int, message: str, details: dict[str, str] | None = None) -> None:
        try:
            call.finish(int(code), message, details or None)
        except Exception as e:  # noqa: BLE001
            logger.error("failed to send error status: %s", e)

    def _fail_exc(self, call: ServerCall, exc: BaseException, where: str) -> None:
        if isinstance(exc, RpcError):
            self._fail(call, exc.code, exc.message, exc.details)
        else:
            logger.error("%s handler error: %s", where, exc)
            self._fail(call, _INTERNAL, str(exc))

    async def _requests(self, call: ServerCall, m: _Method, ctx: Any, interceptors: list[Any]) -> AsyncIterator[Any]:
        """The request stream of a client-stream / bidi call: the first
        payload, then batches pulled from Rust until the end."""
        decode = self._codec.decode
        tp = m.request_type
        count = 1
        item = decode(call.request, tp)
        if interceptors:
            item = await apply_request_interceptors(interceptors, ctx, item)
        yield item
        while True:
            batch = await call.recv_batch(_BATCH)
            if not batch:
                return
            for payload in batch:
                count += 1
                if count > MAX_CLIENT_STREAM_ITEMS:
                    raise RpcError(
                        StatusCode.RESOURCE_EXHAUSTED,
                        f"client stream exceeded {MAX_CLIENT_STREAM_ITEMS} items",
                    )
                item = decode(payload, tp)
                if interceptors:
                    item = await apply_request_interceptors(interceptors, ctx, item)
                yield item

    # -- patterns -------------------------------------------------------------

    async def _unary(self, call: ServerCall, m: _Method) -> None:
        try:
            svc = m.service
            try:
                request = self._codec.decode(call.request, m.request_type)
            except Exception as e:  # noqa: BLE001
                self._fail(call, _INVALID_ARGUMENT, f"request does not decode: {e}")
                return
            ctx = ServerCallContext(call, m, self._gate)
            interceptors = svc.interceptors
            if interceptors:
                request = await apply_request_interceptors(interceptors, ctx, request)
            handler = getattr(self._instance(call, svc), m.name)
            scope = _CallScope(call)
            scope.__enter__()
            token = CallContext._current.set(ctx)
            try:
                result = handler(request, ctx) if m.accepts_ctx else handler(request)
                if inspect.isawaitable(result):
                    result = await result
            finally:
                CallContext._current.reset(token)
                scope.__exit__()
            if interceptors:
                result = await apply_response_interceptors(interceptors, ctx, result)
            await self._respond(call, self._codec.encode(result))
        except asyncio.CancelledError:
            raise
        except BaseException as e:  # noqa: BLE001
            self._fail_exc(call, e, "unary")
        finally:
            self._tasks.pop(call.call_id, None)

    async def _server_stream(self, call: ServerCall, m: _Method) -> None:
        svc = m.service
        ctx = ServerCallContext(call, m, self._gate)
        interceptors = svc.interceptors
        token = None
        try:
            with _CallScope(call):
                if interceptors:
                    await apply_request_interceptors(interceptors, ctx, None)
                try:
                    request = self._codec.decode(call.request, m.request_type)
                except Exception as e:  # noqa: BLE001
                    self._fail(call, _INVALID_ARGUMENT, f"request does not decode: {e}")
                    return
                if interceptors:
                    request = await apply_request_interceptors(interceptors, ctx, request)
                handler = getattr(self._instance(call, svc), m.name)
                token = CallContext._current.set(ctx)
                responses = handler(request, ctx) if m.accepts_ctx else handler(request)
                if inspect.isawaitable(responses):
                    responses = await responses
                encode = self._codec.encode
                async for item in responses:
                    if interceptors:
                        item = await apply_response_interceptors(interceptors, ctx, item)
                    if not await self._send(call, encode(item)):
                        return
                call.finish()
        except asyncio.CancelledError:
            raise
        except BaseException as e:  # noqa: BLE001
            await self._stream_error(call, ctx, interceptors, e, "server stream")
        finally:
            if token is not None:
                CallContext._current.reset(token)
            self._tasks.pop(call.call_id, None)

    async def _client_stream(self, call: ServerCall, m: _Method) -> None:
        svc = m.service
        ctx = ServerCallContext(call, m, self._gate)
        interceptors = svc.interceptors
        token = None
        try:
            with _CallScope(call):
                if interceptors:
                    await apply_request_interceptors(interceptors, ctx, None)
                requests = self._requests(call, m, ctx, interceptors)
                handler = getattr(self._instance(call, svc), m.name)
                token = CallContext._current.set(ctx)
                result = handler(requests, ctx) if m.accepts_ctx else handler(requests)
                if inspect.isawaitable(result):
                    result = await result
                if interceptors:
                    result = await apply_response_interceptors(interceptors, ctx, result)
                await self._respond(call, self._codec.encode(result))
        except asyncio.CancelledError:
            raise
        except BaseException as e:  # noqa: BLE001
            await self._stream_error(call, ctx, interceptors, e, "client stream")
        finally:
            if token is not None:
                CallContext._current.reset(token)
            self._tasks.pop(call.call_id, None)

    async def _bidi_stream(self, call: ServerCall, m: _Method) -> None:
        svc = m.service
        ctx = ServerCallContext(call, m, self._gate)
        interceptors = svc.interceptors
        token = None
        try:
            with _CallScope(call):
                if interceptors:
                    await apply_request_interceptors(interceptors, ctx, None)
                requests = self._requests(call, m, ctx, interceptors)
                handler = getattr(self._instance(call, svc), m.name)
                token = CallContext._current.set(ctx)
                responses = handler(requests, ctx) if m.accepts_ctx else handler(requests)
                if inspect.isawaitable(responses):
                    responses = await responses
                encode = self._codec.encode
                async for item in responses:
                    if interceptors:
                        item = await apply_response_interceptors(interceptors, ctx, item)
                    if not await self._send(call, encode(item)):
                        return
                call.finish()
        except asyncio.CancelledError:
            raise
        except BaseException as e:  # noqa: BLE001
            await self._stream_error(call, ctx, interceptors, e, "bidi stream")
        finally:
            if token is not None:
                CallContext._current.reset(token)
            self._tasks.pop(call.call_id, None)

    async def _stream_error(
        self, call: ServerCall, ctx: Any, interceptors: list[Any], exc: BaseException, where: str
    ) -> None:
        if not isinstance(exc, Exception):
            self._fail(call, _INTERNAL, str(exc))
            return
        if not isinstance(exc, RpcError):
            logger.error("%s handler error: %s", where, exc)
        error = exc if isinstance(exc, RpcError) else RpcError(StatusCode.INTERNAL, str(exc))
        if interceptors:
            try:
                maybe = await apply_error_interceptors(interceptors, ctx, error)
            except Exception as e:  # noqa: BLE001
                maybe = normalize_error(e)
            if maybe is None:
                # Suppressed by an interceptor: end the call cleanly.
                self._fail(call, 0, "")
                return
            error = maybe
        self._fail(call, error.code, error.message, error.details)
