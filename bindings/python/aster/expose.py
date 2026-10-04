"""Expose local services to peers over Aster (Rust ``aster::expose``), with
the Python halves answered by upcall (plan R6 / R9; design
``docs/_internal/bindings/upcall-design.md`` section 6).

Three target shapes on a node's :class:`Expose` registry::

    expose = node.expose()

    # 1. A local TCP socket (raw L4 splice; your own server, any protocol).
    expose.expose_socket("api", "127.0.0.1:8080", authorize=OnlyTeam())

    # 2. A Python HTTP handler, one upcall per request.
    class Hello(HttpHandler):
        async def handle(self, request):
            body = await request.read_body()
            return HttpResponse(200, [("content-type", "text/plain")], b"hi " + body)

    expose.expose_http("app", Hello(), authorize=OnlyTeam())

    # 3. Act as an edge: peers register routes, a Python policy decides.
    class Mine(RoutePolicy):
        async def grant(self, peer, metadata, specs):
            return [s for s in specs if s.host.endswith(".example")]

    router = EdgeRouter()
    expose.serve_routes(router, Mine())

    await expose.serve([b"aster-expose/api"])     # or expose.handle(conn)

Consumer side, on a connection to that ALPN::

    send, recv = await open_relay(conn, "api", metadata=b"token")   # socket target
    resp = await relay_http(conn, "app", "POST", "/", body=b"you")  # HTTP target
    reg = await request_route(conn, [RouteSpec("a.example", 443, service_id="app")])

**authorize** runs once per ``(connection, service)`` (kind
``expose_authorize``): ``True`` admits, ``False`` refuses, and no answer
(exception, timeout) applies the kind's failure mode -- fail-closed unless
configured otherwise (``aster.upcalls``). A plain callable works too.

**HTTP handlers** (kind ``expose_http``). ``request.body`` is ``bytes`` when
the request declared a ``content-length`` of at most ``inline_request_max``
(64 KiB), else an :class:`HttpBodyStream` to ``await read()`` or ``async
for`` over (``await request.read_body()`` covers both). Return an
:class:`HttpResponse` (or ``bytes`` / ``str`` for a ``200``) whose ``body``
is ``bytes`` / ``str`` (sent inline with the head) or an (async) iterable of
chunks (streamed after the head with backpressure, then finished with
``trailers``). The kind's timeout (default 5 s) bounds the time to the
response *head* only; raise it for long-polling handlers. A handler that
raises answers ``500`` (the message is logged, never sent), a timeout
``504``, an unusable value ``502``; an exception while streaming aborts the
response (the consumer sees a reset, not a truncated body). When the
consumer goes away before the head, the handler task is cancelled.
``HEAD`` responses keep their headers and drop the body. Trailers do not
cross the relay hop yet (``aster-expose`` V2).

**Route policies** (kind ``expose_route_policy``): ``grant(peer, metadata,
specs)`` returns the granted :class:`RouteSpec` objects or their indexes
(``True``: all, ``False``: none). No usable answer applies the failure mode:
fail-closed grants nothing, fail-open grants every requested route.
"""

from __future__ import annotations

import abc
import asyncio
import inspect
import logging
from dataclasses import dataclass, field
from typing import (
    TYPE_CHECKING,
    Any,
    AsyncIterable,
    Iterable,
    Literal,
    Optional,
    Sequence,
    Tuple,
    Union,
)

from ._aster import (
    EdgeRouter,
    Expose,
    HttpBodyStream,
    NodeId,
    RouteRegistration,
    open_relay,
    relay_http,
    request_route,
)

if TYPE_CHECKING:  # pragma: no cover
    from .upcalls import UpcallRuntime

__all__ = [
    "EdgeRouter",
    "Expose",
    "ExposeAuthorizer",
    "HttpBodyError",
    "HttpBodyStream",
    "HttpHandler",
    "HttpRequest",
    "HttpResponse",
    "RoutePolicy",
    "RouteRegistration",
    "RouteSpec",
    "open_relay",
    "relay_http",
    "request_route",
]

