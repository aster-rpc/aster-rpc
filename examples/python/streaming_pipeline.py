"""
Streaming Pipeline — Aster RPC Example.

Demonstrates all four RPC streaming patterns in a data-processing pipeline:

  1. ``@server_stream`` — Subscribe to a live data feed (server pushes items)
  2. ``@client_stream`` — Upload a batch of records (client pushes, server aggregates)
  3. ``@bidi_stream``   — Interactive transform (client sends items, server echoes
                          transformed results concurrently)
  4. ``@rpc``           — Unary call for comparison (single request/response)

Usage (two terminals):

  # Terminal 1 — producer
  python streaming_pipeline.py producer

  # Terminal 2 — consumer
  ASTER_ENDPOINT_ADDR=<printed by producer> python streaming_pipeline.py consumer
"""
from __future__ import annotations

import asyncio
import os
import sys
from dataclasses import dataclass
from typing import AsyncIterator

from aster import AsterServer, AsterClient
from aster.codec import wire_type
from aster.decorators import service, rpc, server_stream, client_stream, bidi_stream


# ── Message types ────────────────────────────────────────────────────────────


@wire_type("example.pipeline/StatusRequest")
@dataclass
class StatusRequest:
    """Empty request for the status check."""
    pass


@wire_type("example.pipeline/StatusResponse")
@dataclass
class StatusResponse:
    service_name: str = ""
    methods_available: int = 0


@wire_type("example.pipeline/SubscribeRequest")
@dataclass
class SubscribeRequest:
    topic: str = ""
    max_items: int = 5


@wire_type("example.pipeline/DataPoint")
@dataclass
class DataPoint:
    timestamp: int = 0
    value: float = 0.0
    label: str = ""


@wire_type("example.pipeline/Record")
@dataclass
class Record:
    key: str = ""
    value: float = 0.0


@wire_type("example.pipeline/BatchResult")
@dataclass
class BatchResult:
    count: int = 0
    total: float = 0.0
    average: float = 0.0


@wire_type("example.pipeline/TransformItem")
@dataclass
class TransformItem:
    input_value: str = ""


@wire_type("example.pipeline/TransformedItem")
@dataclass
class TransformedItem:
    original: str = ""
    transformed: str = ""
    sequence: int = 0


# ── Service definition ───────────────────────────────────────────────────────


@service("DataPipeline")
class DataPipelineService:
    """A data processing pipeline demonstrating all streaming patterns."""

    @rpc
    async def status(self, req: StatusRequest) -> StatusResponse:
        """Unary: simple health check."""
        return StatusResponse(service_name="DataPipeline", methods_available=4)

    @server_stream
    async def subscribe(self, req: SubscribeRequest) -> AsyncIterator[DataPoint]:
        """Server stream: push a live data feed to the client.

        The server generates data points and yields them one by one.
        The client receives them as an async iterator.
        """
        import time
        print(f"  [server] Client subscribed to topic={req.topic!r}, max_items={req.max_items}")
        for i in range(req.max_items):
            point = DataPoint(
                timestamp=int(time.time() * 1000),
                value=float(i * 1.5 + 0.1),
                label=f"{req.topic}-{i}",
            )
            yield point
            await asyncio.sleep(0.1)  # simulate real-time data
        print(f"  [server] Finished streaming {req.max_items} data points")

    @client_stream
    async def upload_batch(self, records: AsyncIterator[Record]) -> BatchResult:
        """Client stream: receive a batch of records, return aggregate stats.

        The client sends multiple Record messages; the server accumulates them
        and returns a single BatchResult when the client finishes.
        """
        count = 0
        total = 0.0
        async for record in records:
            count += 1
            total += record.value
            print(f"  [server] Received record: key={record.key}, value={record.value}")
        average = total / count if count > 0 else 0.0
        print(f"  [server] Batch complete: {count} records, total={total:.2f}, avg={average:.2f}")
        return BatchResult(count=count, total=total, average=average)

    @bidi_stream
    async def transform(
        self, items: AsyncIterator[TransformItem]
    ) -> AsyncIterator[TransformedItem]:
        """Bidi stream: transform items as they arrive.

        For each incoming item, the server yields a transformed version.
        Both sides can send/receive concurrently.
        """
        seq = 0
        async for item in items:
            seq += 1
            transformed = TransformedItem(
                original=item.input_value,
                transformed=item.input_value.upper().replace(" ", "_"),
                sequence=seq,
            )
            print(f"  [server] Transform #{seq}: {item.input_value!r} -> {transformed.transformed!r}")
            yield transformed


# ── Producer ─────────────────────────────────────────────────────────────────


async def run_producer() -> None:
    async with AsterServer(services=[DataPipelineService()]) as srv:
        print()
        print("=== Data Pipeline Producer ===")
        print(f"  endpoint_addr : {srv.endpoint_addr_b64}")
        print()
        print("  Run consumer with:")
        print(f"    ASTER_ENDPOINT_ADDR={srv.endpoint_addr_b64} python streaming_pipeline.py consumer")
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
        print(f"[consumer] Connected. Services: {[s.name for s in c.services]}")
        pipeline = await c.client(DataPipelineService)

        # ── 1. Unary RPC ────────────────────────────────────────────────────
        print("\n--- 1. Unary: status check ---")
        status = await pipeline.status(StatusRequest())
        print(f"  Service: {status.service_name}, methods: {status.methods_available}")

        # ── 2. Server stream: subscribe to a data feed ──────────────────────
        print("\n--- 2. Server stream: subscribe to live data ---")
        data_points = []
        async for point in pipeline.subscribe(
            SubscribeRequest(topic="sensors", max_items=5)
        ):
            data_points.append(point)
            print(f"  Received: ts={point.timestamp}, value={point.value:.1f}, label={point.label}")
        print(f"  Total received: {len(data_points)} data points")

        # ── 3. Client stream: upload a batch of records ─────────────────────
        print("\n--- 3. Client stream: upload batch ---")

        async def generate_records() -> AsyncIterator[Record]:
            """Generate a batch of records to upload."""
            for i in range(5):
                record = Record(key=f"item-{i}", value=float(i * 10 + 5))
                print(f"  Sending: key={record.key}, value={record.value}")
                yield record

        result = await pipeline.upload_batch(generate_records())
        print(f"  Batch result: count={result.count}, total={result.total:.2f}, avg={result.average:.2f}")

        # ── 4. Bidi stream: interactive transform ───────────────────────────
        print("\n--- 4. Bidi stream: interactive transform ---")
        channel = pipeline.transform()

        async with channel:
            # Send items
            words = ["hello world", "aster rpc", "bidi stream", "peer to peer"]
            for word in words:
                await channel.send(TransformItem(input_value=word))
                print(f"  Sent: {word!r}")

            # Signal we are done sending
            await channel.close()

            # Receive transformed items
            print("  --- Transformed results ---")
            while True:
                try:
                    item = await channel.recv()
                    print(f"  #{item.sequence}: {item.original!r} -> {item.transformed!r}")
                except Exception:
                    break

    print("\n[consumer] Done.")


# ── Entry point ──────────────────────────────────────────────────────────────


def main() -> None:
    if len(sys.argv) < 2 or sys.argv[1] not in ("producer", "consumer"):
        print("Usage: python streaming_pipeline.py <producer|consumer>")
        print()
        print("  producer  — start the data pipeline service")
        print("  consumer  — connect and exercise all streaming patterns")
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
