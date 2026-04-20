"""
Phase 2 tests: Serialization Integration (Fory).

Tests cover:
- XLANG round-trip for dataclasses
- NATIVE round-trip
- ROW schema encoding
- Compression round-trip
- Untagged type raises TypeError at registration time
- Type graph walking and registration
- Framework-internal type registration
"""

from __future__ import annotations
import dataclasses
import enum
from dataclasses import dataclass, field
from typing import Optional

import pytest

from aster.codec import (
    wire_type,
    ForyCodec,
    ForyConfig,
    DEFAULT_COMPRESSION_THRESHOLD,
    _walk_type_graph,
    _validate_xlang_tags,
)
from aster.rpc_types import SerializationMode
from aster.protocol import StreamHeader, CallHeader, RpcStatus


def test_pyfory_cython_acceleration_is_enabled():
    """Guard against silently running the pure-Python pyfory fallback.

    `pyfory.ENABLE_FORY_CYTHON_SERIALIZATION` should be True on every
    supported platform. If it flips to False (broken wheel, source install
    without the compiled extension, etc.) codec throughput drops several-fold
    and the failure is otherwise silent. This test is the canary.
    """
    import pyfory
    assert pyfory.ENABLE_FORY_CYTHON_SERIALIZATION, (
        "pyfory Cython acceleration is OFF -- the pure-Python fallback is "
        "significantly slower. Reinstall pyfory so the compiled extension "
        "is available: `uv pip install --force-reinstall --no-cache-dir pyfory`."
    )
    # Verify ForyCodec constructed the Cython Fory, not the Python fallback.
    # The Cython class lives in the compiled `pyfory.serialization` extension;
    # the pure-Python fallback lives in `pyfory._fory`.
    codec = ForyCodec()
    fory_cls = type(codec._fory)
    assert fory_cls.__module__ == "pyfory.serialization", (
        f"ForyCodec instantiated {fory_cls.__module__}.{fory_cls.__name__}, "
        f"expected pyfory.serialization.Fory (the Cython fast path)."
    )
from aster.status import StatusCode


# ── Test types ───────────────────────────────────────────────────────────────


@dataclass
@wire_type("test.codec/SimpleMsg")
class SimpleMsg:
    name: str = ""
    value: int = 0
    active: bool = False


@dataclass
@wire_type("test.codec/InnerMsg")
class InnerMsg:
    label: str = ""
    score: int = 0


@dataclass
@wire_type("test.codec/OuterMsg")
class OuterMsg:
    title: str = ""
    inner: InnerMsg = field(default_factory=InnerMsg)
    count: int = 0


@dataclass
@wire_type("test.codec/ListMsg")
class ListMsg:
    items: list[str] = field(default_factory=list)
    values: list[int] = field(default_factory=list)


@dataclass
@wire_type("test.codec/OptMsg")
class OptMsg:
    required: str = ""
    optional_str: Optional[str] = None
    optional_int: Optional[int] = None


@dataclass
class UntaggedMsg:
    """A type WITHOUT @wire_type -- should fail XLANG registration."""

    data: str = ""


@dataclass
@wire_type("test.codec/LargeMsg")
class LargeMsg:
    """A type that can produce payloads larger than the compression threshold."""

    payload: str = ""


# ── @wire_type decorator tests ───────────────────────────────────────────────


class TestForyTagDecorator:
    def test_tag_with_namespace(self):
        assert SimpleMsg.__wire_type__ == "test.codec/SimpleMsg"
        assert SimpleMsg.__fory_namespace__ == "test.codec"
        assert SimpleMsg.__fory_typename__ == "SimpleMsg"

    def test_tag_without_namespace(self):
        @wire_type("PlainTag")
        @dataclass
        class Plain:
            x: int = 0

        assert Plain.__wire_type__ == "PlainTag"
        assert Plain.__fory_namespace__ == ""
        assert Plain.__fory_typename__ == "PlainTag"

    def test_tag_with_deep_namespace(self):
        @wire_type("com.example.deep/MyType")
        @dataclass
        class Deep:
            x: int = 0

        assert Deep.__fory_namespace__ == "com.example.deep"
        assert Deep.__fory_typename__ == "MyType"

    def test_tag_with_multiple_slashes(self):
        """Only the last / is used for splitting."""

        @wire_type("a/b/c/TypeName")
        @dataclass
        class Multi:
            x: int = 0

        assert Multi.__fory_namespace__ == "a/b/c"
        assert Multi.__fory_typename__ == "TypeName"


