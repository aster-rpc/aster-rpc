"""
aster.contract.publication — Contract publication and fetch via Iroh blob store.

Spec reference: Aster-ContractIdentity.md §11.5

Two collection formats are supported:

``"raw"``   — single-blob: the contract canonical bytes are stored as one blob;
              ``collection_hash == contract_id``. Used by RegistryPublisher when
              no multi-file layout is needed.

``"index"`` — multi-file: all collection entries (contract.bin, manifest.json,
              types/*.bin) are uploaded individually; a JSON collection index blob
              maps entry names to their hashes.  ``collection_hash`` is the BLAKE3
              hash of the index blob.

The collection index blob format (JSON, UTF-8):
    {
      "version": 1,
      "entries": [
        {"name": "contract.bin", "hash": "<hex>", "size": <int>},
        ...
      ]
    }
"""

from __future__ import annotations

import json
import logging
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
    pass  # iroh types imported lazily

logger = logging.getLogger(__name__)


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
        List of (entry_name, raw_bytes) pairs in deterministic order.
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


# ── Multi-file collection upload ──────────────────────────────────────────────


async def upload_collection(
    blobs: object,
    entries: list[tuple[str, bytes]],
) -> str:
    """Upload all collection entries and return the collection index hash.

    For each ``(name, data)`` entry:
    - Upload ``data`` as a raw blob → obtain ``hash_hex``.

    Then build a JSON collection index that maps names to hashes, upload it,
    and return its hash (``collection_hash``).

    Args:
        blobs:   A live ``BlobsClient``.
        entries: ``[(name, bytes)]`` list from ``build_collection()``.

    Returns:
        ``collection_hash`` — hex hash of the collection index blob.
    """
    index_entries: list[dict] = []
    for name, data in entries:
        hash_hex = await blobs.add_bytes(data)
        index_entries.append({"name": name, "hash": hash_hex, "size": len(data)})
        logger.debug("upload_collection: %s → %s (%d bytes)", name, hash_hex[:12], len(data))

    index = {"version": 1, "entries": index_entries}
    index_bytes = json.dumps(index, separators=(",", ":"), sort_keys=False).encode("utf-8")
    collection_hash = await blobs.add_bytes(index_bytes)
    logger.debug(
        "upload_collection: index blob → %s (%d entries)", collection_hash[:12], len(index_entries)
    )
    return collection_hash


async def fetch_from_collection(
    blobs: object,
    collection_hash: str,
    entry_name: str,
) -> bytes | None:
    """Fetch a named entry from a multi-file collection index.

    Args:
        blobs:           A live ``BlobsClient``.
        collection_hash: Hash of the collection index blob.
        entry_name:      The entry name (e.g. ``"contract.bin"``).

    Returns:
        The raw bytes of the entry, or ``None`` if not found.
    """
    try:
        index_bytes = await blobs.read_to_bytes(collection_hash)
    except Exception as exc:  # noqa: BLE001
        logger.debug("fetch_from_collection: cannot read index %s: %s", collection_hash[:12], exc)
        return None

    try:
        index = json.loads(index_bytes)
    except Exception as exc:  # noqa: BLE001
        logger.debug("fetch_from_collection: malformed index blob: %s", exc)
        return None

    for entry in index.get("entries", []):
        if entry["name"] == entry_name:
            try:
                return await blobs.read_to_bytes(entry["hash"])
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
        ``(contract_id, collection_hash)`` — both are 64-char hex strings.
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

    ``collection_hash`` provided (multi-file, ``"index"`` format):
        - Reads the collection index blob.
        - Fetches ``contract.bin`` from the index.
        - Verifies ``blake3(contract_bytes).hexdigest() == contract_id``.

    ``collection_hash is None`` or equals ``contract_id`` (single-blob, ``"raw"``):
        - Reads the blob directly by ``contract_id``.

    Args:
        contract_id:     64-char hex string identifying the contract.
        blobs_client:    A live BlobsClient.
        collection_hash: Hash of the collection index blob (multi-file mode) or
                         ``None``/same as ``contract_id`` for single-blob mode.

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
