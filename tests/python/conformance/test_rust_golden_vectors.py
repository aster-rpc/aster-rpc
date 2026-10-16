"""
Golden vectors produced by Rust core / the ``aster`` crate, checked from Python.

- ``conformance/vectors/envelope.json``          -- AEv1 envelope (Aster-SPEC §6.9)
- ``conformance/vectors/compression.json``       -- zstd payload frames (§5.6)
- ``conformance/vectors/contract-defaults.json`` -- contract defaults
  (Aster-ContractIdentity §11.3.2.3)
- ``conformance/vectors/contract-types.json``    -- timestamp / date / decimal
  and recursive types (identity and payload bytes)

Python must produce the exact bytes / ids the Rust side does.
"""

from __future__ import annotations

import dataclasses
import datetime
import decimal
import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

import pyfory
import pytest

from aster import _aster
from aster.codec import contract_default, wire_type
from aster.limits import LimitExceeded
from aster.wire import (
    COMPRESSED,
    COMPRESSION_THRESHOLD,
    MAX_DECOMPRESSED_SIZE,
    ZSTD_LEVEL,
    CallHeader,
    EnvelopeError,
    RpcStatus,
    StreamHeader,
    compress_payload,
    decode_frame,
    encode_frame,
    payload_of,
)

_VECTORS = Path(__file__).resolve().parents[3] / "conformance" / "vectors"


def _load(name: str) -> dict:
    return json.loads((_VECTORS / name).read_text())


# ── Envelope (AEv1) ──────────────────────────────────────────────────────────

_ENVELOPE = _load("envelope.json")
_ENVELOPE_TYPES = {
    "StreamHeader": StreamHeader,
    "CallHeader": CallHeader,
    "RpcStatus": RpcStatus,
}


def _snake(key: str) -> str:
    return re.sub(r"([A-Z])", lambda m: "_" + m.group(1).lower(), key)


@pytest.mark.parametrize("vec", _ENVELOPE["vectors"], ids=lambda v: v["name"])
def test_envelope_encode_matches_rust(vec):
    cls = _ENVELOPE_TYPES[vec["type"]]
    obj = cls(**{_snake(k): v for k, v in vec["input"].items()})
    encoded = obj.encode()
    assert encoded.hex() == vec["hex"]
    assert len(encoded) == vec["length"]


@pytest.mark.parametrize("vec", _ENVELOPE["vectors"], ids=lambda v: v["name"])
def test_envelope_decode_matches_rust(vec):
    cls = _ENVELOPE_TYPES[vec["type"]]
    expected = cls(**{_snake(k): v for k, v in vec["input"].items()})
    assert cls.decode(bytes.fromhex(vec["hex"])) == expected


@pytest.mark.parametrize("vec", _ENVELOPE["invalid"], ids=lambda v: v["name"])
def test_envelope_rejects_invalid(vec):
    cls = _ENVELOPE_TYPES[vec["type"]]
    with pytest.raises(EnvelopeError):
        cls.decode(bytes.fromhex(vec["hex"]))


# ── Compression (§5.6) ───────────────────────────────────────────────────────

_COMPRESSION = _load("compression.json")


def _payload(rule: str, length: int) -> bytes:
    if rule == "byte[i] = i % 251":
        return bytes(i % 251 for i in range(length))
    if rule == "byte[i] = 0":
        return bytes(length)
    raise AssertionError(f"unknown payload_rule {rule!r}")


def test_compression_parameters_match_spec():
    p = _COMPRESSION["parameters"]
    assert COMPRESSION_THRESHOLD == p["threshold_bytes"]
    assert ZSTD_LEVEL == p["zstd_level"]
    assert MAX_DECOMPRESSED_SIZE == p["max_decompressed_bytes"]


@pytest.mark.parametrize("vec", _COMPRESSION["encode_vectors"], ids=lambda v: v["name"])
def test_compression_encode_vectors(vec):
    payload = _payload(vec["payload_rule"], vec["payload_length"])
    data, compressed = compress_payload(payload)
    flags = vec["input_flags"] | (COMPRESSED if compressed else 0)
    assert encode_frame(data, flags).hex() == vec["expected_frame_hex"]


