"""
aster.tracing -- spans and trace context over the Rust OpenTelemetry pipeline.

Spans you start here live in the Rust ``SdkTracer`` and are exported with the
RPC server and client spans Aster creates itself, under the same resource and
sampling decision. Without a tracer provider (:func:`aster.observability.init`
with traces on) :func:`span` is a cheap no-op.

The current trace context rides a ``contextvars`` carrier, so it follows
``await`` and ``asyncio`` tasks. Inside an RPC handler the carrier holds the
server span of the call, so your spans nest under it and outbound Aster calls
continue the trace (a ``traceparent`` is written into the call metadata for
you).

Usage::

    from aster import tracing

    with tracing.span("db.query", {"db.system": "postgresql"}) as s:
        s.add_event("cache.miss")
        rows = await fetch()

    @tracing.traced("orders.price")
    async def price(order): ...

Logs: :class:`aster.logging.LoggingFilter`, ``structlog_processor`` and
``loguru_patcher`` add :func:`current_trace_id` / :func:`current_span_id` to
your own records.
"""

from __future__ import annotations

import contextvars
import functools
import inspect
from typing import Any, Callable, Mapping
from urllib.parse import quote, unquote

from . import _aster

__all__ = [
    "span",
    "traced",
    "current_traceparent",
    "current_trace_id",
    "current_span_id",
    "current_baggage",
    "with_trace_context",
    "inject",
    "SpanKind",
]


class SpanKind:
    INTERNAL = 0
    SERVER = 1
    CLIENT = 2
    PRODUCER = 3
    CONSUMER = 4


_KINDS = {
    "internal": 0, "server": 1, "client": 2, "producer": 3, "consumer": 4,
}

# (traceparent, baggage) of the current context, or None. The traceparent is
# None for a call that carries inbound baggage but whose server span is not
# exported.
_carrier: contextvars.ContextVar[tuple[str | None, Mapping[str, str]] | None] = contextvars.ContextVar(
    "aster_trace_context", default=None
)

_NO_BAGGAGE: Mapping[str, str] = {}

# W3C baggage limits (Rust enforces them when it strips the header; this is
# for the pass-through path, where the header stays in metadata).
_BAGGAGE_MAX_BYTES = 8192
_BAGGAGE_MAX_MEMBERS = 64


def current_traceparent() -> str | None:
    """The current W3C ``traceparent`` (``00-<trace>-<span>-<flags>``), or None."""
    c = _carrier.get()
    return c[0] if c else None


def current_trace_id() -> str | None:
    """The current trace id (32 lowercase hex characters), or None."""
    c = _carrier.get()
    return c[0][3:35] if c and c[0] else None


def current_span_id() -> str | None:
    """The current span id (16 lowercase hex characters), or None."""
    c = _carrier.get()
    return c[0][36:52] if c and c[0] else None


def current_baggage() -> dict[str, str]:
    """A copy of the current W3C baggage."""
    c = _carrier.get()
    return dict(c[1]) if c else {}


class with_trace_context:
    """Make ``traceparent`` (and optional ``baggage``) the current context.

    Use it to continue a trace that arrived by other means (an HTTP header, a
    message queue). An invalid ``traceparent`` raises ``ValueError``.
    """

    def __init__(self, traceparent: str, baggage: Mapping[str, str] | None = None) -> None:
        tp = _aster.traceparent_normalize(traceparent)
        if tp is None:
            raise ValueError("invalid traceparent")
        self._value = (tp, dict(baggage) if baggage else _NO_BAGGAGE)
        self._token: contextvars.Token | None = None

    def __enter__(self) -> "with_trace_context":
        self._token = _carrier.set(self._value)
        return self

    def __exit__(self, *exc: Any) -> None:
        _reset(self._token)
        self._token = None


def _reset(token: contextvars.Token | None) -> None:
    if token is None:
        return
    try:
        _carrier.reset(token)
    except ValueError:  # reset from another context (async generator teardown)
        pass


def _format_baggage(b: Mapping[str, str]) -> str:
    return ",".join(f"{quote(str(k), safe='')}={quote(str(v), safe='')}" for k, v in b.items())


def _parse_baggage(value: str | None) -> Mapping[str, str]:
    if not value:
        return _NO_BAGGAGE
    out: dict[str, str] = {}
    for member in value.split(","):
        pair = member.split(";", 1)[0]
        k, sep, v = pair.partition("=")
        if sep and k.strip():
            out[unquote(k.strip())] = unquote(v.strip())
    return out


def inject(metadata: Mapping[str, str] | None) -> Mapping[str, str] | None:
    """``metadata`` plus the current ``traceparent`` / ``baggage`` (the same
    object when there is no current context or the caller set its own).
    Aster's client calls this for every outbound call."""
    c = _carrier.get()
    if c is None or (metadata and "traceparent" in metadata):
        return metadata
    out = dict(metadata) if metadata else {}
    if c[0]:
        out["traceparent"] = c[0]
    if c[1] and "baggage" not in out:
        out["baggage"] = _format_baggage(c[1])
    return out


