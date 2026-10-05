"""
aster.logging -- Production-grade structured logging for the Aster RPC framework.

Provides:
  - JSON structured logging (default in production) or human-readable (dev)
  - Configurable via environment variables
  - Correlation fields from the current RPC call (``CallContext``) and the
    current trace (``trace_id`` / ``span_id`` from ``aster.tracing``)
  - Sensitive field masking
  - Standard fields: timestamp, level, logger, service, method, peer, duration
  - Rust log records (``aster_origin="rust"``) arrive through the log bridge
    (``aster._logbridge``) as ordinary ``logging`` records
  - Helpers for your own records: ``LoggingFilter``, ``structlog_processor``,
    ``loguru_patcher``, and ``AsterLogHandler`` (OTLP export of application logs)

Configuration:
  ASTER_LOG_FORMAT   = "json" | "text"     (default: "text")
  ASTER_LOG_LEVEL    = "debug" | "info" | "warning" | "error"  (default: "info")
  ASTER_LOG_MASK     = "true" | "false"    (default: "true") -- mask sensitive fields

Usage::

    from aster.logging import configure_logging, get_logger

    configure_logging()  # call once at startup
    logger = get_logger(__name__)

    logger.info("handling request", extra={"payload_size": 1024})

After ``logging.config.dictConfig`` / ``basicConfig`` call
``aster.logging.sync_level()`` so Rust filters by the new ``aster`` level.
"""

from __future__ import annotations

import json
import logging
import os
import time
from typing import Any


# ── Correlation fields ────────────────────────────────────────────────────────


def _call_fields() -> tuple[str, str, str, str]:
    """``(request_id, service, method, peer)`` of the RPC being handled."""
    try:
        from aster.interceptors.base import CallContext
    except Exception:  # noqa: BLE001 -- import cycle during start-up
        return "", "", "", ""
    ctx = CallContext.current()
    if ctx is None:
        return "", "", "", ""
    return ctx.call_id or "", ctx.service or "", ctx.method or "", ctx.peer or ""


def _trace_fields(record: logging.LogRecord) -> tuple[str, str]:
    """``(trace_id, span_id)``: the record's own (a bridged Rust record carries
    the ids of the span it was emitted in), else the current trace context."""
    tid = getattr(record, "trace_id", None)
    if tid is not None:
        return tid, getattr(record, "span_id", "") or ""
    from aster import tracing

    return tracing.current_trace_id() or "", tracing.current_span_id() or ""


class LoggingFilter(logging.Filter):
    """Adds ``trace_id`` and ``span_id`` (``""`` outside a trace) to every record
    that passes, so any formatter can print ``%(trace_id)s``::

        handler.addFilter(aster.logging.LoggingFilter())
    """

    def filter(self, record: logging.LogRecord) -> bool:
        record.trace_id, record.span_id = _trace_fields(record)
        return True


def structlog_processor(logger: Any, method_name: str, event_dict: dict) -> dict:
    """``structlog`` processor adding ``trace_id`` / ``span_id`` when a trace is current."""
    from aster import tracing

    tid = tracing.current_trace_id()
    if tid:
        event_dict.setdefault("trace_id", tid)
        event_dict.setdefault("span_id", tracing.current_span_id())
    return event_dict


def loguru_patcher(record: dict) -> None:
    """``loguru`` patcher: ``logger.configure(patcher=aster.logging.loguru_patcher)``."""
    from aster import tracing

    record["extra"]["trace_id"] = tracing.current_trace_id() or ""
    record["extra"]["span_id"] = tracing.current_span_id() or ""


class AsterLogHandler(logging.Handler):
    """Export your application's log records through Rust's OTLP logs pipeline
    (``otlp_logs=True`` / ``OTEL_LOGS_EXPORTER=otlp``), with the current trace
    context. Records that came from Rust (``aster_origin="rust"``) are skipped:
    Rust already exported them. A no-op when no logs exporter is configured.
    """

    _LEVELS = ((40, 1), (30, 2), (20, 3), (10, 4), (0, 5))

    def emit(self, record: logging.LogRecord) -> None:
        if getattr(record, "aster_origin", None) == "rust":
            return
        try:
            from aster import _aster, tracing

            level = next(r for floor, r in self._LEVELS if record.levelno >= floor)
            fields = [
                ("code.namespace", record.name),
                ("code.function", record.funcName or ""),
            ]
            _aster.log_emit(
                level,
                record.name,
                record.getMessage(),
                fields,
                int(record.created * 1e9),
                tracing.current_traceparent(),
            )
        except Exception:  # noqa: BLE001
            self.handleError(record)