_log = logging.getLogger("aster.expose")

#: ``(name, value)``; names are lowercase, values raw bytes.
Header = Tuple[str, bytes]
HttpVersion = Literal["HTTP/0.9", "HTTP/1.0", "HTTP/1.1", "HTTP/2", "HTTP/3"]
Body = Union[bytes, bytearray, memoryview, str, AsyncIterable, Iterable, None]


class HttpBodyError(Exception):
    """A body operation failed. ``kind``: ``unknown_stream``, ``cancelled``
    (the consumer went away), ``too_large``, ``chunk_too_large``,
    ``finished``, ``timeout``, ``read``, ``invalid_trailers``,
    ``blocking_on_runtime``; ``code`` is the stable ABI code."""

    def __init__(self, message: str, kind: str = "read", code: int = 0) -> None:
        super().__init__(message)
        self.kind = kind
        self.code = code


@dataclass(frozen=True)
class RouteSpec:
    """A route an edge binds for a peer: ``host`` (``""`` = any) and
    ``port`` (``0`` = the listener's default) to match, carried as
    ``protocol`` to the origin's exposed ``service_id``."""

    host: str
    port: int = 0
    protocol: Literal["http", "tcp"] = "http"
    service_id: str = ""


@dataclass(frozen=True)
class HttpRequest:
    """One request for an :class:`HttpHandler`. ``peer`` is the
    transport-authenticated node (``None`` off the relay -- refuse it if you
    attribute requests). ``body``: ``bytes`` (inline) or the
    :class:`HttpBodyStream` to read it from. ``handle`` is the exchange's
    body handle either way."""

    service_id: str
    peer: Optional[NodeId]
    stream_id: int
    version: HttpVersion
    method: str
    uri: str
    headers: Tuple[Header, ...]
    body: Union[bytes, HttpBodyStream]
    handle: HttpBodyStream = field(repr=False, compare=False, default=None)  # type: ignore[assignment]

    def header(self, name: str) -> Optional[bytes]:
        """The first value of header ``name`` (case-insensitive), or
        ``None``."""
        name = name.lower()
        for k, v in self.headers:
            if k == name:
                return v
        return None

    async def read_body(self, limit: Optional[int] = None) -> bytes:
        """The whole request body. ``limit``: raise ``HttpBodyError``
        (``too_large``) past that many bytes."""
        if isinstance(self.body, (bytes, bytearray)):
            data = bytes(self.body)
            if limit is not None and len(data) > limit:
                raise HttpBodyError(f"body exceeds {limit} bytes", "too_large", 3)
            return data
        out = bytearray()
        async for chunk in self.body:
            out += chunk
            if limit is not None and len(out) > limit:
                raise HttpBodyError(f"body exceeds {limit} bytes", "too_large", 3)
        return bytes(out)


@dataclass
class HttpResponse:
    """A handler's answer. ``headers``: ``(name, value)`` pairs or a
    mapping (``str`` or ``bytes`` values; names are lowercased). ``body``:
    ``bytes`` / ``str`` (inline) or an (async) iterable of chunks (streamed,
    then finished with ``trailers``). A decoded streamed response (from
    :func:`aster.upcalls.ucv1_decode`) has ``body`` ``None``."""

    status: int = 200
    headers: Any = ()
    body: Body = b""
    trailers: Any = ()

    def header(self, name: str) -> Optional[bytes]:
        name = name.lower()
        for k, v in self.headers:
            if k == name:
                return v if isinstance(v, bytes) else str(v).encode()
        return None


class ExposeAuthorizer(abc.ABC):
    """Per-``(connection, service)`` admission for an exposed service."""

    upcall_runtime: Optional["UpcallRuntime"] = None

    @abc.abstractmethod
    async def authorize(self, service_id: str, peer: NodeId, metadata: bytes) -> bool:
        """``peer`` is the transport-authenticated node id; ``metadata`` the
        consumer's opaque open-frame bytes (empty if none)."""


