/**
 * Contract identity — canonical serialization and BLAKE3 hashing.
 *
 * Spec reference: Aster-ContractIdentity.md S11.3
 *
 * Contract ID = BLAKE3(canonical_xlang_bytes(ServiceContract))
 *
 * All canonical encoding and hashing is delegated to the Rust core via
 * NAPI-RS. The native binding is required — without it, there is no Iroh
 * transport either, so the entire binding is non-functional.
 */

// -- Enums (normative values from spec) ---------------------------------------

export const TypeKind = { PRIMITIVE: 0, REF: 1, SELF_REF: 2, ANY: 3 } as const;
export const ContainerKind = { NONE: 0, LIST: 1, SET: 2, MAP: 3 } as const;
export const TypeDefKind = { MESSAGE: 0, ENUM: 1, UNION: 2 } as const;
export const MethodPattern = { UNARY: 0, SERVER_STREAM: 1, CLIENT_STREAM: 2, BIDI_STREAM: 3 } as const;
export const CapabilityKind = { ROLE: 0, ANY_OF: 1, ALL_OF: 2 } as const;
export const ScopeKind = { SHARED: 0, STREAM: 1 } as const;

// -- Data types ---------------------------------------------------------------

export interface CapabilityRequirement {
  kind: number;
  roles: string[];
}

export interface MethodDef {
  name: string;
  pattern: number;
  requestType: Uint8Array; // 32-byte BLAKE3 hash
  responseType: Uint8Array; // 32-byte BLAKE3 hash
  idempotent: boolean;
  defaultTimeout: number; // float64
  requires?: CapabilityRequirement;
}

export interface ServiceContract {
  name: string;
  version: number;
  methods: MethodDef[];
  serializationModes: string[];
  scoped: number;
  requires?: CapabilityRequirement;
}

// -- Serde-compatible enum names (must match Rust #[serde(rename_all = "snake_case")]) --

const PATTERN_NAMES: Record<number, string> = {
  0: 'unary', 1: 'server_stream', 2: 'client_stream', 3: 'bidi_stream',
};
const SCOPE_NAMES: Record<number, string> = { 0: 'shared', 1: 'stream' };
const CAP_NAMES: Record<number, string> = { 0: 'role', 1: 'any_of', 2: 'all_of' };

function hex(data: Uint8Array): string {
  return Array.from(data, b => b.toString(16).padStart(2, '0')).join('');
}

/** Convert a ServiceContract to JSON matching the Rust serde format. */
function contractToJson(contract: ServiceContract): string {
  return JSON.stringify({
    name: contract.name,
    version: contract.version,
    methods: contract.methods.map(m => ({
      name: m.name,
      pattern: PATTERN_NAMES[m.pattern] ?? m.pattern,
      request_type: hex(m.requestType),
      response_type: hex(m.responseType),
      idempotent: m.idempotent,
      default_timeout: m.defaultTimeout,
      requires: m.requires ? {
        kind: CAP_NAMES[m.requires.kind] ?? m.requires.kind,
        roles: m.requires.roles,
      } : null,
    })),
    serialization_modes: contract.serializationModes,
    scoped: SCOPE_NAMES[contract.scoped] ?? contract.scoped,
    requires: contract.requires ? {
      kind: CAP_NAMES[contract.requires.kind] ?? contract.requires.kind,
      roles: contract.requires.roles,
    } : null,
  });
}

// -- Native binding (required) ------------------------------------------------

interface NativeContract {
  canonicalBytesFromJson(typeName: string, json: string): Uint8Array;
  computeTypeHash(data: Uint8Array): Uint8Array;
  computeContractIdFromJson(json: string): string;
}

let _native: NativeContract | undefined;

/**
 * Set the native contract binding. Called once at startup when the
 * NAPI-RS addon is loaded.
 */
export function setNativeContract(native: NativeContract): void {
  _native = native;
}

function requireNative(): NativeContract {
  if (!_native) {
    throw new Error(
      'Native contract binding not configured. Call setNativeContract() with ' +
      'the NAPI-RS binding at startup. Without native support, contract ' +
      'identity and Iroh transport are unavailable.',
    );
  }
  return _native;
}

// -- Public API ---------------------------------------------------------------

