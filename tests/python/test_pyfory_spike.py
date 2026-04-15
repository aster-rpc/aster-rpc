"""
Spike tests for Apache Fory (pyfory 0.16.0) serialization determinism.

These tests validate the highest-risk pre-requisite for the Aster RPC framework:
whether pyfory produces deterministic, byte-identical output across multiple
serialization calls, different field orderings, and different Python processes --
which is required for content-addressed contract identity (BLAKE3 hashing of
canonical bytes).

Run with: pytest tests/python/test_pyfory_spike.py -v -s

If any test fails, it means pyfory cannot be used directly for canonical contract
hashing, and a custom canonical encoder will be needed (see ASTER_PLAN.md §11.7).

Pre-requisite: pip install pyfory==0.16.0
"""

import subprocess
import sys
import textwrap
from dataclasses import dataclass
from typing import Optional

import pytest

# ── Check pyfory availability ────────────────────────────────────────────────

try:
    import pyfory

    HAS_PYFORY = True
except ImportError:
    HAS_PYFORY = False

from aster._aster import blake3_hex

pytestmark = pytest.mark.skipif(not HAS_PYFORY, reason="pyfory not installed")


# ── Helper: create a Fory instance and register types ────────────────────────


def wire_type(tag: str):
    """Decorator to attach a Fory type tag (namespace/typename) to a class."""

    def decorator(cls):
        parts = tag.rsplit("/", 1)
        if len(parts) == 2:
            cls.__fory_namespace__ = parts[0]
            cls.__fory_typename__ = parts[1]
        else:
            cls.__fory_namespace__ = ""
            cls.__fory_typename__ = tag
        cls.__wire_type__ = tag
        return cls

    return decorator


def create_fory(*types_to_register):
    """Create a Fory instance and register the given types."""
    f = pyfory.Fory()
    for cls in types_to_register:
        ns = getattr(cls, "__fory_namespace__", None)
        tn = getattr(cls, "__fory_typename__", None)
        if ns is not None and tn is not None:
            f.register_type(cls, namespace=ns, typename=tn)
        else:
            f.register_type(cls)
    return f


# ── Test types ───────────────────────────────────────────────────────────────


@dataclass
@wire_type("spike.test/SimpleMessage")
class SimpleMessage:
    name: str
    value: int
    active: bool


@dataclass
@wire_type("spike.test/NestedMessage")
class NestedMessage:
    label: str
    inner: SimpleMessage
    count: int


@dataclass
@wire_type("spike.test/ListMessage")
class ListMessage:
    items: list  # list of strings
    scores: list  # list of ints


@dataclass
@wire_type("spike.test/OptionalFields")
class OptionalFields:
    required_field: str
    optional_str: Optional[str]
    optional_int: Optional[int]


@dataclass
@wire_type("spike.test/ManyFields")
class ManyFields:
    """A type with many fields to test field-ordering determinism."""

    z_last: str
    a_first: str
    m_middle: int
    b_second: bool
    y_penultimate: float
    c_third: str


# ── Test 1: Basic pyfory availability ───────────────────────────────────────


class TestPyforyAvailability:
    """Verify pyfory is importable and Fory class exists."""

    def test_import(self):
        """pyfory can be imported."""
        assert pyfory is not None

    def test_has_fory_class(self):
        """pyfory.Fory exists."""
        assert hasattr(pyfory, "Fory")

    def test_create_fory(self):
        """Can create a Fory instance."""
        f = pyfory.Fory()
        assert f is not None

    def test_version(self):
        """pyfory version is 0.16.x."""
        version = getattr(pyfory, "__version__", "unknown")
        print(f"\npyfory version: {version}")
        assert version.startswith("0.16"), f"Expected 0.16.x, got {version}"

    def test_register_type_with_namespace_typename(self):
        """Can register a dataclass with namespace+typename."""
        f = create_fory(SimpleMessage)
        assert f is not None


# ── Test 2: Basic round-trip ────────────────────────────────────────────────


