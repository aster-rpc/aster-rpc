"""
aster.logging — Production-grade structured logging for the Aster RPC framework.

Provides:
  - JSON structured logging (default in production) or human-readable (dev)
  - Configurable via environment variables
  - Request correlation IDs via contextvars
  - Sensitive field masking
  - Standard fields: timestamp, level, logger, service, method, peer, duration

Configuration:
  ASTER_LOG_FORMAT   = "json" | "text"     (default: "text")
  ASTER_LOG_LEVEL    = "debug" | "info" | "warning" | "error"  (default: "info")
  ASTER_LOG_MASK     = "true" | "false"    (default: "true") — mask sensitive fields

Usage::

    from aster.logging import configure_logging, get_logger, request_context

    configure_logging()  # call once at startup
    logger = get_logger(__name__)

    with request_context(service="Hello", method="sayHello", peer="abc123"):
        logger.info("handling request", extra={"payload_size": 1024})
"""

from __future__ import annotations

import contextvars
import json
import logging
import os
import time
from typing import Any


# ── Context variables for request correlation ─────────────────────────────────

_request_id: contextvars.ContextVar[str] = contextvars.ContextVar("aster_request_id", default="")
_service_name: contextvars.ContextVar[str] = contextvars.ContextVar("aster_service", default="")
_method_name: contextvars.ContextVar[str] = contextvars.ContextVar("aster_method", default="")
_peer_id: contextvars.ContextVar[str] = contextvars.ContextVar("aster_peer", default="")


class request_context:
    """Context manager that sets correlation fields for the current async scope.

    Usage::

        with request_context(request_id="abc", service="Hello", method="sayHello"):
            logger.info("processing")
            # log output includes service, method, request_id automatically
    """

    def __init__(
        self,
        request_id: str = "",
        service: str = "",
        method: str = "",
        peer: str = "",
    ) -> None:
        self._tokens: list[Any] = []
        self._request_id = request_id
        self._service = service
        self._method = method
        self._peer = peer

    def __enter__(self) -> "request_context":
        if self._request_id:
            self._tokens.append(_request_id.set(self._request_id))
        if self._service:
            self._tokens.append(_service_name.set(self._service))
        if self._method:
            self._tokens.append(_method_name.set(self._method))
        if self._peer:
            self._tokens.append(_peer_id.set(self._peer))
        return self

    def __exit__(self, *args: Any) -> None:
        for token in reversed(self._tokens):
            # contextvars Token.reset() restores the previous value
            token.var.reset(token)
        self._tokens.clear()


def get_request_id() -> str:
    """Get the current request ID from context."""
    return _request_id.get()


def set_request_id(rid: str) -> None:
    """Set the current request ID in context."""
    _request_id.set(rid)


# ── Sensitive field masking ───────────────────────────────────────────────────

_SENSITIVE_FIELDS = frozenset({
    "secret_key", "private_key", "signing_key", "root_privkey",
    "signature", "credential_json", "iid_token", "enrollment_token",
    "password", "token", "auth",
})

_MASK_FIELDS = frozenset({
    "root_pubkey", "endpoint_id", "node_id", "peer",
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

        # Add correlation context
        rid = _request_id.get()
        if rid:
            entry["request_id"] = rid
        svc = _service_name.get()
        if svc:
            entry["service"] = svc
        method = _method_name.get()
        if method:
            entry["method"] = method
        peer = _peer_id.get()
        if peer:
            entry["peer"] = _mask_value("peer", peer)

        # Add extra fields from the log call
        for key in ("service", "method", "peer", "request_id", "duration_ms",
                     "status_code", "error", "payload_size", "stream_count",
                     "contract_id", "endpoint_id", "version"):
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

        12:00:00.123 INFO  aster.server — connection opened [service=Hello method=sayHello]
    """

    _LEVEL_COLORS = {
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
        rid = _request_id.get()
        if rid:
            ctx_parts.append(f"req={rid[:8]}")
        svc = _service_name.get()
        if svc:
            ctx_parts.append(f"svc={svc}")
        method = _method_name.get()
        if method:
            ctx_parts.append(f"method={method}")

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

    # Prevent propagation to root logger (avoids duplicate output)
    aster_logger.propagate = False


def get_logger(name: str) -> logging.Logger:
    """Get a logger for an Aster module.

    Use this instead of ``logging.getLogger(__name__)`` to ensure the logger
    is under the ``aster`` hierarchy and picks up the configured formatter.
    """
    return logging.getLogger(name)
