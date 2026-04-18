"""
tests/python/test_aster_contract_identity.py

Phase 9 conformance tests for Aster contract identity.
Tests canonical encoding, hashing, type graph resolution, manifest
verification, and golden vector equality.

Spec reference: Aster-ContractIdentity.md §11
"""

from __future__ import annotations

import io
import json
from dataclasses import dataclass
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
    build_type_graph,
    canonical_xlang_bytes,
    compute_contract_id,
    compute_type_hash,
    normalize_identifier,
    resolve_with_cycles,
)
from aster.contract.manifest import (
    ContractManifest,
    FatalContractMismatch,
    verify_manifest_or_fatal,
)


# ── Fixtures path ──────────────────────────────────────────────────────────────


_VECTORS_PATH = Path(__file__).parent / "fixtures" / "canonical_test_vectors.json"


def _load_vectors() -> dict[str, dict]:
    """Load the committed golden vectors as a dict keyed by vector id."""
    with open(_VECTORS_PATH, encoding="utf-8") as f:
        data = json.load(f)
    return {v["id"]: v for v in data["vectors"]}



# NOTE: Canonical encoding tests (varint, zigzag, string, bytes, list header,
# optional) were removed -- that code now lives only in Rust core
# (core/src/canonical.rs) and is tested there.


# ── 8. Hash stability ─────────────────────────────────────────────────────────


