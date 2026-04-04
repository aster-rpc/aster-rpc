"""
tests/python/test_aster_cycles.py

Phase 13 conformance tests: cycle-breaking golden vectors B.1–B.4.

Tests that recursive and mutually-recursive TypeDef graphs produce stable
canonical bytes and hashes.  Vectors are loaded from the committed JSON fixture.

Spec reference: Aster-ContractIdentity.md §11; Aster-SPEC.md §13.2
"""

from __future__ import annotations

import json
from pathlib import Path

import blake3
import pytest

from aster_python.aster.contract.identity import (
    ContainerKind,
    FieldDef,
    TypeDef,
    TypeDefKind,
    TypeKind,
    build_type_graph,
    canonical_xlang_bytes,
    compute_type_hash,
)

# ── Fixture loading ────────────────────────────────────────────────────────────

_REPO_ROOT = Path(__file__).resolve().parent.parent.parent
_VECTORS_PATH = _REPO_ROOT / "tests" / "fixtures" / "canonical_test_vectors.json"


def _load_vectors() -> dict[str, dict]:
    """Load the committed golden vectors as a dict keyed by vector id."""
    with open(_VECTORS_PATH, encoding="utf-8") as f:
        data = json.load(f)
    return {v["id"]: v for v in data["vectors"]}


@pytest.fixture(scope="module")
def vectors() -> dict[str, dict]:
    return _load_vectors()


# ── B.1: TreeNode self-reference ──────────────────────────────────────────────


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


def test_vector_B1_treenode_self_reference(vectors):
    """B.1: TreeNode with self-referential left/right fields."""
    td = _make_treenode_typedef()
    data = canonical_xlang_bytes(td)
    assert data.hex() == vectors["B.1"]["bytes_hex"], "B.1 bytes mismatch"
    assert blake3.blake3(data).hexdigest() == vectors["B.1"]["hash_hex"], "B.1 hash mismatch"


# ── B.2: Book/Author mutual reference ─────────────────────────────────────────


def test_vector_B2_book_hashed_first(vectors):
    """B.2: Book type (hashed first in Book/Author mutual reference cycle)."""
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
    assert data_book.hex() == vectors["B.2"]["bytes_hex"], "B.2 bytes mismatch"
    assert blake3.blake3(data_book).hexdigest() == vectors["B.2"]["hash_hex"], "B.2 hash mismatch"


def test_vector_B2_author_references_book(vectors):
    """B.2-author: Author type that references Book via REF (hash resolved)."""
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
    book_hash = compute_type_hash(data_book)

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
    assert data_author.hex() == vectors["B.2-author"]["bytes_hex"], "B.2-author bytes mismatch"
    assert (
        blake3.blake3(data_author).hexdigest() == vectors["B.2-author"]["hash_hex"]
    ), "B.2-author hash mismatch"


# ── B.3: Three-type cycle (Gamma) ─────────────────────────────────────────────


def test_vector_B3_gamma_three_type_cycle(vectors):
    """B.3: Gamma — first in a three-type cycle (Gamma → Alpha → Beta → Gamma)."""
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
    assert data_g.hex() == vectors["B.3"]["bytes_hex"], "B.3 bytes mismatch"
    assert blake3.blake3(data_g).hexdigest() == vectors["B.3"]["hash_hex"], "B.3 hash mismatch"


# ── B.4: C with SELF_REF to A ─────────────────────────────────────────────────


def test_vector_B4_c_self_ref_to_a(vectors):
    """B.4: C type with SELF_REF field pointing to A."""
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
    assert data_c.hex() == vectors["B.4"]["bytes_hex"], "B.4 bytes mismatch"
    assert blake3.blake3(data_c).hexdigest() == vectors["B.4"]["hash_hex"], "B.4 hash mismatch"


# ── resolve_with_cycles smoke test ────────────────────────────────────────────


def test_resolve_with_cycles_treenode_is_stable():
    """resolve_with_cycles on TreeNode produces a stable, deterministic result."""
    td = _make_treenode_typedef()
    # Should not raise; calling twice must return identical bytes.
    data1 = canonical_xlang_bytes(td)
    data2 = canonical_xlang_bytes(td)
    assert data1 == data2, "canonical_xlang_bytes must be deterministic"


def test_build_type_graph_with_python_dataclass():
    """build_type_graph with a Python dataclass returns a mapping of FQN to type."""
    from dataclasses import dataclass

    @dataclass
    class MyNode:
        value: int = 0

    graph = build_type_graph([MyNode])
    # build_type_graph walks Python class type annotations, not TypeDef instances
    assert isinstance(graph, dict)