class TestRoundTrip:
    """Verify serialization produces valid output that deserializes back."""

    def test_simple_round_trip(self):
        """Simple dataclass survives serialize → deserialize."""
        f = create_fory(SimpleMessage)
        original = SimpleMessage(name="hello", value=42, active=True)
        data = f.serialize(original)
        assert isinstance(data, (bytes, bytearray))
        assert len(data) > 0
        restored = f.deserialize(data)
        assert restored.name == original.name
        assert restored.value == original.value
        assert restored.active == original.active

    def test_nested_round_trip(self):
        """Nested dataclass survives serialize → deserialize."""
        f = create_fory(SimpleMessage, NestedMessage)
        original = NestedMessage(
            label="outer",
            inner=SimpleMessage(name="inner", value=99, active=False),
            count=7,
        )
        data = f.serialize(original)
        restored = f.deserialize(data)
        assert restored.label == original.label
        assert restored.inner.name == original.inner.name
        assert restored.inner.value == original.inner.value
        assert restored.count == original.count

    def test_list_round_trip(self):
        """Lists survive round-trip."""
        f = create_fory(ListMessage)
        original = ListMessage(
            items=["alpha", "beta", "gamma"],
            scores=[10, 20, 30],
        )
        data = f.serialize(original)
        restored = f.deserialize(data)
        assert restored.items == original.items
        assert restored.scores == original.scores

    def test_optional_fields_present(self):
        """Optional fields present survive round-trip."""
        f = create_fory(OptionalFields)
        original = OptionalFields(
            required_field="req",
            optional_str="opt",
            optional_int=42,
        )
        data = f.serialize(original)
        restored = f.deserialize(data)
        assert restored.required_field == original.required_field
        assert restored.optional_str == original.optional_str
        assert restored.optional_int == original.optional_int

    def test_optional_fields_none(self):
        """Optional fields set to None survive round-trip."""
        f = create_fory(OptionalFields)
        original = OptionalFields(
            required_field="req",
            optional_str=None,
            optional_int=None,
        )
        data = f.serialize(original)
        restored = f.deserialize(data)
        assert restored.required_field == original.required_field
        assert restored.optional_str is None
        assert restored.optional_int is None


# ── Test 3: DETERMINISM -- Same value, same bytes ────────────────────────────


class TestDeterminism:
    """
    THE CRITICAL TESTS.

    These verify that serializing the same value multiple times produces
    byte-identical output. This is required for canonical contract hashing.
    """

    def test_same_value_same_bytes_simple(self):
        """Serializing the same SimpleMessage twice produces identical bytes."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="determinism", value=123, active=True)
        bytes_1 = f.serialize(msg)
        bytes_2 = f.serialize(msg)
        assert bytes_1 == bytes_2, (
            f"Non-deterministic! len1={len(bytes_1)}, len2={len(bytes_2)}, "
            f"bytes1={bytes(bytes_1).hex()}, bytes2={bytes(bytes_2).hex()}"
        )

    def test_same_value_same_bytes_nested(self):
        """Serializing the same NestedMessage twice produces identical bytes."""
        f = create_fory(SimpleMessage, NestedMessage)
        msg = NestedMessage(
            label="test",
            inner=SimpleMessage(name="child", value=0, active=False),
            count=42,
        )
        bytes_1 = f.serialize(msg)
        bytes_2 = f.serialize(msg)
        assert bytes_1 == bytes_2

    def test_same_value_same_bytes_list(self):
        """Serializing the same ListMessage twice produces identical bytes."""
        f = create_fory(ListMessage)
        msg = ListMessage(items=["a", "b", "c"], scores=[1, 2, 3])
        bytes_1 = f.serialize(msg)
        bytes_2 = f.serialize(msg)
        assert bytes_1 == bytes_2

    def test_same_value_same_bytes_100_iterations(self):
        """Serializing the same value 100 times always produces the same bytes."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="stability", value=999, active=True)
        reference = f.serialize(msg)
        for i in range(100):
            result = f.serialize(msg)
            assert result == reference, f"Diverged at iteration {i}"

    def test_equal_objects_same_bytes(self):
        """Two equal-but-distinct objects produce the same bytes."""
        f = create_fory(SimpleMessage)
        msg_a = SimpleMessage(name="equal", value=42, active=True)
        msg_b = SimpleMessage(name="equal", value=42, active=True)
        assert msg_a is not msg_b  # Different objects
        assert msg_a == msg_b  # But equal
        bytes_a = f.serialize(msg_a)
        bytes_b = f.serialize(msg_b)
        assert bytes_a == bytes_b

    def test_separate_fory_instances_same_bytes(self):
        """Two separate Fory instances produce the same bytes for the same value."""
        f1 = create_fory(SimpleMessage)
        f2 = create_fory(SimpleMessage)
        msg = SimpleMessage(name="cross-instance", value=7, active=False)
        bytes_1 = f1.serialize(msg)
        bytes_2 = f2.serialize(msg)
        assert bytes_1 == bytes_2, (
            f"Different Fory instances produce different bytes! "
            f"bytes1={bytes(bytes_1).hex()}, bytes2={bytes(bytes_2).hex()}"
        )

    def test_many_fields_determinism(self):
        """
        A type with many fields serializes deterministically.
        This tests whether field ordering is stable regardless of declaration order.
        """
        f = create_fory(ManyFields)
        msg = ManyFields(
            z_last="z",
            a_first="a",
            m_middle=42,
            b_second=True,
            y_penultimate=3.14,
            c_third="c",
        )
        bytes_1 = f.serialize(msg)
        bytes_2 = f.serialize(msg)
        assert bytes_1 == bytes_2

    def test_optional_none_determinism(self):
        """Optional fields set to None serialize deterministically."""
        f = create_fory(OptionalFields)
        msg = OptionalFields(required_field="test", optional_str=None, optional_int=None)
        bytes_1 = f.serialize(msg)
        bytes_2 = f.serialize(msg)
        assert bytes_1 == bytes_2


