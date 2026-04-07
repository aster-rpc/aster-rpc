/**
 * ContractManifest — persisted contract identity metadata.
 *
 * Spec reference: Aster-ContractIdentity.md S11.4
 *
 * A manifest records a service contract's identity (BLAKE3 hash),
 * method schemas, type hashes, and metadata. Used for:
 * - Startup verification (contract hasn't changed)
 * - Dynamic discovery (shell, MCP, other language clients)
 * - Contract publication (registry + blobs)
 */

import { MAX_MANIFEST_METHODS, MAX_MANIFEST_TYPE_HASHES } from '../limits.js';

/** Method descriptor within a manifest. */
export interface ManifestMethod {
  name: string;
  pattern: string; // "unary" | "server_stream" | "client_stream" | "bidi_stream"
  requestType: string; // hex hash or type name
  responseType: string;
  timeout: number;
  idempotent: boolean;
  /** Request type field descriptors for dynamic invocation. */
  fields: ManifestField[];
  /** Fory wire tags for request/response types. */
  requestWireTag?: string;
  responseWireTag?: string;
  /** Fory wire tags for response fields (for dynamic type synthesis). */
  responseFields?: ManifestField[];
}

/** Field descriptor for dynamic invocation. */
export interface ManifestField {
  name: string;
  type: string; // "str", "int", "float", "bool", "bytes", "list[X]", etc.
  required: boolean;
  default?: unknown;
}

/** Persisted contract manifest. */
export interface ContractManifest {
  service: string;
  version: number;
  contractId: string; // 64-char hex BLAKE3
  canonicalEncoding: string;
  typeCount: number;
  typeHashes: string[]; // hex BLAKE3 per TypeDef
  methodCount: number;
  methods: ManifestMethod[];
  serializationModes: string[];
  scoped: string; // "shared" | "stream"
  deprecated: boolean;
  semver?: string;
  vcsRevision?: string;
  vcsTag?: string;
  vcsUrl?: string;
  changelog?: string;
}

/** Error raised when a live contract doesn't match the manifest. */
export class FatalContractMismatch extends Error {
  constructor(
    public readonly serviceName: string,
    public readonly serviceVersion: number,
    public readonly expectedId: string,
    public readonly actualId: string,
  ) {
    super(
      `Contract identity mismatch for '${serviceName}' v${serviceVersion}:\n` +
      `  expected: ${expectedId}\n` +
      `  actual:   ${actualId}\n` +
      `This usually means the service definition changed without updating the manifest.`,
    );
    this.name = 'FatalContractMismatch';
  }
}

/** Verify that a contract ID matches the manifest. Throws on mismatch. */
export function verifyManifestOrFatal(
  manifest: ContractManifest,
  actualContractId: string,
): void {
  if (manifest.contractId !== actualContractId) {
    throw new FatalContractMismatch(
      manifest.service,
      manifest.version,
      manifest.contractId,
      actualContractId,
    );
  }
}

/** Serialize a manifest to JSON. */
export function manifestToJson(manifest: ContractManifest): string {
  return JSON.stringify({
    service: manifest.service,
    version: manifest.version,
    contract_id: manifest.contractId,
    canonical_encoding: manifest.canonicalEncoding,
    type_count: manifest.typeCount,
    type_hashes: manifest.typeHashes,
    method_count: manifest.methodCount,
    methods: manifest.methods.map(m => ({
      name: m.name,
      pattern: m.pattern,
      request_type: m.requestType,
      response_type: m.responseType,
      timeout: m.timeout,
      idempotent: m.idempotent,
      fields: m.fields,
      request_wire_tag: m.requestWireTag,
      response_wire_tag: m.responseWireTag,
      response_fields: m.responseFields,
    })),
    serialization_modes: manifest.serializationModes,
    scoped: manifest.scoped,
    deprecated: manifest.deprecated,
    semver: manifest.semver,
  }, null, 2);
}

/**
 * Extract method descriptors from a manifest for introspection.
 * Returns a simplified record of method name → pattern.
 */
export function extractMethodDescriptors(manifest: ContractManifest): Record<string, string> {
  const result: Record<string, string> = {};
  for (const method of manifest.methods) {
    result[method.name] = method.pattern ?? 'unary';
  }
  return result;
}

/**
 * Save a manifest to a JSON file.
 */
export function saveManifest(manifest: ContractManifest, filePath: string): void {
  const { writeFileSync } = require('node:fs');
  writeFileSync(filePath, manifestToJson(manifest), 'utf-8');
}

/** Parse a manifest from JSON. */
export function manifestFromJson(json: string): ContractManifest {
  const data = JSON.parse(json);

  // Validate limits
  const methods = data.methods ?? [];
  if (methods.length > MAX_MANIFEST_METHODS) {
    throw new Error(`manifest has ${methods.length} methods, max is ${MAX_MANIFEST_METHODS}`);
  }
  const typeHashes = data.type_hashes ?? [];
  if (typeHashes.length > MAX_MANIFEST_TYPE_HASHES) {
    throw new Error(`manifest has ${typeHashes.length} type hashes, max is ${MAX_MANIFEST_TYPE_HASHES}`);
  }

  // Coerce version to number
  const version = typeof data.version === 'string' ? parseInt(data.version, 10) : data.version;

  return {
    service: data.service,
    version,
    contractId: data.contract_id,
    canonicalEncoding: data.canonical_encoding ?? 'fory-xlang/0.15',
    typeCount: data.type_count ?? 0,
    typeHashes,
    methodCount: data.method_count ?? methods.length,
    methods: methods.map((m: any) => ({
      name: m.name,
      pattern: m.pattern,
      requestType: m.request_type ?? '',
      responseType: m.response_type ?? '',
      timeout: m.timeout ?? 0,
      idempotent: m.idempotent ?? false,
      fields: m.fields ?? [],
      requestWireTag: m.request_wire_tag,
      responseWireTag: m.response_wire_tag,
      responseFields: m.response_fields,
    })),
    serializationModes: data.serialization_modes ?? [],
    scoped: data.scoped ?? 'shared',
    deprecated: data.deprecated ?? false,
    semver: data.semver,
    vcsRevision: data.vcs_revision,
    vcsTag: data.vcs_tag,
    vcsUrl: data.vcs_url,
    changelog: data.changelog,
  };
}
