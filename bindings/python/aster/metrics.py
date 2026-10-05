"""
aster.metrics -- application metrics over the Rust OpenTelemetry pipeline.

Thin wrappers: each instrument is a handle into ``aster-otel`` (meter scope
``aster.app``), recorded through the same ``MeterProvider`` as Aster's built-in
RPC metrics (``aster.rpc.server.*``, ``aster.admission.*``, ...), so one
exporter and one ``/metrics`` endpoint serve both. Without a pipeline
(:func:`aster.observability.init`) recording is a no-op.

Usage::

    from aster import metrics

    orders = metrics.counter("orders.placed", "Orders placed")
    latency = metrics.histogram("orders.latency", "Order latency", unit="s")

    orders.add(1, {"region": "eu"})
    latency.record(0.042, {"region": "eu"})

Attribute values are stringified. Keep their cardinality bounded: the SDK folds
series beyond 2000 per instrument into ``otel.metric.overflow=true``. Names
follow the OpenTelemetry rules (a letter, then letters, digits, ``_ . - /``);
a name can have only one kind.

Scraping: start an :class:`aster.HealthServer` and read ``/metrics`` (Prometheus
text), or call :func:`prometheus_text` in-process.

Prometheus text appends the unit to the name (``unit="s"`` renders
``..._seconds``) and ``_total`` to counters.

``auto_attrs`` (attribute keys merged in from the call being handled) and
``cardinality_limit`` are accepted and recorded, but have no effect yet.
"""

from __future__ import annotations

import threading
from typing import Any, Callable, Mapping, Sequence

from . import _aster

__all__ = [
    "Counter",
    "UpDownCounter",
    "Histogram",
    "Gauge",
    "counter",
    "up_down_counter",
    "histogram",
    "gauge",
    "prometheus_text",
]

_COUNTER, _UPDOWN, _HISTOGRAM, _GAUGE = 1, 2, 3, 4

Attrs = Mapping[str, Any] | None


def _attrs(a: Attrs) -> list[tuple[str, str]] | None:
    if not a:
        return None
    return [(str(k), ("true" if v is True else "false" if v is False else str(v))) for k, v in a.items()]


class _Instrument:
    _kind = 0
    _lock = threading.Lock()

    def __init__(
        self,
        name: str,
        description: str = "",
        unit: str = "",
        *,
        auto_attrs: Sequence[str] = (),
        cardinality_limit: int = 0,
    ) -> None:
        self.name = name
        self.description = description
        self.unit = unit
        self.auto_attrs = tuple(auto_attrs)
        self.cardinality_limit = cardinality_limit
        self._handle = 0
        self._declare()

    def _declare(self) -> None:
        self._handle = _aster.instrument_new(
            self._kind,
            self.name,
            self.description,
            self.unit,
            list(self.auto_attrs),
            self.cardinality_limit,
        )

    def _record(self, fn: Callable[..., None], value: Any, attrs: Attrs) -> None:
        a = _attrs(attrs)
        try:
            fn(self._handle, value, a)
        except LookupError:
            # The pipeline was shut down and freed the handle: declare again.
            with self._lock:
                self._declare()
            fn(self._handle, value, a)

    def close(self) -> None:
        """Release the instrument's handle (optional; shutdown frees them all)."""
        _aster.instrument_free(self._handle)

    def __repr__(self) -> str:
        return f"{type(self).__name__}({self.name!r})"


class Counter(_Instrument):
    """Monotonic sum."""

    _kind = _COUNTER

    def add(self, value: int = 1, attrs: Attrs = None) -> None:
        if value < 0:
            raise ValueError("a counter only goes up; use an up_down_counter")
        self._record(_aster.metric_add_u64, int(value), attrs)


class UpDownCounter(_Instrument):
    """Sum that may go down (in-flight work, queue depth)."""

    _kind = _UPDOWN

    def add(self, value: int = 1, attrs: Attrs = None) -> None:
        self._record(_aster.metric_add_i64, int(value), attrs)


class Histogram(_Instrument):
    """Distribution of values (durations, sizes)."""

    _kind = _HISTOGRAM

    def record(self, value: float, attrs: Attrs = None) -> None:
        self._record(_aster.metric_record_f64, float(value), attrs)


class Gauge(_Instrument):
    """Last recorded value."""

    _kind = _GAUGE

    def record(self, value: float, attrs: Attrs = None) -> None:
        self._record(_aster.metric_record_f64, float(value), attrs)


def counter(name: str, description: str = "", unit: str = "", *, auto_attrs: Sequence[str] = (), cardinality_limit: int = 0) -> Counter:
    return Counter(name, description, unit, auto_attrs=auto_attrs, cardinality_limit=cardinality_limit)


def up_down_counter(name: str, description: str = "", unit: str = "", *, auto_attrs: Sequence[str] = (), cardinality_limit: int = 0) -> UpDownCounter:
    return UpDownCounter(name, description, unit, auto_attrs=auto_attrs, cardinality_limit=cardinality_limit)


def histogram(name: str, description: str = "", unit: str = "", *, auto_attrs: Sequence[str] = (), cardinality_limit: int = 0) -> Histogram:
    return Histogram(name, description, unit, auto_attrs=auto_attrs, cardinality_limit=cardinality_limit)


def gauge(name: str, description: str = "", unit: str = "", *, auto_attrs: Sequence[str] = (), cardinality_limit: int = 0) -> Gauge:
    return Gauge(name, description, unit, auto_attrs=auto_attrs, cardinality_limit=cardinality_limit)


def prometheus_text() -> str | None:
    """The Prometheus text of the running pipeline (built-ins and custom
    instruments), or ``None`` while no :class:`aster.HealthServer` has given
    the pipeline a pull reader."""
    return _aster.metrics_prometheus()
