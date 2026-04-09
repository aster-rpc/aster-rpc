#!/usr/bin/env python3
"""Mission Control server (producer).

Chapters 1-4: dev mode (open gate, ephemeral keys).

    python -m mission_control.server

Chapter 5: production mode with auth.

    ASTER_ROOT_PUBKEY_FILE=~/.aster/root.pub \
    python -m mission_control.server --auth
"""

import argparse
import asyncio

from aster import AsterConfig, AsterServer


async def run(auth: bool = False) -> None:
    config = AsterConfig.from_env()

    if auth:
        from .services_auth import AgentSession, MissionControl
        config.allow_all_consumers = False
    else:
        from .services import AgentSession, MissionControl

    async with AsterServer(
        services=[MissionControl(), AgentSession()],
        config=config,
    ) as srv:
        print(srv.address)
        await srv.serve()


def main() -> None:
    parser = argparse.ArgumentParser(description="Mission Control server")
    parser.add_argument(
        "--auth", action="store_true",
        help="Require enrollment credentials (Chapter 5)",
    )
    args = parser.parse_args()
    asyncio.run(run(auth=args.auth))


if __name__ == "__main__":
    main()
