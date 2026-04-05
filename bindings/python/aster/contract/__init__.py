"""
aster.contract — Contract identity and publication for Aster RPC.

Spec reference: Aster-ContractIdentity.md §11

Provides content-addressed contract identity: Python types → canonical XLANG bytes
→ BLAKE3 hash → Merkle DAG.
"""

from aster.contract.canonical import (
    CanonicalWriter,
    write_varint,
    write_zigzag_i32,
    write_zigzag_i64,
    write_string,
    write_bytes_field,
    write_bool,
    write_float64,
    write_list_header,
    write_optional_absent,
    write_optional_present_prefix,
)
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
    build_type_graph,
    resolve_with_cycles,
)
from aster.contract.manifest import (
    ContractManifest,
    FatalContractMismatch,
    verify_manifest_or_fatal,
)

__all__ = [
    # canonical
    "CanonicalWriter",
    "write_varint",
    "write_zigzag_i32",
    "write_zigzag_i64",
    "write_string",
    "write_bytes_field",
    "write_bool",
    "write_float64",
    "write_list_header",
    "write_optional_absent",
    "write_optional_present_prefix",
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
    "build_type_graph",
    "resolve_with_cycles",
    # manifest
    "ContractManifest",
    "FatalContractMismatch",
    "verify_manifest_or_fatal",
]
