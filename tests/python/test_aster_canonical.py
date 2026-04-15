"""
tests/python/test_aster_canonical.py

Phase 13 conformance tests: canonical encoding golden vectors A.2--A.6.

Each vector is tested as a separate parametrized test case to allow pinpointing
failures.  Vectors are loaded from the committed JSON fixture file.

Spec reference: Aster-ContractIdentity.md §11; Aster-SPEC.md §13.2
"""

from __future__ import annotations

import json
from pathlib import Path

from aster._aster import blake3_hex
import pytest

from aster.contract.identity import (
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
    compute_contract_id,
)

# ── Fixture loading ────────────────────────────────────────────────────────────


_VECTORS_PATH = Path(__file__).parent / "fixtures" / "canonical_test_vectors.json"


def _load_vectors() -> dict[str, dict]:
    """Load the committed golden vectors as a dict keyed by vector id."""
    with open(_VECTORS_PATH, encoding="utf-8") as f:
        data = json.load(f)
    return {v["id"]: v for v in data["vectors"]}


# ── A.2--A.6 parametrized tests ─────────────────────────────────────────────────


@pytest.fixture(scope="module")
def vectors() -> dict[str, dict]:
    return _load_vectors()


def test_vector_A2_minimal_service_contract(vectors):
    """A.2: Minimal ServiceContract canonical bytes and hash."""
    sc = ServiceContract(
        name="EmptyService",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
        requires=None,
    )
    data = canonical_xlang_bytes(sc)
    assert data.hex() == vectors["A.2"]["bytes_hex"], "A.2 bytes mismatch"
    assert blake3_hex(data) == vectors["A.2"]["hash_hex"], "A.2 hash mismatch"


def test_vector_A3_minimal_typedef_enum(vectors):
    """A.3: Minimal TypeDef (enum) canonical bytes and hash."""
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
    data = canonical_xlang_bytes(td)
    assert data.hex() == vectors["A.3"]["bytes_hex"], "A.3 bytes mismatch"
    assert blake3_hex(data) == vectors["A.3"]["hash_hex"], "A.3 hash mismatch"


def test_vector_A4_typedef_with_type_reference(vectors):
    """A.4: TypeDef with type reference (REF field) canonical bytes and hash."""
    aa_hash = bytes([0xAA] * 32)
    td4 = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="test",
        name="Wrapper",
        fields=[
            FieldDef(
                id=1, name="inner", type_kind=TypeKind.REF,
                type_primitive="", type_ref=aa_hash, self_ref_name="",
                optional=False, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            )
        ],
        enum_values=[],
        union_variants=[],
    )
    data = canonical_xlang_bytes(td4)
    assert data.hex() == vectors["A.4"]["bytes_hex"], "A.4 bytes mismatch"
    assert blake3_hex(data) == vectors["A.4"]["hash_hex"], "A.4 hash mismatch"


def test_vector_A5_methoddef_with_requires(vectors):
    """A.5: MethodDef with requires present canonical bytes and hash."""
    req_hash = bytes([0x11] * 32)
    resp_hash = bytes([0x22] * 32)
    md5 = MethodDef(
        name="do_work",
        pattern=MethodPattern.UNARY,
        request_type=req_hash,
        response_type=resp_hash,
        idempotent=True,
        default_timeout=30.0,
        requires=CapabilityRequirement(kind=CapabilityKind.ANY_OF, roles=["Admin", "Operator"]),
    )
    data = canonical_xlang_bytes(md5)
    assert data.hex() == vectors["A.5"]["bytes_hex"], "A.5 bytes mismatch"
    assert blake3_hex(data) == vectors["A.5"]["hash_hex"], "A.5 hash mismatch"


def test_vector_A6_methoddef_without_requires(vectors):
    """A.6: MethodDef with requires absent canonical bytes and hash."""
    req_hash = bytes([0x11] * 32)
    resp_hash = bytes([0x22] * 32)
    md6 = MethodDef(
        name="do_work",
        pattern=MethodPattern.UNARY,
        request_type=req_hash,
        response_type=resp_hash,
        idempotent=False,
        default_timeout=0.0,
        requires=None,
    )
    data = canonical_xlang_bytes(md6)
    assert data.hex() == vectors["A.6"]["bytes_hex"], "A.6 bytes mismatch"
    assert blake3_hex(data) == vectors["A.6"]["hash_hex"], "A.6 hash mismatch"


# ── Scope distinctness ────────────────────────────────────────────────────────


def test_scope_distinctness_hashes_differ():
    """SHARED vs SESSION ServiceContracts must produce different contract_ids."""
    shared = ServiceContract(
        name="Svc",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    session = ServiceContract(
        name="Svc",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SESSION,
    )
    id_shared = compute_contract_id(canonical_xlang_bytes(shared))
    id_session = compute_contract_id(canonical_xlang_bytes(session))
    assert id_shared != id_session, "SHARED and SESSION contracts must have different contract_ids"