class HttpHandler(abc.ABC):
    """A Python HTTP service for ``Expose.expose_http``."""

    upcall_runtime: Optional["UpcallRuntime"] = None

    @abc.abstractmethod
    async def handle(self, request: HttpRequest) -> HttpResponse:
        """Answer one request (see the module docs)."""


class RoutePolicy(abc.ABC):
    """An edge's admission hook for ``Expose.serve_routes``."""

    upcall_runtime: Optional["UpcallRuntime"] = None

    @abc.abstractmethod
    async def grant(
        self, peer: NodeId, metadata: bytes, specs: Tuple[RouteSpec, ...]
    ) -> Union[bool, Iterable[Union[int, RouteSpec]]]:
        """The specs (or indexes into ``specs``) to grant; ``True`` grants
        all, ``False`` none."""


# -- upcall glue (used by aster.upcalls) ---------------------------------------------


def _callable(obj, name: str):
    fn = getattr(obj, name, None)
    if callable(fn):
        return fn
    return obj if callable(obj) else None


async def _await(value):
    if inspect.isawaitable(value):
        return await value
    return value


def _http_entry(obj):
    fn = _callable(obj, "handle")
    if fn is None:
        return None

    async def serve(request: HttpRequest) -> HttpResponse:
        resp = await _await(fn(request))
        if isinstance(resp, HttpResponse):
            return resp
        if isinstance(resp, (bytes, bytearray, memoryview, str)):
            return HttpResponse(200, (), resp)
        if isinstance(resp, int) and not isinstance(resp, bool):
            return HttpResponse(resp, (), b"")
        raise TypeError(
            f"an HTTP handler must return HttpResponse, bytes or str, not {type(resp).__name__}"
        )

    return serve


def _route_entry(obj):
    fn = _callable(obj, "grant")
    if fn is None:
        return None

    async def grant(peer, metadata, specs):
        answer = await _await(fn(peer, metadata, specs))
        if answer is True:
            return list(range(len(specs)))
        if answer is False or answer is None:
            return []
        out = set()
        for g in answer:
            if isinstance(g, RouteSpec):
                out.add(specs.index(g))  # ValueError: not requested
            elif isinstance(g, bool) or not isinstance(g, int):
                raise TypeError(f"grant() must yield RouteSpec or int, not {type(g).__name__}")
            else:
                out.add(g)
        return sorted(out)

    return grant


async def _stream_response(response: HttpResponse, request: HttpRequest) -> None:
    """After a streamed response head went out: write ``response.body``
    through the exchange's handle, then finish (or abort on failure)."""
    body = response.body
    if isinstance(body, (bytes, bytearray, memoryview, str)):
        return
    handle = request.handle
    try:
        if body is not None:
            if hasattr(body, "__aiter__"):
                async for chunk in body:
                    await handle.write(chunk)
            else:
                for chunk in body:
                    await handle.write(chunk)
        handle.finish(response.trailers or None)
    except HttpBodyError as exc:
        # The consumer went away (or the exchange is over): nothing to tell.
        _log.debug("streamed response on %s ended: %s", request.service_id, exc)
        if exc.kind not in ("cancelled", "unknown_stream", "finished"):
            _abort(handle, str(exc))
    except asyncio.CancelledError:
        _abort(handle, "cancelled")
        raise
    except Exception as exc:
        _log.warning("HTTP handler for %s failed mid-body: %r", request.service_id, exc)
        _abort(handle, f"{type(exc).__name__}: {exc}")
    finally:
        close = getattr(body, "aclose", None)
        if close is not None:
            try:
                await close()
            except Exception:  # pragma: no cover - best effort
                pass


def _abort(handle: HttpBodyStream, message: str) -> None:
    try:
        handle.abort(message)
    except HttpBodyError:
        pass