# ── Type graph walking tests ────────────────────────────────────────────────


class TestTypeGraphWalking:
    def test_simple_type(self):
        types = _walk_type_graph([SimpleMsg])
        assert SimpleMsg in types

    def test_nested_types_dependency_order(self):
        """Nested types appear before their parents (leaves first)."""
        types = _walk_type_graph([OuterMsg])
        assert InnerMsg in types
        assert OuterMsg in types
        idx_inner = types.index(InnerMsg)
        idx_outer = types.index(OuterMsg)
        assert idx_inner < idx_outer, "InnerMsg should come before OuterMsg"

    def test_deduplication(self):
        """Same type is not listed twice."""
        types = _walk_type_graph([SimpleMsg, SimpleMsg])
        assert types.count(SimpleMsg) == 1

    def test_primitives_excluded(self):
        """Primitive types (str, int, etc.) are not included."""
        types = _walk_type_graph([SimpleMsg])
        assert int not in types
        assert str not in types
        assert bool not in types

    def test_list_types_excluded(self):
        """Generic container types are not included, but their dataclass args are."""
        types = _walk_type_graph([ListMsg])
        assert ListMsg in types
        # str and int are primitives, shouldn't be in the list
        assert len([t for t in types if t is ListMsg]) == 1

    def test_optional_types_unwrapped(self):
        """Optional[X] is unwrapped to find the inner type."""
        types = _walk_type_graph([OptMsg])
        assert OptMsg in types


class TestXlangTagValidation:
    def test_tagged_types_pass(self):
        """Tagged types pass validation without error."""
        _validate_xlang_tags([SimpleMsg, InnerMsg])

    def test_untagged_type_raises(self):
        """Untagged dataclass raises TypeError."""
        with pytest.raises(TypeError, match="UntaggedMsg"):
            _validate_xlang_tags([UntaggedMsg])

    def test_mixed_tagged_untagged_raises(self):
        """If any type is untagged, validation fails."""
        with pytest.raises(TypeError, match="UntaggedMsg"):
            _validate_xlang_tags([SimpleMsg, UntaggedMsg])


# ── ForyCodec initialization tests ──────────────────────────────────────────


