"""
Multiple Services on One Endpoint — Aster RPC Example.

Demonstrates registering multiple independent services on a single
``AsterServer``.  All services share one QUIC endpoint and one node ID,
so the consumer discovers them all in a single admission handshake and
can call any of them.

Key concepts:
  - Pass multiple service instances to ``AsterServer(services=[...])``
  - Consumer uses ``await c.client(ServiceClass)`` to get a typed stub
    for each service
  - Each service has its own contract ID, version, and methods
  - Services can use different options (e.g., timeouts, idempotency)

Usage (two terminals):

  # Terminal 1 — producer
  python multi_service.py producer

  # Terminal 2 — consumer
  ASTER_ENDPOINT_ADDR=<printed by producer> python multi_service.py consumer
"""
from __future__ import annotations

import asyncio
import math
import os
import sys
import time
from dataclasses import dataclass
from typing import AsyncIterator

from aster import AsterServer, AsterClient
from aster.codec import wire_type
from aster.decorators import service, rpc, server_stream


# ═══════════════════════════════════════════════════════════════════════════════
# Service 1: Calculator
# ═══════════════════════════════════════════════════════════════════════════════


@wire_type("example.calc/CalcRequest")
@dataclass
class CalcRequest:
    a: float = 0.0
    b: float = 0.0
    op: str = "add"


@wire_type("example.calc/CalcResponse")
@dataclass
class CalcResponse:
    result: float = 0.0
    expression: str = ""


@service("Calculator", version=1)
class CalculatorService:
    """Basic arithmetic service."""

    @rpc(idempotent=True)
    async def calculate(self, req: CalcRequest) -> CalcResponse:
        ops = {
            "add": lambda a, b: a + b,
            "sub": lambda a, b: a - b,
            "mul": lambda a, b: a * b,
            "div": lambda a, b: a / b if b != 0 else float("inf"),
        }
        fn = ops.get(req.op, ops["add"])
        result = fn(req.a, req.b)
        return CalcResponse(
            result=result,
            expression=f"{req.a} {req.op} {req.b} = {result}",
        )


# ═══════════════════════════════════════════════════════════════════════════════
# Service 2: TimeService
# ═══════════════════════════════════════════════════════════════════════════════


@wire_type("example.time/TimeRequest")
@dataclass
class TimeRequest:
    timezone: str = "UTC"


@wire_type("example.time/TimeResponse")
@dataclass
class TimeResponse:
    epoch_ms: int = 0
    formatted: str = ""


@wire_type("example.time/TickRequest")
@dataclass
class TickRequest:
    count: int = 3
    interval_ms: int = 500


@wire_type("example.time/TickEvent")
@dataclass
class TickEvent:
    sequence: int = 0
    epoch_ms: int = 0


@service("TimeService", version=1)
class TimeService:
    """Time and clock utilities."""

    @rpc(idempotent=True)
    async def now(self, req: TimeRequest) -> TimeResponse:
        """Get the current time."""
        now_ms = int(time.time() * 1000)
        return TimeResponse(
            epoch_ms=now_ms,
            formatted=time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime()),
        )

    @server_stream
    async def tick(self, req: TickRequest) -> AsyncIterator[TickEvent]:
        """Stream periodic tick events."""
        for i in range(1, req.count + 1):
            yield TickEvent(sequence=i, epoch_ms=int(time.time() * 1000))
            if i < req.count:
                await asyncio.sleep(req.interval_ms / 1000.0)


# ═══════════════════════════════════════════════════════════════════════════════
# Service 3: KeyValueStore
# ═══════════════════════════════════════════════════════════════════════════════


@wire_type("example.kv/GetRequest")
@dataclass
class GetRequest:
    key: str = ""


@wire_type("example.kv/GetResponse")
@dataclass
class GetResponse:
    key: str = ""
    value: str = ""
    found: bool = False


@wire_type("example.kv/PutRequest")
@dataclass
class PutRequest:
    key: str = ""
    value: str = ""


@wire_type("example.kv/PutResponse")
@dataclass
class PutResponse:
    key: str = ""
    was_update: bool = False


