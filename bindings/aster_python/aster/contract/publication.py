"""
aster.contract.publication — Contract publication and fetch stubs.

Spec reference: Aster-ContractIdentity.md §11.5

The collection layout logic (building the list of (name, bytes) pairs) is
implemented as pure Python. The Iroh-dependent upload and fetch operations
raise NotImplementedError until the transport layer is wired up.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from aster_python.aster.contract.identity import (
    ServiceContract,
    TypeDef,
    canonical_xlang_bytes,
    compute_contract_id,
    compute_type_hash,
)
from aster_python.aster.contract.manifest import ContractManifest

if TYPE_CHECKING:
    pass  # iroh types imported lazily to avoid hard dependency


# ── Collection layout ─────────────────────────────────────────────────────────


def build_collection(
    contract: ServiceContract,
    type_defs: dict[str, TypeDef],
) -> list[tuple[str, bytes]]:
    """Build the list of (name, bytes) pairs for the contract collection.

    The collection contains:
    - ``contract.bin``: canonical bytes of the ServiceContract
    - ``manifest.json``: JSON-serialized ContractManifest
    - For each TypeDef: ``types/<hex_hash>.bin``

    This is pure Python — no Iroh dependencies.

    Args:
        contract: The resolved ServiceContract.
        type_defs: Dict mapping FQN to TypeDef (from resolve_with_cycles).

    Returns:
        List of (entry_name, raw_bytes) pairs.
    """
    entries: list[tuple[str, bytes]] = []

    # Canonical contract bytes
    contract_bytes = canonical_xlang_bytes(contract)
    contract_id = compute_contract_id(contract_bytes)
    entries.append(("contract.bin", contract_bytes))

    # Type blobs
    type_hashes_hex: list[str] = []
    for _fqn, td in type_defs.items():
        td_bytes = canonical_xlang_bytes(td)
        h = compute_type_hash(td_bytes)
        h_hex = h.hex()
        type_hashes_hex.append(h_hex)
        entries.append((f"types/{h_hex}.bin", td_bytes))

    type_hashes_hex_sorted = sorted(type_hashes_hex)

    # Serialization modes from contract
    ser_modes = list(contract.serialization_modes)
    scoped_str = contract.scoped.name.lower()

    # Manifest JSON
    manifest = ContractManifest(
        service=contract.name,
        version=contract.version,
        contract_id=contract_id,
        canonical_encoding="fory-xlang/0.15",
        type_count=len(type_defs),
        type_hashes=type_hashes_hex_sorted,
        method_count=len(contract.methods),
        serialization_modes=ser_modes,
        alpn=contract.alpn,
        scoped=scoped_str,
    )
    manifest_bytes = manifest.to_json().encode("utf-8")
    entries.append(("manifest.json", manifest_bytes))

    return entries


# ── Publication stub ──────────────────────────────────────────────────────────


async def publish_contract(
    contract: ServiceContract,
    type_defs: dict[str, TypeDef],
    blobs_client: object | None = None,
    docs_client: object | None = None,
) -> str:
    """Publish a contract to the Iroh blob/doc store.

    The collection layout is built from ``build_collection()``. The actual
    upload to Iroh blobs/docs requires a live connection and is stubbed out.

    Args:
        contract: The resolved ServiceContract.
        type_defs: Dict mapping FQN to TypeDef.
        blobs_client: An Iroh BlobsClient (or None for dry-run).
        docs_client: An Iroh DocsClient (or None for dry-run).

    Returns:
        The contract_id hex string.

    Raises:
        NotImplementedError: Always — Iroh upload not yet implemented.
    """
    # Compute the collection entries (pure Python, no iroh dependency)
    entries = build_collection(contract, type_defs)

    contract_bytes = canonical_xlang_bytes(contract)
    contract_id = compute_contract_id(contract_bytes)

    if blobs_client is None and docs_client is None:
        # Dry-run mode: just return the contract_id
        return contract_id

    raise NotImplementedError(
        "publish_contract: Iroh blob upload is not yet implemented. "
        "The collection layout has been computed — connect an Iroh BlobsClient "
        "to upload the following entries:\n"
        + "\n".join(f"  {name}: {len(data)} bytes" for name, data in entries)
    )


async def fetch_contract(
    contract_id: str,
    blobs_client: object | None = None,
) -> ContractManifest:
    """Fetch a contract manifest from the Iroh blob store by contract_id.

    Args:
        contract_id: 64-char hex string identifying the contract.
        blobs_client: An Iroh BlobsClient.

    Returns:
        The fetched ContractManifest.

    Raises:
        NotImplementedError: Always — Iroh fetch not yet implemented.
    """
    raise NotImplementedError(
        f"fetch_contract({contract_id!r}): Iroh blob fetch is not yet implemented. "
        "Connect an Iroh BlobsClient to retrieve the contract manifest."
    )
