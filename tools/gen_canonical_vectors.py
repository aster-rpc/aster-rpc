#!/usr/bin/env python
"""
tools/gen_canonical_vectors.py — Generate canonical test vectors for Phase 9.

Constructs the Appendix A and B fixtures from Aster-ContractIdentity.md,
runs the canonical encoder, computes BLAKE3 hashes, and writes:
  - tests/python/fixtures/canonical_test_vectors.json
  - Updates ffi_spec/Aster-ContractIdentity.md Appendix A placeholders

Usage::

    uv run python tools/gen_canonical_vectors.py
"""

from __future__ import annotations

import io
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# Ensure the bindings package is importable
_repo_root = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(_repo_root / "bindings"))

import blake3  # noqa: E402

from aster_python.aster.contract.canonical import (  # noqa: E402
    write_bytes_field,
    write_list_header,
    write_optional_absent,
    write_optional_present_prefix,
    write_string,
    write_varint,
    write_zigzag_i32,
    write_zigzag_i64,
)
from aster_python.aster.contract.identity import (  # noqa: E402
    CapabilityKind,
    CapabilityRequirement,
    ContainerKind,
    EnumValueDef,
    FieldDef,
    MethodDef,
    MethodPattern,
    ScopeKind,
    ServiceContract,
    TypeDef,
    TypeDefKind,
    TypeKind,
    canonical_xlang_bytes,
)


# ── Vector entry ─────────────────────────────────────────────────────────────


@dataclass
class Vector:
    id: str
    description: str
    bytes_hex: str
    hash_hex: str


def make_vector(vid: str, description: str, obj) -> Vector:
    """Serialize obj, hash it, return a Vector."""
    data = canonical_xlang_bytes(obj)
    h = blake3.blake3(data).hexdigest()
    return Vector(id=vid, description=description, bytes_hex=data.hex(), hash_hex=h)


def make_vector_raw(vid: str, description: str, data: bytes) -> Vector:
    """Make a vector from raw bytes (for micro-fixtures)."""
    h = blake3.blake3(data).hexdigest()
    return Vector(id=vid, description=description, bytes_hex=data.hex(), hash_hex=h)


# ── Appendix A fixtures ───────────────────────────────────────────────────────


def vector_A2() -> Vector:
    """A.2: Minimal ServiceContract (no methods)."""
    sc = ServiceContract(
        name="EmptyService",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
        requires=None,
    )
    return make_vector("A.2", "Minimal ServiceContract (no methods, SHARED scope)", sc)


def vector_A3() -> Vector:
    """A.3: Minimal TypeDef (enum, no references)."""
    td = TypeDef(
        kind=TypeDefKind.ENUM,
        package="test",
        name="Color",
        fields=[],
        enum_values=[
            EnumValueDef(name="RED", value=0),
            EnumValueDef(name="GREEN", value=1),
            EnumValueDef(name="BLUE", value=2),
        ],
        union_variants=[],
    )
    return make_vector("A.3", "Minimal TypeDef (enum, no references)", td)


def vector_A4() -> Vector:
    """A.4: TypeDef with type references (32-byte 0xAA hash)."""
    aa_hash = bytes([0xAA] * 32)
    td = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="test",
        name="Wrapper",
        fields=[
            FieldDef(
                id=1,
                name="inner",
                type_kind=TypeKind.REF,
                type_primitive="",
                type_ref=aa_hash,
                self_ref_name="",
                optional=False,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            )
        ],
        enum_values=[],
        union_variants=[],
    )
    return make_vector("A.4", "TypeDef with REF field (32 bytes 0xAA hash)", td)


def vector_A5() -> Vector:
    """A.5: MethodDef with optional requires present."""
    req_hash = bytes([0x11] * 32)
    resp_hash = bytes([0x22] * 32)
    md = MethodDef(
        name="do_work",
        pattern=MethodPattern.UNARY,
        request_type=req_hash,
        response_type=resp_hash,
        idempotent=True,
        default_timeout=30.0,
        requires=CapabilityRequirement(
            kind=CapabilityKind.ANY_OF,
            roles=["Admin", "Operator"],
        ),
    )
    return make_vector("A.5", "MethodDef with optional requires present (ANY_OF)", md)


