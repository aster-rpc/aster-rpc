"""
aster.contract.publication -- Contract publication and fetch via Iroh blob store.

Spec reference: Aster-ContractIdentity.md §11.5

Two collection formats are supported:

``"raw"``      -- single-blob: the contract canonical bytes are stored as one blob;
                 ``collection_hash == contract_id``. Used by RegistryPublisher when
                 no multi-file layout is needed.

``"hashseq"``  -- multi-file: all collection entries (contract.bin, manifest.json,
                 types/*.bin) are stored as individual blobs and wrapped in a native
                 iroh ``Collection`` (HashSeq format).  The HashSeq blob is
                 auto-tagged for GC protection.  ``collection_hash`` is the BLAKE3
                 hash of the HashSeq blob.
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING

from aster.contract.identity import (
    ServiceContract,
    TypeDef,
    canonical_xlang_bytes,
    compute_contract_id,
    compute_type_hash,
)
from aster.contract.manifest import ContractManifest

if TYPE_CHECKING:
    pass  # iroh types imported lazily

logger = logging.getLogger(__name__)


# ── Collection layout ─────────────────────────────────────────────────────────


def build_collection(
    contract: ServiceContract,
    type_defs: dict[str, TypeDef],
    service_info: object | None = None,
) -> list[tuple[str, bytes]]:
    """Build the list of (name, bytes) pairs for the contract collection.

    The collection contains:
    - ``contract.bin``: canonical bytes of the ServiceContract
    - ``manifest.json``: JSON-serialized ContractManifest (includes method schemas)
    - For each TypeDef: ``types/<hex_hash>.bin``

    This is pure Python -- no Iroh dependencies.

    Args:
        contract: The resolved ServiceContract.
        type_defs: Dict mapping FQN to TypeDef (from resolve_with_cycles).
        service_info: Optional ServiceInfo for extracting method field details.

    Returns:
        List of (entry_name, raw_bytes) pairs in deterministic order.
    """
    from aster.contract.manifest import extract_method_descriptors

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

    # Extract method descriptors with field definitions
    methods: list[dict] = []
    if service_info is not None:
        methods = extract_method_descriptors(service_info)
    else:
        # Fallback: basic method info from the ServiceContract (no field details)
        for md in contract.methods:
            methods.append({
                "name": md.name,
                "pattern": md.pattern.name.lower() if hasattr(md.pattern, "name") else str(md.pattern),
                "request_type": md.request_type.hex() if isinstance(md.request_type, bytes) else str(md.request_type),
                "response_type": md.response_type.hex() if isinstance(md.response_type, bytes) else str(md.response_type),
                "timeout": md.default_timeout if md.default_timeout else None,
                "idempotent": md.idempotent,
                "fields": [],
            })

    # Manifest JSON
    manifest = ContractManifest(
        service=contract.name,
        version=contract.version,
        contract_id=contract_id,
        canonical_encoding="fory-xlang/0.15",
        type_count=len(type_defs),
        type_hashes=type_hashes_hex_sorted,
        method_count=len(contract.methods),
        methods=methods,
        serialization_modes=ser_modes,
        scoped=scoped_str,
    )
    manifest_bytes = manifest.to_json().encode("utf-8")
    entries.append(("manifest.json", manifest_bytes))

    return entries


# ── Multi-file collection upload ──────────────────────────────────────────────


async def upload_collection(
    blobs: object,
    entries: list[tuple[str, bytes]],
    tag_prefix: str | None = None,
) -> str:
    """Upload all collection entries as a native iroh HashSeq collection.

    Uses ``blobs.add_collection(entries)`` which stores each entry as a
    raw blob, builds a native iroh ``Collection`` (HashSeq), and sets a
    persistent tag for GC protection -- all in a single Rust call.

    The ``tag_prefix`` parameter is accepted for backwards compatibility
    but is ignored; GC protection is handled natively by HashSeq tagging.

    Args:
        blobs:      A live ``BlobsClient``.
        entries:    ``[(name, bytes)]`` list from ``build_collection()``.
        tag_prefix: Deprecated -- ignored. HashSeq handles GC natively.

    Returns:
        ``collection_hash`` -- hex hash of the HashSeq collection blob.
    """
    collection_hash = await blobs.add_collection(entries)
    logger.debug(
        "upload_collection: HashSeq collection → %s (%d entries)",
        collection_hash[:12],
        len(entries),
    )
    return collection_hash


async def fetch_from_collection(
    blobs: object,
    collection_hash: str,
    entry_name: str,
) -> bytes | None:
    """Fetch a named entry from a native iroh HashSeq collection.

    Uses ``blobs.list_collection(hash)`` to enumerate the collection's
    entries, finds the one matching ``entry_name``, and reads it.

    Args:
        blobs:           A live ``BlobsClient``.
        collection_hash: Hash of the HashSeq collection blob.
        entry_name:      The entry name (e.g. ``"contract.bin"``).

    Returns:
        The raw bytes of the entry, or ``None`` if not found.
    """
    try:
        entries_list = await blobs.list_collection(collection_hash)
    except Exception as exc:  # noqa: BLE001
        logger.debug("fetch_from_collection: cannot list collection %s: %s", collection_hash[:12], exc)
        return None

    for name, entry_hash, _size in entries_list:
        if name == entry_name:
            try:
                return await blobs.read_to_bytes(entry_hash)
            except Exception as exc:  # noqa: BLE001
                logger.debug(
                    "fetch_from_collection: cannot read entry %s: %s", entry_name, exc
                )
                return None

    logger.debug("fetch_from_collection: entry %r not found in collection", entry_name)
    return None


# ── Publication ───────────────────────────────────────────────────────────────


async def publish_contract(
    contract: ServiceContract,
    type_defs: dict[str, TypeDef],
    blobs_client: object | None = None,
    docs_client: object | None = None,
) -> tuple[str, str]:
    """Publish a contract's full collection to the Iroh blob store.

    All entries from ``build_collection()`` are uploaded individually. The
    collection index blob is uploaded last and its hash is returned.

    Args:
        contract:     The resolved ServiceContract.
        type_defs:    Dict mapping FQN to TypeDef.
        blobs_client: A live BlobsClient (or None for dry-run).
        docs_client:  Unused; reserved for future docs integration.

    Returns:
        ``(contract_id, collection_hash)`` -- both are 64-char hex strings.
        In dry-run mode (``blobs_client is None``), ``collection_hash == contract_id``.
    """
    entries = build_collection(contract, type_defs)
    contract_bytes = canonical_xlang_bytes(contract)
    contract_id = compute_contract_id(contract_bytes)

    if blobs_client is None:
        # Dry-run: return without uploading
        return contract_id, contract_id

    collection_hash = await upload_collection(blobs_client, entries)
    logger.info(
        "publish_contract: %s v%d uploaded (%d entries, collection_hash=%s)",
        contract.name,
        contract.version,
        len(entries),
        collection_hash[:12],
    )
    return contract_id, collection_hash


# ── Fetch + verification ──────────────────────────────────────────────────────


async def fetch_contract(
    contract_id: str,
    blobs_client: object,
    collection_hash: str | None = None,
) -> bytes | None:
    """Fetch and verify contract bytes from the Iroh blob store.

    Two modes:

    ``collection_hash`` provided (multi-file, HashSeq format):
        - Lists the collection entries via ``blobs.list_collection()``.
        - Fetches ``contract.bin`` from the collection.
        - Verifies ``blake3(contract_bytes).hexdigest() == contract_id``.

    ``collection_hash is None`` or equals ``contract_id`` (single-blob, ``"raw"``):
        - Reads the blob directly by ``contract_id``.

    Args:
        contract_id:     64-char hex string identifying the contract.
        blobs_client:    A live BlobsClient.
        collection_hash: Hash of the HashSeq collection blob (multi-file mode)
                         or ``None``/same as ``contract_id`` for single-blob mode.

    Returns:
        The raw canonical contract bytes on success, or ``None`` if unavailable.

    Raises:
        ValueError: if the fetched bytes fail the BLAKE3 hash check.
    """
    import blake3  # type: ignore[import]

    if collection_hash is None or collection_hash == contract_id:
        # Single-blob mode: the blob hash IS the contract_id.
        try:
            return await blobs_client.read_to_bytes(contract_id)
        except Exception as exc:  # noqa: BLE001
            logger.debug("fetch_contract: cannot read blob %s: %s", contract_id[:12], exc)
            return None

    # Multi-file index mode
    contract_bytes = await fetch_from_collection(blobs_client, collection_hash, "contract.bin")
    if contract_bytes is None:
        return None

    # Integrity check
    actual_id = blake3.blake3(contract_bytes).hexdigest()
    if actual_id != contract_id:
        raise ValueError(
            f"fetch_contract: hash mismatch for contract {contract_id[:12]}: "
            f"got {actual_id[:12]}"
        )

    return contract_bytes
