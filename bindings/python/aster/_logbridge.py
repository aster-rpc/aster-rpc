"""
aster._logbridge -- deliver Rust log records to stdlib ``logging``.

The Rust core emits ``tracing`` events (connection, admission, dispatch, ...)
into a bounded, drop-counting queue (``aster_otel::bridge``). A Rust thread,
``aster-log-drain``, blocks on that queue without the GIL and calls
:func:`_emit` once per batch (up to 256 records). Each record becomes a
``logging.LogRecord`` on the logger named after the Rust target
(``aster::rpc::dispatch`` -> ``aster.rpc.dispatch``), after an
``isEnabledFor`` check, so ``structlog`` and ``loguru`` get it through their
stdlib integration.

Extra attributes on every bridged record:

- ``aster_origin`` = ``"rust"`` (the OTel log appender skips these: Rust
  already exported them);
- ``trace_id`` / ``span_id``: hex ids of the enclosing exported span, ``""``
  when the event was outside one;
- ``aster_call_id``, ``aster_audit``, and ``aster_fields`` (the event's
  structured fields); each field is also set as an attribute with ``.``
  replaced by ``_`` (``rpc_service``, ``rpc_method``, ``aster_peer``,
  ``thread_name``) unless that would shadow a ``LogRecord`` attribute.

Level sync: Rust filters by level before allocating anything, so its filter
must follow the Python logger. :func:`sync_level` copies the effective level
of the ``aster`` logger to Rust; call it after ``logging.config.dictConfig``
or ``basicConfig`` (no stdlib monkey-patching). The drain also re-checks every
5 s and calls it when the effective level changed, which catches loggers
changed directly. ``ASTER_LOG`` / an explicit ``log_filter`` pin the Rust
filter and disable the automatic re-check.
"""

from __future__ import annotations

import atexit
import logging
import os
import threading
from typing import Any

from . import _aster

TRACE = 5
logging.addLevelName(TRACE, "TRACE")

_lock = threading.Lock()
_registered = False
_last_level: int | None = None
_pinned = False

# Attributes a bridged record must not overwrite.
_RESERVED = frozenset(logging.LogRecord("x", 0, "", 0, "", (), None).__dict__) | {
    "message",
    "asctime",
}


def _directive(level: int) -> str:
    if level >= logging.CRITICAL:
        name = "error"
    elif level >= logging.ERROR:
        name = "error"
    elif level >= logging.WARNING:
        name = "warn"
    elif level >= logging.INFO:
        name = "info"
    elif level >= logging.DEBUG:
        name = "debug"
    else:
        name = "trace"
    return f"warn,aster={name}"


def effective_level() -> int:
    """The effective level of the ``aster`` logger."""
    return logging.getLogger("aster").getEffectiveLevel()


def initial_filter() -> str | None:
    """The Rust filter directives matching the Python logger, or ``None`` when
    ``ASTER_LOG`` / ``ASTER_LOG_LEVEL`` already say what Rust should do."""
    if os.environ.get("ASTER_LOG") or os.environ.get("ASTER_LOG_LEVEL"):
        return None
    return _directive(effective_level())


def pin(pinned: bool = True) -> None:
    """Pin the Rust filter (an explicit filter was given): no automatic re-check."""
    global _pinned
    _pinned = pinned


def is_pinned() -> bool:
    """Whether an explicit Rust filter (``ASTER_LOG`` or ``log_filter=``) pins it."""
    return _pinned


def sync_level() -> str | None:
    """Make Rust filter by the ``aster`` logger's effective level now.

    Returns the directives applied, or ``None`` when the bridge is not open.
    """
    global _last_level
    level = effective_level()
    directive = _directive(level)
    try:
        _aster.log_set_filter(directive)
    except LookupError:
        return None
    _last_level = level
    return directive


def _recheck() -> None:
    """Drain-thread tick: re-sync only when the Python level changed."""
    if _pinned:
        return
    level = effective_level()
    if level != _last_level:
        sync_level()


def _emit(batch: list[tuple]) -> None:
    """Called by the drain thread with the GIL held, once per batch."""
    get_logger = logging.getLogger
    for rec in batch:
        try:
            level, name, msg, ts_ns, flags, tid, sid, call_id, fields = rec
            logger = get_logger(name)
            if not logger.isEnabledFor(level):
                continue
            extra: dict[str, Any] = {
                "aster_origin": "rust",
                "trace_id": tid or "",
                "span_id": sid or "",
                "aster_call_id": call_id if flags & 2 else 0,
                "aster_audit": bool(flags & 8),
                "aster_fields": dict(fields),
            }
            for k, v in fields:
                attr = k.replace(".", "_")
                if attr not in _RESERVED and attr not in extra:
                    extra[attr] = v
            record = logger.makeRecord(
                name, level, "(rust)", 0, msg, (), None, "(rust)", extra
            )
            record.created = ts_ns / 1e9
            record.msecs = (ts_ns // 1_000_000) % 1000
            logger.handle(record)
        except Exception:  # noqa: BLE001 -- one bad record must not stop the batch
            pass


def start() -> bool:
    """Start the drain thread (idempotent). The bridge must be open
    (``observability.init(log_bridge=True)``)."""
    global _registered, _last_level
    with _lock:
        if not _registered:
            _registered = True
            atexit.register(stop)
        started = _aster.log_drain_start(_emit, _recheck)
        if started and _last_level is None:
            _last_level = effective_level()
        return started


def stop(timeout: float = 1.0) -> bool:
    """Stop the drain and join it (the ``atexit`` handler). Queued records are
    dropped, and no further attach is attempted once this returns."""
    return _aster.log_drain_stop(int(timeout * 1000))


def _after_fork_child() -> None:
    global _last_level, _lock
    _lock = threading.Lock()  # a parent thread may have held it at fork time
    _aster.observability_after_fork()
    _last_level = None


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_after_fork_child)
