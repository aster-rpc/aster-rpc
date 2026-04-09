"""Capability interceptor -- Gate 3 method-level access control.

Evaluates ``CapabilityRequirement`` from the service and method definitions
against the caller's admission attributes (``CallContext.attributes``).

Both the service-level ``requires`` (baseline) and the method-level
``requires`` must be satisfied (conjunction).  If neither is defined the
call passes through unconditionally.
"""

from __future__ import annotations

import logging
from typing import Any

from aster.contract.identity import CapabilityRequirement
from aster.interceptors.base import CallContext, Interceptor
from aster.service import ServiceInfo
from aster.status import RpcError, StatusCode
from aster.trust.rcan import evaluate_capability

logger = logging.getLogger(__name__)


def _normalize_requirement(req: Any) -> CapabilityRequirement | None:
    """Normalize a requires= value to a CapabilityRequirement.

    Accepts:
    - None -> None
    - CapabilityRequirement -> pass through
    - str or str enum -> wrap as ROLE requirement
    """
    if req is None:
        return None
    if isinstance(req, CapabilityRequirement):
        return req
    if isinstance(req, str):
        from aster.contract.identity import CapabilityKind
        return CapabilityRequirement(kind=CapabilityKind.ROLE, roles=[str(req)])
    return req


class CapabilityInterceptor(Interceptor):
    """Enforces capability requirements on incoming RPC calls.

    Constructed with a mapping of service names to their ``ServiceInfo`` so
    that both service-level and method-level ``requires`` can be looked up
    at request time.
    """

    def __init__(self, service_map: dict[str, ServiceInfo]) -> None:
        self._service_map = service_map

    async def on_request(self, ctx: CallContext, request: object) -> object:
        svc_info = self._service_map.get(ctx.service)
        if svc_info is None:
            # Service not known to this interceptor -- pass through.
            return request

        # Service-level baseline requirement.
        svc_req = _normalize_requirement(svc_info.requires)
        # Method-level requirement.
        method_req = None
        method_info = svc_info.get_method(ctx.method)
        if method_info is not None:
            method_req = _normalize_requirement(method_info.requires)

        # No requirements at all -- pass through.
        if svc_req is None and method_req is None:
            return request

        # Evaluate service-level requirement.
        if svc_req is not None:
            if not evaluate_capability(svc_req, ctx.attributes):
                logger.warning(
                    "Capability denied: service=%s method=%s peer=%s "
                    "(service-level requirement not met)",
                    ctx.service, ctx.method, ctx.peer,
                )
                raise RpcError(
                    StatusCode.PERMISSION_DENIED,
                    f"capability check failed for service '{ctx.service}'",
                )

        # Evaluate method-level requirement.
        if method_req is not None:
            if not evaluate_capability(method_req, ctx.attributes):
                logger.warning(
                    "Capability denied: service=%s method=%s peer=%s "
                    "(method-level requirement not met)",
                    ctx.service, ctx.method, ctx.peer,
                )
                raise RpcError(
                    StatusCode.PERMISSION_DENIED,
                    f"capability check failed for method '{ctx.service}.{ctx.method}'",
                )

        return request
