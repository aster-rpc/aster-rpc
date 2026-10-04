"""Upcalls -- Rust asking Python a question (plan R6; design
``docs/_internal/bindings/upcall-design.md``).

Several Rust extension points are implemented in Python: a lease serializer
backend (:class:`aster.lease.LeaseSerializer`), a fenced resource
(:class:`aster.lease.FencedResource`), a lease authorizer
(``MemorySerializer.set_authorizer``), the topology ``admitted`` filter
(:class:`aster.topology.AdmittedFilter`), the placement manager's providers
(:class:`aster.coordination.PlacementManager`), the expose authorize hook
(``Expose.expose_socket(..., authorize=...)``), expose HTTP handlers and
route policies (``Expose.expose_http`` / ``Expose.serve_routes``, see
:mod:`aster.expose`) and Gate 0 decisions (``node.serve_admission(handler)``).
They all run the same way: Rust emits a typed event, an asyncio *pump* task decodes it (in Rust, into the usual
``aster`` dataclasses), runs your method as its own task, and answers by id.
You never see bytes.

An :class:`UpcallRuntime` owns the pump and the per-kind policies. Each event
loop gets a default one on first use (:func:`get_runtime`); make your own to
change policies before binding anything::

    rt = UpcallRuntime(default_policy=UpcallPolicy(timeout=2.0))
    rt.set_policy("lease_authorize", UpcallPolicy(timeout=0.5, failure_mode="fail_open"))
    set_runtime(rt)                    # the current loop's default from now on
    ...
    await rt.aclose()                  # pending upcalls fail; decisions fall back

or pin one object to a runtime with an ``upcall_runtime`` attribute.

**Policies.** Each kind has a timeout (default 5 s) and, for the *decision*
kinds (``lease_authorize``, ``topology_admitted``, ``expose_authorize``,
``expose_route_policy``, ``admission_connect``, ``admission_handshake``), a
failure mode applied when
there is no usable answer -- an exception, a timeout, a full queue, a closed
runtime, a value of the wrong type: ``"fail_closed"`` (default: deny) or
``"fail_open"`` (allow). Every other kind surfaces the failure to its Rust
caller, which raises :class:`UpcallError` here (a serializer fault fails the
acquire; a provider failure stops the placement pass; a fenced mutation with
no answer raises :class:`FenceOutcomeUnknownError` -- the write may or may
not have happened; an HTTP handler with no answer becomes a ``5xx``).

**Cancellation.** When Rust stops waiting (timeout, or its caller gave up),
the handler's task is cancelled (``asyncio.CancelledError`` inside your
coroutine); a late answer is dropped.

**Handlers** may be ``async def`` or plain functions (a plain function runs
on the event loop thread -- keep it quick). Handler methods run on the loop
that bound them; keep that loop running while Rust may ask.
"""

from __future__ import annotations

import abc
import asyncio
import inspect
import logging
import weakref
from dataclasses import dataclass
from typing import Any, Callable, Dict, Literal, Optional, Tuple

from ._aster import _UpcallRuntime, _upcall_decode, _upcall_encode, _upcall_kinds

__all__ = [
    "AdmissionHandler",
    "DECISION_KINDS",
    "FailureMode",
    "FenceOutcomeUnknownError",
    "UPCALL_KINDS",
    "UpcallError",
    "UpcallErrorKind",
    "UpcallPolicy",
    "UpcallRuntime",
    "get_runtime",
    "set_runtime",
    "ucv1_decode",
    "ucv1_encode",
]

_log = logging.getLogger("aster.upcalls")

#: What happens to a decision with no usable answer.
FailureMode = Literal["fail_open", "fail_closed"]
#: Why an upcall produced no answer.
UpcallErrorKind = Literal[
    "timeout", "overloaded", "closed", "foreign", "unimplemented", "decode"
]

#: Kind name -> ABI code (``0`` is the ``cancel`` control event).
UPCALL_KINDS: Dict[str, int] = {name: code for code, name, _ in _upcall_kinds()}
#: The yes/no kinds a :data:`FailureMode` applies to.
DECISION_KINDS = frozenset(name for _, name, decision in _upcall_kinds() if decision)


