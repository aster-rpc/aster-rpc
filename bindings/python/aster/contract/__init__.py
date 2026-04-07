"""
aster.contract — Contract identity and publication for Aster RPC.

Spec reference: Aster-ContractIdentity.md §11

Canonical encoding and BLAKE3 hashing are delegated to the Rust core
via _aster.contract. The Python-side canonical.py is retained only as
a reference for the encoding format — it is not used at runtime.
"""

from aster.contract.identity import (
    TypeKind,
    ContainerKind,
    TypeDefKind,
    MethodPattern,
    CapabilityKind,
    ScopeKind,
    FieldDef,
    EnumValueDef,
    UnionVariantDef,
    TypeDef,
    CapabilityRequirement,
    MethodDef,
    ServiceContract,
    normalize_identifier,
    canonical_xlang_bytes,
    compute_type_hash,
    compute_contract_id,
    contract_id_from_service,
    build_type_graph,
    resolve_with_cycles,
)
from aster.contract.manifest import (
    ContractManifest,
    FatalContractMismatch,
    verify_manifest_or_fatal,
)

__all__ = [
    # identity
    "TypeKind",
    "ContainerKind",
    "TypeDefKind",
    "MethodPattern",
    "CapabilityKind",
    "ScopeKind",
    "FieldDef",
    "EnumValueDef",
    "UnionVariantDef",
    "TypeDef",
    "CapabilityRequirement",
    "MethodDef",
    "ServiceContract",
    "normalize_identifier",
    "canonical_xlang_bytes",
    "compute_type_hash",
    "compute_contract_id",
    "contract_id_from_service",
    "build_type_graph",
    "resolve_with_cycles",
    # manifest
    "ContractManifest",
    "FatalContractMismatch",
    "verify_manifest_or_fatal",
]
