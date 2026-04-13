"""
tests/python/test_aster_handler_context.py

Verify that @rpc handlers can receive a CallContext as an explicit
parameter, and that CallContext.current() returns the active context
inside the handler even when it isn't declared as a parameter.

Spec reference: ffi_spec/handler-context-design.md
"""

from __future__ import annotations

from dataclasses import dataclass

import pytest

from aster.codec import wire_type
from aster.decorators import rpc, service
from aster.interceptors import CallContext
from aster.rpc_types import SerializationMode
from aster.status import RpcError, StatusCode
from aster.testing import AsterTestHarness


@wire_type("test.handler_ctx/Req")
@dataclass
class Req:
    value: str = ""


@wire_type("test.handler_ctx/Resp")
@dataclass
class Resp:
    peer: str = ""
    service: str = ""
    method: str = ""
    attr_role: str = ""
    meta_x: str = ""
    via_currentfn: bool = False


@service(name="HandlerCtxService", version=1, serialization=[SerializationMode.XLANG])
class HandlerCtxService:

    @rpc
    async def with_explicit_ctx(self, req: Req, ctx: CallContext) -> Resp:
        # Also confirm CallContext.current() matches the injected ctx
        current = CallContext.current()
        return Resp(
            peer=(ctx.peer or ""),
            service=ctx.service,
            method=ctx.method,
            attr_role=ctx.attributes.get("role", ""),
            meta_x=ctx.metadata.get("x", ""),
            via_currentfn=(current is ctx),
        )

    @rpc
    async def with_implicit_ctx(self, req: Req) -> Resp:
        # Handler did not declare ctx param; retrieve via contextvar.
        ctx = CallContext.current()
        assert ctx is not None, "CallContext.current() must be set inside handler"
        return Resp(
            peer=(ctx.peer or ""),
            service=ctx.service,
            method=ctx.method,
            via_currentfn=True,
        )

    @rpc
    async def unauthorized_unless_ops(self, req: Req, ctx: CallContext) -> Resp:
        if "ops" not in ctx.attributes.get("roles", ""):
            raise RpcError(StatusCode.PERMISSION_DENIED, "ops role required")
        return Resp(service=ctx.service, method=ctx.method)


@pytest.mark.asyncio
async def test_explicit_ctx_param_is_injected():
    harness = AsterTestHarness()
    client, _impl = await harness.create_local_pair(
        HandlerCtxService, HandlerCtxService(), wire_compatible=True,
    )

    resp: Resp = await client.with_explicit_ctx(Req(value="hi"))
    assert resp.service == "HandlerCtxService"
    assert resp.method == "with_explicit_ctx"
    assert resp.via_currentfn is True


@pytest.mark.asyncio
async def test_implicit_ctx_via_current():
    harness = AsterTestHarness()
    client, _impl = await harness.create_local_pair(
        HandlerCtxService, HandlerCtxService(), wire_compatible=True,
    )

    resp: Resp = await client.with_implicit_ctx(Req(value="hi"))
    assert resp.service == "HandlerCtxService"
    assert resp.method == "with_implicit_ctx"
    assert resp.via_currentfn is True


@pytest.mark.asyncio
async def test_handler_can_raise_permission_denied():
    harness = AsterTestHarness()
    client, _impl = await harness.create_local_pair(
        HandlerCtxService, HandlerCtxService(), wire_compatible=True,
    )

    with pytest.raises(RpcError) as exc:
        await client.unauthorized_unless_ops(Req(value="hi"))
    assert exc.value.code == StatusCode.PERMISSION_DENIED


def test_method_info_accepts_ctx_flag():
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(HandlerCtxService, _SERVICE_INFO_ATTR)
    assert info.methods["with_explicit_ctx"].accepts_ctx is True
    assert info.methods["with_implicit_ctx"].accepts_ctx is False
    assert info.methods["unauthorized_unless_ops"].accepts_ctx is True
    # Request type extraction must skip the CallContext param.
    assert info.methods["with_explicit_ctx"].request_type is Req
