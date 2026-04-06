"""
aster.limits — Security limits for the Aster RPC framework.

All size caps, entry limits, and validation constants live here.
Import from this module — never hardcode limits elsewhere.

These limits protect against:
  - Allocation bombs (oversized strings, lists, compressed payloads)
  - CPU exhaustion (deeply nested JSON, huge metadata dicts)
  - Hangs (missing timeouts on network reads)
  - Type confusion (invalid hex fields, wrong JSON types)

Spec reference: Aster-SPEC.md §Security, Aster-ContractIdentity.md §11.4.4.1
"""

from __future__ import annotations

# ── Wire-level limits ─────────────────────────────────────────────────────────

MAX_FRAME_SIZE: int = 16 * 1024 * 1024
"""Maximum wire frame size in bytes (16 MiB). Spec §4.3."""

MAX_DECOMPRESSED_SIZE: int = 16 * 1024 * 1024
"""Maximum decompressed payload size in bytes (16 MiB).
Prevents zstd decompression bombs where a small compressed payload
expands to exhaust memory."""

DEFAULT_FRAME_READ_TIMEOUT_S: float = 30.0
"""Default timeout for reading a frame from a QUIC stream (seconds).
Applied when no RPC deadline is set. Prevents hangs from peers that
send a length header but never send the body."""

# ── Metadata limits ───────────────────────────────────────────────────────────

MAX_METADATA_ENTRIES: int = 64
"""Maximum number of key-value pairs in StreamHeader/CallHeader metadata."""

MAX_METADATA_TOTAL_BYTES: int = 8192
"""Maximum total size of all metadata keys + values in bytes."""

MAX_METADATA_KEY_LEN: int = 256
"""Maximum length of a single metadata key."""

MAX_METADATA_VALUE_LEN: int = 4096
"""Maximum length of a single metadata value."""

# ── RPC status limits ─────────────────────────────────────────────────────────

MAX_STATUS_MESSAGE_LEN: int = 4096
"""Maximum length of the RpcStatus message string."""

MAX_STATUS_DETAIL_ENTRIES: int = 32
"""Maximum number of detail key-value pairs in an RpcStatus."""

# ── Admission limits ──────────────────────────────────────────────────────────

MAX_SERVICES_IN_ADMISSION: int = 10_000
"""Maximum number of ServiceSummary entries in a ConsumerAdmissionResponse."""

MAX_ADMISSION_PAYLOAD_SIZE: int = 64 * 1024
"""Maximum size of the admission request/response JSON payload."""

MAX_CHANNELS_PER_SERVICE: int = 100
"""Maximum number of channels per ServiceSummary."""

# ── Credential field lengths (hex characters) ─────────────────────────────────

HEX_FIELD_LENGTHS: dict[str, int] = {
    "root_pubkey": 64,      # 32 bytes → 64 hex chars
    "nonce": 64,            # 32 bytes → 64 hex chars
    "signature": 128,       # 64 bytes → 128 hex chars
    "endpoint_id": 64,      # 32 bytes → 64 hex chars
    "contract_id": 64,      # 32 bytes → 64 hex chars
}
"""Expected hex string lengths for credential/identity fields."""

# ── Registry / collection limits ──────────────────────────────────────────────

MAX_COLLECTION_INDEX_ENTRIES: int = 10_000
"""Maximum number of entries in a contract collection index."""

MAX_COLLECTION_ENTRY_NAME_LEN: int = 256
"""Maximum length of a collection entry name (e.g. 'types/abc123.bin')."""

MAX_ACL_LIST_SIZE: int = 10_000
"""Maximum number of entries in a registry ACL list."""

MAX_MANIFEST_METHODS: int = 10_000
"""Maximum number of methods in a ContractManifest."""

MAX_MANIFEST_FIELDS_PER_METHOD: int = 1_000
"""Maximum number of fields per method in a ContractManifest."""

MAX_MANIFEST_TYPE_HASHES: int = 100_000
"""Maximum number of type hashes in a ContractManifest."""

# ── Gossip limits ─────────────────────────────────────────────────────────────

MAX_GOSSIP_PAYLOAD_SIZE: int = 64 * 1024
"""Maximum gossip message payload before JSON parsing."""

MAX_JSON_DEPTH: int = 50
"""Maximum nesting depth for JSON deserialization from untrusted sources."""

# ── General string limits ─────────────────────────────────────────────────────

MAX_SERVICE_NAME_LEN: int = 256
"""Maximum length of a service name."""

MAX_METHOD_NAME_LEN: int = 256
"""Maximum length of a method name."""


# ── Validation helpers ────────────────────────────────────────────────────────


class LimitExceeded(Exception):
    """Raised when a security limit is exceeded.

    Maps to StatusCode.RESOURCE_EXHAUSTED in the RPC layer.
    """

    def __init__(self, field: str, limit: int, actual: int | None = None) -> None:
        self.field = field
        self.limit = limit
        self.actual = actual
        detail = f" (got {actual})" if actual is not None else ""
        super().__init__(f"{field} exceeds limit of {limit}{detail}")


def validate_hex_field(name: str, value: str) -> None:
    """Validate that a hex-encoded field has the expected length.

    Args:
        name: Field name (must be a key in HEX_FIELD_LENGTHS).
        value: The hex string to validate.

    Raises:
        LimitExceeded: If the length doesn't match.
        ValueError: If the string contains non-hex characters.
    """
    if not value:
        return  # empty is allowed (optional fields)

    expected = HEX_FIELD_LENGTHS.get(name)
    if expected is not None and len(value) != expected:
        raise LimitExceeded(name, expected, len(value))

    # Validate hex characters
    try:
        bytes.fromhex(value)
    except ValueError as e:
        raise ValueError(f"{name}: invalid hex string: {e}") from e


def validate_metadata(keys: list[str], values: list[str]) -> None:
    """Validate metadata key-value pairs against limits.

    Args:
        keys: Metadata keys.
        values: Metadata values.

    Raises:
        LimitExceeded: If any limit is exceeded.
    """
    count = len(keys)
    if count > MAX_METADATA_ENTRIES:
        raise LimitExceeded("metadata entries", MAX_METADATA_ENTRIES, count)

    total_bytes = 0
    for k in keys:
        if len(k) > MAX_METADATA_KEY_LEN:
            raise LimitExceeded("metadata key length", MAX_METADATA_KEY_LEN, len(k))
        total_bytes += len(k.encode("utf-8"))

    for v in values:
        if len(v) > MAX_METADATA_VALUE_LEN:
            raise LimitExceeded("metadata value length", MAX_METADATA_VALUE_LEN, len(v))
        total_bytes += len(v.encode("utf-8"))

    if total_bytes > MAX_METADATA_TOTAL_BYTES:
        raise LimitExceeded("metadata total bytes", MAX_METADATA_TOTAL_BYTES, total_bytes)


def validate_status_message(message: str) -> str:
    """Truncate an RpcStatus message to the maximum allowed length."""
    if len(message) > MAX_STATUS_MESSAGE_LEN:
        return message[:MAX_STATUS_MESSAGE_LEN - 3] + "..."
    return message