@service("KeyValueStore", version=1)
class KeyValueStoreService:
    """Simple in-memory key-value store."""

    def __init__(self) -> None:
        self._store: dict[str, str] = {}

    @rpc
    async def get(self, req: GetRequest) -> GetResponse:
        value = self._store.get(req.key)
        return GetResponse(
            key=req.key,
            value=value or "",
            found=value is not None,
        )

    @rpc
    async def put(self, req: PutRequest) -> PutResponse:
        was_update = req.key in self._store
        self._store[req.key] = req.value
        return PutResponse(key=req.key, was_update=was_update)


# ── Producer ─────────────────────────────────────────────────────────────────


async def run_producer() -> None:
    # Register all three services on a single endpoint.
    services = [
        CalculatorService(),
        TimeService(),
        KeyValueStoreService(),
    ]

    async with AsterServer(services=services) as srv:
        print()
        print("=== Multi-Service Producer ===")
        print(f"  endpoint_addr : {srv.endpoint_addr_b64}")
        print()
        print("  Registered services:")
        for s in srv.services:
            print(f"    - {s.name} v{s.version} (contract_id: {s.contract_id[:16]}...)")
        print()
        print("  Run consumer with:")
        print(f"    ASTER_ENDPOINT_ADDR={srv.endpoint_addr_b64} python multi_service.py consumer")
        print()
        print("  Waiting for connections... (Ctrl+C to stop)")
        try:
            await srv.serve()
        except (KeyboardInterrupt, asyncio.CancelledError):
            pass
    print("\n[producer] Stopped.")


# ── Consumer ─────────────────────────────────────────────────────────────────


async def run_consumer() -> None:
    async with AsterClient() as c:
        # All services are discovered in one admission handshake.
        print(f"[consumer] Connected. Services: {[s.name for s in c.services]}")

        # ── Calculator ──────────────────────────────────────────────────────
        print("\n--- Calculator Service ---")
        calc = await c.client(CalculatorService)

        for op, a, b in [("add", 10, 3), ("sub", 10, 3), ("mul", 7, 6), ("div", 22, 7)]:
            resp = await calc.calculate(CalcRequest(a=a, b=b, op=op))
            print(f"  {resp.expression}")

        # ── TimeService ─────────────────────────────────────────────────────
        print("\n--- Time Service ---")
        ts = await c.client(TimeService)

        now = await ts.now(TimeRequest(timezone="UTC"))
        print(f"  Current time: {now.formatted} ({now.epoch_ms}ms)")

        print("  Tick stream (3 ticks at 200ms):")
        async for tick in ts.tick(TickRequest(count=3, interval_ms=200)):
            print(f"    tick #{tick.sequence} at {tick.epoch_ms}ms")

        # ── KeyValueStore ───────────────────────────────────────────────────
        print("\n--- Key-Value Store ---")
        kv = await c.client(KeyValueStoreService)

        # Put some values
        for key, val in [("name", "Aster"), ("version", "0.2.0"), ("lang", "Python")]:
            resp = await kv.put(PutRequest(key=key, value=val))
            print(f"  put({key!r}, {val!r}) -> was_update={resp.was_update}")

        # Read them back
        for key in ["name", "version", "lang", "missing"]:
            resp = await kv.get(GetRequest(key=key))
            if resp.found:
                print(f"  get({key!r}) -> {resp.value!r}")
            else:
                print(f"  get({key!r}) -> NOT FOUND")

    print("\n[consumer] Done.")


# ── Entry point ──────────────────────────────────────────────────────────────


def main() -> None:
    if len(sys.argv) < 2 or sys.argv[1] not in ("producer", "consumer"):
        print("Usage: python multi_service.py <producer|consumer>")
        print()
        print("  producer  — start all three services on one endpoint")
        print("  consumer  — connect and call all services")
        sys.exit(1)

    role = sys.argv[1]
    try:
        if role == "producer":
            asyncio.run(run_producer())
        else:
            asyncio.run(run_consumer())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
