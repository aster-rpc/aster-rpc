#!/usr/bin/env python3
"""Example operator that tails logs from Mission Control.

Demonstrates server streaming (Chapter 2):
  - tailLogs streams log entries as they arrive

Usage:
    python -m mission_control.operator <server-address>

Press Ctrl+C to stop tailing.
"""

import asyncio
import sys

from aster import AsterClient


async def run(address: str) -> None:
    client = AsterClient(address=address)
    await client.connect()
    mc = await client.proxy("MissionControl")

    print("Tailing logs (Ctrl+C to stop)...")
    try:
        async for entry in mc.tailLogs.stream({"level": "info"}):
            ts = entry.get("timestamp", 0)
            level = entry.get("level", "?")
            msg = entry.get("message", "")
            agent = entry.get("agent_id", "")
            print(f"  [{level:>5}] {agent}: {msg}  (t={ts:.1f})")
    except KeyboardInterrupt:
        pass

    await client.close()


def main() -> None:
    if len(sys.argv) < 2:
        print("Usage: python -m mission_control.operator <server-address>")
        sys.exit(1)
    asyncio.run(run(sys.argv[1]))


if __name__ == "__main__":
    main()