def test_compression_threshold_is_strict():
    """> 4096 compresses (when smaller); == 4096 does not."""
    at = bytes(COMPRESSION_THRESHOLD)
    above = bytes(COMPRESSION_THRESHOLD + 1)
    assert compress_payload(at) == (at, False)
    data, compressed = compress_payload(above)
    assert compressed and len(data) < len(above)


def test_compression_only_if_smaller():
    import os

    incompressible = os.urandom(COMPRESSION_THRESHOLD * 4)
    assert compress_payload(incompressible) == (incompressible, False)


@pytest.mark.parametrize("vec", _COMPRESSION["decode_vectors"], ids=lambda v: v["name"])
def test_compression_decode_vectors(vec):
    payload, flags, consumed = decode_frame(bytes.fromhex(vec["frame_hex"]))
    assert flags == vec["flags"]
    assert consumed == len(vec["frame_hex"]) // 2
    out = payload_of(payload, flags)
    assert len(out) == vec["payload_length"]
    assert _aster.blake3_hex(out) == vec["payload_blake3"]


@pytest.mark.parametrize("vec", _COMPRESSION["reject_vectors"], ids=lambda v: v["name"])
def test_compression_reject_vectors(vec):
    payload, flags, _ = decode_frame(bytes.fromhex(vec["frame_hex"]))
    if "exceeds" in vec["reason"]:
        with pytest.raises(LimitExceeded):
            payload_of(payload, flags)
    else:
        with pytest.raises(ValueError):
            payload_of(payload, flags)


# ── Contract defaults (§11.3.2.3) ────────────────────────────────────────────

_DEFAULTS = _load("contract-defaults.json")["vector"]


@wire_type("defaults.v1/Settings")
@dataclass
class Settings:
    """Python mirror of ``aster/tests/it/contract_defaults_vector.rs::Settings``."""

    name: str = ""  # required: plain construction default only
    state: str = contract_default("idle")
    note: str = contract_default("")
    enabled: bool = contract_default(True)
    retries: pyfory.Int32 = contract_default(-3)
    since_ms: int = contract_default(1_700_000_000_000)
    ratio: float = contract_default(0.5)
    label: Optional[str] = contract_default("x")
    tags: list[str] = contract_default(default_factory=list)
    attrs: dict[str, str] = contract_default(default_factory=dict)
    blob: bytes = b""  # binary defaults are reserved in v1 -> required


def _settings_type_def():
    from aster.contract.identity import build_type_graph, resolve_with_cycles

    defs = resolve_with_cycles(build_type_graph([Settings]))
    (td,) = [d for d in defs.values() if d.name == "Settings"]
    return td


def test_contract_defaults_field_lowering():
    td = _settings_type_def()
    assert td.package == _DEFAULTS["type_def"]["package"]
    by_name = {f.name: f for f in td.fields}
    for exp in _DEFAULTS["type_def"]["fields_in_declaration_order"]:
        f = by_name[exp["name"]]
        assert f.type_primitive == exp["type_primitive"], exp["name"]
        assert f.optional == exp["optional"], exp["name"]
        assert int(f.container) == exp["container"], exp["name"]
        assert f.required == exp["required"], exp["name"]
        assert f.default_value.hex() == exp["default_value_hex"], exp["name"]


def test_contract_defaults_canonical_type_def_and_hash():
    from aster.contract.identity import canonical_xlang_bytes, compute_type_hash

    canonical = canonical_xlang_bytes(_settings_type_def())
    assert canonical.hex() == _DEFAULTS["expected_canonical_type_def_hex"]
    assert compute_type_hash(canonical).hex() == _DEFAULTS["expected_type_hash"]


def test_contract_defaults_contract_id():
    from aster import rpc, service
    from aster.contract.identity import contract_id_from_service

    @service(name="Defaults", version=1)
    class DefaultsService:
        @rpc
        async def put(self, req: Settings) -> Settings:
            return req

    assert contract_id_from_service(DefaultsService) == _DEFAULTS["expected_contract_id"]