def sync_level() -> str | None:
    """Make the Rust log filter follow the ``aster`` logger's effective level.
    Call it after ``logging.config.dictConfig`` or ``basicConfig``. Returns the
    directives applied, or ``None`` when the log bridge is not open."""
    from aster import _logbridge

    return _logbridge.sync_level()


# ── Sensitive field masking ───────────────────────────────────────────────────

_SENSITIVE_FIELDS = frozenset({
    "secret_key", "private_key", "signing_key", "root_privkey",
    "signature", "credential_json", "iid_token", "enrollment_token",
    "password", "token", "auth",
})

_MASK_FIELDS = frozenset({
    "root_pubkey", "endpoint_id", "node_id", "peer", "aster_peer",
    "nonce", "contract_id",
})

_masking_enabled = True


def _mask_value(key: str, value: Any) -> Any:
    """Mask a value if it's a sensitive field."""
    if not _masking_enabled:
        return value
    if not isinstance(value, str):
        return value

    key_lower = key.lower()
    if key_lower in _SENSITIVE_FIELDS:
        return "***"
    if key_lower in _MASK_FIELDS and len(value) > 12:
        return value[:8] + "..." + value[-4:]
    return value


def mask_dict(d: dict[str, Any]) -> dict[str, Any]:
    """Mask sensitive fields in a dict for safe logging."""
    return {k: _mask_value(k, v) for k, v in d.items()}


# ── JSON structured formatter ─────────────────────────────────────────────────


class JsonFormatter(logging.Formatter):
    """Structured JSON log formatter following Kubernetes/ELK conventions.

    Output format::

        {"ts":"2026-04-06T12:00:00.123Z","level":"info","logger":"aster.server",
         "msg":"connection opened","service":"Hello","method":"sayHello",
         "request_id":"abc123","peer":"node1234...5678","duration_ms":42}
    """

    def format(self, record: logging.LogRecord) -> str:
        entry: dict[str, Any] = {
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(record.created))
            + f".{int(record.msecs):03d}Z",
            "level": record.levelname.lower(),
            "logger": record.name,
            "msg": record.getMessage(),
        }

        # Add correlation context: the call being handled, the current trace
        rid, svc, method, peer = _call_fields()
        if rid:
            entry["request_id"] = rid
        if svc:
            entry["service"] = svc
        if method:
            entry["method"] = method
        if peer:
            entry["peer"] = _mask_value("peer", peer)
        tid, sid = _trace_fields(record)
        if tid:
            entry["trace_id"] = tid
            entry["span_id"] = sid

        # Add extra fields from the log call (and from bridged Rust records)
        for key in ("service", "method", "peer", "request_id", "duration_ms",
                     "status_code", "error", "payload_size", "stream_count",
                     "contract_id", "endpoint_id", "version",
                     "rpc_service", "rpc_method", "aster_peer", "aster_call_id",
                     "aster_origin"):
            val = getattr(record, key, None)
            if val is not None and key not in entry:
                entry[key] = _mask_value(key, val)

        # Exception info
        if record.exc_info and record.exc_info[1]:
            entry["error"] = str(record.exc_info[1])
            entry["error_type"] = type(record.exc_info[1]).__name__

        return json.dumps(entry, default=str, separators=(",", ":"))


# ── Human-readable formatter ──────────────────────────────────────────────────