/** Serialize a ServiceContract to canonical XLANG bytes via Rust core. */
export function canonicalXlangBytes(contract: ServiceContract): Uint8Array {
  const native = requireNative();
  return new Uint8Array(native.canonicalBytesFromJson('ServiceContract', contractToJson(contract)));
}

/** Compute BLAKE3 hash of canonical bytes. Returns 64-char hex string. */
export function computeContractId(canonicalBytes: Uint8Array): string {
  const native = requireNative();
  const digest = native.computeTypeHash(canonicalBytes);
  return Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2, '0')).join('');
}

/** Compute contract ID directly from a ServiceContract (most efficient path). */
export function contractIdFromJson(contract: ServiceContract): string {
  const native = requireNative();
  return native.computeContractIdFromJson(contractToJson(contract));
}

/** Alias for contractIdFromJson. */
export function contractIdFromContract(contract: ServiceContract): string {
  return contractIdFromJson(contract);
}

/**
 * Compute a contract ID from a @Service-decorated class.
 * Builds the ServiceContract from service info and computes its ID.
 */
export function contractIdFromService(serviceClass: new (...args: any[]) => any): string {
  // Build a minimal ServiceContract from service info
  const info = (serviceClass as any)[Symbol.for('aster.service_info')];
  if (!info) {
    throw new TypeError(`${serviceClass.name} is not decorated with @Service`);
  }
  const contract: ServiceContract = {
    name: info.name,
    version: info.version,
    methods: [],
    serializationModes: [],
    scoped: info.scoped === 'stream' ? 1 : 0,
    requires: undefined,
  };
  for (const [, m] of info.methods as Map<string, any>) {
    contract.methods.push({
      name: m.name,
      pattern: m.pattern === 'server_stream' ? 1 : m.pattern === 'client_stream' ? 2 : m.pattern === 'bidi_stream' ? 3 : 0,
      requestType: new Uint8Array(32),
      responseType: new Uint8Array(32),
      idempotent: m.idempotent ?? false,
      defaultTimeout: 0,
      requires: undefined,
    });
  }
  return contractIdFromJson(contract);
}

/**
 * Build the transitive type graph for a set of @WireType classes.
 * Alias for walkTypeGraph (from codec.ts) for contract identity use.
 */
export function buildTypeGraph(rootTypes: (new (...args: any[]) => any)[]): (new (...args: any[]) => any)[] {
  const WIRE_TYPE_KEY = Symbol.for('aster.wire_type');
  const visited = new Set<Function>();
  const ordered: (new (...args: any[]) => any)[] = [];
  function visit(cls: new (...args: any[]) => any): void {
    if (visited.has(cls)) return;
    visited.add(cls);
    const tag = (cls as any)[WIRE_TYPE_KEY];
    if (!tag) return;
    try {
      const inst = new cls();
      for (const key of Object.keys(inst)) {
        const ctor = inst[key]?.constructor;
        if (ctor && (ctor as any)[WIRE_TYPE_KEY]) visit(ctor);
      }
    } catch { /* ignore */ }
    ordered.push(cls);
  }
  for (const cls of rootTypes) visit(cls);
  return ordered;
}

/**
 * Build a ServiceContract from a ServiceInfo object.
 * Used for contract identity verification.
 */
export function fromServiceInfo(info: {
  name: string;
  version: number;
  scoped?: string;
  methods: Map<string, { name: string; pattern: string; idempotent?: boolean }>;
  requires?: { kind: string; roles: string[] };
}): ServiceContract {
  const contract: ServiceContract = {
    name: info.name,
    version: info.version,
    methods: [],
    serializationModes: [],
    scoped: info.scoped === 'stream' ? 1 : 0,
    requires: info.requires ? { kind: info.requires.kind === 'any_of' ? 1 : 2, roles: info.requires.roles } : undefined,
  };
  for (const [, m] of info.methods) {
    contract.methods.push({
      name: m.name,
      pattern: m.pattern === 'server_stream' ? 1 : m.pattern === 'client_stream' ? 2 : m.pattern === 'bidi_stream' ? 3 : 0,
      requestType: new Uint8Array(32),
      responseType: new Uint8Array(32),
      idempotent: m.idempotent ?? false,
      defaultTimeout: 0,
      requires: undefined,
    });
  }
  return contract;
}
