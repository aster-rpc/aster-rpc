"""Strict-mode tests for JsonProxyCodec.

The producer owns the contract: any JSON dict key that doesn't map to
a declared field on the expected @wire_type dataclass MUST raise
ContractViolationError. The codec does not silently drop or rename
keys, no matter how innocuous they look. These tests pin that
behaviour at every depth.
"""

from __future__ import annotations

import dataclasses
import json
from enum import Enum, IntEnum
from typing import Optional

import pytest

from aster import wire_type
from aster.json_codec import JsonProxyCodec, json_decode
from aster.status import ContractViolationError, RpcError, StatusCode


# ── Fixtures ─────────────────────────────────────────────────────────────────


@wire_type("test.codec/StatusRequest")
@dataclasses.dataclass
class StatusRequest:
    agent_id: str = ""
    region: str = ""


@wire_type("test.codec/Tag")
@dataclasses.dataclass
class Tag:
    key: str = ""
    value: str = ""


@wire_type("test.codec/StatusResponse")
@dataclasses.dataclass
class StatusResponse:
    agent_id: str = ""
    status: str = ""
    tags: list[Tag] = dataclasses.field(default_factory=list)
    nested: Tag = dataclasses.field(default_factory=Tag)


# ── Top-level violations ─────────────────────────────────────────────────────


def test_unexpected_top_level_key_raises():
    """A bogus top-level field fails fast with the offending key listed."""
    payload = json.dumps({"agent_id": "ok", "bogus": 1}).encode()
    with pytest.raises(ContractViolationError) as exc_info:
        json_decode(payload, StatusRequest)
    err = exc_info.value
    assert err.code == StatusCode.CONTRACT_VIOLATION
    assert "bogus" in err.message
    assert "StatusRequest" in err.message
    assert err.details["expected_class"] == "StatusRequest"
    assert "bogus" in err.details["unexpected_fields"]


def test_strict_mode_via_codec_decode():
    """Going through the public JsonProxyCodec API also enforces strict."""
    codec = JsonProxyCodec()
    payload = codec.encode({"agent_id": "ok", "extra": "x"})
    with pytest.raises(ContractViolationError):
        codec.decode(payload, StatusRequest)


def test_valid_request_decodes_cleanly():
    """The happy path still works with no violations."""
    payload = json.dumps({"agent_id": "edge-7", "region": "us-east"}).encode()
    req = json_decode(payload, StatusRequest)
    assert isinstance(req, StatusRequest)
    assert req.agent_id == "edge-7"
    assert req.region == "us-east"


def test_missing_field_uses_default():
    """Missing fields use the dataclass default -- only EXTRA fields fail."""
    payload = json.dumps({"agent_id": "edge-7"}).encode()
    req = json_decode(payload, StatusRequest)
    assert req.agent_id == "edge-7"
    assert req.region == ""  # default


# ── Nested violations ────────────────────────────────────────────────────────


def test_unexpected_nested_object_key_raises_with_path():
    """A bad field in a nested @wire_type object reports the dotted path."""
    payload = json.dumps({
        "agent_id": "ok",
        "status": "running",
        "tags": [],
        "nested": {"key": "k", "value": "v", "rogue": 1},
    }).encode()
    with pytest.raises(ContractViolationError) as exc_info:
        json_decode(payload, StatusResponse)
    err = exc_info.value
    assert "rogue" in err.message
    # The error message should contain the dotted path so the user
    # can find the violation in a deeply-nested payload.
    assert "nested" in err.message or "Tag" in err.message


def test_unexpected_array_element_key_raises():
    """A bad field in a list[@wire_type] element fails too."""
    payload = json.dumps({
        "agent_id": "ok",
        "status": "running",
        "tags": [
            {"key": "good", "value": "1"},
            {"key": "bad", "value": "2", "snuck_in": True},
        ],
    }).encode()
    with pytest.raises(ContractViolationError) as exc_info:
        json_decode(payload, StatusResponse)
    err = exc_info.value
    assert "snuck_in" in err.message
    # Should include the array index in the path
    assert "[1]" in err.message or "tags" in err.message


# ── Sanitization ─────────────────────────────────────────────────────────────


def test_control_characters_in_key_are_repr_escaped():
    """Bad keys with control chars / ANSI are escaped before being logged."""
    bad_key = "fake\nINFO server compromised\x1b[31m"
    payload = json.dumps({"agent_id": "ok", bad_key: 1}).encode()
    with pytest.raises(ContractViolationError) as exc_info:
        json_decode(payload, StatusRequest)
    err = exc_info.value
    # The raw newline / escape sequence MUST NOT appear in the
    # message verbatim -- repr() should have escaped them.
    assert "\n" not in err.message.replace("\\n", "")
    assert "\x1b" not in err.message
    # And the escaped form should be present
    assert "\\n" in err.message or "\\u001b" in err.message


def test_long_key_is_truncated():
    """Megabyte-long key names get capped to prevent log explosion."""
    huge_key = "x" * 5000
    payload = json.dumps({"agent_id": "ok", huge_key: 1}).encode()
    with pytest.raises(ContractViolationError) as exc_info:
        json_decode(payload, StatusRequest)
    err = exc_info.value
    # The huge key shouldn't appear in full
    assert "x" * 200 not in err.message
    assert "truncated" in err.message


def test_many_unexpected_keys_are_capped():
    """A flood of bad keys gets capped at 5 + a 'more' marker."""
    bad_payload = {"agent_id": "ok"}
    for i in range(100):
        bad_payload[f"bad{i}"] = i
    payload = json.dumps(bad_payload).encode()
    with pytest.raises(ContractViolationError) as exc_info:
        json_decode(payload, StatusRequest)
    err = exc_info.value
    assert "more" in err.message


