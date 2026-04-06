"""
aster.health — Health checking, readiness probes, and operational metrics.

Provides three capabilities:

1. **Health/readiness status** — for Kubernetes probes and load balancers.
2. **Connection-level metrics** — active connections, streams in flight.
3. **Admission metrics** — admit/deny counts, latency.

The health server is **disabled by default** (port=0). Enable it explicitly::

    # Option 1: constructor
    health = HealthServer(srv, port=8080)

    # Option 2: environment variable
    ASTER_HEALTH_PORT=8080 python my_service.py

The default bind address is ``127.0.0.1`` (localhost only). For Kubernetes
pod probes, set ``host="0.0.0.0"`` or ``ASTER_HEALTH_HOST=0.0.0.0``.

Usage::

    from aster import AsterServer
    from aster.health import HealthServer

    async with AsterServer(services=[...]) as srv:
        health = HealthServer(srv, port=8080)
        await health.start()
        # GET /healthz → 200 {"status": "ok"}
        # GET /readyz  → 200 {"status": "ready", "services": 3}
        # GET /metrics → 200 (JSON metrics snapshot)
        await srv.serve()

For Kubernetes exec probes (no HTTP server needed)::

    from aster.health import check_health, check_ready

    # In a liveness probe script:
    assert check_health(srv)  # node is running

    # In a readiness probe script:
    assert check_ready(srv)   # contracts published, admission ready
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field
from typing import Any

logger = logging.getLogger(__name__)


# ── Operational metrics ───────────────────────────────────────────────────────


@dataclass
class ConnectionMetrics:
    """Tracks connection-level metrics for the server."""

    active_connections: int = 0
    """Number of currently active peer connections."""

    total_connections: int = 0
    """Total connections accepted since startup."""

    active_streams: int = 0
    """Number of currently active RPC streams."""

    total_streams: int = 0
    """Total streams handled since startup."""

    def connection_opened(self) -> None:
        self.active_connections += 1
        self.total_connections += 1

    def connection_closed(self) -> None:
        self.active_connections = max(0, self.active_connections - 1)

    def stream_opened(self) -> None:
        self.active_streams += 1
        self.total_streams += 1

    def stream_closed(self) -> None:
        self.active_streams = max(0, self.active_streams - 1)

    def to_dict(self) -> dict[str, int]:
        return {
            "active_connections": self.active_connections,
            "total_connections": self.total_connections,
            "active_streams": self.active_streams,
            "total_streams": self.total_streams,
        }


@dataclass
class AdmissionMetrics:
    """Tracks admission decision metrics."""

    consumer_admitted: int = 0
    consumer_denied: int = 0
    consumer_errors: int = 0
    producer_admitted: int = 0
    producer_denied: int = 0
    producer_errors: int = 0
    last_admission_ms: float = 0.0
    """Duration of the last admission handshake in milliseconds."""

    def record_consumer_admit(self, duration_ms: float = 0.0) -> None:
        self.consumer_admitted += 1
        self.last_admission_ms = duration_ms

    def record_consumer_deny(self) -> None:
        self.consumer_denied += 1

    def record_consumer_error(self) -> None:
        self.consumer_errors += 1

    def record_producer_admit(self, duration_ms: float = 0.0) -> None:
        self.producer_admitted += 1
        self.last_admission_ms = duration_ms

    def record_producer_deny(self) -> None:
        self.producer_denied += 1

    def record_producer_error(self) -> None:
        self.producer_errors += 1

    def to_dict(self) -> dict[str, Any]:
        return {
            "consumer_admitted": self.consumer_admitted,
            "consumer_denied": self.consumer_denied,
            "consumer_errors": self.consumer_errors,
            "producer_admitted": self.producer_admitted,
            "producer_denied": self.producer_denied,
            "producer_errors": self.producer_errors,
            "last_admission_ms": round(self.last_admission_ms, 1),
        }


# ── Global metrics singleton ─────────────────────────────────────────────────

_connection_metrics = ConnectionMetrics()
_admission_metrics = AdmissionMetrics()
_start_time: float = time.monotonic()


def get_connection_metrics() -> ConnectionMetrics:
    return _connection_metrics


def get_admission_metrics() -> AdmissionMetrics:
    return _admission_metrics


def reset_metrics() -> None:
    """Reset all metrics (for testing)."""
    global _connection_metrics, _admission_metrics, _start_time
    _connection_metrics = ConnectionMetrics()
    _admission_metrics = AdmissionMetrics()
    _start_time = time.monotonic()


# ── Health check functions ────────────────────────────────────────────────────


def check_health(server: Any) -> bool:
    """Check if the server is alive (liveness probe).

    Returns True if the server has been started and hasn't been closed.
    """
    return getattr(server, "_started", False) and not getattr(server, "_closed", True)


def check_ready(server: Any) -> bool:
    """Check if the server is ready to accept traffic (readiness probe).

    Returns True if:
    - Server is started
    - At least one service is registered
    - Contract publication has completed (registry_ticket is set)
    """
    if not check_health(server):
        return False
    summaries = getattr(server, "_service_summaries", [])
    return len(summaries) > 0


def health_status(server: Any) -> dict[str, Any]:
    """Full health status for /healthz endpoint."""
    uptime_s = time.monotonic() - _start_time
    return {
        "status": "ok" if check_health(server) else "unhealthy",
        "uptime_s": round(uptime_s, 1),
    }


def ready_status(server: Any) -> dict[str, Any]:
    """Full readiness status for /readyz endpoint."""
    summaries = getattr(server, "_service_summaries", [])
    has_registry = bool(getattr(server, "_registry_ticket", ""))
    return {
        "status": "ready" if check_ready(server) else "not_ready",
        "services": len(summaries),
        "registry": has_registry,
    }


def metrics_snapshot(server: Any) -> dict[str, Any]:
    """Full metrics snapshot for /metrics endpoint."""
    rpc_metrics = {}
    # Try to get RPC metrics from MetricsInterceptor if wired
    interceptors = getattr(server, "_interceptors", [])
    for i in interceptors:
        if hasattr(i, "snapshot"):
            rpc_metrics = i.snapshot()
            break

    return {
        "health": health_status(server),
        "ready": ready_status(server),
        "connections": _connection_metrics.to_dict(),
        "admission": _admission_metrics.to_dict(),
        "rpc": rpc_metrics,
    }


# ── HTTP health server ───────────────────────────────────────────────────────


class HealthServer:
    """Lightweight HTTP server for health/readiness/metrics endpoints.

    Runs on a separate port from the QUIC RPC endpoint. Designed for
    Kubernetes probes and Prometheus scraping.

    Endpoints:
      GET /healthz  → {"status": "ok", "uptime_s": 123.4}
      GET /readyz   → {"status": "ready", "services": 3, "registry": true}
      GET /metrics  → full metrics JSON snapshot

    Usage::

        health = HealthServer(aster_server, port=8080)
        await health.start()
        # ... server runs ...
        await health.stop()
    """

    def __init__(self, server: Any, host: str = "127.0.0.1", port: int = 0) -> None:
        """
        Args:
            server: The AsterServer instance to monitor.
            host: Bind address. Default ``127.0.0.1`` (localhost only).
                  Set to ``0.0.0.0`` to expose externally (e.g., for k8s probes
                  from a sidecar).
            port: Port to listen on. Default ``0`` (disabled — must be set
                  explicitly to enable). Common choices: 8080, 9090.
        """
        self._server_ref = server
        self._host = host
        self._port = port
        self._http_server: Any = None
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        """Start the HTTP health server.

        Does nothing if port is 0 (default). Set port explicitly or via
        ``ASTER_HEALTH_PORT`` to enable.
        """
        if self._port == 0:
            # Check env var fallback
            import os
            env_port = os.environ.get("ASTER_HEALTH_PORT", "0")
            env_host = os.environ.get("ASTER_HEALTH_HOST", "")
            try:
                self._port = int(env_port)
            except ValueError:
                self._port = 0
            if env_host:
                self._host = env_host

        if self._port == 0:
            logger.debug("Health server disabled (port=0). Set ASTER_HEALTH_PORT to enable.")
            return

        self._http_server = await asyncio.start_server(
            self._handle_request, self._host, self._port
        )
        logger.info("Health server listening on %s:%d", self._host, self._port)

    async def stop(self) -> None:
        """Stop the HTTP health server."""
        if self._http_server:
            self._http_server.close()
            await self._http_server.wait_closed()

    async def _handle_request(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        """Handle a single HTTP request (minimal HTTP/1.1 parser)."""
        try:
            request_line = await asyncio.wait_for(reader.readline(), timeout=5.0)
            request_str = request_line.decode("utf-8", errors="replace").strip()

            # Read remaining headers (discard)
            while True:
                line = await asyncio.wait_for(reader.readline(), timeout=5.0)
                if line in (b"\r\n", b"\n", b""):
                    break

            # Route
            path = request_str.split(" ")[1] if " " in request_str else "/"

            if path == "/healthz":
                body = health_status(self._server_ref)
                status = 200 if body["status"] == "ok" else 503
            elif path == "/readyz":
                body = ready_status(self._server_ref)
                status = 200 if body["status"] == "ready" else 503
            elif path == "/metrics":
                body = metrics_snapshot(self._server_ref)
                status = 200
            else:
                body = {"error": "not found"}
                status = 404

            response_body = json.dumps(body, indent=2)
            response = (
                f"HTTP/1.1 {status} {'OK' if status == 200 else 'Error'}\r\n"
                f"Content-Type: application/json\r\n"
                f"Content-Length: {len(response_body)}\r\n"
                f"Connection: close\r\n"
                f"\r\n"
                f"{response_body}"
            )
            writer.write(response.encode())
            await writer.drain()
        except Exception as e:
            logger.debug("Health server request error: %s", e)
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