def test_plain_default_is_not_a_contract_default():
    """`name: str = ""` stays required (rule 4: no default != zero default)."""
    by_name = {f.name: f for f in _settings_type_def().fields}
    assert by_name["name"].required is True
    assert by_name["name"].default_value == b""


@pytest.mark.parametrize(
    "annotation, marker, message",
    [
        (list[str], contract_default(default_factory=lambda: ["a"]), "empty container"),
        (Optional[str], contract_default(None), "None contract default"),
        (bytes, contract_default(b"x"), "reserved"),
        (pyfory.Int16, contract_default(1), "reserved"),
        (pyfory.Int32, contract_default(1 << 31), "out of range"),
        (float, contract_default(float("nan")), "finite"),
    ],
)
def test_contract_default_rejects_unsupported(annotation, marker, message):
    from aster.contract.identity import build_type_graph, resolve_with_cycles

    Bad = dataclasses.make_dataclass("Bad", [("f", annotation, marker)])
    Bad = wire_type("defaults.v1/Bad")(Bad)
    with pytest.raises(TypeError, match=message):
        resolve_with_cycles(build_type_graph([Bad]))


def test_contract_default_does_not_change_payload_bytes():
    from aster.codec import ForyCodec

    @wire_type("defaults.v1/Plain")
    @dataclass
    class Plain:
        state: str = "idle"

    @wire_type("defaults.v1/Plain")
    @dataclass
    class Marked:
        state: str = contract_default("idle")

    assert ForyCodec(types=[Plain]).encode(Plain()) == ForyCodec(types=[Marked]).encode(Marked())


def test_contract_default_requires_exactly_one_value():
    with pytest.raises(TypeError):
        contract_default()
    with pytest.raises(TypeError):
        contract_default(1, default_factory=list)


def test_contract_default_accepts_pyfory_field_options():
    f = contract_default("x", nullable=True)
    assert f.metadata["aster.contract_default"] is True
    assert f.default == "x"


# ── Fory options / naming (cross-binding) ────────────────────────────────────


def test_fory_registration_name_matches_rust():
    """Rust registers `#[aster(wire = "pkg/Name")]` as `"pkg.Name"`
    (``register_by_name``), Fory splitting on the last '.'."""
    from aster.codec import fory_registration_name

    assert fory_registration_name("defaults.v1", "Settings") == "defaults.v1.Settings"
    assert fory_registration_name("", "Bare") == "Bare"


def test_pyfory_matches_pinned_release_line():
    from aster.wire import FORY_VERSION

    assert pyfory.__version__.split(".")[:2] == FORY_VERSION.split(".")[:2]


# ── Canonical temporal / decimal types and recursive types ──────────────────
#
# Python mirrors of ``aster/tests/it/contract_types.rs``. A Rust ``Box<T>`` is
# transparent, so ``Option<Box<T>>`` mirrors as ``Optional[T]``.

_TYPES = _load("contract-types.json")["vector"]
_EPOCH = datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc)


@wire_type("types.v1/Temporal")
@dataclass
class Temporal:
    at: datetime.datetime = _EPOCH
    day: datetime.date = datetime.date(1970, 1, 1)
    amount: decimal.Decimal = decimal.Decimal(0)
    maybe_at: Optional[datetime.datetime] = None
    history: list[datetime.datetime] = field(default_factory=list)


@wire_type("types.v1/TreeNode")
@dataclass
class TreeNode:
    value: str = ""
    children: list[TreeNode] = field(default_factory=list)


@wire_type("types.v1/Chain")
@dataclass
class Chain:
    value: pyfory.Int32 = 0
    next: Optional[Chain] = None


@wire_type("types.v1/Author")
@dataclass
class Author:
    name: str = ""
    books: list[Book] = field(default_factory=list)


@wire_type("types.v1/Book")
@dataclass
class Book:
    title: str = ""
    written_by: Optional[Author] = None


@wire_type("types.v1/Alpha")
@dataclass
class Alpha:
    next: Optional[Beta] = None


@wire_type("types.v1/Beta")
@dataclass
class Beta:
    next: Optional[Gamma] = None