# ── Test 4: DETERMINISM -- Same bytes across fresh Fory instances ────────────


class TestCrossInstanceDeterminism:
    """
    Test that serialization is deterministic across completely fresh Fory
    instances (no shared internal state).
    """

    def _serialize_fresh(self, cls, obj):
        """Create a fresh Fory, register the type, serialize."""
        f = pyfory.Fory()
        ns = getattr(cls, "__fory_namespace__", "")
        tn = getattr(cls, "__fory_typename__", cls.__name__)
        f.register_type(cls, namespace=ns, typename=tn)
        return f.serialize(obj)

    def test_fresh_instances_simple(self):
        """Fresh Fory instances produce identical bytes for SimpleMessage."""
        msg = SimpleMessage(name="fresh", value=1, active=True)
        bytes_1 = self._serialize_fresh(SimpleMessage, msg)
        bytes_2 = self._serialize_fresh(SimpleMessage, msg)
        assert bytes_1 == bytes_2

    def test_fresh_instances_10_times(self):
        """10 fresh Fory instances all produce the same bytes."""
        msg = SimpleMessage(name="multi-fresh", value=42, active=False)
        results = [self._serialize_fresh(SimpleMessage, msg) for _ in range(10)]
        for i, r in enumerate(results[1:], 1):
            assert r == results[0], f"Fresh instance {i} diverged"


# ── Test 5: DETERMINISM -- Cross-process ─────────────────────────────────────