class _CallScope:
    """The server span of a dispatched call as the current context (used by
    ``aster._dispatch``)."""

    __slots__ = ("_call", "_token")

    def __init__(self, call: Any) -> None:
        self._call = call
        self._token: contextvars.Token | None = None

    def __enter__(self) -> None:
        call = self._call
        tp = call.trace_parent
        # Inbound baggage comes off the call (Rust strips the reserved keys
        # from metadata), whether or not the call's span is exported.
        raw = call.baggage
        if raw is None and tp is None and call.has_metadata():
            # No tracing pipeline: trace keys pass through as plain metadata.
            raw = call.metadata().get("baggage")
            if raw and (len(raw) > _BAGGAGE_MAX_BYTES or raw.count(",") >= _BAGGAGE_MAX_MEMBERS):
                raw = None
        baggage = _parse_baggage(raw) if raw else _NO_BAGGAGE
        if tp is None and not baggage:
            if _carrier.get() is None:
                return
            value = None
        else:
            value = (tp, baggage)
        self._token = _carrier.set(value)

    def __exit__(self, *exc: Any) -> None:
        _reset(self._token)
        self._token = None


def _strs(attrs: Mapping[str, Any] | None) -> list[tuple[str, str]] | None:
    if not attrs:
        return None
    return [(str(k), ("true" if v is True else "false" if v is False else str(v))) for k, v in attrs.items()]


class span:
    """A span. Works as ``with`` and ``async with``; the span ends on exit and
    an exception escaping the block is recorded on it (status error).

    Args:
        name: Span name.
        attrs: Initial attributes (values are stringified).
        kind: ``"internal"`` (default), ``"server"``, ``"client"``,
            ``"producer"`` or ``"consumer"``.
        parent: A ``traceparent`` to parent under instead of the current context.
    """

    __slots__ = ("name", "_attrs", "_kind", "_parent", "_handle", "_token")

    def __init__(
        self,
        name: str,
        attrs: Mapping[str, Any] | None = None,
        *,
        kind: str | int = "internal",
        parent: str | None = None,
    ) -> None:
        self.name = name
        self._attrs = attrs
        self._kind = _KINDS[kind] if isinstance(kind, str) else int(kind)
        self._parent = parent
        self._handle = 0
        self._token: contextvars.Token | None = None

    def start(self) -> "span":
        """Start the span without a ``with`` block; pair with :meth:`end`."""
        c = _carrier.get()
        parent = self._parent or (c[0] if c else None)
        try:
            self._handle = _aster.span_start(self.name, self._kind, parent, _strs(self._attrs))
        except RuntimeError:  # live-span table full: degrade to a no-op
            self._handle = 0
            return self
        tp = _aster.span_traceparent(self._handle)
        if tp is not None:
            self._token = _carrier.set((tp, c[1] if c else _NO_BAGGAGE))
        return self

    @property
    def traceparent(self) -> str | None:
        """This span's ``traceparent``, or None for a no-op span."""
        if not self._handle:
            return None
        try:
            return _aster.span_traceparent(self._handle)
        except LookupError:
            return None

    @property
    def trace_id(self) -> str | None:
        tp = self.traceparent
        return tp[3:35] if tp else None

    @property
    def span_id(self) -> str | None:
        tp = self.traceparent
        return tp[36:52] if tp else None

    def _call(self, fn: Callable[..., None], *args: Any) -> None:
        if self._handle:
            try:
                fn(self._handle, *args)
            except LookupError:  # ended, or the pipeline was shut down
                self._handle = 0

    def set_attribute(self, key: str, value: Any) -> None:
        self.set_attributes({key: value})

    def set_attributes(self, attrs: Mapping[str, Any]) -> None:
        self._call(_aster.span_set_attributes, _strs(attrs) or [])

    def add_event(self, name: str, attrs: Mapping[str, Any] | None = None) -> None:
        self._call(_aster.span_add_event, name, _strs(attrs))

    def set_status(self, ok: bool, message: str = "") -> None:
        self._call(_aster.span_set_status, 1 if ok else 2, message)

    def record_exception(self, exc: BaseException) -> None:
        self._call(_aster.span_record_error, type(exc).__name__, str(exc))

    def end(self) -> None:
        """End the span (idempotent) and restore the previous context."""
        _reset(self._token)
        self._token = None
        h, self._handle = self._handle, 0
        if h:
            try:
                _aster.span_end(h)
            except LookupError:
                pass

    def __enter__(self) -> "span":
        return self.start()

    def __exit__(self, exc_type: Any, exc: BaseException | None, tb: Any) -> None:
        if exc is not None:
            self.record_exception(exc)
        self.end()

    async def __aenter__(self) -> "span":
        return self.start()

    async def __aexit__(self, exc_type: Any, exc: BaseException | None, tb: Any) -> None:
        self.__exit__(exc_type, exc, tb)

    def __del__(self) -> None:  # a span left open is ended, not leaked
        try:
            if self._handle:
                _aster.span_end(self._handle)
        except Exception:  # noqa: BLE001
            pass


def traced(name: str | None = None, attrs: Mapping[str, Any] | None = None, *, kind: str = "internal") -> Callable:
    """Decorator: run the function (sync or async) inside a span."""

    def deco(fn: Callable) -> Callable:
        span_name = name or f"{fn.__module__}.{fn.__qualname__}"
        if inspect.iscoroutinefunction(fn):

            @functools.wraps(fn)
            async def awrapper(*a: Any, **k: Any) -> Any:
                with span(span_name, attrs, kind=kind):
                    return await fn(*a, **k)

            return awrapper

        @functools.wraps(fn)
        def wrapper(*a: Any, **k: Any) -> Any:
            with span(span_name, attrs, kind=kind):
                return fn(*a, **k)

        return wrapper

    return deco
