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
    resp = await relay_http_stream(conn, "app", "PUT", "/", body=chunks())  # streamed
    async for chunk in resp.body: ...
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
consumer goes away before the head, the handler task is cancelled; so are
in-flight handlers and streamed bodies when the upcall runtime closes (your
iterator sees ``CancelledError`` or ``aclose()``; ``aclose()`` of the runtime
waits a bounded time, then abandons an iterator that ignores it).
``expose_http(max_chunk=...)`` (default 1 MiB) sets the largest body chunk:
reads return at most that many bytes and ``write`` splits bigger chunks.
``HEAD`` responses keep their headers and drop the body. Trailers do not
cross the relay hop yet (``aster-expose`` V2).

**Streaming consumer** (:func:`relay_http_stream`): the request ``body``
may be ``bytes`` / ``str`` or an (async) iterable of chunks, uploaded with
backpressure; the response's ``body`` is an :class:`HttpRelayBody` to
``await read()``, ``async for`` over, or ``await response.read_body()``.
The exchange is full duplex: the call returns as soon as the response head
is in -- possibly before the body is uploaded (an early ``413``, or a
handler that streams back as it reads) -- and the upload carries on in the
background until it is done, the handler stops reading, or you close the
response body before its end (which resets an unfinished upload; a
response that has ended cleanly leaves the upload to finish). If the body iterator
raises, the request is reset (the handler sees a failed body, never a
truncated one); before the head the iterator's exception propagates from
the call, after it the error is logged (``aster.expose``). A response that fails partway raises ``HttpBodyError``
(``read``) from the read, never a clean end. Close the body early with
``aclose()`` / ``async with resp.body:`` and the handler's further writes
are cancelled. Because the body is an async iterable, a handler can proxy
by returning the relayed :class:`HttpResponse` as its own.
:func:`relay_http_streaming` is the lower-level form: it returns the
:class:`HttpUpload` to write yourself plus the response awaitable.

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
    HttpRelayBody,
    HttpUpload,
    NodeId,
    RouteRegistration,
    open_relay,
    relay_http,
    relay_http_streaming,
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
    "HttpRelayBody",
    "HttpRequest",
    "HttpResponse",
    "RoutePolicy",
    "RouteRegistration",
    "RouteSpec",
    "HttpUpload",
    "open_relay",
    "relay_http",
    "relay_http_stream",
    "relay_http_streaming",
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

    async def read_body(self, limit: Optional[int] = None) -> bytes:
        """The whole body: ``bytes`` / ``str`` as is, an
        :class:`HttpRelayBody` (or any async / sync iterable of chunks)
        drained. ``limit``: raise ``HttpBodyError`` (``too_large``) past
        that many bytes (a relayed body is closed first)."""
        body = self.body
        if body is None:
            return b""
        if isinstance(body, str):
            body = body.encode()
        if isinstance(body, (bytes, bytearray, memoryview)):
            data = bytes(body)
            if limit is not None and len(data) > limit:
                raise HttpBodyError(f"body exceeds {limit} bytes", "too_large", 3)
            return data
        out = bytearray()
        chunks = body if hasattr(body, "__aiter__") else _aiter_sync(body)
        try:
            async for chunk in chunks:
                out += chunk.encode() if isinstance(chunk, str) else chunk
                if limit is not None and len(out) > limit:
                    raise HttpBodyError(f"body exceeds {limit} bytes", "too_large", 3)
        except HttpBodyError:
            close = getattr(body, "aclose", None)
            if close is not None:
                await close()
            raise
        return bytes(out)


async def _aiter_sync(it):
    for chunk in it:
        yield chunk


async def relay_http_stream(
    conn,
    service_id: str,
    method: str = "GET",
    uri: str = "/",
    headers: Any = None,
    body: Body = None,
    *,
    max_chunk: int = 1 << 20,
    capacity: int = 8,
) -> HttpResponse:
    """Consumer side: one HTTP exchange with ``service_id`` on ``conn``,
    streamed both ways (see the module docs). ``body``: ``bytes`` / ``str``
    / ``None`` or an (async) iterable of chunks. Returns an
    :class:`HttpResponse` whose ``body`` is an :class:`HttpRelayBody`.
    ``max_chunk`` bounds each upload piece and each read; ``capacity`` is
    the number of upload chunks buffered before a write waits."""
    upload, pending = relay_http_streaming(
        conn, service_id, method, uri, headers, max_chunk=max_chunk, capacity=capacity
    )
    pump = asyncio.ensure_future(_pump_upload(upload, body))
    try:
        response = await pending
    except BaseException:
        # The upload's own failure (the caller's iterator raising) is the
        # better error than the reset relay it caused.
        if not pump.done():
            pump.cancel()
        try:
            await pump
        except asyncio.CancelledError:
            pass
        raise
    # Full duplex: the head can arrive while the upload is still going. It
    # carries on in the background (held here: asyncio keeps only weak
    # references to tasks); closing the response body stops it.
    if pump.done():
        pump.result()
    else:
        _PUMPS.add(pump)
        pump.add_done_callback(_pump_done)
    return response


#: Upload pumps still running after their response head arrived.
_PUMPS: "set[asyncio.Future]" = set()


def _pump_done(task: "asyncio.Future") -> None:
    _PUMPS.discard(task)
    if not task.cancelled() and task.exception() is not None:
        _log.warning("request body failed after the response head: %r", task.exception())


async def _pump_upload(upload: HttpUpload, body: Body) -> None:
    """Write ``body`` through ``upload``, then finish; abort on any
    failure (including cancellation) so the request is reset, never
    finished short."""
    try:
        if body is None:
            pass
        elif isinstance(body, (bytes, bytearray, memoryview, str)):
            await upload.write(body)
        elif hasattr(body, "__aiter__"):
            async for chunk in body:
                await upload.write(chunk)
        else:
            for chunk in body:
                await upload.write(chunk)
        upload.finish()
    except HttpBodyError as exc:
        if exc.kind == "cancelled":
            # The request is gone; the response awaitable reports why.
            _abort_upload(upload, str(exc))
            return
        _abort_upload(upload, str(exc))
        raise
    except BaseException as exc:
        _abort_upload(upload, f"{type(exc).__name__}: {exc}")
        raise
    finally:
        close = getattr(body, "aclose", None)
        if close is not None:
            try:
                await close()
            except Exception:  # pragma: no cover - best effort
                pass


def _abort_upload(upload: HttpUpload, message: str) -> None:
    try:
        upload.abort(message)
    except HttpBodyError:
        pass


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