class TestForyCodecInit:
    def test_fory_config_defaults_to_xlang_for_xlang_mode(self, monkeypatch):
        seen: dict[str, object] = {}

        class DummyFory:
            def __init__(self, **kwargs):
                seen["kwargs"] = kwargs
            def register_type(self, *args, **kwargs):
                return None

        monkeypatch.setattr("aster.codec.pyfory.Fory", DummyFory)

        ForyCodec(mode=SerializationMode.XLANG, types=[])

        # ref=True and strict=True are part of Aster's Fory baseline
        # (XLANG + ref-tracking + strict) so the Python config stays in
        # lockstep with Java's `ForyCodec`. See
        # docs/_internal/fory-cross-binding.md.
        assert seen["kwargs"] == {"xlang": True, "ref": True, "strict": True}

    def test_fory_config_defaults_to_non_xlang_for_native_mode(self, monkeypatch):
        seen: dict[str, object] = {}

        class DummyFory:
            def __init__(self, **kwargs):
                seen["kwargs"] = kwargs
            def register_type(self, *args, **kwargs):
                return None

        monkeypatch.setattr("aster.codec.pyfory.Fory", DummyFory)

        ForyCodec(mode=SerializationMode.NATIVE, types=[])

        assert seen["kwargs"] == {"xlang": False, "ref": True, "strict": True}

    def test_fory_config_allows_explicit_override(self, monkeypatch):
        seen: dict[str, object] = {}

        class DummyFory:
            def __init__(self, **kwargs):
                seen["kwargs"] = kwargs
            def register_type(self, *args, **kwargs):
                return None

        monkeypatch.setattr("aster.codec.pyfory.Fory", DummyFory)

        ForyCodec(
            mode=SerializationMode.XLANG,
            types=[],
            fory_config=ForyConfig(xlang=False, extra_kwargs={"require_class_registration": True}),
        )

        assert seen["kwargs"] == {
            "xlang": False,
            "require_class_registration": True,
            "ref": True,
            "strict": True,
        }

    def test_xlang_mode_creation(self):
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[SimpleMsg])
        assert codec.mode == SerializationMode.XLANG

    def test_native_mode_creation(self):
        codec = ForyCodec(mode=SerializationMode.NATIVE, types=[SimpleMsg])
        assert codec.mode == SerializationMode.NATIVE

    def test_row_mode_creation(self):
        codec = ForyCodec(mode=SerializationMode.ROW, types=[SimpleMsg])
        assert codec.mode == SerializationMode.ROW

    def test_no_types(self):
        """Codec can be created with no user types (framework-internal only)."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[])
        assert len(codec.registered_types) > 0  # internal types

    def test_untagged_type_xlang_raises(self):
        """Creating an XLANG codec with untagged types raises TypeError."""
        with pytest.raises(TypeError, match="UntaggedMsg"):
            ForyCodec(mode=SerializationMode.XLANG, types=[UntaggedMsg])

    def test_untagged_type_native_ok(self):
        """NATIVE mode does not require tags."""
        # UntaggedMsg has no tag, but NATIVE should still work
        codec = ForyCodec(mode=SerializationMode.NATIVE, types=[UntaggedMsg])
        assert codec.mode == SerializationMode.NATIVE

    def test_framework_internal_types_registered(self):
        """StreamHeader, CallHeader, RpcStatus are always registered."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[])
        registered = codec.registered_types
        assert StreamHeader in registered
        assert CallHeader in registered
        assert RpcStatus in registered

    def test_nested_types_auto_discovered(self):
        """Nested types are automatically discovered and registered."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[OuterMsg])
        registered = codec.registered_types
        assert InnerMsg in registered
        assert OuterMsg in registered

    def test_custom_compression_threshold(self):
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=[SimpleMsg],
            compression_threshold=1024,
        )
        assert codec.compression_threshold == 1024

    def test_compression_disabled(self):
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=[SimpleMsg],
            compression_threshold=-1,
        )
        assert codec.compression_threshold == -1


# ── XLANG round-trip tests ──────────────────────────────────────────────────


class TestXlangRoundTrip:
    def test_simple_round_trip(self):
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[SimpleMsg])
        original = SimpleMsg(name="hello", value=42, active=True)
        data = codec.encode(original)
        assert isinstance(data, bytes)
        assert len(data) > 0
        restored = codec.decode(data, SimpleMsg)
        assert restored.name == original.name
        assert restored.value == original.value
        assert restored.active == original.active

    def test_nested_round_trip(self):
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[OuterMsg])
        original = OuterMsg(
            title="outer",
            inner=InnerMsg(label="inner", score=99),
            count=7,
        )
        data = codec.encode(original)
        restored = codec.decode(data, OuterMsg)
        assert restored.title == original.title
        assert restored.inner.label == original.inner.label
        assert restored.inner.score == original.inner.score
        assert restored.count == original.count

    def test_list_round_trip(self):
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[ListMsg])
        original = ListMsg(items=["a", "b", "c"], values=[1, 2, 3])
        data = codec.encode(original)
        restored = codec.decode(data, ListMsg)
        assert restored.items == original.items
        assert restored.values == original.values

    def test_optional_present_round_trip(self):
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[OptMsg])
        original = OptMsg(required="req", optional_str="opt", optional_int=42)
        data = codec.encode(original)
        restored = codec.decode(data, OptMsg)
        assert restored.required == original.required
        assert restored.optional_str == original.optional_str
        assert restored.optional_int == original.optional_int

    def test_optional_none_round_trip(self):
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[OptMsg])
        original = OptMsg(required="req", optional_str=None, optional_int=None)
        data = codec.encode(original)
        restored = codec.decode(data, OptMsg)
        assert restored.required == original.required
        assert restored.optional_str is None
        assert restored.optional_int is None

    def test_framework_types_round_trip(self):
        """Framework-internal types (StreamHeader, RpcStatus) round-trip."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[])
        header = StreamHeader(
            service="TestSvc",
            method="do_thing",
            version=1,
            callId=1,
            deadline=30,
            serializationMode=0,
            metadataKeys=["k1"],
            metadataValues=["v1"],
        )
        data = codec.encode(header)
        restored = codec.decode(data, StreamHeader)
        assert restored.service == header.service
        assert restored.method == header.method
        assert restored.version == header.version
        assert restored.metadataKeys == header.metadataKeys

    def test_rpc_status_round_trip(self):
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[])
        status = RpcStatus(
            code=StatusCode.INTERNAL,
            message="something broke",
            detailKeys=["trace"],
            detailValues=["abc"],
        )
        data = codec.encode(status)
        restored = codec.decode(data, RpcStatus)
        assert restored.code == StatusCode.INTERNAL
        assert restored.message == "something broke"

    def test_determinism(self):
        """Same object encodes to identical bytes."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[SimpleMsg])
        msg = SimpleMsg(name="det", value=7, active=True)
        b1 = codec.encode(msg)
        b2 = codec.encode(msg)
        assert b1 == b2

    def test_expected_type_mismatch_raises(self):
        """Decoding with wrong expected_type raises TypeError."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[SimpleMsg])
        msg = SimpleMsg(name="test", value=1, active=False)
        data = codec.encode(msg)
        with pytest.raises(TypeError, match="Expected RpcStatus"):
            codec.decode(data, RpcStatus)


