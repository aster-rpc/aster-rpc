/**
 * lowering.ts -- decorators → `DynamicService` (plan T4, design
 * `dynamic-dispatch-design.md` §1.5 / §4.3).
 *
 * Turns a `@Service` class's {@link ServiceInfo} (from the TC39 decorators
 * or `registerGenerated`) into the JSON registration form of
 * `aster::rpc::DynamicService`, which the native `DispatchServer` serves.
 * Rust derives the contract from it, runs Gate 3 on the declared `requires`
 * and enforces the method timeouts, so what is enforced is exactly what the
 * `contract_id` commits to.
 *
 * Type hashes come from `aster-gen` (`requestTypeHash` / `responseTypeHash`,
 * stamped by `registerGenerated`); without codegen a method falls back to
 * the zero hash, as {@link fromServiceInfo} always has.
 *
 * `method_idx` is the position in `ServiceInfo.methods` (insertion order),
 * which is also the index the dispatcher's handler table uses.
 */

import type { CapabilityRequirement, MethodInfo, ServiceInfo } from './service.js';
import { getServiceInfo } from './service.js';
import { canonicalSerializationModes } from './contract/identity.js';
import { loadNative } from './native.js';

/** One method in a `DynamicService` registration (design §1.5). */
export interface DynamicMethodSpec {
  name: string;
  pattern: 'unary' | 'server_stream' | 'client_stream' | 'bidi_stream';
  request: { hash: string; display?: string };
  response: { hash: string; display?: string };
  idempotent?: boolean;
  default_timeout?: number | null;
  requires?: { kind: 'role' | 'any_of' | 'all_of'; roles: string[] } | null;
  limits?: { max_frame_size?: number };
}

/** The JSON registration form of a `DynamicService` (design §1.5). */
export interface DynamicServiceSpec {
  name: string;
  version: number;
  scope?: 'shared' | 'session';
  requires?: { kind: 'role' | 'any_of' | 'all_of'; roles: string[] } | null;
  serialization_modes?: string[];
  producer_language?: string;
  iroh_only?: boolean;
  methods: DynamicMethodSpec[];
  type_defs?: unknown[];
  manifest_json?: string | null;
}

const ZERO_HASH = '0'.repeat(64);
const WIRE_TYPE_KEY = Symbol.for('aster.wire_type');
const PATTERNS = new Set(['unary', 'server_stream', 'client_stream', 'bidi_stream']);

/**
 * A `requires=` value as `{kind, roles}` (or `undefined`). Accepts a
 * {@link CapabilityRequirement} or a role string (one role, `role` kind).
 */
export function normalizeRequirement(
  req: CapabilityRequirement | string | undefined | null,
): { kind: 'role' | 'any_of' | 'all_of'; roles: string[] } | undefined {
  if (req == null) return undefined;
  if (typeof req === 'string') return { kind: 'role', roles: [req] };
  const kind = String(req.kind) as 'role' | 'any_of' | 'all_of';
  if (kind !== 'role' && kind !== 'any_of' && kind !== 'all_of') {
    throw new TypeError(`unknown capability kind ${JSON.stringify(req.kind)}`);
  }
  return { kind, roles: req.roles.map(String) };
}

function hex(data: Uint8Array | undefined): string {
  if (!data) return ZERO_HASH;
  let out = '';
  for (const b of data) out += b.toString(16).padStart(2, '0');
  return out;
}

function display(tp: unknown): string | undefined {
  if (typeof tp !== 'function') return undefined;
  const tag = (tp as any)[WIRE_TYPE_KEY];
  if (typeof tag === 'string' && tag) return tag;
  return (tp as { name?: string }).name || undefined;
}

function lowerMethod(name: string, mi: MethodInfo): DynamicMethodSpec {
  const pattern = (PATTERNS.has(mi.pattern) ? mi.pattern : 'unary') as DynamicMethodSpec['pattern'];
  const m: DynamicMethodSpec = {
    name: name.normalize('NFC'),
    pattern,
    request: { hash: hex(mi.requestTypeHash) },
    response: { hash: hex(mi.responseTypeHash) },
  };
  const reqDisplay = display(mi.requestType);
  const respDisplay = display(mi.responseType);
  if (reqDisplay) m.request.display = reqDisplay;
  if (respDisplay) m.response.display = respDisplay;
  if (mi.idempotent) m.idempotent = true;
  if (mi.timeout && mi.timeout > 0) m.default_timeout = Number(mi.timeout);
  const req = normalizeRequirement(mi.requires as CapabilityRequirement | string | undefined);
  if (req) m.requires = req;
  return m;
}

/** A `@Service` class, an instance of one, or its {@link ServiceInfo}. */
export type ServiceLike = ServiceInfo | object | (new (...args: any[]) => any);

function infoOf(service: ServiceLike): ServiceInfo {
  if ((service as ServiceInfo).methods instanceof Map && typeof (service as ServiceInfo).name === 'string') {
    return service as ServiceInfo;
  }
  const info = getServiceInfo(service);
  if (!info) throw new TypeError('not a @Service class, instance or ServiceInfo');
  return info;
}

/**
 * Lower a service to its `DynamicService` JSON object (design §1.5).
 * `manifestJson` passes a binding manifest through verbatim (§1.3).
 */
export function lowerService(service: ServiceLike, opts?: { manifestJson?: string }): DynamicServiceSpec {
  const info = infoOf(service);
  const modes = canonicalSerializationModes(info.serializationModes as readonly (number | string)[]);
  const out: DynamicServiceSpec = {
    name: info.name.normalize('NFC'),
    version: info.version,
    scope: info.scoped === 'session' || (info.scoped as string) === 'stream' ? 'session' : 'shared',
    serialization_modes: modes,
    methods: [],
  };
  if (modes.includes('native')) out.producer_language = 'typescript';
  const req = normalizeRequirement(info.requires);
  if (req) out.requires = req;
  for (const [name, mi] of info.methods) out.methods.push(lowerMethod(name, mi));
  if (opts?.manifestJson !== undefined) out.manifest_json = opts.manifestJson;
  return out;
}

/** {@link lowerService}, serialized. */
export function lowerServiceJson(service: ServiceLike, opts?: { manifestJson?: string }): string {
  return JSON.stringify(lowerService(service, opts));
}

/**
 * The contract id Rust derives from the lowered service (it validates the
 * registration too). Equals `contractIdFromContract(fromServiceInfo(info))`.
 */
export function dynamicContractId(service: ServiceLike | DynamicServiceSpec): string {
  const spec =
    Array.isArray((service as DynamicServiceSpec).methods)
      ? (service as DynamicServiceSpec)
      : lowerService(service as ServiceLike);
  return loadNative().dynamicServiceContractId(JSON.stringify(spec));
}