@wire_type("types.v1/Gamma")
@dataclass
class Gamma:
    next: list[Alpha] = field(default_factory=list)


@wire_type("types.v1/Forest")
@dataclass
class Forest:
    trees: list[TreeNode] = field(default_factory=list)


_TYPE_CLASSES = [Temporal, TreeNode, Chain, Author, Book, Alpha, Beta, Gamma, Forest]


def _types_type_defs():
    from aster.contract.identity import build_type_graph, resolve_with_cycles

    return resolve_with_cycles(build_type_graph(_TYPE_CLASSES))


@pytest.mark.parametrize("fqn", sorted(_TYPES["types"]))
def test_contract_types_type_hash_matches_rust(fqn):
    from aster.contract.identity import canonical_xlang_bytes, compute_type_hash

    td = _types_type_defs()[fqn]
    exp = _TYPES["types"][fqn]
    by_name = {f.name: f for f in td.fields}
    for ef in exp["fields_in_declaration_order"]:
        f = by_name[ef["name"]]
        assert int(f.type_kind) == ef["type_kind"], ef["name"]
        assert f.type_primitive == ef["type_primitive"], ef["name"]
        assert f.type_ref.hex() == ef["type_ref"], ef["name"]
        assert f.self_ref_name == ef["self_ref_name"], ef["name"]
        assert f.optional == ef["optional"], ef["name"]
        assert int(f.container) == ef["container"], ef["name"]
    canonical = canonical_xlang_bytes(td)
    assert canonical.hex() == exp["expected_canonical_type_def_hex"]
    assert compute_type_hash(canonical).hex() == exp["expected_type_hash"]


def test_contract_types_contract_id_matches_rust():
    from aster import rpc, service
    from aster.contract.identity import contract_id_from_service

    @service(name="Types", version=1)
    class TypesService:
        @rpc
        async def echo_temporal(self, req: Temporal) -> Temporal:
            return req

        @rpc
        async def plant(self, req: TreeNode) -> Forest:
            return Forest()

        @rpc
        async def author(self, req: Book) -> Author:
            return Author()

        @rpc
        async def cycle(self, req: Alpha) -> Gamma:
            return Gamma()

    assert contract_id_from_service(TypesService) == _TYPES["expected_contract_id"]


def _timestamp(s: str) -> datetime.datetime:
    return datetime.datetime.strptime(s, "%Y-%m-%dT%H:%M:%S.%fZ").replace(
        tzinfo=datetime.timezone.utc
    )


def _expected(cls: type, value):
    """Build the Python value the vector's JSON ``value`` describes."""
    if value is None:
        return None
    if cls is Temporal:
        return Temporal(
            at=_timestamp(value["at"]),
            day=datetime.date.fromisoformat(value["day"]),
            amount=decimal.Decimal(value["amount"]),
            maybe_at=None if value["maybe_at"] is None else _timestamp(value["maybe_at"]),
            history=[_timestamp(t) for t in value["history"]],
        )
    if cls is TreeNode:
        return TreeNode(value["value"], [_expected(TreeNode, c) for c in value["children"]])
    if cls is Author:
        return Author(value["name"], [_expected(Book, b) for b in value["books"]])
    if cls is Book:
        return Book(value["title"], _expected(Author, value["written_by"]))
    if cls is Alpha:
        return Alpha(_expected(Beta, value["next"]))
    if cls is Beta:
        return Beta(_expected(Gamma, value["next"]))
    if cls is Gamma:
        return Gamma([_expected(Alpha, a) for a in value["next"]])
    raise AssertionError(cls)


@pytest.mark.parametrize("name", sorted(_TYPES["payloads"]))
def test_contract_types_rust_payload_decodes(name):
    from aster.codec import ForyCodec

    case = _TYPES["payloads"][name]
    cls = {f"types.v1.{c.__name__}": c for c in _TYPE_CLASSES}[case["type"]]
    codec = ForyCodec(types=_TYPE_CLASSES)
    got = codec.decode(bytes.fromhex(case["hex"]), cls)
    assert got == _expected(cls, case["value"])
    # And back: what Python writes decodes to the same value.
    assert codec.decode(codec.encode(got), cls) == got