# ── NATIVE round-trip tests ─────────────────────────────────────────────────


class TestNativeRoundTrip:
    def test_simple_native_round_trip(self):
        codec = ForyCodec(mode=SerializationMode.NATIVE, types=[SimpleMsg])
        original = SimpleMsg(name="native", value=100, active=True)
        data = codec.encode(original)
        assert isinstance(data, bytes)
        restored = codec.decode(data, SimpleMsg)
        assert restored.name == original.name
        assert restored.value == original.value

    def test_untagged_native_round_trip(self):
        """NATIVE mode works with untagged types."""
        codec = ForyCodec(mode=SerializationMode.NATIVE, types=[UntaggedMsg])
        original = UntaggedMsg(data="native-untagged")
        data = codec.encode(original)
        restored = codec.decode(data, UntaggedMsg)
        assert restored.data == original.data

    def test_nested_native_round_trip(self):
        codec = ForyCodec(mode=SerializationMode.NATIVE, types=[OuterMsg])
        original = OuterMsg(
            title="native-outer",
            inner=InnerMsg(label="native-inner", score=50),
            count=3,
        )
        data = codec.encode(original)
        restored = codec.decode(data, OuterMsg)
        assert restored.title == original.title
        assert restored.inner.label == original.inner.label


# ── ROW mode tests ──────────────────────────────────────────────────────────