def test_hash_stability():
    """Same input bytes → same hash across multiple calls."""
    sc = ServiceContract(
        name="StableService",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    data = canonical_xlang_bytes(sc)
    h1 = compute_contract_id(data)
    h2 = compute_contract_id(data)
    h3 = compute_contract_id(canonical_xlang_bytes(sc))
    assert h1 == h2 == h3


# ── 9. Scope distinctness ─────────────────────────────────────────────────────


def test_scope_distinctness():
    """SHARED and SESSION scoped contracts have different contract_ids."""
    base = dict(
        name="ScopeTest",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
    )
    sc_shared = ServiceContract(**base, scoped=ScopeKind.SHARED)
    sc_session = ServiceContract(**base, scoped=ScopeKind.SESSION)

    id_shared = compute_contract_id(canonical_xlang_bytes(sc_shared))
    id_session = compute_contract_id(canonical_xlang_bytes(sc_session))

    assert id_shared != id_session, "SHARED and SESSION must produce different contract_ids"


# ── 10. NFC normalization ─────────────────────────────────────────────────────


def test_nfc_normalization():
    """NFC-normalized identifiers via normalize_identifier → stable identity."""
    # "café" -- both NFC and NFD forms
    nfc_name = "caf\u00e9"         # precomposed é (U+00E9)
    nfd_name = "cafe\u0301"        # decomposed e + combining acute

    # After normalize_identifier, both should give the NFC form
    norm_nfc = normalize_identifier(nfc_name)
    norm_nfd = normalize_identifier(nfd_name)

    assert norm_nfc == norm_nfd, "NFC and NFD identifiers should normalize to same string"
    assert norm_nfc == nfc_name   # NFC form is canonical

    # Build contracts using the normalized names -- they should be identical
    sc_nfc = ServiceContract(
        name=norm_nfc, version=1, methods=[], serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    sc_nfd = ServiceContract(
        name=norm_nfd, version=1, methods=[], serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    # Both normalized to the same name, so same bytes and same contract_id
    assert canonical_xlang_bytes(sc_nfc) == canonical_xlang_bytes(sc_nfd)
    assert compute_contract_id(canonical_xlang_bytes(sc_nfc)) == \
           compute_contract_id(canonical_xlang_bytes(sc_nfd))


# ── 11. Golden vectors A.2--A.6 ───────────────────────────────────────────────


def test_vectors_A2_to_A6():
    """Byte and hash equality against committed golden vectors."""
    vecs = _load_vectors()

    # A.2: Minimal ServiceContract
    sc = ServiceContract(
        name="EmptyService",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
        requires=None,
    )
    data = canonical_xlang_bytes(sc)
    assert data.hex() == vecs["A.2"]["bytes_hex"], "A.2 bytes mismatch"
    assert blake3_hex(data) == vecs["A.2"]["hash_hex"], "A.2 hash mismatch"

    # A.3: Minimal TypeDef (enum)
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
    assert data.hex() == vecs["A.3"]["bytes_hex"], "A.3 bytes mismatch"
    assert blake3_hex(data) == vecs["A.3"]["hash_hex"], "A.3 hash mismatch"

    # A.4: TypeDef with type reference
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
    assert data.hex() == vecs["A.4"]["bytes_hex"], "A.4 bytes mismatch"
    assert blake3_hex(data) == vecs["A.4"]["hash_hex"], "A.4 hash mismatch"

    # A.5: MethodDef with requires present
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
    assert data.hex() == vecs["A.5"]["bytes_hex"], "A.5 bytes mismatch"
    assert blake3_hex(data) == vecs["A.5"]["hash_hex"], "A.5 hash mismatch"

    # A.6: MethodDef with requires absent
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
    assert data.hex() == vecs["A.6"]["bytes_hex"], "A.6 bytes mismatch"
    assert blake3_hex(data) == vecs["A.6"]["hash_hex"], "A.6 hash mismatch"


# ── 12. Golden vectors B.1--B.4 ───────────────────────────────────────────────


def _make_treenode_typedef() -> TypeDef:
    return TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="TreeNode",
        fields=[
            FieldDef(
                id=1, name="value", type_kind=TypeKind.PRIMITIVE,
                type_primitive="string", type_ref=b"", self_ref_name="",
                optional=False, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
            FieldDef(
                id=2, name="left", type_kind=TypeKind.SELF_REF,
                type_primitive="", type_ref=b"", self_ref_name="example.TreeNode",
                optional=True, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
            FieldDef(
                id=3, name="right", type_kind=TypeKind.SELF_REF,
                type_primitive="", type_ref=b"", self_ref_name="example.TreeNode",
                optional=True, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )


def test_vectors_B1_to_B4():
    """Cycle-breaking golden vector equality."""
    vecs = _load_vectors()

    # B.1: TreeNode self-reference
    td = _make_treenode_typedef()
    data = canonical_xlang_bytes(td)
    assert data.hex() == vecs["B.1"]["bytes_hex"], "B.1 bytes mismatch"
    assert blake3_hex(data) == vecs["B.1"]["hash_hex"], "B.1 hash mismatch"

    # B.2: Book (hashed first)
    td_book = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="Book",
        fields=[
            FieldDef(
                id=1, name="title", type_kind=TypeKind.PRIMITIVE,
                type_primitive="string", type_ref=b"", self_ref_name="",
                optional=False, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
            FieldDef(
                id=2, name="written_by", type_kind=TypeKind.SELF_REF,
                type_primitive="", type_ref=b"", self_ref_name="example.Author",
                optional=False, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    data_book = canonical_xlang_bytes(td_book)
    assert data_book.hex() == vecs["B.2"]["bytes_hex"], "B.2 bytes mismatch"
    assert blake3_hex(data_book) == vecs["B.2"]["hash_hex"], "B.2 hash mismatch"

    book_hash = compute_type_hash(data_book)

    # B.2-author: Author references Book via REF
    td_author = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="Author",
        fields=[
            FieldDef(
                id=1, name="name", type_kind=TypeKind.PRIMITIVE,
                type_primitive="string", type_ref=b"", self_ref_name="",
                optional=False, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
            FieldDef(
                id=2, name="books", type_kind=TypeKind.REF,
                type_primitive="", type_ref=book_hash, self_ref_name="",
                optional=False, ref_tracked=False,
                container=ContainerKind.LIST,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    data_author = canonical_xlang_bytes(td_author)
    assert data_author.hex() == vecs["B.2-author"]["bytes_hex"], "B.2-author bytes mismatch"
    assert blake3_hex(data_author) == vecs["B.2-author"]["hash_hex"], "B.2-author hash mismatch"

    # B.3: Gamma (first in three-type cycle)
    td_gamma = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="Gamma",
        fields=[
            FieldDef(
                id=1, name="next", type_kind=TypeKind.SELF_REF,
                type_primitive="", type_ref=b"", self_ref_name="example.Alpha",
                optional=False, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    data_g = canonical_xlang_bytes(td_gamma)
    assert data_g.hex() == vecs["B.3"]["bytes_hex"], "B.3 bytes mismatch"
    assert blake3_hex(data_g) == vecs["B.3"]["hash_hex"], "B.3 hash mismatch"

    # B.4: C (a_field is SELF_REF to A)
    td_c = TypeDef(
        kind=TypeDefKind.MESSAGE,
        package="example",
        name="C",
        fields=[
            FieldDef(
                id=1, name="a_field", type_kind=TypeKind.SELF_REF,
                type_primitive="", type_ref=b"", self_ref_name="example.A",
                optional=False, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="", container_key_ref=b"",
            ),
        ],
        enum_values=[],
        union_variants=[],
    )
    data_c = canonical_xlang_bytes(td_c)
    assert data_c.hex() == vecs["B.4"]["bytes_hex"], "B.4 bytes mismatch"
    assert blake3_hex(data_c) == vecs["B.4"]["hash_hex"], "B.4 hash mismatch"


# ── 13. All micro-fixtures ────────────────────────────────────────────────────


def test_all_micro_fixtures():
    """Micro-fixture golden vectors exist and are loadable.

    NOTE: The canonical encoding tests (varint, zigzag, string, etc.) that
    previously verified these vectors were removed -- that code now lives in
    Rust core (core/src/canonical.rs) and is tested there.
    """
    vecs = _load_vectors()
    micro_ids = [vid for vid in vecs if vid.startswith("micro.")]
    assert len(micro_ids) > 0, "Expected micro-fixture vectors in JSON"


# ── 14. Changing type changes contract_id ─────────────────────────────────────


def test_changing_type_changes_contract_id():
    """Adding a field to a service changes the contract_id."""
    base = ServiceContract(
        name="ChangeTest",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    # Add a method
    modified = ServiceContract(
        name="ChangeTest",
        version=1,
        methods=[
            MethodDef(
                name="ping",
                pattern=MethodPattern.UNARY,
                request_type=bytes([0x01] * 32),
                response_type=bytes([0x02] * 32),
                idempotent=True,
                default_timeout=5.0,
            )
        ],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    id_base = compute_contract_id(canonical_xlang_bytes(base))
    id_modified = compute_contract_id(canonical_xlang_bytes(modified))
    assert id_base != id_modified


# ── 14b. Version changes produce different contract IDs ──────────────────────


def test_version_change_produces_different_contract_id():
    """Same service name with different version numbers produce different contract IDs."""
    v1 = ServiceContract(
        name="UserService",
        version=1,
        methods=[
            MethodDef(name="get_user", pattern=MethodPattern.UNARY,
                      request_type=bytes(32), response_type=bytes(32),
                      idempotent=True, default_timeout=0.0),
        ],
    )
    v2 = ServiceContract(
        name="UserService",
        version=2,
        methods=[
            MethodDef(name="get_user", pattern=MethodPattern.UNARY,
                      request_type=bytes(32), response_type=bytes(32),
                      idempotent=True, default_timeout=0.0),
        ],
    )
    id_v1 = compute_contract_id(canonical_xlang_bytes(v1))
    id_v2 = compute_contract_id(canonical_xlang_bytes(v2))
    assert id_v1 != id_v2, "Different versions must produce different contract IDs"


def test_extra_method_produces_different_contract_id():
    """Same service name+version but with an extra method produces different contract IDs."""
    base = ServiceContract(
        name="UserService",
        version=2,
        methods=[
            MethodDef(name="get_user", pattern=MethodPattern.UNARY,
                      request_type=bytes(32), response_type=bytes(32),
                      idempotent=True, default_timeout=0.0),
        ],
    )
    extended = ServiceContract(
        name="UserService",
        version=2,
        methods=[
            MethodDef(name="get_user", pattern=MethodPattern.UNARY,
                      request_type=bytes(32), response_type=bytes(32),
                      idempotent=True, default_timeout=0.0),
            MethodDef(name="delete_user", pattern=MethodPattern.UNARY,
                      request_type=bytes(32), response_type=bytes(32),
                      idempotent=False, default_timeout=30000.0),
        ],
    )
    id_base = compute_contract_id(canonical_xlang_bytes(base))
    id_extended = compute_contract_id(canonical_xlang_bytes(extended))
    assert id_base != id_extended, "Adding a method must change the contract ID"


def test_method_signature_change_produces_different_contract_id():
    """Changing a method's signature (e.g. idempotent flag) produces different contract IDs."""
    original = ServiceContract(
        name="UserService",
        version=1,
        methods=[
            MethodDef(name="get_user", pattern=MethodPattern.UNARY,
                      request_type=bytes(32), response_type=bytes(32),
                      idempotent=True, default_timeout=0.0),
        ],
    )
    modified = ServiceContract(
        name="UserService",
        version=1,
        methods=[
            MethodDef(name="get_user", pattern=MethodPattern.UNARY,
                      request_type=bytes(32), response_type=bytes(32),
                      idempotent=False, default_timeout=0.0),  # changed
        ],
    )
    id_original = compute_contract_id(canonical_xlang_bytes(original))
    id_modified = compute_contract_id(canonical_xlang_bytes(modified))
    assert id_original != id_modified, "Changing method signature must change the contract ID"


# ── 15. Manifest mismatch raises FatalContractMismatch ───────────────────────


def test_manifest_mismatch_fatal(tmp_path):
    """Wrong hash in manifest raises FatalContractMismatch."""
    sc = ServiceContract(
        name="TestService",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    live_bytes = canonical_xlang_bytes(sc)
    real_id = compute_contract_id(live_bytes)

    # Write a manifest with a wrong contract_id
    wrong_manifest = ContractManifest(
        service="TestService",
        version=1,
        contract_id="a" * 64,  # wrong hash
    )
    manifest_path = str(tmp_path / "manifest.json")
    wrong_manifest.save(manifest_path)

    with pytest.raises(FatalContractMismatch) as exc_info:
        verify_manifest_or_fatal(live_bytes, manifest_path)

    err = exc_info.value
    assert err.expected_id == "a" * 64
    assert err.actual_id == real_id
    assert "rerun" in str(err).lower() or "aster contract gen" in str(err)


def test_manifest_roundtrip(tmp_path):
    """Correct manifest passes verification."""
    sc = ServiceContract(
        name="RoundtripService",
        version=2,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    live_bytes = canonical_xlang_bytes(sc)
    real_id = compute_contract_id(live_bytes)

    manifest = ContractManifest(
        service="RoundtripService",
        version=2,
        contract_id=real_id,
    )
    manifest_path = str(tmp_path / "manifest.json")
    manifest.save(manifest_path)

    loaded = verify_manifest_or_fatal(live_bytes, manifest_path)
    assert loaded.contract_id == real_id
    assert loaded.service == "RoundtripService"


# ── 16. ServiceInfo → ServiceContract ─────────────────────────────────────────


def test_service_to_contract():
    """ServiceInfo → ServiceContract (from @service decorated class)."""
    from typing import get_type_hints

    from aster.codec import wire_type
    from aster.decorators import rpc, service

    @wire_type("test.contract/PingRequest")
    @dataclass
    class PingRequest:
        message: str = ""

    @wire_type("test.contract/PingResponse")
    @dataclass
    class PingResponse:
        reply: str = ""

    @service(name="PingService2", version=1)
    class PingServiceClass:
        @rpc
        async def ping(self, request: PingRequest) -> PingResponse:
            ...

    service_info = PingServiceClass.__aster_service_info__

    # With `from __future__ import annotations`, service_info.methods[*].request_type is the
    # string "PingRequest"; resolve it to the real class so the type-graph walker sees a type,
    # not a forward reference. This mirrors what runtime dispatch does at call time.
    local_ns = {"PingRequest": PingRequest, "PingResponse": PingResponse}
    for mi in service_info.methods.values():
        if isinstance(mi.request_type, str):
            mi.request_type = local_ns[mi.request_type]
        if isinstance(mi.response_type, str):
            mi.response_type = local_ns[mi.response_type]

    root_types = []
    for mi in service_info.methods.values():
        if mi.request_type:
            root_types.append(mi.request_type)
        if mi.response_type:
            root_types.append(mi.response_type)

    type_graph = build_type_graph(root_types)
    type_defs = resolve_with_cycles(type_graph)

    type_hashes: dict[str, bytes] = {}
    for fqn, td in type_defs.items():
        type_hashes[fqn] = compute_type_hash(canonical_xlang_bytes(td))

    contract = ServiceContract.from_service_info(service_info, type_hashes)

    assert contract.name == "PingService2"
    assert contract.version == 1
    assert len(contract.methods) == 1
    assert contract.methods[0].name == "ping"
    assert contract.scoped == ScopeKind.SHARED

    # Request and response hashes now flow through (previously this silently returned zeros
    # when @wire_type was applied because _resolve_type_hash used module.__qualname__ instead
    # of the wire-tag FQN).
    assert contract.methods[0].request_type != b"\x00" * 32
    assert contract.methods[0].response_type != b"\x00" * 32

    data = canonical_xlang_bytes(contract)
    assert len(data) > 0

    contract_id = compute_contract_id(data)
    assert len(contract_id) == 64


# ── 17. normalize_identifier -- valid ─────────────────────────────────────────


def test_normalize_identifier_valid():
    """Valid identifiers normalize without error."""
    assert normalize_identifier("hello") == "hello"
    assert normalize_identifier("CamelCase") == "CamelCase"
    assert normalize_identifier("_under_score") == "_under_score"
    assert normalize_identifier("a123") == "a123"

    # NFC normalization
    nfd = "cafe\u0301"   # NFD form of "café"
    nfc = "caf\u00e9"    # NFC form
    assert normalize_identifier(nfd) == nfc


# ── 18. normalize_identifier -- invalid ───────────────────────────────────────


def test_normalize_identifier_invalid():
    """Non-identifier strings raise ValueError."""
    with pytest.raises(ValueError, match="Not a valid identifier"):
        normalize_identifier("hello world")

    with pytest.raises(ValueError, match="Not a valid identifier"):
        normalize_identifier("123abc")

    with pytest.raises(ValueError, match="Not a valid identifier"):
        normalize_identifier("has-hyphen")

    with pytest.raises(ValueError, match="Not a valid identifier"):
        normalize_identifier("")
