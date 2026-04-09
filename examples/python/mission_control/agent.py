#!/usr/bin/env python3
"""Example agent that connects to Mission Control.

Demonstrates proxy client usage (Chapters 1-3):
  - getStatus (unary)
  - submitLog (unary)
  - ingestMetrics (client streaming)

Usage:
    python -m mission_control.agent <server-address>
"""

import asyncio
import sys
import time
from random import random

from aster import AsterClient


async def run(address: str) -> None:
    client = AsterClient(address=address)
    await client.connect()
    mc = client.proxy("MissionControl")

    # Chapter 1: check in
    status = await mc.getStatus({"agent_id": "py-agent-1"})
    print(f"Status: {status}")

    # Chapter 2: push a log entry
    await mc.submitLog({
        "timestamp": time.time(),
        "level": "info",
        "message": "agent started",
        "agent_id": "py-agent-1",
    })
    print("Log submitted")

    # Chapter 3: stream 1000 metrics
    async def metrics():
        for _ in range(1000):
            yield {
                "name": "cpu.usage",
                "value": random() * 100,
                "timestamp": time.time(),
            }

    result = await mc.ingestMetrics(metrics())
    print(f"Metrics accepted: {result['accepted']}")

    await client.close()


def main() -> None:
    if len(sys.argv) < 2:
        print("Usage: python -m mission_control.agent <server-address>")
        sys.exit(1)
    asyncio.run(run(sys.argv[1]))


if __name__ == "__main__":
    main()