class TestRowMode:
    def test_row_codec_creation(self):
        """ROW mode codec can be created."""
        codec = ForyCodec(mode=SerializationMode.ROW, types=[SimpleMsg])
        assert codec.mode == SerializationMode.ROW

    def test_row_round_trip(self):
        """ROW mode supports native row encode/decode round-trip."""
        codec = ForyCodec(mode=SerializationMode.ROW, types=[SimpleMsg])
        original = SimpleMsg(name="row", value=42, active=True)
        data = codec.encode(original)
        restored = codec.decode(data, SimpleMsg)
        assert restored.name == original.name
        assert restored.value == original.value

    def test_row_random_access_reads(self):
        """ROW mode exposes RowData for random-access field reads."""
        codec = ForyCodec(mode=SerializationMode.ROW, types=[SimpleMsg])
        original = SimpleMsg(name="row-access", value=99, active=True)
        data = codec.encode(original)
        row = codec.decode_row_data(data)
        assert row.get_boolean(0) is True
        assert row.get_str(1) == "row-access"
        assert row.get_int64(2) == 99

    def test_encode_row_schema(self):
        """encode_row_schema produces bytes in ROW mode."""
        codec = ForyCodec(mode=SerializationMode.ROW, types=[SimpleMsg])
        schema = codec.encode_row_schema()
        assert isinstance(schema, bytes)
        assert len(schema) > 0

    def test_row_mode_requires_single_root_type(self):
        """ROW mode currently requires exactly one root dataclass type."""
        with pytest.raises(ValueError, match="exactly one root type"):
            ForyCodec(mode=SerializationMode.ROW, types=[SimpleMsg, InnerMsg])

    def test_encode_row_schema_wrong_mode_raises(self):
        """encode_row_schema raises ValueError if not in ROW mode."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[SimpleMsg])
        with pytest.raises(ValueError, match="ROW mode"):
            codec.encode_row_schema()


# ── Compression round-trip tests ────────────────────────────────────────────


class TestCompressionRoundTrip:
    def test_small_payload_not_compressed(self):
        """Payloads below threshold are not compressed."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[SimpleMsg])
        msg = SimpleMsg(name="small", value=1, active=True)
        data, compressed = codec.encode_compressed(msg)
        assert not compressed
        # Should still be decodable
        restored = codec.decode_compressed(data, compressed, SimpleMsg)
        assert restored.name == "small"

    def test_large_payload_compressed(self):
        """Payloads above threshold are compressed."""
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=[LargeMsg],
            compression_threshold=100,  # Low threshold to trigger compression
        )
        msg = LargeMsg(payload="x" * 10_000)
        data, compressed = codec.encode_compressed(msg)
        assert compressed
        # Compressed data should be smaller than raw
        raw = codec.encode(msg)
        assert len(data) < len(raw)

    def test_compression_round_trip(self):
        """Compressed data decompresses and deserializes correctly."""
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=[LargeMsg],
            compression_threshold=100,
        )
        msg = LargeMsg(payload="y" * 10_000)
        data, compressed = codec.encode_compressed(msg)
        assert compressed
        restored = codec.decode_compressed(data, compressed, LargeMsg)
        assert restored.payload == msg.payload

    def test_compression_disabled(self):
        """With threshold=-1, compression is never applied."""
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=[LargeMsg],
            compression_threshold=-1,
        )
        msg = LargeMsg(payload="z" * 10_000)
        data, compressed = codec.encode_compressed(msg)
        assert not compressed

    def test_raw_compress_decompress(self):
        """Raw compress/decompress methods work correctly."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[])
        original = b"hello world " * 1000
        compressed = codec.compress(original)
        assert len(compressed) < len(original)
        decompressed = codec.decompress(compressed)
        assert decompressed == original

    def test_default_threshold(self):
        assert DEFAULT_COMPRESSION_THRESHOLD == 4096


# ── Registration error tests ───────────────────────────────────────────────


class TestRegistrationErrors:
    def test_untagged_type_xlang_raises_at_init(self):
        """Creating XLANG codec with untagged type raises TypeError immediately."""
        with pytest.raises(TypeError, match="has no @wire_type"):
            ForyCodec(mode=SerializationMode.XLANG, types=[UntaggedMsg])

    def test_untagged_nested_type_xlang_raises(self):
        """An untagged type discovered via type graph walking raises TypeError."""

        @dataclass
        @wire_type("test.codec/WrapperMsg")
        class WrapperMsg:
            inner: UntaggedMsg = field(default_factory=UntaggedMsg)

        with pytest.raises(TypeError, match="UntaggedMsg"):
            ForyCodec(mode=SerializationMode.XLANG, types=[WrapperMsg])


# ── Enum support tests ───────────────────────────────────────────────────


class Color(enum.Enum):
    RED = 1
    GREEN = 2
    BLUE = 3


class Priority(enum.IntEnum):
    LOW = 0
    MEDIUM = 1
    HIGH = 2


@dataclass
@wire_type("test.codec/EnumMsg")
class EnumMsg:
    color: Color = Color.RED
    priority: Priority = Priority.LOW
    label: str = ""


@dataclass
@wire_type("test.codec/OptionalEnumMsg")
class OptionalEnumMsg:
    color: Optional[Color] = None


class Status(enum.Enum):
    ACTIVE = "active"
    INACTIVE = "inactive"


@dataclass
@wire_type("test.codec/StatusMsg")
class StatusMsg:
    status: Status = Status.ACTIVE


class TestEnumTypeGraphWalking:
    def test_enum_discovered_in_type_graph(self):
        """Enum types in dataclass fields are discovered by the type walker."""
        types = _walk_type_graph([EnumMsg])
        assert Color in types
        assert Priority in types
        assert EnumMsg in types

    def test_enum_before_parent(self):
        """Enum types appear before the dataclass that uses them (leaves first)."""
        types = _walk_type_graph([EnumMsg])
        assert types.index(Color) < types.index(EnumMsg)
        assert types.index(Priority) < types.index(EnumMsg)

    def test_optional_enum_discovered(self):
        """Enum inside Optional[Enum] is unwrapped and discovered."""
        types = _walk_type_graph([OptionalEnumMsg])
        assert Color in types

    def test_enum_not_rejected_by_xlang_validation(self):
        """Enums don't need @wire_type -- they should pass XLANG validation."""
        types = _walk_type_graph([EnumMsg])
        _validate_xlang_tags(types)  # should not raise


