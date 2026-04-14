"""
aster.interceptors.metrics -- Metrics interceptor with OpenTelemetry integration.

Provides RED metrics (Rate, Errors, Duration) and distributed tracing.
OpenTelemetry is an optional dependency -- metrics degrade gracefully to
in-memory counters when OTel is not installed.

Usage::

    from aster.interceptors import MetricsInterceptor

    server = AsterServer(
        services=[MyService()],
        interceptors=[MetricsInterceptor()],
    )

OTel setup (application responsibility)::

    from opentelemetry import trace, metrics
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.metrics import MeterProvider
    from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
    from opentelemetry.exporter.otlp.proto.grpc.metric_exporter import OTLPMetricExporter

    trace.set_tracer_provider(TracerProvider())
    trace.get_tracer_provider().add_span_processor(
        BatchSpanProcessor(OTLPSpanExporter())
    )
    metrics.set_meter_provider(MeterProvider())
"""

from __future__ import annotations

import time
from typing import Any

from aster.interceptors.base import CallContext, Interceptor
from aster.logging import set_request_id
from aster.status import RpcError

try:
    from opentelemetry import trace as _otel_trace  # type: ignore
    from opentelemetry import metrics as _otel_metrics  # type: ignore
    from opentelemetry.trace import StatusCode as _OtelStatusCode  # type: ignore
except ImportError:
    _otel_trace = None  # type: ignore
    _otel_metrics = None  # type: ignore
    _OtelStatusCode = None  # type: ignore


class MetricsInterceptor(Interceptor):
    """Collects RED metrics and creates OTel spans for each RPC call.

    Metrics collected:
      - ``aster.rpc.started`` -- counter, labels: service, method, pattern
      - ``aster.rpc.completed`` -- counter, labels: service, method, status
      - ``aster.rpc.duration`` -- histogram (seconds), labels: service, method

    Tracing:
      - One span per RPC call: ``{service}/{method}``
      - Span attributes: rpc.service, rpc.method, rpc.system, rpc.status_code

    Falls back to simple in-memory counters when OTel is not installed.
    """

    def __init__(self) -> None:
        # In-memory fallback counters (always available)
        self.started = 0
        self.succeeded = 0
        self.failed = 0

        # Try to set up OTel
        self._tracer: Any = None
        self._meter: Any = None
        self._started_counter: Any = None
        self._completed_counter: Any = None
        self._duration_histogram: Any = None

        if _otel_trace is not None and _otel_metrics is not None:
            try:
                self._tracer = _otel_trace.get_tracer("aster.rpc", "0.2.0")
                self._meter = _otel_metrics.get_meter("aster.rpc", "0.2.0")

                self._started_counter = self._meter.create_counter(
                    "aster.rpc.started",
                    description="Total RPC calls started",
                    unit="1",
                )
                self._completed_counter = self._meter.create_counter(
                    "aster.rpc.completed",
                    description="Total RPC calls completed",
                    unit="1",
                )
                self._duration_histogram = self._meter.create_histogram(
                    "aster.rpc.duration",
                    description="RPC call duration",
                    unit="s",
                )
            except Exception:
                pass  # OTel provider not configured -- use fallback counters

        # Track start times per call_id for duration calculation
        self._call_starts: dict[str, float] = {}

    @property
    def has_otel(self) -> bool:
        """Whether OpenTelemetry is available and configured."""
        return self._tracer is not None

    async def on_request(self, ctx: CallContext, request: object) -> object:
        self.started += 1

        labels = {
            "service": ctx.service,
            "method": ctx.method,
            "pattern": ctx.pattern or "unary",
        }

        # OTel counter
        if self._started_counter:
            self._started_counter.add(1, labels)

        # Start timing
        call_key = f"{ctx.service}.{ctx.method}.{id(request)}"
        self._call_starts[call_key] = time.monotonic()

        # Start OTel span
        if self._tracer:
            span = self._tracer.start_span(
                f"{ctx.service}/{ctx.method}",
                kind=_otel_trace.SpanKind.SERVER,
                attributes={
                    "rpc.system": "aster",
                    "rpc.service": ctx.service,
                    "rpc.method": ctx.method,
                    "rpc.aster.pattern": ctx.pattern or "unary",
                    "rpc.aster.idempotent": ctx.idempotent,
                },
            )
            # Store span on context for on_response/on_error
            ctx._otel_span = span  # type: ignore[attr-defined]
            ctx._otel_call_key = call_key  # type: ignore[attr-defined]

        # Set correlation ID for structured logging
        set_request_id(ctx.call_id or f"{ctx.service}.{ctx.method}")

        return request

    async def on_response(self, ctx: CallContext, response: object) -> object:
        self.succeeded += 1

        labels = {"service": ctx.service, "method": ctx.method, "status": "OK"}

        if self._completed_counter:
            self._completed_counter.add(1, labels)

        # Record duration
        call_key = getattr(ctx, "_otel_call_key", None)
        if call_key and call_key in self._call_starts:
            duration = time.monotonic() - self._call_starts.pop(call_key)
            if self._duration_histogram:
                self._duration_histogram.record(
                    duration, {"service": ctx.service, "method": ctx.method}
                )

        # End OTel span
        span = getattr(ctx, "_otel_span", None)
        if span:
            span.set_status(_OtelStatusCode.OK)
            span.end()

        return response

    async def on_error(self, ctx: CallContext, error: RpcError) -> RpcError | None:
        self.failed += 1

        labels = {
            "service": ctx.service,
            "method": ctx.method,
            "status": error.code.name if hasattr(error.code, "name") else str(error.code),
        }

        if self._completed_counter:
            self._completed_counter.add(1, labels)

        # Record duration
        call_key = getattr(ctx, "_otel_call_key", None)
        if call_key and call_key in self._call_starts:
            duration = time.monotonic() - self._call_starts.pop(call_key)
            if self._duration_histogram:
                self._duration_histogram.record(
                    duration, {"service": ctx.service, "method": ctx.method}
                )

        # End OTel span with error
        span = getattr(ctx, "_otel_span", None)
        if span:
            span.set_status(_OtelStatusCode.ERROR, str(error.message))
            span.set_attribute("rpc.aster.error_code", str(error.code))
            span.record_exception(error)
            span.end()

        return error

    def snapshot(self) -> dict[str, int]:
        """Return a snapshot of in-memory counters."""
        return {
            "started": self.started,
            "succeeded": self.succeeded,
            "failed": self.failed,
            "in_flight": self.started - self.succeeded - self.failed,
        }
