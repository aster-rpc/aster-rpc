#!/usr/bin/env python3
"""Mission Control server (producer).

Chapters 1-4: dev mode (no enrollment configured: an open server).

    python -m mission_control.server

Chapter 5: auth. The same services with ``requires=`` on each method (roles
in ``roles.py``). Nothing in the code reads a credential: ``AsterConfig``
switches Enrollment on, the server admits only nodes holding a credential
from your root, and each peer's ``aster.role`` claim becomes the role set
``requires=`` is checked against. See README.md for the ``aster`` CLI steps.

    ASTER_ROOT_PUBKEY=$ROOT \\
    ASTER_ENROLLMENT_AUDIENCE=mission-control \\
    ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET \\
    ASTER_SECRET_KEY_FILE=server.key \\
    ASTER_ENROLLMENT_CREDENTIAL=server.cred \\
    ASTER_ENROLLMENT_STATE_PATH=./mc-enrollment.json \\
    python -m mission_control.server --auth

or ``--config aster.toml`` with the same settings in ``[trust]``,
``[network]`` and ``[enrollment]``.
"""

from __future__ import annotations

import argparse
import asyncio
import sys

from aster import AsterConfig, AsterServer


def load_config(path: str | None) -> AsterConfig:
    return AsterConfig.from_file(path) if path else AsterConfig.from_env()


async def run(auth: bool = False, config_path: str | None = None) -> None:
    config = load_config(config_path)

    if auth:
        if not config.is_enrollment_enabled:
            sys.exit(
                "--auth needs enrollment configured: set ASTER_ROOT_PUBKEY, "
                "ASTER_ENROLLMENT_AUDIENCE, ASTER_ENROLLMENT_FLEET_NAMESPACE and "
                "ASTER_ENROLLMENT_CREDENTIAL (or [trust]/[enrollment] in --config). "
                "See examples/python/mission_control/README.md."
            )
        from .services_auth import AgentSession, MissionControl
    else:
        from .services import AgentSession, MissionControl

    async with AsterServer(
        services=[MissionControl(), AgentSession()],
        config=config,
    ) as srv:
        print(srv.address, flush=True)
        await srv.serve()


def main() -> None:
    parser = argparse.ArgumentParser(description="Mission Control server")
    parser.add_argument(
        "--auth", action="store_true",
        help="Serve the role-gated services; requires enrollment configured (Chapter 5)",
    )
    parser.add_argument(
        "--config", default=None, metavar="PATH",
        help="aster.toml to load (default: ASTER_* environment variables)",
    )
    args = parser.parse_args()
    asyncio.run(run(auth=args.auth, config_path=args.config))


if __name__ == "__main__":
    main()
