"""``conformance/vectors/contract-types.json`` ``field_names``
(``ffi_spec/Aster-FieldNaming.md``): Python reproduces the Orders contract as
Rust defines it (``order_id``: the attribute names, verbatim) and as Java
defines it (``orderId``: the same attributes with ``wire_name``), decodes the
shared payload, pins pyfory's snake_case conversion, and refuses what the
spec refuses.
"""

from __future__ import annotations

import dataclasses
import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

import pyfory
import pytest

from aster import wire_name
from aster.codec import ForyCodec, contract_default, wire_type
from aster.contract.field_names import (
    WIRE_NAME_KEY,
    check_field_names,
    not_plain,
    plain_suggestion,
    reference_snake_case,
)

_V = json.loads(
    (Path(__file__).resolve().parents[3] / "conformance" / "vectors" / "contract-types.json").read_text()
)["vector"]["field_names"]


@wire_type("names.v1/Order")
@dataclass
class RsOrder:
    order_id: str = ""
    line_items: list[RsLineItem] = field(default_factory=list)
    total_cents: pyfory.Int32 = 0
    note: Optional[str] = None


@wire_type("names.v1/LineItem")
@dataclass
class RsLineItem:
    sku_code: str = ""
    unit_count: pyfory.Int32 = 0


@wire_type("names.v1/Order")
@dataclass
class JvOrder:
    order_id: str = wire_name("orderId", default="")
    line_items: list[JvLineItem] = wire_name("lineItems", default_factory=list)
    total_cents: pyfory.Int32 = wire_name("totalCents", default=0)
    note: Optional[str] = None


@wire_type("names.v1/LineItem")
@dataclass
class JvLineItem:
    sku_code: str = wire_name("skuCode", default="")
    unit_count: pyfory.Int32 = wire_name("unitCount", default=0)


def _rs_service():
    from aster import rpc, service

    @service(name="Orders", version=1)
    class OrdersService:
        @rpc
        async def place(self, req: RsOrder) -> RsOrder:
            return req

    return OrdersService


def _jv_service():
    from aster import rpc, service

    @service(name="Orders", version=1)
    class OrdersService:
        @rpc
        async def place(self, req: JvOrder) -> JvOrder:
            return req

    return OrdersService


def _hashes(*classes):
    from aster.contract.identity import build_type_graph, canonical_xlang_bytes, compute_type_hash, resolve_with_cycles

    tds = resolve_with_cycles(build_type_graph(list(classes)))
    return {fqn: (canonical_xlang_bytes(td).hex(), compute_type_hash(canonical_xlang_bytes(td)).hex()) for fqn, td in tds.items()}


def test_pyfory_snake_case_and_plainness_match_the_vector():
    from pyfory.struct import _to_snake_case

    for name, row in _V["snake_case"].items():
        assert _to_snake_case(name) == row["python"], name
        assert reference_snake_case(name) == row["rust"], name
        assert (not_plain(name) is None) == row["plain"], name
        if not row["plain"]:
            assert not_plain(plain_suggestion(name)) is None, name


def _pairs(c):
    return [(f["member_snake"], f["name"]) for f in c["fields"]]


def test_refused_and_accepted_cases_match_the_vector():
    for c in _V["refused"]:
        with pytest.raises(TypeError, match=c["error"]):
            check_field_names(_pairs(c))
    for c in _V["accepted"]:
        check_field_names(_pairs(c))


@pytest.mark.parametrize(
    "fields, phrase",
    [
        ([("userID", str, field(default=""))], "not a plain field name"),
        ([("author", str, wire_name("writtenBy", default=""))], "does not match"),
        ([("user_id", str, field(default="")), ("userId", str, field(default=""))], "collide"),
        # pyfory writes `type_` as `type`: the contract name must be `type`.
        ([("type_", str, field(default=""))], "not a plain field name"),
    ],
)
def test_python_types_are_refused_when_the_contract_is_computed(fields, phrase):
    Bad = wire_type("names.v1/Bad")(dataclasses.make_dataclass("Bad", fields))
    with pytest.raises(TypeError, match=phrase):
        _hashes(Bad)


def test_a_trailing_underscore_attribute_takes_the_plain_name():
    Ok = wire_type("names.v1/Ok")(
        dataclasses.make_dataclass("Ok", [("type_", str, wire_name("type", default=""))])
    )
    from aster.contract.identity import build_type_graph, resolve_with_cycles

    td = resolve_with_cycles(build_type_graph([Ok]))["names.v1.Ok"]
    assert [f.name for f in td.fields] == ["type"]


def test_wire_name_combines_with_a_contract_default():
    Ok = wire_type("names.v1/Ok")(
        dataclasses.make_dataclass(
            "Ok", [("state_name", str, contract_default("idle", metadata={WIRE_NAME_KEY: "stateName"}))]
        )
    )
    from aster.contract.identity import build_type_graph, resolve_with_cycles

    td = resolve_with_cycles(build_type_graph([Ok]))["names.v1.Ok"]
    assert [(f.name, f.required) for f in td.fields] == [("stateName", False)]


@pytest.mark.parametrize("definition", ["rust_defined", "java_defined"])
def test_orders_contract_hashes_ids_and_payload(definition):
    from aster.contract.identity import contract_id_from_service

    d = _V[definition]
    order, item = (RsOrder, RsLineItem) if definition == "rust_defined" else (JvOrder, JvLineItem)
    got = _hashes(order, item)
    for fqn, t in d["types"].items():
        assert got[fqn] == (t["expected_canonical_type_def_hex"], t["expected_type_hash"]), fqn

    svc = _rs_service() if definition == "rust_defined" else _jv_service()
    assert contract_id_from_service(svc) == d["expected_contract_id"]

    codec = ForyCodec(types=[item, order])
    value = codec.decode(bytes.fromhex(d["payload"]["hex"]), order)
    expected = d["payload"]["value"]

    def as_contract(o):
        from aster.contract.field_names import contract_field_name

        if dataclasses.is_dataclass(o):
            return {contract_field_name(f): as_contract(getattr(o, f.name)) for f in dataclasses.fields(o)}
        if isinstance(o, list):
            return [as_contract(x) for x in o]
        return o

    assert as_contract(value) == expected
    assert as_contract(codec.decode(codec.encode(value), order)) == expected