@dataclass(frozen=True)
class UpcallPolicy:
    """Timeout (seconds) and, for decision kinds, the failure mode."""

    timeout: float = 5.0
    failure_mode: FailureMode = "fail_closed"


class UpcallError(Exception):
    """An upcall-backed Rust operation got no answer from Python.
    ``kind``: ``timeout``, ``overloaded`` (queue full), ``closed`` (runtime
    closed), ``foreign`` (the handler raised), ``unimplemented`` (no handler
    method) or ``decode`` (the handler's value did not encode)."""

    def __init__(self, message: str, kind: UpcallErrorKind = "foreign") -> None:
        super().__init__(message)
        self.kind = kind


class FenceOutcomeUnknownError(UpcallError):
    """A fenced mutation through Rust got no answer: the write **may or may
    not** have been applied. Re-read the resource before retrying."""


def _kind(kind) -> str:
    if isinstance(kind, int):
        for name, code in UPCALL_KINDS.items():
            if code == kind:
                return name
    if kind not in UPCALL_KINDS or kind == "cancel":
        raise ValueError(f"unknown upcall kind {kind!r}")
    return kind


def ucv1_decode(message_type: str, data: bytes) -> Any:
    """Decode UCv1 bytes as ``message_type`` (``"LeaseSnapshot"``, ...) into
    the Python value a handler receives or returns. Raises ``ValueError``."""
    return _upcall_decode(message_type, data)


def ucv1_encode(message_type: str, value: Any) -> bytes:
    """Encode a Python value as UCv1 ``message_type``. Raises
    ``ValueError``."""
    return _upcall_encode(message_type, value)


class AdmissionHandler(abc.ABC):
    """Gate 0 decided in Python over the upcall channel
    (``node.serve_admission(handler)``, an alternative to ``async for req in
    node.admission()``). Override :meth:`handshake` (inbound); left out, it
    answers ``unimplemented`` and the ``admission_handshake`` failure mode
    decides (fail-closed: rejected with ``403 "hook unavailable"``).
    :meth:`connect` (this node's own dials) allows by default."""

    upcall_runtime: Optional["UpcallRuntime"] = None

    async def connect(self, peer, alpn: bytes) -> bool:
        """Outbound: may this node dial ``peer`` on ``alpn``? Default: yes."""
        return True

    async def handshake(self, peer, alpn: bytes, is_alive: bool):  # pragma: no cover
        """Inbound, after the handshake: ``True`` accepts, ``False`` rejects
        with ``403 "peer not admitted"``, ``(code, reason)`` rejects with
        those."""
        raise NotImplementedError


# -- Handler tables per role -----------------------------------------------------


def _method(obj, name: str) -> Optional[Callable]:
    fn = getattr(obj, name, None)
    return fn if callable(fn) else None


def _overridden(obj, name: str) -> Optional[Callable]:
    # An AdmissionHandler method left at its base definition is "no handler".
    base = getattr(AdmissionHandler, name, None)
    impl = getattr(type(obj), name, None)
    if base is not None and impl is base:
        return None
    return _method(obj, name)


def _authorize_fn(obj) -> Optional[Callable]:
    return _method(obj, "authorize") or (obj if callable(obj) else None)


def _handlers(obj, role: str) -> Dict[str, Callable]:
    """``kind -> callable(*args)`` for an object bound in ``role``. A kind
    without a callable answers ``unimplemented``."""
    if role == "lease_serializer":
        table = {
            "lease_transition": _method(obj, "transition"),
            "lease_snapshot": _method(obj, "snapshot"),
        }
    elif role == "lease_authorizer":
        table = {"lease_authorize": _authorize_fn(obj)}
    elif role == "fenced_resource":
        table = {"fenced_mutate": _method(obj, "mutate")}
    elif role == "topology_admitted":
        table = {"topology_admitted": _method(obj, "_admitted_batch")}
    elif role == "placement":
        table = {
            "topology_snapshot": _method(obj, "_topology_snapshot"),
            "placement_policy": _method(obj, "_placement_policy"),
            "placement_provision": _method(obj, "_provision"),
        }
    elif role == "expose_authorize":
        table = {"expose_authorize": _authorize_fn(obj)}
    elif role == "expose_http":
        from .expose import _http_entry

        table = {"expose_http": _http_entry(obj)}
    elif role == "expose_route_policy":
        from .expose import _route_entry

        table = {"expose_route_policy": _route_entry(obj)}
    elif role == "admission":
        table = {
            "admission_connect": _method(obj, "connect"),
            "admission_handshake": _overridden(obj, "handshake"),
        }
    else:
        raise ValueError(f"unknown upcall role {role!r}")
    return {k: v for k, v in table.items() if v is not None}