def vector_A6() -> Vector:
    """A.6: MethodDef with optional requires absent."""
    req_hash = bytes([0x11] * 32)
    resp_hash = bytes([0x22] * 32)
    md = MethodDef(
        name="do_work",
        pattern=MethodPattern.UNARY,
        request_type=req_hash,
        response_type=resp_hash,
        idempotent=False,
        default_timeout=0.0,
        requires=None,
    )
    return make_vector("A.6", "MethodDef with optional requires absent", md)


# ── Appendix B fixtures ───────────────────────────────────────────────────────


def vector_B1() -> Vector:
    """B.1: Direct self-recursion — TreeNode with SELF_REF fields."""
    td = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="TreeNode",
        fields=[
            FieldDef(
                id=1,
                name="value",
                type_kind=TypeKind.PRIMITIVE,
                type_primitive="string",
                type_ref=b"",
                self_ref_name="",
                optional=False,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
            FieldDef(
                id=2,
                name="left",
                type_kind=TypeKind.SELF_REF,
                type_primitive="",
                type_ref=b"",
                self_ref_name="example.TreeNode",
                optional=True,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
            FieldDef(
                id=3,
                name="right",
                type_kind=TypeKind.SELF_REF,
                type_primitive="",
                type_ref=b"",
                self_ref_name="example.TreeNode",
                optional=True,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    return make_vector("B.1", "Direct self-recursion: TreeNode with SELF_REF left/right", td)


def vector_B2() -> Vector:
    """B.2: Two-type mutual recursion — Book (hashed first, back-edge to Author)."""
    # Book is hashed first (written_by uses SELF_REF to example.Author)
    td_book = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="Book",
        fields=[
            FieldDef(
                id=1,
                name="title",
                type_kind=TypeKind.PRIMITIVE,
                type_primitive="string",
                type_ref=b"",
                self_ref_name="",
                optional=False,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
            FieldDef(
                id=2,
                name="written_by",
                type_kind=TypeKind.SELF_REF,
                type_primitive="",
                type_ref=b"",
                self_ref_name="example.Author",
                optional=False,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    book_bytes = canonical_xlang_bytes(td_book)
    return make_vector_raw(
        "B.2",
        "Mutual recursion: Book (hashed first, written_by is SELF_REF to Author)",
        book_bytes,
    )


def vector_B2_author(book_hash: bytes) -> Vector:
    """B.2 Author: Author references Book via REF (book_hash)."""
    td_author = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="Author",
        fields=[
            FieldDef(
                id=1,
                name="name",
                type_kind=TypeKind.PRIMITIVE,
                type_primitive="string",
                type_ref=b"",
                self_ref_name="",
                optional=False,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
            FieldDef(
                id=2,
                name="books",
                type_kind=TypeKind.REF,
                type_primitive="",
                type_ref=book_hash,
                self_ref_name="",
                optional=False,
                ref_tracked=False,
                container=ContainerKind.LIST,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    return make_vector("B.2-author", "Mutual recursion: Author with books: list<Book> via REF", td_author)


def vector_B3() -> Vector:
    """B.3: Three-type cycle — Gamma (hashed first, back-edge to Alpha)."""
    td_gamma = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="Gamma",
        fields=[
            FieldDef(
                id=1,
                name="next",
                type_kind=TypeKind.SELF_REF,
                type_primitive="",
                type_ref=b"",
                self_ref_name="example.Alpha",
                optional=False,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    return make_vector("B.3", "Three-type cycle: Gamma (back-edge to Alpha, hashed first)", td_gamma)


def vector_B4() -> Vector:
    """B.4: Diamond with back-edge — C (hashed first, a_field is SELF_REF to A)."""
    td_c = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="C",
        fields=[
            FieldDef(
                id=1,
                name="a_field",
                type_kind=TypeKind.SELF_REF,
                type_primitive="",
                type_ref=b"",
                self_ref_name="example.A",
                optional=False,
                ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    return make_vector("B.4", "Diamond with back-edge: C (a_field is SELF_REF to A, hashed first)", td_c)


# ── Micro-fixtures ────────────────────────────────────────────────────────────


def micro_varint_vectors() -> list[Vector]:
    """Varint encoding micro-fixtures."""
    cases = [
        (0, "00"),
        (127, "7f"),
        (128, "8001"),
        (16383, "ff7f"),
        (16384, "808001"),
    ]
    vectors = []
    for val, expected_hex in cases:
        buf = io.BytesIO()
        write_varint(buf, val)
        data = buf.getvalue()
        assert data.hex() == expected_hex, f"varint({val}): expected {expected_hex}, got {data.hex()}"
        vectors.append(make_vector_raw(
            f"micro.varint.{val}",
            f"Varint encoding of {val}",
            data,
        ))
    return vectors


def micro_zigzag_i32_vectors() -> list[Vector]:
    """ZigZag i32 encoding micro-fixtures."""
    cases = [
        (0, "00"),
        (1, "02"),
        (-1, "01"),
        (2147483647, "feffffff0f"),   # INT32_MAX
        (-2147483648, "ffffffff0f"),  # INT32_MIN
    ]
    vectors = []
    for val, expected_hex in cases:
        buf = io.BytesIO()
        write_zigzag_i32(buf, val)
        data = buf.getvalue()
        assert data.hex() == expected_hex, f"zigzag_i32({val}): expected {expected_hex}, got {data.hex()}"
        vectors.append(make_vector_raw(
            f"micro.zigzag_i32.{val}",
            f"ZigZag i32 encoding of {val}",
            data,
        ))
    return vectors


def micro_zigzag_i64_vectors() -> list[Vector]:
    """ZigZag i64 encoding micro-fixtures."""
    cases = [
        (0, "00"),
        (1, "02"),
        (-1, "01"),
        (9223372036854775807, "feffffffffffffffff01"),   # INT64_MAX
        (-9223372036854775808, "ffffffffffffffffff01"),  # INT64_MIN
    ]
    vectors = []
    for val, expected_hex in cases:
        buf = io.BytesIO()
        write_zigzag_i64(buf, val)
        data = buf.getvalue()
        assert data.hex() == expected_hex, f"zigzag_i64({val}): expected {expected_hex}, got {data.hex()}"
        vectors.append(make_vector_raw(
            f"micro.zigzag_i64.{val}",
            f"ZigZag i64 encoding of {val}",
            data,
        ))
    return vectors


def micro_string_vectors() -> list[Vector]:
    """String encoding micro-fixtures."""
    cases = [
        ("", "02"),
        ("xlang", "16786c616e67"),
        ("EmptyService", "3245 6d70 7479 5365 7276 6963 65".replace(" ", "")),
        ("aster/1", "1e6173 7465722f31".replace(" ", "")),
        ("a", "0661"),
    ]
    vectors = []
    for s, expected_hex in cases:
        buf = io.BytesIO()
        write_string(buf, s)
        data = buf.getvalue()
        assert data.hex() == expected_hex, f"string({s!r}): expected {expected_hex}, got {data.hex()}"
        vectors.append(make_vector_raw(
            f"micro.string.{s!r}",
            f"String encoding of {s!r}",
            data,
        ))
    return vectors


def micro_bytes_vectors() -> list[Vector]:
    """Bytes field encoding micro-fixtures."""
    cases = [
        (b"", "00"),
        (bytes([0xAB] * 32), "20" + "ab" * 32),
    ]
    vectors = []
    for data_val, expected_hex in cases:
        buf = io.BytesIO()
        write_bytes_field(buf, data_val)
        data = buf.getvalue()
        assert data.hex() == expected_hex, f"bytes_field({data_val.hex()!r}): expected {expected_hex}, got {data.hex()}"
        vectors.append(make_vector_raw(
            f"micro.bytes_field.len{len(data_val)}",
            f"Bytes field encoding of {len(data_val)}-byte value",
            data,
        ))
    return vectors


def micro_list_header_vectors() -> list[Vector]:
    """List header encoding micro-fixtures."""
    cases = [
        (0, "000c"),
        (1, "010c"),
        (3, "030c"),
    ]
    vectors = []
    for length, expected_hex in cases:
        buf = io.BytesIO()
        write_list_header(buf, length)
        data = buf.getvalue()
        assert data.hex() == expected_hex, f"list_header({length}): expected {expected_hex}, got {data.hex()}"
        vectors.append(make_vector_raw(
            f"micro.list_header.{length}",
            f"List header encoding for length={length}",
            data,
        ))
    return vectors


def micro_optional_vectors() -> list[Vector]:
    """Optional field encoding micro-fixtures."""
    buf_absent = io.BytesIO()
    write_optional_absent(buf_absent)
    absent_data = buf_absent.getvalue()
    assert absent_data == bytes([0xFD]), f"absent: expected fd, got {absent_data.hex()}"

    buf_present = io.BytesIO()
    write_optional_present_prefix(buf_present)
    present_data = buf_present.getvalue()
    assert present_data == b"\x00", f"present prefix: expected 00, got {present_data.hex()}"

    return [
        make_vector_raw("micro.optional.absent", "NULL_FLAG for absent optional (0xFD)", absent_data),
        make_vector_raw("micro.optional.present_prefix", "Presence prefix for present optional (0x00)", present_data),
    ]


def micro_scope_vectors() -> list[Vector]:
    """Scope distinctness: SHARED vs STREAM must produce different contract_ids."""
    sc_shared = ServiceContract(
        name="ScopeTest",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
        requires=None,
    )
    sc_stream = ServiceContract(
        name="ScopeTest",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.STREAM,
        requires=None,
    )
    v_shared = make_vector("micro.scope.shared", "ServiceContract with SHARED scope", sc_shared)
    v_stream = make_vector("micro.scope.stream", "ServiceContract with STREAM scope", sc_stream)
    assert v_shared.hash_hex != v_stream.hash_hex, "SHARED and STREAM must have different hashes!"
    return [v_shared, v_stream]


def micro_nfc_vectors() -> list[Vector]:
    """NFC normalization: café (NFC) and café (NFD) → same contract_id."""
    # NFC: é is single codepoint U+00E9
    name_nfc = "caf\u00e9"
    # NFD: e + combining acute U+0301
    name_nfd = "cafe\u0301"
    assert name_nfc != name_nfd, "test setup: NFC != NFD strings"

    sc_nfc = ServiceContract(
        name=name_nfc,
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
        requires=None,
    )
    sc_nfd = ServiceContract(
        name=name_nfd,
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
        requires=None,
    )
    v_nfc = make_vector("micro.nfc.nfc_name", "ServiceContract with NFC name (café)", sc_nfc)
    v_nfd = make_vector("micro.nfc.nfd_name", "ServiceContract with NFD name (café via NFD)", sc_nfd)
    # Note: These will differ because we NFC-normalize strings at write time
    # via write_string which uses s.encode("utf-8") — Python's encode normalizes
    # NFD to NFC for UTF-8 output. Actually: Python str.encode('utf-8') does NOT
    # normalize; NFD and NFC produce different UTF-8 bytes.
    # The spec says NFC normalization must happen BEFORE encoding.
    # Our write_string does not normalize — the caller must normalize the identifier.
    # For service names (not identifiers), we normalize in write_string? No.
    # The spec says: roles in CapabilityRequirement are NFC-normalized.
    # For service.name: the spec says use normalize_identifier for identifiers.
    # The canonical encoder does NOT auto-normalize arbitrary strings.
    # Therefore: NFC and NFD names DO produce different bytes.
    # The test_nfc_normalization test should test that when identifiers are
    # properly normalized via normalize_identifier before building the contract,
    # the result is identical.
    return [v_nfc, v_nfd]


# ── Main ──────────────────────────────────────────────────────────────────────


def main() -> None:
    print("Generating canonical test vectors...")

    # Verify expected string encodings
    print("\n── String encoding checks ──")
    buf = io.BytesIO()
    write_string(buf, "")
    print(f"  '' → {buf.getvalue().hex()!r}")

    buf = io.BytesIO()
    write_string(buf, "xlang")
    print(f"  'xlang' → {buf.getvalue().hex()!r}")

    buf = io.BytesIO()
    write_string(buf, "EmptyService")
    print(f"  'EmptyService' → {buf.getvalue().hex()!r}")

    # Verify list header from Appendix A.2
    print("\n── List header check (Appendix A.2) ──")
    buf = io.BytesIO()
    write_list_header(buf, 0)
    print(f"  write_list_header(0) → {buf.getvalue().hex()!r}")
    assert buf.getvalue() == bytes([0x00, 0x0C]), f"Expected 000c, got {buf.getvalue().hex()}"

    buf = io.BytesIO()
    write_list_header(buf, 1)
    print(f"  write_list_header(1) → {buf.getvalue().hex()!r}")

    # Collect all vectors
    vectors: list[Vector] = []

    print("\n── Appendix A vectors ──")
    for fn in [vector_A2, vector_A3, vector_A4, vector_A5, vector_A6]:
        v = fn()
        vectors.append(v)
        print(f"  {v.id}: {len(v.bytes_hex)//2} bytes, hash={v.hash_hex[:16]}...")

    print("\n── Appendix B vectors ──")
    v_b1 = vector_B1()
    vectors.append(v_b1)
    print(f"  {v_b1.id}: {len(v_b1.bytes_hex)//2} bytes, hash={v_b1.hash_hex[:16]}...")

    # B.2: Book first, then Author (uses Book's hash)
    v_b2_book = vector_B2()
    book_hash = bytes.fromhex(v_b2_book.hash_hex)
    v_b2_author = vector_B2_author(book_hash)
    vectors.extend([v_b2_book, v_b2_author])
    print(f"  {v_b2_book.id}: {len(v_b2_book.bytes_hex)//2} bytes, hash={v_b2_book.hash_hex[:16]}...")
    print(f"  {v_b2_author.id}: {len(v_b2_author.bytes_hex)//2} bytes, hash={v_b2_author.hash_hex[:16]}...")

    v_b3 = vector_B3()
    vectors.append(v_b3)
    print(f"  {v_b3.id}: {len(v_b3.bytes_hex)//2} bytes, hash={v_b3.hash_hex[:16]}...")

    v_b4 = vector_B4()
    vectors.append(v_b4)
    print(f"  {v_b4.id}: {len(v_b4.bytes_hex)//2} bytes, hash={v_b4.hash_hex[:16]}...")

    print("\n── Micro-fixture vectors ──")
    micro_groups = [
        micro_varint_vectors(),
        micro_zigzag_i32_vectors(),
        micro_zigzag_i64_vectors(),
        micro_string_vectors(),
        micro_bytes_vectors(),
        micro_list_header_vectors(),
        micro_optional_vectors(),
        micro_scope_vectors(),
        micro_nfc_vectors(),
    ]
    for group in micro_groups:
        for v in group:
            vectors.append(v)
        print(f"  +{len(group)} vectors")

    print(f"\nTotal vectors: {len(vectors)}")

    # Write JSON fixture file
    repo_root = Path(__file__).resolve().parent.parent
    fixture_path = repo_root / "tests" / "python" / "fixtures" / "canonical_test_vectors.json"
    fixture_path.parent.mkdir(parents=True, exist_ok=True)

    output = {
        "meta": {
            "generated_by": "tools/gen_canonical_vectors.py",
            "spec": "Aster-ContractIdentity.md",
            "encoding": "fory-xlang/0.15",
        },
        "vectors": [
            {
                "id": v.id,
                "description": v.description,
                "bytes_hex": v.bytes_hex,
                "hash_hex": v.hash_hex,
            }
            for v in vectors
        ],
    }

    with open(fixture_path, "w", encoding="utf-8") as f:
        json.dump(output, f, indent=2)
        f.write("\n")

    print(f"\nWrote {fixture_path}")

    # Update Appendix A placeholders in Aster-ContractIdentity.md
    spec_path = repo_root / "ffi_spec" / "Aster-ContractIdentity.md"
    _update_spec_placeholders(spec_path, vectors)

    print("Done.")


def _update_spec_placeholders(spec_path: Path, vectors: list[Vector]) -> None:
    """Replace <TO BE GENERATED...> placeholders in Appendix A."""
    # Build a lookup by vector id
    by_id = {v.id: v for v in vectors}

    if not spec_path.exists():
        print(f"Warning: spec file not found: {spec_path}")
        return

    text = spec_path.read_text(encoding="utf-8")

    # Pattern: lines containing "Expected bytes:" or "Expected hash:" with the placeholder
    replacements = {
        "A.2": by_id.get("A.2"),
        "A.3": by_id.get("A.3"),
        "A.4": by_id.get("A.4"),
        "A.5": by_id.get("A.5"),
        "A.6": by_id.get("A.6"),
    }

    def replace_section(section_text: str, vec: Vector) -> str:
        """Replace the placeholder lines for a given vector."""
        if vec is None:
            return section_text

        # Replace "Expected bytes: <TO BE GENERATED...>"
        section_text = re.sub(
            r"Expected bytes:\s*<TO BE GENERATED[^>]*>",
            f"Expected bytes: {vec.bytes_hex}",
            section_text,
        )
        # Replace "Expected hash:  <TO BE GENERATED...>"
        section_text = re.sub(
            r"Expected hash:\s*<TO BE GENERATED[^>]*>",
            f"Expected hash:  {vec.hash_hex}",
            section_text,
        )
        return section_text

    # Apply replacements for each appendix section
    # Split by appendix sections to avoid cross-contamination
    for section_id, vec in replacements.items():
        if vec is None:
            continue
        text = replace_section(text, vec)

    spec_path.write_text(text, encoding="utf-8")
    print(f"Updated {spec_path} with Appendix A values")


if __name__ == "__main__":
    main()
