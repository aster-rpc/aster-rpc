"""Observability O6: the Python binding over the Rust OpenTelemetry pipeline.

Design: docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md (sections 3,
4.2, 4.3, 5.3, 6, 8 row O6).

The pipeline and Rust's global ``tracing`` subscriber are once per process, so
tests that need a specific configuration (traces on, a re-init, a fork) run in
a child interpreter; the rest share the pytest process, where the implicit
start (log bridge on, exporters off) is in effect.
"""

from __future__ import annotations

import asyncio
import http.client
import json
import logging
import os
import subprocess
import sys
import textwrap
import time
from dataclasses import dataclass

import pytest

import aster
from aster import (
    AsterClient,
    AsterConfig,
    AsterServer,
    HealthServer,
    Node,
    _aster,
    metrics,
    observability,
    rpc,
    service,
    tracing,
    wire_type,
)
from aster import _logbridge
from aster import logging as aster_logging

HEX32 = "0123456789abcdef" * 2
TRACEPARENT = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"


needs_fork = pytest.mark.skipif(not hasattr(os, "fork"), reason="needs os.fork (POSIX)")


# -- helpers ---------------------------------------------------------------------


def _child_env(**extra: str) -> dict[str, str]:
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(("OTEL_", "ASTER_LOG", "ASTER_HEALTH"))
    }
    env.update(extra)
    return env


def run_child(code: str, timeout: float = 90, **env: str) -> subprocess.CompletedProcess:
    """Run ``code`` in a fresh interpreter (own pipeline, own subscriber)."""
    return subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code)],
        env=_child_env(**env),
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def child_json(code: str, **env: str) -> dict:
    proc = run_child(code, **env)
    assert proc.returncode == 0, f"child failed:\n{proc.stdout}\n{proc.stderr}"
    return json.loads(proc.stdout.strip().splitlines()[-1])


def http_get(addr: str, path: str) -> tuple[int, dict[str, str], str]:
    host, _, port = addr.rpartition(":")
    conn = http.client.HTTPConnection(host, int(port), timeout=10)
    try:
        conn.request("GET", path)
        resp = conn.getresponse()
        return resp.status, {k.lower(): v for k, v in resp.getheaders()}, resp.read().decode()
    finally:
        conn.close()


async def get(addr: str, path: str) -> tuple[int, dict[str, str], str]:
    return await asyncio.to_thread(http_get, addr, path)