def _describe(exc: BaseException) -> str:
    text = str(exc)
    return f"{type(exc).__name__}: {text}" if text else type(exc).__name__


# -- The runtime ---------------------------------------------------------------------


class UpcallRuntime:
    """The pump plus per-kind policies. One per event loop is the norm
    (:func:`get_runtime`); it starts its pump task on first use and serves
    on that loop. ``capacity`` bounds each bound object's queue (a full one
    refuses: ``overloaded``); ``batch`` is how many events one wake drains.
    """

    def __init__(
        self,
        *,
        capacity: int = 1024,
        default_policy: Optional[UpcallPolicy] = None,
        policies: Optional[Dict[str, UpcallPolicy]] = None,
        batch: int = 64,
    ) -> None:
        self._native = _UpcallRuntime(capacity)
        self._batch = max(1, int(batch))
        self._handlers: Dict[int, Dict[str, Callable]] = {}
        # (id(obj), role) -> (obj, channel); the strong ref keeps id() stable
        # and the handler alive while Rust may still ask it.
        self._bound: Dict[Tuple[int, str], Tuple[Any, int]] = {}
        self._tasks: Dict[Tuple[int, int], asyncio.Task] = {}
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._pump: Optional[asyncio.Task] = None
        if default_policy is not None:
            self.set_default_policy(default_policy)
        for kind, policy in (policies or {}).items():
            self.set_policy(kind, policy)

    # -- policies --

    def set_policy(self, kind, policy: UpcallPolicy) -> None:
        """Set ``kind``'s policy (a kind name such as ``"lease_authorize"``,
        or its code), for upcalls issued from now on."""
        self._native.set_policy(_kind(kind), float(policy.timeout), policy.failure_mode)

    def set_default_policy(self, policy: UpcallPolicy) -> None:
        """The policy of kinds without their own."""
        self._native.set_default_policy(float(policy.timeout), policy.failure_mode)

    def policy(self, kind) -> UpcallPolicy:
        timeout, mode = self._native.policy(_kind(kind))
        return UpcallPolicy(timeout=timeout, failure_mode=mode)

    # -- state --

    @property
    def pending(self) -> int:
        """Upcalls Rust is waiting on."""
        return self._native.pending()

    @property
    def closed(self) -> bool:
        return self._native.closed

    @property
    def running(self) -> bool:
        return self._pump is not None and not self._pump.done()

    def start(self) -> None:
        """Start the pump on the running loop (idempotent; binding an object
        does it)."""
        if self.closed:
            raise UpcallError("the upcall runtime is closed", "closed")
        loop = asyncio.get_running_loop()
        if self._pump is not None and not self._pump.done():
            if self._loop is not loop:
                raise RuntimeError("this UpcallRuntime serves another event loop")
            return
        self._loop = loop
        self._pump = loop.create_task(self._run_pump(), name="aster-upcall-pump")

    def close(self) -> None:
        """Stop: pending upcalls fail (decisions fall back to their failure
        mode), running handlers are cancelled, later upcalls fail at once.
        Idempotent."""
        self._native.close()

    async def aclose(self) -> None:
        """:meth:`close`, then wait for the pump to finish."""
        self.close()
        pump = self._pump
        if pump is not None and not pump.done() and pump is not asyncio.current_task():
            try:
                await pump
            except asyncio.CancelledError:
                pass

    async def __aenter__(self) -> "UpcallRuntime":
        self.start()
        return self

    async def __aexit__(self, *exc) -> None:
        await self.aclose()

    def __repr__(self) -> str:
        return (
            f"UpcallRuntime(channels={self._native.channel_count()}, "
            f"pending={self.pending}, running={self.running}, closed={self.closed})"
        )

    # -- binding --

    def _bind(self, obj, role: str) -> Tuple[_UpcallRuntime, int]:
        key = (id(obj), role)
        hit = self._bound.get(key)
        if hit is not None and hit[0] is obj:
            self.start()
            return self._native, hit[1]
        handlers = _handlers(obj, role)
        self.start()
        channel = self._native.open_channel()
        self._handlers[channel] = handlers
        self._bound[key] = (obj, channel)
        return self._native, channel

    def unbind(self, obj) -> None:
        """Forget every binding of ``obj``: Rust adapters built on it get
        ``closed`` from now on (decisions fall back)."""
        for key in [k for k, v in self._bound.items() if v[0] is obj]:
            _, channel = self._bound.pop(key)
            self._handlers.pop(channel, None)
            self._native.close_channel(channel)

    # -- the pump --

    async def _run_pump(self) -> None:
        native = self._native
        loop = asyncio.get_running_loop()
        try:
            while True:
                alive = await native.ready()
                for channel, iid, kind, _deadline, args in native.drain(self._batch):
                    if kind == "cancel":
                        task = self._tasks.pop((channel, iid), None)
                        if task is not None:
                            task.cancel()
                        continue
                    fn = self._handlers.get(channel, {}).get(kind)
                    if fn is None:
                        native.respond_unimplemented(channel, iid)
                        continue
                    self._tasks[(channel, iid)] = loop.create_task(
                        self._serve(channel, iid, kind, fn, args)
                    )
                if not alive:
                    return
        finally:
            native.close()
            for task in list(self._tasks.values()):
                task.cancel()
            self._tasks.clear()

    async def _serve(self, channel: int, iid: int, kind: str, fn: Callable, args) -> None:
        native = self._native
        try:
            try:
                value = fn(*args)
                if inspect.isawaitable(value):
                    value = await value
            except asyncio.CancelledError:
                return
            except Exception as exc:  # the handler's own failure
                if kind == "fenced_mutate" and _is_fence_rejection(exc):
                    value = exc
                else:
                    _log.debug("upcall %s handler failed: %r", kind, exc)
                    native.respond_error(channel, iid, _describe(exc))
                    return
        finally:
            self._tasks.pop((channel, iid), None)
        try:
            delivered = native.respond(channel, iid, kind, value)
        except Exception as exc:
            native.respond_error(
                channel, iid, f"{kind}: the handler returned an unusable value: {exc}"
            )
            return
        if kind == "expose_http" and delivered:
            # Respond first, then write: a streamed body follows the head.
            from .expose import _stream_response

            await _stream_response(value, args[0])