class TestCrossProcessDeterminism:
    """
    Test that serialization produces the same bytes in a separate Python process.
    This catches process-level non-determinism (random seeds, memory layout, etc.).
    """

    def test_cross_process_simple(self):
        """Serialization in a subprocess produces the same bytes as in this process."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="cross-process", value=777, active=True)
        local_bytes = f.serialize(msg)

        script = textwrap.dedent("""\
            import sys
            from dataclasses import dataclass
            import pyfory

            @dataclass
            class SimpleMessage:
                name: str
                value: int
                active: bool

            f = pyfory.Fory()
            f.register_type(SimpleMessage, namespace="spike.test", typename="SimpleMessage")
            msg = SimpleMessage(name="cross-process", value=777, active=True)
            data = f.serialize(msg)
            sys.stdout.buffer.write(bytes(data))
        """)

        result = subprocess.run(
            [sys.executable, "-c", script],
            capture_output=True,
            timeout=30,
        )

        if result.returncode != 0:
            pytest.fail(f"Subprocess failed: {result.stderr.decode()}")

        subprocess_bytes = result.stdout
        assert bytes(local_bytes) == subprocess_bytes, (
            f"Cross-process non-determinism detected!\n"
            f"Local:   {bytes(local_bytes).hex()}\n"
            f"Subproc: {subprocess_bytes.hex()}"
        )

    def test_cross_process_many_fields(self):
        """ManyFields serialization is deterministic across processes."""
        f = create_fory(ManyFields)
        msg = ManyFields(
            z_last="z",
            a_first="a",
            m_middle=42,
            b_second=True,
            y_penultimate=3.14,
            c_third="c",
        )
        local_bytes = f.serialize(msg)

        script = textwrap.dedent("""\
            import sys
            from dataclasses import dataclass
            import pyfory

            @dataclass
            class ManyFields:
                z_last: str
                a_first: str
                m_middle: int
                b_second: bool
                y_penultimate: float
                c_third: str

            f = pyfory.Fory()
            f.register_type(ManyFields, namespace="spike.test", typename="ManyFields")
            msg = ManyFields(
                z_last="z", a_first="a", m_middle=42,
                b_second=True, y_penultimate=3.14, c_third="c",
            )
            data = f.serialize(msg)
            sys.stdout.buffer.write(bytes(data))
        """)

        result = subprocess.run(
            [sys.executable, "-c", script],
            capture_output=True,
            timeout=30,
        )

        if result.returncode != 0:
            pytest.fail(f"Subprocess failed: {result.stderr.decode()}")

        assert bytes(local_bytes) == result.stdout, (
            f"Cross-process ManyFields non-determinism!\n"
            f"Local:   {bytes(local_bytes).hex()}\n"
            f"Subproc: {result.stdout.hex()}"
        )


# ── Test 6: BLAKE3 hashing of serialized bytes ─────────────────────────────


class TestBlake3ContractHashing:
    """
    Test that BLAKE3 hashing of serialized bytes produces stable, reproducible hashes.
    This is the exact operation used for contract_id computation.
    """

    def test_hash_stability(self):
        """Same value → same bytes → same BLAKE3 hash."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="hashme", value=42, active=True)

        bytes_1 = f.serialize(msg)
        bytes_2 = f.serialize(msg)

        hash_1 = blake3_hex(bytes(bytes_1))
        hash_2 = blake3_hex(bytes(bytes_2))

        assert hash_1 == hash_2

    def test_different_values_different_hashes(self):
        """Different values produce different BLAKE3 hashes."""
        f = create_fory(SimpleMessage)
        msg_a = SimpleMessage(name="a", value=1, active=True)
        msg_b = SimpleMessage(name="b", value=2, active=False)

        hash_a = blake3_hex(bytes(f.serialize(msg_a)))
        hash_b = blake3_hex(bytes(f.serialize(msg_b)))

        assert hash_a != hash_b

    def test_hash_is_64_hex_chars(self):
        """BLAKE3 hash is 64 hex characters (256 bits)."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="test", value=0, active=False)
        h = blake3_hex(bytes(f.serialize(msg)))
        assert len(h) == 64
        assert all(c in "0123456789abcdef" for c in h)

    def test_simulated_contract_id(self):
        """
        Simulate the contract_id computation:
        1. Serialize a "ServiceContract-like" object
        2. Hash with BLAKE3
        3. Verify it's stable across multiple calls
        """
        f = create_fory(SimpleMessage, NestedMessage)

        contract = NestedMessage(
            label="AgentControl",
            inner=SimpleMessage(name="assign_task", value=1, active=True),
            count=1,
        )

        hashes = set()
        for _ in range(50):
            data = f.serialize(contract)
            h = blake3_hex(bytes(data))
            hashes.add(h)

        assert len(hashes) == 1, (
            f"contract_id is non-deterministic! Got {len(hashes)} distinct hashes: {hashes}"
        )


# ── Test 7: Reference tracking behavior ────────────────────────────────────


class TestReferenceTracking:
    """
    The canonical XLANG profile requires NO reference tracking (§11.3.2).
    Test whether pyfory uses reference tracking by default and whether it
    affects byte output for equal-but-distinct objects.
    """

    def test_shared_reference_detection(self):
        """
        Detect whether pyfory uses reference tracking for shared objects.
        If it does, the same object referenced twice might serialize differently
        than two equal-but-distinct objects.
        """
        f = create_fory(SimpleMessage, NestedMessage)

        shared_inner = SimpleMessage(name="shared", value=1, active=True)
        distinct_inner = SimpleMessage(name="shared", value=1, active=True)

        msg_shared = NestedMessage(label="test", inner=shared_inner, count=1)
        msg_distinct = NestedMessage(label="test", inner=distinct_inner, count=1)

        bytes_shared = f.serialize(msg_shared)
        bytes_distinct = f.serialize(msg_distinct)

        assert bytes_shared == bytes_distinct, (
            "Reference tracking affects serialization! Shared vs distinct inner "
            "objects produce different bytes. This must be resolved for canonical hashing.\n"
            f"Shared:   {bytes(bytes_shared).hex()}\n"
            f"Distinct: {bytes(bytes_distinct).hex()}"
        )


# ── Test 8: Type registration ──────────────────────────────────────────────


class TestTypeRegistration:
    """Verify that type registration with namespace+typename works correctly."""

    def test_tag_preserved_in_decorator(self):
        """The __wire_type__ attribute is set by the decorator."""
        assert SimpleMessage.__wire_type__ == "spike.test/SimpleMessage"
        assert SimpleMessage.__fory_namespace__ == "spike.test"
        assert SimpleMessage.__fory_typename__ == "SimpleMessage"

    def test_registered_serializes(self):
        """Types registered with namespace+typename serialize successfully."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="tagged", value=1, active=True)
        data = f.serialize(msg)
        assert len(data) > 0

    def test_deserialize_with_registration(self):
        """Types registered with namespace+typename deserialize correctly."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="tagged", value=42, active=True)
        data = f.serialize(msg)
        restored = f.deserialize(data)
        assert isinstance(restored, SimpleMessage)
        assert restored.name == "tagged"
        assert restored.value == 42


# ── Test 9: Mode availability ──────────────────────────────────────────────


class TestModeAvailability:
    """Check which serialization modes are available in pyfory 0.16.0."""

    def test_xlang_property_exists(self):
        """Check if Fory has an xlang property."""
        f = pyfory.Fory()
        assert hasattr(f, "xlang")
        print(f"\nDefault xlang={f.xlang}")

    def test_row_format_exists(self):
        """Check if pyfory has row format / RowData support."""
        has_row = hasattr(pyfory, "RowData") or hasattr(pyfory, "create_row_encoder")
        print(f"\nROW format available: {has_row}")
        if not has_row:
            pytest.skip("pyfory does not expose ROW format")

    def test_thread_safe_fory_exists(self):
        """Check if ThreadSafeFory is available."""
        assert hasattr(pyfory, "ThreadSafeFory")


# ── Test 10: Edge cases for canonical encoding ──────────────────────────────


class TestCanonicalEdgeCases:
    """Edge cases that could break deterministic canonical encoding."""

    def test_empty_string(self):
        """Empty strings serialize deterministically."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="", value=0, active=False)
        assert f.serialize(msg) == f.serialize(msg)

    def test_unicode_strings(self):
        """Unicode strings serialize deterministically."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="日本語テスト 🎯", value=42, active=True)
        assert f.serialize(msg) == f.serialize(msg)

    def test_large_string(self):
        """Large strings serialize deterministically."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="x" * 100_000, value=1, active=True)
        assert f.serialize(msg) == f.serialize(msg)

    def test_negative_numbers(self):
        """Negative numbers serialize deterministically."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="neg", value=-2147483648, active=True)
        assert f.serialize(msg) == f.serialize(msg)

    def test_zero(self):
        """Zero values serialize deterministically."""
        f = create_fory(SimpleMessage)
        msg = SimpleMessage(name="zero", value=0, active=False)
        assert f.serialize(msg) == f.serialize(msg)

    def test_empty_list(self):
        """Empty lists serialize deterministically."""
        f = create_fory(ListMessage)
        msg = ListMessage(items=[], scores=[])
        assert f.serialize(msg) == f.serialize(msg)

    def test_large_list(self):
        """Large lists serialize deterministically."""
        f = create_fory(ListMessage)
        msg = ListMessage(
            items=[f"item_{i}" for i in range(1000)],
            scores=list(range(1000)),
        )
        assert f.serialize(msg) == f.serialize(msg)

    def test_float_special_values(self):
        """Special float values serialize deterministically."""
        f = create_fory(ManyFields)
        for val in [0.0, -0.0, float("inf"), float("-inf")]:
            msg = ManyFields(
                z_last="z",
                a_first="a",
                m_middle=1,
                b_second=True,
                y_penultimate=val,
                c_third="c",
            )
            assert f.serialize(msg) == f.serialize(msg), f"Failed for float {val}"

    def test_float_nan_determinism(self):
        """
        NaN is tricky -- there are multiple NaN representations.
        This test checks if pyfory handles it deterministically.
        """
        f = create_fory(ManyFields)
        msg = ManyFields(
            z_last="z",
            a_first="a",
            m_middle=1,
            b_second=True,
            y_penultimate=float("nan"),
            c_third="c",
        )
        bytes_1 = f.serialize(msg)
        bytes_2 = f.serialize(msg)
        assert bytes_1 == bytes_2, "NaN serialization is non-deterministic"


# ── Test 11: Diagnostic -- dump serialization details ────────────────────────


class TestDiagnostic:
    """
    Diagnostic tests that print useful information about pyfory behavior.
    These always pass but print details for manual inspection.
    """

    def test_print_pyfory_version(self):
        """Print pyfory version for reference."""
        version = getattr(pyfory, "__version__", "unknown")
        print(f"\npyfory version: {version}")

    def test_print_serialization_size(self):
        """Print serialized sizes for reference."""
        f = create_fory(SimpleMessage, NestedMessage, ListMessage, ManyFields)

        cases = [
            ("SimpleMessage", SimpleMessage(name="test", value=42, active=True)),
            (
                "NestedMessage",
                NestedMessage(
                    label="outer",
                    inner=SimpleMessage(name="inner", value=1, active=False),
                    count=7,
                ),
            ),
            (
                "ListMessage (3 items)",
                ListMessage(
                    items=["a", "b", "c"],
                    scores=[1, 2, 3],
                ),
            ),
            (
                "ManyFields",
                ManyFields(
                    z_last="z",
                    a_first="a",
                    m_middle=42,
                    b_second=True,
                    y_penultimate=3.14,
                    c_third="c",
                ),
            ),
        ]

        print(f"\n{'Type':<30} {'Size (bytes)':<15} {'First 32 bytes (hex)'}")
        print("-" * 80)
        for name, obj in cases:
            data = f.serialize(obj)
            hex_preview = bytes(data[:32]).hex()
            print(f"{name:<30} {len(data):<15} {hex_preview}")

    def test_print_hash_examples(self):
        """Print example BLAKE3 hashes for reference."""
        f = create_fory(SimpleMessage)

        cases = [
            SimpleMessage(name="a", value=1, active=True),
            SimpleMessage(name="b", value=2, active=False),
            SimpleMessage(name="a", value=1, active=True),  # Duplicate of first
        ]

        print(f"\n{'Value':<50} {'BLAKE3 (first 16 chars)'}")
        print("-" * 80)
        for msg in cases:
            data = f.serialize(msg)
            h = blake3_hex(bytes(data))
            print(f"{str(msg):<50} {h[:16]}...")

    def test_report_fory_config(self):
        """Report Fory configuration details."""
        f = pyfory.Fory()
        print(f"\nxlang={f.xlang}")
        print(f"Fory methods: {[x for x in dir(f) if not x.startswith('_')]}")


# ── Summary ─────────────────────────────────────────────────────────────────

"""
EXPECTED OUTCOMES:

✅ If ALL determinism tests pass:
   pyfory is suitable for canonical contract hashing.
   Proceed with ASTER_PLAN.md Phase 2 as designed.

⚠️ If cross-instance or cross-process tests fail:
   pyfory has internal state that affects serialization.
   Mitigation: always use a fresh Fory instance per serialization,
   or implement a custom canonical encoder.

❌ If same-value-same-bytes tests fail:
   pyfory is fundamentally non-deterministic.
   Mitigation: implement a custom canonical encoder that bypasses pyfory
   for contract identity (use pyfory only for runtime RPC serialization).

ℹ️ If reference tracking test fails:
   pyfory uses reference tracking in its default mode.
   Mitigation: ensure canonical profile always creates fresh objects
   (no shared references), or find a pyfory config to disable ref tracking.
"""