async def wait_for(pred, timeout: float = 10.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        v = pred()
        if v:
            return v
        await asyncio.sleep(0.05)
    return pred()


@pytest.fixture
def aster_logger_capture(caplog):
    """``caplog`` on the ``aster`` logger itself: ``configure_logging`` makes it
    non-propagating, so the root-attached handler would never see its records."""
    lg = logging.getLogger("aster")
    old_level, old_propagate = lg.level, lg.propagate
    lg.propagate = False  # the handler below is the only route to caplog
    lg.addHandler(caplog.handler)
    caplog.set_level(5, logger="aster")
    try:
        yield caplog
    finally:
        lg.removeHandler(caplog.handler)
        lg.setLevel(old_level)
        lg.propagate = old_propagate


# -- log bridge ------------------------------------------------------------------


def test_bridged_record_reaches_caplog_with_trace_id(aster_logger_capture):
    caplog = aster_logger_capture
    tid, sid = "ab" * 16, "cd" * 8
    batch = [
        # level, logger, message, ts_ns, flags (HAS_TRACE|IN_CALL), trace, span, call_id, fields
        (20, "aster.rpc.dispatch", "rpc completed", 1_700_000_000_123_000_000, 3, tid, sid, 7,
         [("rpc.service", "Echo"), ("rpc.method", "say"), ("aster.peer", "peer-1"), ("thread.name", "tokio-0")]),
        (10, "aster.rpc.server", "below the logger level", 1_700_000_000_000_000_000, 0, None, None, 0, []),
        (5, "aster.rpc.foreign", "trace record", 1_700_000_000_000_000_000, 8, None, None, 0, []),
    ]
    caplog.set_level(15, logger="aster")  # DEBUG (10) is filtered, TRACE too
    _logbridge._emit(batch)
    recs = [r for r in caplog.records if getattr(r, "aster_origin", "") == "rust"]
    assert [r.getMessage() for r in recs] == ["rpc completed"]
    r = recs[0]
    assert r.name == "aster.rpc.dispatch" and r.levelno == logging.INFO
    assert r.trace_id == tid and r.span_id == sid
    assert r.aster_call_id == 7 and r.aster_audit is False
    assert r.rpc_service == "Echo" and r.rpc_method == "say" and r.aster_peer == "peer-1"
    assert r.aster_fields["thread.name"] == "tokio-0"
    assert abs(r.created - 1_700_000_000.123) < 1e-6

    caplog.clear()
    caplog.set_level(5, logger="aster")
    _logbridge._emit(batch)
    levels = {r.levelno for r in caplog.records}
    assert levels == {5, 10, 20}
    assert logging.getLevelName(5) == "TRACE"
    audit = next(r for r in caplog.records if r.levelno == 5)
    assert audit.aster_audit is True and audit.trace_id == ""


def test_a_bad_record_does_not_stop_the_batch(aster_logger_capture):
    good = (20, "aster.x", "kept", 1_700_000_000_000_000_000, 0, None, None, 0, [])
    _logbridge._emit([("not", "a", "record"), good])
    assert [r.getMessage() for r in aster_logger_capture.records if r.name == "aster.x"] == ["kept"]


async def test_real_rust_record_reaches_caplog(aster_logger_capture):
    """Drain thread end to end: the ops listener logs on target aster::ops."""
    observability.ensure_started()
    _aster.log_set_filter("warn,aster=info")
    caplog = aster_logger_capture
    node = await Node.start(AsterConfig())
    try:
        server = await HealthServer.start(node, "127.0.0.1", 0)
        try:
            rec = await wait_for(
                lambda: next(
                    (r for r in caplog.records if r.name == "aster.ops" and "listener started" in r.getMessage()),
                    None,
                )
            )
            assert rec is not None, [r.getMessage() for r in caplog.records]
            assert rec.aster_origin == "rust" and rec.levelno == logging.INFO
            assert rec.trace_id == ""  # not inside an exported span
            assert rec.thread_name  # the emitting thread, from the Rust fields
        finally:
            await server.stop()
    finally:
        await node.shutdown()
        _logbridge.sync_level()


def test_sync_level_follows_the_aster_logger():
    observability.ensure_started()
    lg = logging.getLogger("aster")
    old = lg.level
    try:
        lg.setLevel(logging.DEBUG)
        assert _logbridge.sync_level() == "warn,aster=debug"
        assert _aster.log_filter() == "warn,aster=debug"
        assert aster_logging.sync_level() == "warn,aster=debug"
        lg.setLevel(logging.ERROR)
        aster_logging.sync_level()
        assert _aster.log_filter() == "warn,aster=error"
        lg.setLevel(5)
        aster_logging.sync_level()
        assert _aster.log_filter() == "warn,aster=trace"
    finally:
        lg.setLevel(old)
        _logbridge.sync_level()


def test_rust_records_carry_the_span_trace_id_end_to_end():
    """Traces on (exporter unreachable on purpose): Rust events inside the RPC
    server span carry its trace ids, the handler sees the client's trace, and
    the Python client's traceparent crosses the wire."""
    out = child_json(
        """
        import asyncio, json, logging
        from dataclasses import dataclass
        from aster import AsterClient, AsterServer, observability, rpc, service, tracing, wire_type

        @wire_type("test.o6/Req")
        @dataclass
        class Req:
            text: str = ""

        @wire_type("test.o6/Resp")
        @dataclass
        class Resp:
            text: str = ""

        seen = {}

        @service(name="O6Echo", version=1)
        class O6Echo:
            @rpc()
            async def echo(self, req: Req) -> Resp:
                seen["handler_trace"] = tracing.current_trace_id()
                seen["handler_span"] = tracing.current_span_id()
                logging.getLogger("app").warning("inside the handler")
                return Resp(text=req.text)

        class Cap(logging.Handler):
            def __init__(self):
                super().__init__(0)
                self.records = []
            def emit(self, record):
                self.records.append(record)

        async def main():
            observability.init(traces=True, otlp_endpoint="http://127.0.0.1:9",
                               log_filter="warn,aster=trace")
            cap = Cap()
            async with AsterServer(services=[O6Echo()], allow_all_consumers=True) as server:
                logging.getLogger("aster").addHandler(cap)
                logging.getLogger("aster").setLevel(5)
                server.serve()
                async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
                    svc = await client.client(O6Echo)
                    with tracing.span("client.work") as outer:
                        reply = await svc.echo(Req(text="hi"))
                        client_trace = outer.trace_id
                    await asyncio.sleep(0.5)
            rust = [r for r in cap.records if getattr(r, "aster_origin", "") == "rust"]
            print(json.dumps({
                "client_trace": client_trace,
                "reply": reply.text,
                "handler_trace": seen["handler_trace"],
                "rust_traces": sorted({r.trace_id for r in rust if r.trace_id}),
                "with_trace": [r.getMessage() for r in rust if r.trace_id],
                "service_field": sorted({getattr(r, "rpc_service", "") for r in rust if r.trace_id}),
            }))

        asyncio.run(main())
        """
    )
    assert out["reply"] == "hi"
    trace = out["client_trace"]
    assert trace and len(trace) == 32 and set(trace) <= set("0123456789abcdef")
    assert out["handler_trace"] == trace  # traceparent crossed the wire
    assert out["rust_traces"] == [trace], out  # Rust events inside the server span
    assert "rpc completed" in out["with_trace"]
    assert out["service_field"] == ["O6Echo"]


# -- tracing ---------------------------------------------------------------------


def test_trace_context_carrier_and_inject():
    assert tracing.current_trace_id() is None and tracing.current_traceparent() is None
    assert tracing.inject({"a": "b"}) == {"a": "b"}
    with tracing.with_trace_context(TRACEPARENT, {"tenant": "t 1", "k": "v,w"}):
        assert tracing.current_trace_id() == "4bf92f3577b34da6a3ce929d0e0e4736"
        assert tracing.current_span_id() == "00f067aa0ba902b7"
        assert tracing.current_baggage() == {"tenant": "t 1", "k": "v,w"}
        md = tracing.inject({"a": "b"})
        assert md["traceparent"] == TRACEPARENT and md["a"] == "b"
        assert md["baggage"] == "tenant=t%201,k=v%2Cw"
        # the caller's own traceparent wins
        assert tracing.inject({"traceparent": "x"}) == {"traceparent": "x"}
    assert tracing.current_trace_id() is None
    with pytest.raises(ValueError):
        tracing.with_trace_context("00-not-a-traceparent")
    assert tracing._parse_baggage("a=1;prop=x, b=%20c") == {"a": "1", "b": " c"}


async def test_carrier_follows_asyncio_tasks():
    async def probe():
        return tracing.current_trace_id()

    with tracing.with_trace_context(TRACEPARENT):
        assert await asyncio.create_task(probe()) == "4bf92f3577b34da6a3ce929d0e0e4736"
    assert await asyncio.create_task(probe()) is None


def test_spans_nest_and_record_errors():
    out = child_json(
        """
        import asyncio, json
        from aster import observability, tracing

        observability.init(traces=True, otlp_endpoint="http://127.0.0.1:9")
        res = {}
        with tracing.span("outer", {"k": 1, "flag": True}, kind="server") as outer:
            res["outer_trace"] = outer.trace_id
            res["current_is_outer"] = tracing.current_span_id() == outer.span_id
            with tracing.span("inner") as inner:
                res["same_trace"] = inner.trace_id == outer.trace_id
                res["distinct_span"] = inner.span_id != outer.span_id
                res["current_is_inner"] = tracing.current_span_id() == inner.span_id
                inner.add_event("tick", {"n": 2})
                inner.set_attribute("x", "y")
                res["inject"] = tracing.inject(None)["traceparent"] == inner.traceparent
            res["restored"] = tracing.current_span_id() == outer.span_id
        res["cleared"] = tracing.current_trace_id() is None

        try:
            with tracing.span("failing"):
                raise RuntimeError("boom")
        except RuntimeError:
            res["error_propagated"] = True

        @tracing.traced("decorated")
        async def work():
            return tracing.current_trace_id()

        async def main():
            async with tracing.span("async") as s:
                return await work() == s.trace_id

        res["decorated_in_span"] = asyncio.run(main())
        with tracing.span("child", parent="00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01") as c:
            res["explicit_parent"] = c.trace_id == "4bf92f3577b34da6a3ce929d0e0e4736"
        observability.shutdown()
        print(json.dumps(res))
        """
    )
    assert out == {
        "outer_trace": out["outer_trace"],
        "current_is_outer": True,
        "same_trace": True,
        "distinct_span": True,
        "current_is_inner": True,
        "inject": True,
        "restored": True,
        "cleared": True,
        "error_propagated": True,
        "decorated_in_span": True,
        "explicit_parent": True,
    }
    assert len(out["outer_trace"]) == 32


def test_spans_are_noops_without_a_tracer_provider():
    out = child_json(
        """
        import json
        from aster import observability, tracing
        observability.init(traces=False)
        with tracing.span("quiet") as s:
            res = {"trace": s.trace_id, "current": tracing.current_trace_id()}
            s.set_attribute("a", "b")
            s.add_event("e")
        print(json.dumps(res))
        """
    )
    assert out == {"trace": None, "current": None}


# -- metrics ---------------------------------------------------------------------


def series(text: str, name: str) -> list[str]:
    return [ln for ln in text.splitlines() if ln.startswith(name) and not ln.startswith("#")]


def sample(line: str) -> float:
    return float(line.rsplit(" ", 1)[1])


class running_health:
    """A HealthServer on a fresh in-memory node; both stop on exit."""

    async def __aenter__(self) -> HealthServer:
        self.node = await Node.start(AsterConfig())
        self.server = await HealthServer.start(self.node, "127.0.0.1", 0)
        return self.server

    async def __aexit__(self, *exc) -> None:
        await self.server.stop()
        await self.node.shutdown()


async def test_custom_metrics_visible_in_prometheus_text():
    async with running_health() as health:
        orders = metrics.counter("o6.orders.placed", "Orders placed")
        depth = metrics.up_down_counter("o6.queue.depth", "Queue depth")
        latency = metrics.histogram("o6.order.latency", "Order latency", unit="s")
        temp = metrics.gauge("o6.temperature", "Temperature")
        orders.add(2, {"region": "eu", "express": True})
        orders.add(1, {"region": "eu", "express": True})
        orders.add(5, {"region": "us"})
        depth.add(3)
        depth.add(-1)
        latency.record(0.25, {"region": "eu"})
        temp.record(21.5)
        with pytest.raises(ValueError):
            orders.add(-1)
        with pytest.raises(ValueError):
            metrics.gauge("o6.orders.placed")  # a name has one kind
        with pytest.raises(ValueError):
            metrics.counter("1 bad name")

        text = metrics.prometheus_text()
        assert text is not None
        eu = [ln for ln in series(text, "o6_orders_placed_total") if 'region="eu"' in ln]
        assert len(eu) == 1 and 'express="true"' in eu[0] and sample(eu[0]) == 3
        us = [ln for ln in series(text, "o6_orders_placed_total") if 'region="us"' in ln]
        assert len(us) == 1 and sample(us[0]) == 5
        assert sample(series(text, "o6_queue_depth")[0]) == 2
        assert sample(series(text, "o6_temperature")[0]) == 21.5
        assert sample(series(text, "o6_order_latency_seconds_count")[0]) == 1
        assert series(text, "aster_health_ready")  # a built-in, same pipeline

        status, headers, body = await get(health.addr, "/metrics")
        assert status == 200
        assert headers["content-type"].startswith("text/plain; version=0.0.4")
        assert series(body, "o6_orders_placed_total") and series(body, "aster_health_ready")
        assert observability.metrics_text() is not None


# -- health ----------------------------------------------------------------------


async def test_health_http_endpoints():
    async with running_health() as health:
        assert health.addr.startswith("127.0.0.1:") and health.port > 0

        status, headers, body = await get(health.addr, "/healthz")
        assert status == 200 and json.loads(body)["status"] == "ok"
        assert headers["content-type"].startswith("application/json")

        status, _, body = await get(health.addr, "/readyz")
        assert status == 503 and not health.is_ready()
        health.set_ready(True)
        status, _, body = await get(health.addr, "/readyz")
        assert status == 200 and health.is_ready()
        ready = json.loads(body)
        # "audit" is the built-in check, present once the log bridge was opened
        assert ready["status"] == "ready" and set(ready["checks"]) <= {"audit"}

        # a named check is created failing; ready = ready AND every check
        db = health.check("db")
        assert not db.is_ok() and db.name == "db"
        status, _, body = await get(health.addr, "/readyz")
        assert status == 503 and json.loads(body)["checks"]["db"] is False
        assert (await get(health.addr, "/healthz"))[0] == 200  # liveness ignores checks
        db.set(True)
        assert (await get(health.addr, "/readyz"))[0] == 200
        assert health.remove_check("db") and not health.remove_check("db")

        health.set_healthy(False)
        assert (await get(health.addr, "/healthz"))[0] == 503
        health.set_healthy(True)
        assert (await get(health.addr, "/nope"))[0] == 404

        health.set_services(3)
        health.set_registry(True)
        body = (await get(health.addr, "/readyz"))[2]
        assert json.loads(body)["services"] == 3

        addr = health.addr
        await health.stop()
        assert health.is_shut_down
        await health.stop()  # idempotent
        with pytest.raises(OSError):
            await get(addr, "/healthz")


@wire_type("test.o6inproc/Req")
@dataclass
class Req:
    text: str = ""


@wire_type("test.o6inproc/Resp")
@dataclass
class Resp:
    text: str = ""


_ECHO_CALLS = metrics.counter("o6.echo.calls", "Echo handler calls")


@service(name="O6InprocEcho", version=1)
class O6InprocEcho:
    @rpc()
    async def echo(self, req: Req) -> Resp:
        _ECHO_CALLS.add(1, {"kind": "unary"})
        return Resp(text=req.text)


async def test_asterserver_starts_ops_endpoints_from_config_and_counts_calls():
    cfg = AsterConfig(health_enabled=True)  # port 0 + enabled: an ephemeral port
    server = AsterServer(services=[O6InprocEcho()], allow_all_consumers=True, config=cfg)
    await server.start()
    try:
        health = server.health
        assert health is not None
        addr = health.addr
        assert (await get(addr, "/healthz"))[0] == 200
        assert (await get(addr, "/readyz"))[0] == 503  # started, not serving yet
        server.serve()
        status, _, body = await get(addr, "/readyz")
        assert status == 200 and json.loads(body)["services"] == 1

        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            svc = await client.client(O6InprocEcho)
            for i in range(3):
                assert (await svc.echo(Req(text=str(i)))).text == str(i)

        text = (await get(addr, "/metrics"))[2]
        done = [
            ln
            for ln in series(text, "aster_rpc_server_requests_completed_total")
            if 'rpc_service="O6InprocEcho"' in ln and 'aster_status="OK"' in ln
        ]
        assert len(done) == 1 and sample(done[0]) == 3, done
        assert series(text, "aster_rpc_server_duration_seconds_count")
        assert series(text, "aster_rpc_client_duration_seconds_count")
        assert sample(series(text, "o6_echo_calls_total")[0]) == 3
    finally:
        await server.close()
    # close: not ready first, then the listener is gone
    assert health.is_shut_down
    with pytest.raises(OSError):
        await get(addr, "/healthz")


def test_health_is_off_by_default():
    assert AsterConfig().health_port == 0 and AsterConfig().health_enabled is None
    assert AsterConfig().health_host == "127.0.0.1"


def test_health_config_from_env_and_file(monkeypatch, tmp_path):
    monkeypatch.setenv("ASTER_HEALTH_PORT", "18080")
    monkeypatch.setenv("ASTER_HEALTH_HOST", "0.0.0.0")
    cfg = AsterConfig.from_env()
    assert (cfg.health_port, cfg.health_host, cfg.health_enabled) == (18080, "0.0.0.0", None)
    assert cfg._sources["health_port"] == "ASTER_HEALTH_PORT"

    monkeypatch.delenv("ASTER_HEALTH_PORT")
    monkeypatch.delenv("ASTER_HEALTH_HOST")
    path = tmp_path / "aster.toml"
    path.write_text('[health]\nport = 9091\nhost = "10.0.0.5"\nenabled = false\n')
    cfg = AsterConfig.from_file(path)
    assert (cfg.health_port, cfg.health_host, cfg.health_enabled) == (9091, "10.0.0.5", False)
    assert cfg._sources["health_port"].endswith("[health]")
    assert cfg.replace(health_enabled=True).health_enabled is True


# -- logging helpers -------------------------------------------------------------


def _record(msg: str = "m", **extra) -> logging.LogRecord:
    rec = logging.LogRecord("app", logging.INFO, __file__, 1, msg, (), None)
    rec.__dict__.update(extra)
    return rec


def test_logging_helpers_add_trace_context():
    f = aster_logging.LoggingFilter()
    rec = _record()
    assert f.filter(rec) and rec.trace_id == "" and rec.span_id == ""

    with tracing.with_trace_context(TRACEPARENT):
        rec = _record()
        f.filter(rec)
        assert rec.trace_id == "4bf92f3577b34da6a3ce929d0e0e4736" and rec.span_id == "00f067aa0ba902b7"
        ev = aster_logging.structlog_processor(None, "info", {"event": "x"})
        assert ev["trace_id"] == "4bf92f3577b34da6a3ce929d0e0e4736" and ev["span_id"] == "00f067aa0ba902b7"
        loguru_rec = {"extra": {}}
        aster_logging.loguru_patcher(loguru_rec)
        assert loguru_rec["extra"]["trace_id"] == "4bf92f3577b34da6a3ce929d0e0e4736"
    assert aster_logging.structlog_processor(None, "info", {"event": "x"}) == {"event": "x"}
    loguru_rec = {"extra": {}}
    aster_logging.loguru_patcher(loguru_rec)
    assert loguru_rec["extra"] == {"trace_id": "", "span_id": ""}


def test_json_formatter_uses_call_context_and_trace_ids():
    from aster.interceptors.base import CallContext

    fmt = aster_logging.JsonFormatter()
    ctx = CallContext(service="Orders", method="place", call_id="42", peer="peer-1")
    token = CallContext._current.set(ctx)
    try:
        with tracing.with_trace_context(TRACEPARENT):
            entry = json.loads(fmt.format(_record("handling")))
    finally:
        CallContext._current.reset(token)
    assert entry["service"] == "Orders" and entry["method"] == "place" and entry["request_id"] == "42"
    assert entry["trace_id"] == "4bf92f3577b34da6a3ce929d0e0e4736"
    assert entry["span_id"] == "00f067aa0ba902b7"
    assert "service" not in json.loads(fmt.format(_record("outside a call")))

    # a bridged Rust record carries the ids of its own span, not the current one
    bridged = _record("rust", trace_id="aa" * 16, span_id="bb" * 8, rpc_service="S", aster_origin="rust")
    with tracing.with_trace_context(TRACEPARENT):
        entry = json.loads(fmt.format(bridged))
    assert entry["trace_id"] == "aa" * 16 and entry["rpc_service"] == "S"
    text = aster_logging.TextFormatter(use_color=False).format(bridged)
    assert "trace=aaaaaaaa" in text


def test_otlp_log_handler_skips_rust_records_and_never_raises(monkeypatch):
    sent = []
    monkeypatch.setattr(_aster, "log_emit", lambda *a, **k: sent.append(a))
    h = aster_logging.AsterLogHandler()
    h.emit(_record("from rust", aster_origin="rust"))
    assert sent == []
    with tracing.with_trace_context(TRACEPARENT):
        h.emit(_record("from the app"))
    assert len(sent) == 1
    level, name, message, _fields, _ts, traceparent = sent[0]
    assert (level, name, message, traceparent) == (3, "app", "from the app", TRACEPARENT)
    # no logs exporter configured: the real call is a quiet no-op
    monkeypatch.undo()
    errors = []
    h.handleError = lambda record: errors.append(record)  # type: ignore[method-assign]
    h.emit(_record("quiet"))
    assert errors == []


def test_directive_for_levels():
    assert _logbridge._directive(logging.DEBUG) == "warn,aster=debug"
    assert _logbridge._directive(logging.INFO) == "warn,aster=info"
    assert _logbridge._directive(logging.WARNING) == "warn,aster=warn"
    assert _logbridge._directive(logging.CRITICAL) == "warn,aster=error"
    assert _logbridge._directive(1) == "warn,aster=trace"


def test_python_side_metrics_and_health_are_gone():
    import importlib.util

    assert importlib.util.find_spec("aster.health") is None
    assert importlib.util.find_spec("aster.interceptors.metrics") is None
    for name in ("MetricsInterceptor", "check_health", "check_ready", "metrics_snapshot"):
        assert not hasattr(aster, name), name
    assert not hasattr(aster.interceptors, "MetricsInterceptor")
    assert not hasattr(Node, "transport_metrics_prometheus")
    assert HealthServer.__module__ == "aster._aster"


# -- lifecycle: shutdown, re-init, fork, exit -------------------------------------


def test_shutdown_and_reinit_in_one_process():
    out = child_json(
        """
        import asyncio, json, time
        from aster import AsterConfig, HealthServer, Node, metrics, observability, _aster
        import logging

        got = []
        class C(logging.Handler):
            def emit(self, r): got.append(r)
        lg = logging.getLogger("aster")
        lg.addHandler(C(0)); lg.setLevel(logging.INFO)

        async def serve_once():
            node = await Node.start(AsterConfig())
            hs = await HealthServer.start(node, "127.0.0.1", 0)
            text = metrics.prometheus_text()
            await hs.stop(); await node.shutdown()
            return text

        async def wait_listener(n):
            end = time.time() + 10
            while time.time() < end:
                if sum(1 for r in got if "listener started" in r.getMessage()) >= n:
                    return True
                await asyncio.sleep(0.05)
            return False

        async def main():
            res = {}
            observability.init(service_name="first", traces=False, metrics=False)
            counter = metrics.counter("o6.reinit.count")
            counter.add(1)
            observability.shutdown()
            counter.add(1)  # no pipeline: a quiet no-op, the handle was freed
            res["refs_after_shutdown"] = _aster.observability_refs()
            res["state_after_shutdown"] = _aster.observability_state()

            observability.init(service_name="second", traces=False, metrics=False)  # new config: fine
            node = await Node.start(AsterConfig())
            hs = await HealthServer.start(node, "127.0.0.1", 0)
            counter.add(4)  # re-declares itself (recorded once the pull reader exists)
            text = metrics.prometheus_text()
            await hs.stop(); await node.shutdown()
            res["value"] = [ln for ln in text.splitlines() if ln.startswith("o6_reinit_count_total")][0].rsplit(" ", 1)[1]
            res["second_drain_delivers"] = await wait_listener(1)
            try:
                observability.init(service_name="third", traces=False, metrics=False)
                res["conflict"] = None
            except ValueError as e:
                res["conflict"] = "service" in str(e) or "differ" in str(e)
            print(json.dumps(res))

        asyncio.run(main())
        """
    )
    assert out["refs_after_shutdown"] == 0 and out["state_after_shutdown"] is None
    assert float(out["value"]) == 4
    assert out["second_drain_delivers"] is True
    assert out["conflict"] is True


@needs_fork
def test_fork_before_init_child_gets_its_own_pipeline_and_drain():
    out = child_json(
        """
        import asyncio, json, logging, os, sys, time, traceback
        import aster  # imported, not initialised: forking now is the supported model

        pid = os.fork()
        if pid == 0:
            try:
                from aster import AsterConfig, HealthServer, Node, observability

                got = []
                class C(logging.Handler):
                    def emit(self, r): got.append(r)

                async def child():
                    observability.ensure_started()
                    lg = logging.getLogger("aster")
                    lg.addHandler(C(0)); lg.setLevel(logging.INFO)
                    from aster import _logbridge
                    _logbridge.sync_level()
                    node = await Node.start(AsterConfig())
                    hs = await HealthServer.start(node, "127.0.0.1", 0)
                    end = time.time() + 10
                    while time.time() < end and not any("listener started" in r.getMessage() for r in got):
                        await asyncio.sleep(0.05)
                    await hs.stop(); await node.shutdown()
                    return any("listener started" in r.getMessage() for r in got)

                os._exit(0 if asyncio.run(child()) else 3)
            except BaseException:
                traceback.print_exc()
                os._exit(2)
        _, status = os.waitpid(pid, 0)
        print(json.dumps({"child_exit": os.waitstatus_to_exitcode(status)}))
        """
    )
    assert out == {"child_exit": 0}


@needs_fork
def test_fork_after_init_child_is_inert_and_does_not_hang():
    proc = run_child(
        """
        import json, os
        from aster import metrics, observability, tracing

        observability.init(traces=False)
        counter = metrics.counter("o6.fork.count")
        counter.add(1)

        pid = os.fork()
        if pid == 0:
            code = 0
            try:
                observability.init(traces=False)  # inert handle, one warning
                counter.add(1)
                with tracing.span("child.work") as s:
                    s.set_attribute("a", "b")
                observability.flush(0.5)
                observability.shutdown(timeout=0.5)
            except BaseException as e:
                print("child error:", repr(e), flush=True)
                code = 2
            os._exit(code)
        _, status = os.waitpid(pid, 0)
        counter.add(1)  # the parent is unaffected
        observability.shutdown()
        print(json.dumps({"child_exit": os.waitstatus_to_exitcode(status)}))
        """,
        timeout=60,
    )
    assert proc.returncode == 0, f"{proc.stdout}\n{proc.stderr}"
    assert json.loads(proc.stdout.strip().splitlines()[-1]) == {"child_exit": 0}
    assert "forked after" in proc.stderr  # the one-time warning from aster-otel


def test_interpreter_exit_joins_the_drain_thread():
    """Records flowing while the interpreter exits: the atexit handler stops
    the drain, so exit neither hangs nor crashes on a late attach."""
    proc = run_child(
        """
        import asyncio, logging
        from aster import AsterConfig, HealthServer, Node, observability

        async def main():
            observability.init(log_filter="warn,aster=trace", traces=False)
            logging.getLogger("aster").setLevel(5)
            node = await Node.start(AsterConfig())
            for _ in range(5):
                hs = await HealthServer.start(node, "127.0.0.1", 0)
                await hs.stop()
            print("done", flush=True)

        asyncio.run(main())
        """,
        timeout=60,
    )
    assert proc.returncode == 0, f"{proc.stdout}\n{proc.stderr}"
    assert proc.stdout.strip().endswith("done")
