"""
aster.observability -- metrics, traces and logs over the Rust pipeline.

The whole pipeline lives in Rust (``aster-otel``): the OpenTelemetry SDK, the
exporters, the ``tracing`` subscriber, the Prometheus renderer and the log
bridge. This module only starts and stops it and hands Rust log records to
``logging``. Configuration is parsed once, in Rust. Precedence, highest first:
the arguments to :func:`init`, then ``OTEL_*`` / ``ASTER_*`` environment
variables, then the ``[observability]`` TOML table, then defaults.

Without any call, the first ``AsterServer`` / ``AsterClient`` start runs
:func:`ensure_started`: the log bridge opens (Rust logs appear in ``logging``)
and exporters start only when ``OTEL_*`` asks for them. To configure the
pipeline yourself, call :func:`init` before that::

    import aster
    from aster import observability

    observability.init(service_name="orders", otlp_endpoint="https://otel.example.com:4318")

A configuration that differs from the running pipeline's raises ``ValueError``
naming the differing settings (the SDK's globals are per process). Forking
after ``init`` is unsupported: the child's telemetry is disabled with one
warning, so initialise in each worker, not in a pre-fork master.

Metrics and spans: :mod:`aster.metrics` and :mod:`aster.tracing`. Health and
Prometheus scrape endpoints: :class:`aster.HealthServer`.
"""

from __future__ import annotations

import atexit
import logging
import os
import threading
from typing import Mapping, Sequence

from . import _aster, _logbridge

logger = logging.getLogger(__name__)

__all__ = ["init", "shutdown", "flush", "ensure_started", "is_initialized", "metrics_text"]

_lock = threading.Lock()
_auto = False
_atexit_registered = False


def is_initialized() -> bool:
    """Whether this module holds a reference to the running pipeline."""
    return _aster.observability_refs() > 0


def _pairs(m: Mapping[str, str] | Sequence[tuple[str, str]] | None) -> list[tuple[str, str]] | None:
    if m is None:
        return None
    items = m.items() if isinstance(m, Mapping) else m
    return [(str(k), str(v)) for k, v in items]


def init(
    *,
    service_name: str | None = None,
    otlp_endpoint: str | None = None,
    otlp_headers: Mapping[str, str] | None = None,
    otlp_protocol: str | None = None,
    resource_attributes: Mapping[str, str] | None = None,
    traces: bool | None = None,
    metrics: bool | None = None,
    otlp_logs: bool | None = None,
    sample_ratio: float | None = None,
    log_filter: str | None = None,
    log_queue: int | None = None,
    log_bridge: bool = True,
    from_env: bool = True,
    config_toml: str | None = None,
) -> None:
    """Start the observability pipeline, or join the running one.

    Args:
        service_name: ``service.name`` resource attribute.
        otlp_endpoint: OTLP/HTTP collector base URL. Plain ``http://`` to a
            non-loopback host logs a warning.
        otlp_headers: Request headers (often a bearer token). Never logged.
        otlp_protocol: ``"http/protobuf"`` (the only protocol that runs today).
        resource_attributes: Extra resource attributes.
        traces / metrics / otlp_logs: ``True`` exports that signal over OTLP,
            ``False`` turns it off, ``None`` lets ``OTEL_*`` decide (traces
            default to OTLP, metrics only with an endpoint, logs off).
        sample_ratio: Parent-based trace-id ratio sampler in ``[0, 1]``.
        log_filter: ``EnvFilter`` directives for Rust events (``ASTER_LOG``),
            e.g. ``"warn,aster=debug"``. Pins the Rust filter; without it Rust
            follows the ``aster`` logger's level (:func:`aster.logging.sync_level`).
        log_queue: Bound of the Rust log queue (default 8192 records).
        log_bridge: Deliver Rust log records to ``logging`` (default True).
        from_env: Apply ``OTEL_*`` / ``ASTER_*`` under the explicit arguments.
        config_toml: The ``[observability]`` TOML table (header optional), used
            below the environment. The only place for ``attribute_extractors``
            and ``propagate_as_baggage``.

    Raises:
        ValueError: invalid configuration, or one that differs from the running
            pipeline's.
    """
    global _atexit_registered
    pinned = log_filter is not None
    if log_bridge and not pinned:
        log_filter = _logbridge.initial_filter()
        pinned = log_filter is None  # ASTER_LOG(_LEVEL) in the environment
    _aster.observability_init(
        service_name=service_name,
        otlp_endpoint=otlp_endpoint,
        otlp_headers=_pairs(otlp_headers),
        otlp_protocol=otlp_protocol,
        resource_attributes=_pairs(resource_attributes),
        log_filter=log_filter,
        extra_toml=config_toml,
        from_env=from_env,
        traces=traces,
        metrics=metrics,
        otlp_logs=otlp_logs,
        trace_sample_ratio=sample_ratio,
        log_queue=log_queue,
        log_bridge=log_bridge,
    )
    if log_bridge:
        _logbridge.pin(pinned)
        _logbridge.start()
    with _lock:
        if not _atexit_registered:
            _atexit_registered = True
            atexit.register(_at_exit)


def _env_wants_traces() -> bool:
    env = os.environ
    exporter = env.get("OTEL_TRACES_EXPORTER", "").strip().lower()
    if exporter:
        return exporter != "none"
    return bool(
        env.get("OTEL_EXPORTER_OTLP_ENDPOINT") or env.get("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
    )


def ensure_started() -> None:
    """The implicit start (called when a server or client starts): open the
    log bridge with an env-only configuration. Exporters start only when
    ``OTEL_*`` asks for them. A no-op after any :func:`init`."""
    global _auto
    with _lock:
        if _auto or is_initialized():
            return
        _auto = True
    try:
        init(traces=None if _env_wants_traces() else False)
    except Exception as exc:  # noqa: BLE001 -- observability must not stop a server
        logger.warning("observability could not start: %s", exc)


def flush(timeout: float = 2.0) -> None:
    """Flush every signal, waiting at most ``timeout`` seconds."""
    _aster.observability_flush(int(timeout * 1000))


def shutdown(timeout: float = 2.0) -> bool:
    """Release every reference this process holds on the pipeline: the log
    drain stops, providers flush and stop (unless an ops listener still holds
    the pipeline), spans end and metric instruments are freed (``aster.metrics``
    objects re-declare themselves on next use). Returns whether anything was
    held. A later :func:`init` starts a new pipeline."""
    global _auto
    held = False
    _logbridge.stop(min(timeout, 1.0))
    while _aster.observability_shutdown(int(timeout * 1000)):
        held = True
    with _lock:
        _auto = False
    return held


def _at_exit() -> None:
    try:
        shutdown(timeout=1.0)
    except Exception:  # noqa: BLE001
        pass


def metrics_text() -> str | None:
    """The Prometheus text of the running pipeline (what ``/metrics`` serves),
    or ``None`` while no :class:`aster.HealthServer` has given the pipeline a
    pull reader."""
    return _aster.metrics_prometheus()