# ── Status code identity ─────────────────────────────────────────────────────


def test_contract_violation_is_in_aster_native_range():
    """CONTRACT_VIOLATION lives in the 100+ Aster-native range, not the
    gRPC mirror, so it can't collide with a future gRPC code."""
    assert StatusCode.CONTRACT_VIOLATION >= 100
    # And it's not the same numeric value as any gRPC-mirrored code
    grpc_codes = {
        StatusCode.OK, StatusCode.CANCELLED, StatusCode.UNKNOWN,
        StatusCode.INVALID_ARGUMENT, StatusCode.DEADLINE_EXCEEDED,
        StatusCode.NOT_FOUND, StatusCode.ALREADY_EXISTS,
        StatusCode.PERMISSION_DENIED, StatusCode.RESOURCE_EXHAUSTED,
        StatusCode.FAILED_PRECONDITION, StatusCode.ABORTED,
        StatusCode.OUT_OF_RANGE, StatusCode.UNIMPLEMENTED,
        StatusCode.INTERNAL, StatusCode.UNAVAILABLE,
        StatusCode.DATA_LOSS, StatusCode.UNAUTHENTICATED,
    }
    assert StatusCode.CONTRACT_VIOLATION not in grpc_codes


def test_contract_violation_error_subclass():
    """ContractViolationError IS-A RpcError so existing handlers see it."""
    err = ContractViolationError(message="x")
    assert isinstance(err, RpcError)
    assert err.code == StatusCode.CONTRACT_VIOLATION


# ── Edge cases: enums, Optional, generics, primitive containers ──────────────


def test_int_enum_field_passes_through():
    """IntEnum-typed fields don't trip strict validation."""
    class Status(IntEnum):
        IDLE = 0
        RUNNING = 1
        ERROR = 2

    @wire_type("test.codec/EnumReq")
    @dataclasses.dataclass
    class EnumReq:
        status: int = Status.IDLE  # Wire form is the int
        name: str = ""

    payload = json.dumps({"status": 1, "name": "ok"}).encode()
    req = json_decode(payload, EnumReq)
    assert req.status == 1
    assert req.name == "ok"

    # An unexpected key alongside the enum still raises
    bad = json.dumps({"status": 1, "name": "ok", "rogue": True}).encode()
    with pytest.raises(ContractViolationError):
        json_decode(bad, EnumReq)


def test_str_enum_field_passes_through():
    """String-valued enum fields work the same way."""
    class Color(str, Enum):
        RED = "red"
        GREEN = "green"

    @wire_type("test.codec/ColorReq")
    @dataclasses.dataclass
    class ColorReq:
        color: str = Color.RED.value
        count: int = 0

    payload = json.dumps({"color": "green", "count": 3}).encode()
    req = json_decode(payload, ColorReq)
    assert req.color == "green"
    assert req.count == 3


def test_optional_field_accepts_null():
    """Optional[X] field accepts a null value without complaint."""
    @wire_type("test.codec/OptReq")
    @dataclasses.dataclass
    class OptReq:
        nested: Optional[Tag] = None
        name: str = ""

    payload = json.dumps({"nested": None, "name": "x"}).encode()
    req = json_decode(payload, OptReq)
    assert req.nested is None
    assert req.name == "x"


def test_optional_nested_dataclass_strict_check():
    """Optional[X] with a populated value still validates the inner shape."""
    @wire_type("test.codec/OptReq2")
    @dataclasses.dataclass
    class OptReq2:
        nested: Optional[Tag] = None

    payload = json.dumps({"nested": {"key": "k", "value": "v", "rogue": 1}}).encode()
    with pytest.raises(ContractViolationError) as exc_info:
        json_decode(payload, OptReq2)
    assert "rogue" in exc_info.value.message


def test_primitive_dict_field_decodes_permissively():
    """`dict[str, str]`-typed fields don't recurse into the dict values.

    Documented limitation: Python json codec doesn't validate value
    types of dict-typed fields. Top-level validation still runs.
    """
    @wire_type("test.codec/DictReq")
    @dataclasses.dataclass
    class DictReq:
        labels: dict = dataclasses.field(default_factory=dict)
        name: str = ""

    payload = json.dumps({
        "labels": {"any": "value", "another": "ok"},
        "name": "x",
    }).encode()
    req = json_decode(payload, DictReq)
    assert req.labels == {"any": "value", "another": "ok"}

    # Top-level still strict
    bad = json.dumps({"labels": {}, "name": "x", "rogue": 1}).encode()
    with pytest.raises(ContractViolationError):
        json_decode(bad, DictReq)


def test_list_of_primitives_decodes_permissively():
    """`list[str]`-typed fields don't try to recurse into elements."""
    @wire_type("test.codec/ListReq")
    @dataclasses.dataclass
    class ListReq:
        capabilities: list[str] = dataclasses.field(default_factory=list)

    payload = json.dumps({"capabilities": ["gpu", "arm64"]}).encode()
    req = json_decode(payload, ListReq)
    assert req.capabilities == ["gpu", "arm64"]


def test_empty_dataclass_rejects_any_field():
    """A dataclass with no fields rejects ALL incoming JSON keys."""
    @wire_type("test.codec/Empty")
    @dataclasses.dataclass
    class Empty:
        pass

    payload = json.dumps({"anything": 1}).encode()
    with pytest.raises(ContractViolationError):
        json_decode(payload, Empty)

    # An empty dict against an empty class is valid
    payload = json.dumps({}).encode()
    req = json_decode(payload, Empty)
    assert isinstance(req, Empty)
