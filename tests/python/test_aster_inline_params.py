"""
tests/python/test_aster_inline_params.py

Mode 2 (inline request params) tests:
- Detection + synthesis at decoration time
- Dispatch round-trip via LocalTransport
- Contract-id equivalence between Mode 1 (explicit) and Mode 2 (inline)
- Mixed with CallContext injection
- No-request methods
- Optional nested @wire_type fields

Spec reference: ffi_spec/handler-context-design.md §"Inline Request Parameters"
"""

from __future__ import annotations

import dataclasses
from typing import Optional

import pytest

from aster.codec import wire_type
from aster.contract.identity import contract_id_from_service
from aster.decorators import rpc, service
from aster.interceptors import CallContext
from aster.rpc_types import SerializationMode
from aster.testing import AsterTestHarness


# ── Wire types ────────────────────────────────────────────────────────────────


@wire_type("test.inline/StatusResponse")
@dataclasses.dataclass
class StatusResponse:
    agent_id: str = ""
    status: str = ""
    via_currentfn: bool = False


@wire_type("test.inline/AgentConfig")
@dataclasses.dataclass
class AgentConfig:
    name: str = ""
    max_parallel: int = 0


# ── Mode 2 service: all the shapes ────────────────────────────────────────────


@service(name="InlineSvc", version=1, serialization=[SerializationMode.XLANG])
class InlineSvc:

    @rpc
    async def get_status(self, agent_id: str) -> StatusResponse:
        # Sole primitive param → Mode 2
        return StatusResponse(agent_id=agent_id, status="running")

    @rpc
    async def mixed(self, agent_id: str, count: int) -> StatusResponse:
        # Two primitives → Mode 2
        return StatusResponse(agent_id=f"{agent_id}#{count}", status="ok")

    @rpc
    async def no_request(self) -> StatusResponse:
        # Zero params → Mode 2 with empty synthesized request
        return StatusResponse(agent_id="", status="empty")

    @rpc
    async def with_ctx(self, agent_id: str, ctx: CallContext) -> StatusResponse:
        # Mode 2 + CallContext injection -- ctx should be excluded from the
        # synthesized request and dispatch should still inject it.
        assert ctx is CallContext.current()
        return StatusResponse(
            agent_id=agent_id,
            status=ctx.method,
            via_currentfn=True,
        )

    @rpc
    async def with_optional_ref(
        self, name: str, config: Optional[AgentConfig] = None,
    ) -> StatusResponse:
        # Mixed primitive + optional @wire_type dataclass
        cfg_name = config.name if config else ""
        return StatusResponse(agent_id=name, status=cfg_name)


# Note: the Mode 1 twin used for contract-id equivalence is constructed
# inside the test function's own private namespace (via ``exec``) so its
# ``@wire_type`` tag doesn't collide with the synthesized class from
# ``InlineSvc`` during codec registration in the other tests.


# ── Tests ─────────────────────────────────────────────────────────────────────


def test_mode_detection_sets_request_style():
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)

    assert info.methods["get_status"].request_style == "inline"
    assert info.methods["mixed"].request_style == "inline"
    assert info.methods["no_request"].request_style == "inline"
    assert info.methods["with_ctx"].request_style == "inline"
    assert info.methods["with_optional_ref"].request_style == "inline"


def test_inline_params_recorded_in_order():
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)

    mixed = info.methods["mixed"]
    assert [p[0] for p in mixed.inline_params] == ["agent_id", "count"]
    assert mixed.inline_params[0][1] is str
    assert mixed.inline_params[1][1] is int


def test_ctx_param_excluded_from_inline_params():
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)

    with_ctx = info.methods["with_ctx"]
    # The ctx parameter must NOT be a wire field on the synthesized type
    assert [p[0] for p in with_ctx.inline_params] == ["agent_id"]
    assert with_ctx.accepts_ctx is True


def test_synthesized_request_type_is_dataclass():
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)

    req_cls = info.methods["get_status"].request_type
    assert dataclasses.is_dataclass(req_cls)
    assert hasattr(req_cls, "__wire_type__")
    # Synthesized marker for debugging / codegen
    assert getattr(req_cls, "__aster_inline_synthesized__", False) is True
    field_names = [f.name for f in dataclasses.fields(req_cls)]
    assert field_names == ["agent_id"]