class TestEnumCoercion:
    """Test that raw primitives are coerced to enum members after deserialization.

    This simulates cross-language interop where the producer (e.g. TypeScript)
    serializes enums as their primitive value because Fory JS lacks NAMED_ENUM.
    TODO: add true cross-language round-trip tests (Python <-> TS) when TS
    binding tests are in place.
    """

    def test_coerce_int_enum(self):
        from aster.codec import _coerce_enum_fields
        msg = EnumMsg(color=Color.RED, priority=Priority.HIGH, label="test")
        # Simulate what Fory deserializes from a TS producer
        object.__setattr__(msg, "color", 1)       # raw int instead of Color.RED
        object.__setattr__(msg, "priority", 2)     # raw int instead of Priority.HIGH
        _coerce_enum_fields(msg)
        assert msg.color is Color.RED
        assert msg.priority is Priority.HIGH
        assert msg.label == "test"

    def test_coerce_string_enum(self):
        from aster.codec import _coerce_enum_fields
        msg = StatusMsg()
        object.__setattr__(msg, "status", "inactive")
        _coerce_enum_fields(msg)
        assert msg.status is Status.INACTIVE

    def test_coerce_optional_enum_none(self):
        from aster.codec import _coerce_enum_fields
        msg = OptionalEnumMsg(color=None)
        _coerce_enum_fields(msg)
        assert msg.color is None

    def test_coerce_optional_enum_raw_value(self):
        from aster.codec import _coerce_enum_fields
        msg = OptionalEnumMsg()
        object.__setattr__(msg, "color", 2)  # raw int for Color.GREEN
        _coerce_enum_fields(msg)
        assert msg.color is Color.GREEN

    def test_coerce_invalid_value_left_as_is(self):
        from aster.codec import _coerce_enum_fields
        msg = EnumMsg()
        object.__setattr__(msg, "color", 999)  # no such member
        _coerce_enum_fields(msg)
        assert msg.color == 999  # unchanged


class TestEnumCodecRegistration:
    def test_xlang_codec_with_enum_fields(self):
        """XLANG codec can be created with types containing enum fields."""
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[EnumMsg])
        assert Color in codec.registered_types
        assert Priority in codec.registered_types

    def test_native_codec_with_enum_fields(self):
        """NATIVE codec can be created with types containing enum fields."""
        codec = ForyCodec(mode=SerializationMode.NATIVE, types=[EnumMsg])
        assert Color in codec.registered_types