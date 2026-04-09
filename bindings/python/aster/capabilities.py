"""
aster.capabilities -- Convenience functions for capability requirements.

Use these with the ``requires`` parameter on ``@rpc``, ``@server_stream``,
etc. to define method-level access control::

    from aster import rpc, any_of, all_of

    class Role(str, Enum):
        STATUS = "ops.status"
        LOGS   = "ops.logs"
        ADMIN  = "ops.admin"

    @rpc(requires=Role.STATUS)
    async def get_status(self, req): ...

    @rpc(requires=any_of(Role.LOGS, Role.ADMIN))
    async def tail_logs(self, req): ...

    @rpc(requires=all_of(Role.STATUS, Role.ADMIN))
    async def admin_status(self, req): ...
"""

from __future__ import annotations

from aster.contract.identity import CapabilityKind, CapabilityRequirement


def any_of(*roles: str) -> CapabilityRequirement:
    """Require ANY ONE of the listed roles (OR logic).

    The caller is admitted if they have at least one of the specified roles.

    Example::

        @rpc(requires=any_of("ops.logs", "ops.admin"))
        async def tail_logs(self, req): ...
    """
    return CapabilityRequirement(
        kind=CapabilityKind.ANY_OF,
        roles=list(roles),
    )


def all_of(*roles: str) -> CapabilityRequirement:
    """Require ALL of the listed roles (AND logic).

    The caller is admitted only if they have every specified role.

    Example::

        @rpc(requires=all_of("ops.status", "ops.admin"))
        async def admin_status(self, req): ...
    """
    return CapabilityRequirement(
        kind=CapabilityKind.ALL_OF,
        roles=list(roles),
    )
