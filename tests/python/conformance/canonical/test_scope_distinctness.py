"""
tests/conformance/canonical/test_scope_distinctness.py

Phase 13 conformance test: scope distinctness.

Two ServiceContracts identical in all fields except `scoped` must produce
different contract_ids.  This ensures that SHARED and SESSION contracts cannot
be confused at the identity layer.

Spec reference: Aster-ContractIdentity.md §11; Aster-SPEC.md §13.2
"""

from __future__ import annotations


from aster.contract.identity import (
    ServiceContract,
    ScopeKind,
    canonical_xlang_bytes,
    compute_contract_id,
)


def test_scope_distinctness():
    """SHARED and SESSION ServiceContracts must have different contract_ids."""
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
    assert id_shared != id_session, (
        "SHARED and SESSION contracts must have different contract_ids"
    )


def test_scope_distinctness_shared_vs_shared_is_equal():
    """Two identical SHARED contracts produce the same contract_id."""
    a = ServiceContract(
        name="Svc",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    b = ServiceContract(
        name="Svc",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    assert compute_contract_id(canonical_xlang_bytes(a)) == compute_contract_id(canonical_xlang_bytes(b))


def test_scope_distinctness_session_vs_session_is_equal():
    """Two identical SESSION contracts produce the same contract_id."""
    a = ServiceContract(
        name="Svc",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SESSION,
    )
    b = ServiceContract(
        name="Svc",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SESSION,
    )
    assert compute_contract_id(canonical_xlang_bytes(a)) == compute_contract_id(canonical_xlang_bytes(b))


def test_name_distinctness():
    """Contracts with different names must have different contract_ids."""
    a = ServiceContract(
        name="ServiceA",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    b = ServiceContract(
        name="ServiceB",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    assert compute_contract_id(canonical_xlang_bytes(a)) != compute_contract_id(canonical_xlang_bytes(b))


def test_version_distinctness():
    """Contracts with different versions must have different contract_ids."""
    a = ServiceContract(
        name="Svc",
        version=1,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    b = ServiceContract(
        name="Svc",
        version=2,
        methods=[],
        serialization_modes=["xlang"],
        scoped=ScopeKind.SHARED,
    )
    assert compute_contract_id(canonical_xlang_bytes(a)) != compute_contract_id(canonical_xlang_bytes(b))