class TextFormatter(logging.Formatter):
    """Human-readable log formatter for development.

    Output format::

        12:00:00.123 INFO  aster.server -- connection opened [service=Hello method=sayHello]
    """

    _LEVEL_COLORS = {
        "TRACE": "\033[90m",    # grey
        "DEBUG": "\033[36m",    # cyan
        "INFO": "\033[32m",     # green
        "WARNING": "\033[33m",  # yellow
        "ERROR": "\033[31m",    # red
    }
    _RESET = "\033[0m"

    def __init__(self, use_color: bool = True) -> None:
        super().__init__()
        self._use_color = use_color

    def format(self, record: logging.LogRecord) -> str:
        ts = time.strftime("%H:%M:%S", time.localtime(record.created))
        ts += f".{int(record.msecs):03d}"

        level = record.levelname
        if self._use_color:
            color = self._LEVEL_COLORS.get(level, "")
            level_str = f"{color}{level:5s}{self._RESET}"
        else:
            level_str = f"{level:5s}"

        # Shorten logger name
        name = record.name
        if name.startswith("aster."):
            name = name[6:]

        msg = record.getMessage()

        # Build context suffix
        ctx_parts: list[str] = []
        rid, svc, method, _peer = _call_fields()
        if rid:
            ctx_parts.append(f"req={rid[:8]}")
        if svc:
            ctx_parts.append(f"svc={svc}")
        if method:
            ctx_parts.append(f"method={method}")
        tid, _sid = _trace_fields(record)
        if tid:
            ctx_parts.append(f"trace={tid[:8]}")

        for key in ("duration_ms", "status_code", "payload_size"):
            val = getattr(record, key, None)
            if val is not None:
                ctx_parts.append(f"{key}={val}")

        ctx = f" [{' '.join(ctx_parts)}]" if ctx_parts else ""

        line = f"{ts} {level_str} {name} \u2014 {msg}{ctx}"

        if record.exc_info and record.exc_info[1]:
            line += f"\n  {type(record.exc_info[1]).__name__}: {record.exc_info[1]}"

        return line


# ── Configuration ─────────────────────────────────────────────────────────────


def configure_logging(
    format: str | None = None,
    level: str | None = None,
    mask: bool | None = None,
) -> None:
    """Configure Aster logging. Call once at startup.

    Args:
        format: "json" or "text". Default: ASTER_LOG_FORMAT env or "text".
        level: "debug", "info", "warning", "error". Default: ASTER_LOG_LEVEL env or "info".
        mask: Whether to mask sensitive fields. Default: ASTER_LOG_MASK env or True.
    """
    global _masking_enabled

    fmt = format or os.environ.get("ASTER_LOG_FORMAT", "text").lower()
    lvl = level or os.environ.get("ASTER_LOG_LEVEL", "info").lower()
    if mask is not None:
        _masking_enabled = mask
    else:
        _masking_enabled = os.environ.get("ASTER_LOG_MASK", "true").lower() != "false"

    # Map level string
    level_map = {
        "trace": 5,
        "debug": logging.DEBUG,
        "info": logging.INFO,
        "warning": logging.WARNING,
        "warn": logging.WARNING,
        "error": logging.ERROR,
    }
    log_level = level_map.get(lvl, logging.INFO)

    # Create formatter
    if fmt == "json":
        formatter = JsonFormatter()
    else:
        use_color = os.isatty(2)  # color if stderr is a terminal
        formatter = TextFormatter(use_color=use_color)

    # Configure the root aster logger
    aster_logger = logging.getLogger("aster")
    aster_logger.setLevel(log_level)

    # Remove existing handlers to avoid duplicates on re-configure
    aster_logger.handlers.clear()

    handler = logging.StreamHandler()
    handler.setFormatter(formatter)
    aster_logger.addHandler(handler)
    # Rust filters by level before allocating: keep it in step (unless an
    # explicit Rust filter was given).
    from aster import _logbridge

    if not _logbridge.is_pinned():
        _logbridge.sync_level()

    # Prevent propagation to root logger (avoids duplicate output)
    aster_logger.propagate = False


def get_logger(name: str) -> logging.Logger:
    """Get a logger for an Aster module.

    Use this instead of ``logging.getLogger(__name__)`` to ensure the logger
    is under the ``aster`` hierarchy and picks up the configured formatter.
    """
    return logging.getLogger(name)