def _is_fence_rejection(exc: BaseException) -> bool:
    from .lease import FenceRejectedError

    return isinstance(exc, FenceRejectedError)


# -- The per-loop default -----------------------------------------------------------

_RUNTIMES: "weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, UpcallRuntime]" = (
    weakref.WeakKeyDictionary()
)


def _running_loop() -> asyncio.AbstractEventLoop:
    try:
        return asyncio.get_running_loop()
    except RuntimeError:
        raise RuntimeError(
            "Python upcall handlers need a running asyncio event loop: create and "
            "bind them from async code"
        ) from None


def get_runtime() -> UpcallRuntime:
    """The running loop's default :class:`UpcallRuntime` (created on first
    use; a closed one is replaced)."""
    loop = _running_loop()
    rt = _RUNTIMES.get(loop)
    if rt is None or rt.closed:
        rt = UpcallRuntime()
        _RUNTIMES[loop] = rt
    return rt


def set_runtime(runtime: UpcallRuntime) -> None:
    """Make ``runtime`` the running loop's default (for objects bound from
    now on)."""
    _RUNTIMES[_running_loop()] = runtime


def _bind(obj, role: str):
    """Called by the native module when a Rust adapter is built on ``obj``:
    ``(native runtime, channel id)``."""
    rt = getattr(obj, "upcall_runtime", None) or get_runtime()
    return rt._bind(obj, role)