def test_contract_id_equivalence_mode1_mode2():
    """Mode 2 and Mode 1 with equivalent schemas must produce the same contract_id.

    Both services are built in a fresh synthetic module so their
    ``@wire_type`` tags (which derive from ``cls.__module__``) match.
    """
    import sys
    import types as _types

    # Fresh module so __module__ is consistent and neither class leaks into
    # the real test module's codec scan.
    mod = _types.ModuleType("aster_test_inline_equivalence_scratch")
    sys.modules[mod.__name__] = mod  # dataclass field type resolution needs it
    mod.StatusResponse = StatusResponse
    mod.wire_type = wire_type
    mod.dataclass = dataclasses.dataclass
    mod.service = service
    mod.rpc = rpc
    mod.SerializationMode = SerializationMode

    src = (
        "@wire_type('aster_test_inline_equivalence_scratch/GetStatusRequest')\n"
        "@dataclass\n"
        "class GetStatusRequest:\n"
        "    agent_id: str = ''\n"
        "\n"
        "@service(name='TwinSvc', version=1, serialization=[SerializationMode.XLANG])\n"
        "class TwinMode1:\n"
        "    @rpc\n"
        "    async def get_status(self, req: GetStatusRequest) -> StatusResponse:\n"
        "        return StatusResponse()\n"
        "\n"
        "@service(name='TwinSvc', version=1, serialization=[SerializationMode.XLANG])\n"
        "class TwinMode2:\n"
        "    @rpc\n"
        "    async def get_status(self, agent_id: str) -> StatusResponse:\n"
        "        return StatusResponse()\n"
    )
    exec(src, mod.__dict__)

    mode1_id = contract_id_from_service(mod.TwinMode1)
    mode2_id = contract_id_from_service(mod.TwinMode2)
    assert mode1_id == mode2_id, (
        f"Mode 2 and Mode 1 must produce identical contract_id for "
        f"equivalent schemas: mode1={mode1_id} != mode2={mode2_id}"
    )


@pytest.mark.asyncio
async def test_mode2_unary_dispatch_single_primitive():
    harness = AsterTestHarness()
    client, _impl = await harness.create_local_pair(
        InlineSvc, InlineSvc(), wire_compatible=True,
    )
    # Client code accepts the synthesized request class -- we look it up
    # off the MethodInfo and construct it directly.
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)
    req_cls = info.methods["get_status"].request_type
    resp = await client.get_status(req_cls(agent_id="alpha"))
    assert resp.agent_id == "alpha"
    assert resp.status == "running"


@pytest.mark.asyncio
async def test_mode2_unary_dispatch_multiple_primitives():
    harness = AsterTestHarness()
    client, _impl = await harness.create_local_pair(
        InlineSvc, InlineSvc(), wire_compatible=True,
    )
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)
    req_cls = info.methods["mixed"].request_type
    resp = await client.mixed(req_cls(agent_id="beta", count=7))
    assert resp.agent_id == "beta#7"


@pytest.mark.asyncio
async def test_mode2_no_request_method():
    harness = AsterTestHarness()
    client, _impl = await harness.create_local_pair(
        InlineSvc, InlineSvc(), wire_compatible=True,
    )
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)
    req_cls = info.methods["no_request"].request_type
    resp = await client.no_request(req_cls())
    assert resp.status == "empty"


@pytest.mark.asyncio
async def test_mode2_with_call_context():
    harness = AsterTestHarness()
    client, _impl = await harness.create_local_pair(
        InlineSvc, InlineSvc(), wire_compatible=True,
    )
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)
    req_cls = info.methods["with_ctx"].request_type
    resp = await client.with_ctx(req_cls(agent_id="gamma"))
    # Handler echoes ctx.method in status, confirming ctx injection worked
    assert resp.agent_id == "gamma"
    assert resp.status == "with_ctx"
    assert resp.via_currentfn is True


@pytest.mark.asyncio
async def test_mode2_optional_nested_wire_type():
    harness = AsterTestHarness()
    client, _impl = await harness.create_local_pair(
        InlineSvc, InlineSvc(), wire_compatible=True,
    )
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)
    req_cls = info.methods["with_optional_ref"].request_type

    # With a config present
    resp = await client.with_optional_ref(
        req_cls(name="delta", config=AgentConfig(name="cfg-1", max_parallel=4)),
    )
    assert resp.agent_id == "delta"
    assert resp.status == "cfg-1"

    # With config None
    resp2 = await client.with_optional_ref(req_cls(name="epsilon", config=None))
    assert resp2.agent_id == "epsilon"
    assert resp2.status == ""


def test_manifest_records_request_style_and_inline_params():
    """The contract manifest extractor must surface Mode 2 metadata so
    codegen and the shell can render inline signatures."""
    from aster.contract.manifest import extract_method_descriptors
    from aster.decorators import _SERVICE_INFO_ATTR

    info = getattr(InlineSvc, _SERVICE_INFO_ATTR)
    descriptors = extract_method_descriptors(info)
    by_name = {d["name"]: d for d in descriptors}

    get_status = by_name["get_status"]
    assert get_status["request_style"] == "inline"
    assert len(get_status["inline_params"]) == 1
    assert get_status["inline_params"][0]["name"] == "agent_id"

    mixed = by_name["mixed"]
    assert mixed["request_style"] == "inline"
    assert [p["name"] for p in mixed["inline_params"]] == ["agent_id", "count"]

    no_request = by_name["no_request"]
    assert no_request["request_style"] == "inline"
    assert no_request["inline_params"] == []
