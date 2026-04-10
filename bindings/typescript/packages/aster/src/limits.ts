/**
 * Security limits for the Aster RPC framework.
 *
 * All size caps, entry limits, and validation constants live here.
 * Import from this module -- never hardcode limits elsewhere.
 *
 * Spec reference: Aster-SPEC.md S-Security, Aster-ContractIdentity.md S11.4.4.1
 */

// -- Wire-level limits -------------------------------------------------------

/** Maximum wire frame size in bytes (16 MiB). Spec S4.3. */
export const MAX_FRAME_SIZE = 16 * 1024 * 1024;

/** Maximum decompressed payload size (16 MiB). Prevents zstd decompression bombs. */
export const MAX_DECOMPRESSED_SIZE = 16 * 1024 * 1024;

/** Default timeout for reading a frame from a QUIC stream (seconds). */
export const DEFAULT_FRAME_READ_TIMEOUT_S = 30.0;

/** Server-side upper bound on handler execution time (5 minutes).
 *  Applied regardless of client deadline. If the client sends no deadline
 *  (deadlineEpochMs=0), this is used as the default. If the client sends
 *  a deadline further in the future, it is clamped to this value. */
export const MAX_HANDLER_TIMEOUT_S = 300.0;

/** Maximum number of items in a client-stream or bidi-stream before
 *  the server stops accepting. Prevents memory exhaustion from a
 *  malicious client sending millions of tiny frames. */
export const MAX_CLIENT_STREAM_ITEMS = 100_000;

// -- Metadata limits ---------------------------------------------------------

/** Maximum number of key-value pairs in StreamHeader/CallHeader metadata. */
export const MAX_METADATA_ENTRIES = 64;

/** Maximum total size of all metadata keys + values in bytes. */
export const MAX_METADATA_TOTAL_BYTES = 8192;

/** Maximum length of a single metadata key. */
export const MAX_METADATA_KEY_LEN = 256;

/** Maximum length of a single metadata value. */
export const MAX_METADATA_VALUE_LEN = 4096;

// -- RPC status limits -------------------------------------------------------

/** Maximum length of the RpcStatus message string. */
export const MAX_STATUS_MESSAGE_LEN = 4096;

/** Maximum number of detail key-value pairs in an RpcStatus. */
export const MAX_STATUS_DETAIL_ENTRIES = 32;

// -- Admission limits --------------------------------------------------------

/** Maximum number of ServiceSummary entries in a ConsumerAdmissionResponse. */
export const MAX_SERVICES_IN_ADMISSION = 10_000;

/** Maximum size of the admission request/response JSON payload. */
export const MAX_ADMISSION_PAYLOAD_SIZE = 64 * 1024;

/** Maximum number of channels per ServiceSummary. */
export const MAX_CHANNELS_PER_SERVICE = 100;

// -- Credential field lengths (hex characters) --------------------------------

/** Expected hex string lengths for credential/identity fields. */
export const HEX_FIELD_LENGTHS: Record<string, number> = {
  root_pubkey: 64,    // 32 bytes -> 64 hex chars
  nonce: 64,
  signature: 128,     // 64 bytes -> 128 hex chars
  endpoint_id: 64,
  contract_id: 64,
};

// -- Registry / collection limits --------------------------------------------

/** Maximum number of entries in a contract collection index. */
export const MAX_COLLECTION_INDEX_ENTRIES = 10_000;

/** Maximum length of a collection entry name. */
export const MAX_COLLECTION_ENTRY_NAME_LEN = 256;

/** Maximum number of entries in a registry ACL list. */
export const MAX_ACL_LIST_SIZE = 10_000;

/** Maximum number of methods in a ContractManifest. */
export const MAX_MANIFEST_METHODS = 10_000;

/** Maximum number of fields per method in a ContractManifest. */
export const MAX_MANIFEST_FIELDS_PER_METHOD = 1_000;

/** Maximum number of type hashes in a ContractManifest. */
export const MAX_MANIFEST_TYPE_HASHES = 100_000;

// -- Gossip limits -----------------------------------------------------------

/** Maximum gossip message payload before JSON parsing. */
export const MAX_GOSSIP_PAYLOAD_SIZE = 64 * 1024;

/** Maximum nesting depth for JSON deserialization from untrusted sources. */
export const MAX_JSON_DEPTH = 50;

// -- General string limits ---------------------------------------------------

/** Maximum length of a service name. */
export const MAX_SERVICE_NAME_LEN = 256;

/** Maximum length of a method name. */
export const MAX_METHOD_NAME_LEN = 256;

// -- Validation helpers -------------------------------------------------------

/** Raised when a security limit is exceeded. Maps to RESOURCE_EXHAUSTED. */
export class LimitExceeded extends Error {
  readonly field: string;
  readonly limit: number;
  readonly actual: number | undefined;

  constructor(field: string, limit: number, actual?: number) {
    const detail = actual !== undefined ? ` (got ${actual})` : '';
    super(`${field} exceeds limit of ${limit}${detail}`);
    this.name = 'LimitExceeded';
    this.field = field;
    this.limit = limit;
    this.actual = actual;
  }
}

const HEX_REGEX = /^[0-9a-fA-F]*$/;

/** Validate that a hex-encoded field has the expected length. */
export function validateHexField(name: string, value: string): void {
  if (!value) return; // empty is allowed (optional fields)

  const expected = HEX_FIELD_LENGTHS[name];
  if (expected !== undefined && value.length !== expected) {
    throw new LimitExceeded(name, expected, value.length);
  }

  if (!HEX_REGEX.test(value)) {
    throw new Error(`${name}: invalid hex string`);
  }
}

const encoder = new TextEncoder();

/** Validate metadata key-value pairs against limits. */
export function validateMetadata(keys: string[], values: string[]): void {
  const count = keys.length;
  if (count > MAX_METADATA_ENTRIES) {
    throw new LimitExceeded('metadata entries', MAX_METADATA_ENTRIES, count);
  }

  let totalBytes = 0;
  for (const k of keys) {
    if (k.length > MAX_METADATA_KEY_LEN) {
      throw new LimitExceeded('metadata key length', MAX_METADATA_KEY_LEN, k.length);
    }
    totalBytes += encoder.encode(k).byteLength;
  }

  for (const v of values) {
    if (v.length > MAX_METADATA_VALUE_LEN) {
      throw new LimitExceeded('metadata value length', MAX_METADATA_VALUE_LEN, v.length);
    }
    totalBytes += encoder.encode(v).byteLength;
  }

  if (totalBytes > MAX_METADATA_TOTAL_BYTES) {
    throw new LimitExceeded('metadata total bytes', MAX_METADATA_TOTAL_BYTES, totalBytes);
  }
}

/** Truncate an RpcStatus message to the maximum allowed length. */
export function validateStatusMessage(message: string): string {
  if (message.length > MAX_STATUS_MESSAGE_LEN) {
    return message.slice(0, MAX_STATUS_MESSAGE_LEN - 3) + '...';
  }
  return message;
}
