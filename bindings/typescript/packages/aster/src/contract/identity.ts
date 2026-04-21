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
export const ScopeKind = {
  SHARED: 0,
  SESSION: 1,
  /** Legacy alias kept for back-compat with code that imported ScopeKind.STREAM.
   *  Resolves to the same integer (1), so contract ids are unaffected. */
  STREAM: 1,
} as const;

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
// The Rust canonical encoder accepts both "session" and "stream" via
// #[serde(alias = "stream")]. We send "stream" here for compat with
// pre-rename NAPI binaries that haven't been rebuilt yet.
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
  canonicalBytesToJson(typeName: string, data: Uint8Array): string;
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

// -- Canonical decoder: bytes → struct ---------------------------------------

/**
 * Decoded `FieldDef` shape matching the Rust core struct. Lives here
 * rather than in `manifest.ts` because it's the canonical view — the
 * manifest's `ManifestField` is a human-friendly projection on top of
 * it. Hex-encoded fields (type_ref, container_key_ref, default_value)
 * are kept as hex strings to round-trip the canonical byte form.
 */
export interface CanonicalFieldDef {
  id: number;
  name: string;
  typeKind: number;
  typePrimitive: string;
  typeRef: string;
  selfRefName: string;
  optional: boolean;
  refTracked: boolean;
  container: number;
  containerKeyKind: number;
  containerKeyPrimitive: string;
  containerKeyRef: string;
  required: boolean;
  defaultValue: string;
}

export interface CanonicalEnumValueDef {
  name: string;
  value: number;
}

export interface CanonicalUnionVariantDef {
  name: string;
  id: number;
  typeRef: string;
}

export interface CanonicalTypeDef {
  kind: number;
  package: string;
  name: string;
  fields: CanonicalFieldDef[];
  enumValues: CanonicalEnumValueDef[];
  unionVariants: CanonicalUnionVariantDef[];
}

// Rust `#[serde(rename_all = "snake_case")]` renders the enum variants
// as strings; we map them back to the numeric constants exposed on this
// module (TypeKind, ContainerKind, TypeDefKind) so call-site code stays
// symbolic rather than stringly-typed.
const TYPE_KIND_FROM_STR: Record<string, number> = {
  primitive: TypeKind.PRIMITIVE,
  ref: TypeKind.REF,
  self_ref: TypeKind.SELF_REF,
  any: TypeKind.ANY,
};

const CONTAINER_KIND_FROM_STR: Record<string, number> = {
  none: ContainerKind.NONE,
  list: ContainerKind.LIST,
  set: ContainerKind.SET,
  map: ContainerKind.MAP,
};

const TYPEDEF_KIND_FROM_STR: Record<string, number> = {
  message: TypeDefKind.MESSAGE,
  enum: TypeDefKind.ENUM,
  union: TypeDefKind.UNION,
};

function mustLookup(table: Record<string, number>, value: unknown, kind: string): number {
  if (typeof value !== 'string' || !(value in table)) {
    throw new Error(`canonical decode: unknown ${kind} value ${JSON.stringify(value)}`);
  }
  return table[value]!;
}

function fieldDefFromJson(raw: any): CanonicalFieldDef {
  return {
    id: raw.id,
    name: raw.name,
    typeKind: mustLookup(TYPE_KIND_FROM_STR, raw.type_kind, 'TypeKind'),
    typePrimitive: raw.type_primitive,
    typeRef: raw.type_ref,
    selfRefName: raw.self_ref_name,
    optional: raw.optional,
    refTracked: raw.ref_tracked,
    container: mustLookup(CONTAINER_KIND_FROM_STR, raw.container, 'ContainerKind'),
    containerKeyKind: mustLookup(TYPE_KIND_FROM_STR, raw.container_key_kind, 'TypeKind'),
    containerKeyPrimitive: raw.container_key_primitive,
    containerKeyRef: raw.container_key_ref,
    required: raw.required,
    defaultValue: raw.default_value,
  };
}

/**
 * Decode canonical XLANG bytes of a `TypeDef` (produced by
 * `canonicalXlangBytes` on a single type graph node) back into the
 * struct form. Hash-bearing bytes fields are returned hex-encoded.
 *
 * Wraps `canonicalBytesToJson("TypeDef", bytes)` in the NAPI binding.
 * Callers that only need the field list (dynamic proxy nested-type
 * resolution) can read `.fields` directly; the decoded shape mirrors
 * the Rust `TypeDef` struct one-to-one.
 */
export function decodeTypeDefBytes(bytes: Uint8Array): CanonicalTypeDef {
  const native = requireNative();
  const raw = JSON.parse(native.canonicalBytesToJson('TypeDef', bytes));
  return {
    kind: mustLookup(TYPEDEF_KIND_FROM_STR, raw.kind, 'TypeDefKind'),
    package: raw.package,
    name: raw.name,
    fields: (raw.fields ?? []).map(fieldDefFromJson),
    enumValues: (raw.enum_values ?? []).map((e: any) => ({ name: e.name, value: e.value })),
    unionVariants: (raw.union_variants ?? []).map((u: any) => ({
      name: u.name,
      id: u.id,
      typeRef: u.type_ref,
    })),
  };
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
    // Accept both the canonical 'session' value and the legacy 'stream' alias.
    scoped: (info.scoped === 'session' || info.scoped === 'stream') ? 1 : 0,
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
 *
 * When `requestTypeHash` / `responseTypeHash` are present on a method
 * (populated by `registerGenerated` from the aster-gen build-time
 * scanner), they're threaded through to the ServiceContract so that
 * the resulting `contract_id` matches what Python/Java compute for the
 * same logical service. When absent (runtime-only path without
 * codegen), the method falls back to 32 zero bytes — contract_id is
 * then stable-per-service but not cross-language equivalent.
 */
export function fromServiceInfo(info: {
  name: string;
  version: number;
  scoped?: string;
  methods: Map<string, {
    name: string;
    pattern: string;
    idempotent?: boolean;
    requestTypeHash?: Uint8Array;
    responseTypeHash?: Uint8Array;
  }>;
  requires?: { kind: string; roles: string[] };
}): ServiceContract {
  const contract: ServiceContract = {
    name: info.name,
    version: info.version,
    methods: [],
    serializationModes: [],
    // Accept both the canonical 'session' value and the legacy 'stream' alias.
    scoped: (info.scoped === 'session' || info.scoped === 'stream') ? 1 : 0,
    requires: info.requires ? { kind: info.requires.kind === 'any_of' ? 1 : 2, roles: info.requires.roles } : undefined,
  };
  for (const [, m] of info.methods) {
    contract.methods.push({
      name: m.name,
      pattern: m.pattern === 'server_stream' ? 1 : m.pattern === 'client_stream' ? 2 : m.pattern === 'bidi_stream' ? 3 : 0,
      requestType: m.requestTypeHash ?? new Uint8Array(32),
      responseType: m.responseTypeHash ?? new Uint8Array(32),
      idempotent: m.idempotent ?? false,
      defaultTimeout: 0,
      requires: undefined,
    });
  }
  return contract;
}
