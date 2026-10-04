/**
 * Gate 0 — connection-level peer admission, decided in Rust.
 *
 * {@link GatePolicy} wraps the Rust `GatePolicy`: the admission ALPNs
 * (`aster.consumer_admission`, `aster.admission`, `aster.producer_admission`)
 * are open to every peer so it can present a credential; any other ALPN
 * needs a peer admitted into the gate's {@link PeerAdmissionStore} (or
 * `'open_dev'` mode). Rejected peers are closed with the gate's reject code
 * and reason. Attach a gate with `Node.start(config, { gate })` or
 * `node.setGate(gate)`; no JavaScript runs per connection.
 *
 * To decide in JavaScript instead, start a node with `enableHooks: true` and
 * no gate, and answer `node.admission()`:
 *
 * ```ts
 * for await (const req of node.admission()) {
 *   if (allowed(req.peer, req.alpn)) req.accept();
 *   else req.reject(403, 'not on the list');
 * }
 * ```
 */

import type {
  AdmissionRequest as NativeAdmissionRequest,
  GatePolicy as NativeGatePolicy,
  PeerAdmission as NativePeerAdmission,
  PeerAdmissionStore as NativePeerAdmissionStore,
} from '@aster-rpc/transport';
import type { AsterConfig } from './config.js';
import { loadNative } from './native.js';

/**
 * Gate-0 trust posture. `'protected'`: un-admitted peers may only use the
 * admission ALPNs. `'open_dev'`: anyone may use any ALPN (local / dev only).
 * @group Trust
 */
export type TrustMode = 'protected' | 'open_dev';

/** Where an admission came from. @group Trust */
export type AdmissionSource =
  | 'manual'
  | 'credential'
  | 'attestation'
  | 'root_policy_doc'
  | 'workload_identity'
  | (string & {});

/**
 * An admission request raised by a node with hooks and no gate. Answer once
 * with `accept()` or `reject(code, reason)`.
 * @group Trust
 */
export type AdmissionRequest = NativeAdmissionRequest;

/** The Gate-0 decision for a `(peer, alpn)` pair. @group Trust */
export interface GateDecision {
  allow: boolean;
  /** QUIC close code on reject. */
  errorCode?: number;
  /** Close reason on reject. */
  reason?: string;
}

/** Options for {@link PeerAdmissionStore.admit}. @group Trust */
export interface AdmitOptions {
  /** Attributes later RPC / capability checks read. */
  attributes?: Record<string, string> | Map<string, string>;
  /** Absolute expiry (a `Date` or ms since the Unix epoch). */
  expiresAt?: Date | number;
  /** Expiry relative to now, in ms (the earlier expiry wins). */
  ttlMs?: number;
  /** Root / owner id responsible for the peer. */
  ownerId?: string;
  /** Default `'manual'`. */
  source?: AdmissionSource;
}

/** A stored, unexpired admission. @group Trust */
export interface PeerAdmission {
  peer: string;
  ownerId?: string;
  attributes: Record<string, string>;
  admittedAt: Date;
  expiresAt?: Date;
  source: AdmissionSource;
}

function nativeAdmitOptions(o: AdmitOptions | undefined) {
  if (!o) return undefined;
  const attributes = o.attributes instanceof Map ? Object.fromEntries(o.attributes) : o.attributes;
  const expiresAtMs = o.expiresAt instanceof Date ? o.expiresAt.getTime() : o.expiresAt;
  return {
    attributes,
    expiresAtMs,
    ttlMs: o.ttlMs,
    ownerId: o.ownerId,
    source: o.source,
  };
}

function fromNativeAdmission(a: NativePeerAdmission): PeerAdmission {
  return {
    peer: a.peer,
    ...(a.ownerId != null ? { ownerId: a.ownerId } : {}),
    attributes: { ...a.attributes },
    admittedAt: new Date(a.admittedAtMs),
    ...(a.expiresAtMs != null ? { expiresAt: new Date(a.expiresAtMs) } : {}),
    source: a.source,
  };
}

/**
 * Admitted peers (the Rust `PeerAdmissionStore`), with attributes and
 * expiry. A gate's store (`gate.store`) is the one Gate 0 consults:
 * admitting a peer there lets it past the gate.
 *
 * Peer ids are 64-hex node ids.
 * @group Trust
 */
export class PeerAdmissionStore {
  /** @internal */
  readonly native: NativePeerAdmissionStore;

  constructor(native?: NativePeerAdmissionStore) {
    this.native = native ?? new (loadNative().PeerAdmissionStore)();
  }

  /** Admit `peer`, replacing any previous record; `true` if one was replaced. */
  admit(peer: string, options?: AdmitOptions): boolean {
    return this.native.admit(peer, nativeAdmitOptions(options));
  }

  /** Revoke `peer`; `true` if a record was removed. New connections are gated. */
  revoke(peer: string): boolean {
    return this.native.revoke(peer);
  }

  /** Whether `peer` holds an unexpired admission. */
  isAdmitted(peer: string): boolean {
    return this.native.isAdmitted(peer);
  }

  /** The unexpired admission of `peer`, if any. */
  get(peer: string): PeerAdmission | undefined {
    const a = this.native.get(peer);
    return a ? fromNativeAdmission(a) : undefined;
  }

  /** Attributes of an unexpired admission (empty when not admitted). */
  attributes(peer: string): Record<string, string> {
    return { ...this.native.attributes(peer) };
  }

  /** Raw record count, including expired records not yet pruned. */
  get size(): number {
    return this.native.size;
  }

  /** Remove expired records; returns how many were removed. */
  pruneExpired(): number {
    return this.native.pruneExpired();
  }
}

/** Options for `new GatePolicy(...)`. @group Trust */
export interface GatePolicyOptions {
  /** Default `'protected'`. */
  mode?: TrustMode;
  /** Share an existing store (default: a fresh one). */
  store?: PeerAdmissionStore;
  /** ALPNs open to un-admitted peers (default: the three admission ALPNs). */
  admissionAlpns?: Array<string | Uint8Array>;
  /** Close code for rejected peers (default 403). */
  rejectCode?: number;
  /** Close reason for rejected peers (default `"peer not admitted"`). */
  rejectReason?: string;
}

/**
 * Gate-0 policy (the Rust `GatePolicy`). Changes apply to the next
 * admission decision of every node it is attached to.
 * @group Trust
 */
export class GatePolicy {
  /** @internal */
  readonly native: NativeGatePolicy;

  constructor(options: GatePolicyOptions = {}) {
    const { store, ...rest } = options;
    this.native = new (loadNative().GatePolicy)(rest, store?.native);
  }

  /** A protected gate (the default). */
  static protected(options: Omit<GatePolicyOptions, 'mode'> = {}): GatePolicy {
    return new GatePolicy({ ...options, mode: 'protected' });
  }

  /** An open (dev) gate: every peer passes. */
  static openDev(options: Omit<GatePolicyOptions, 'mode'> = {}): GatePolicy {
    return new GatePolicy({ ...options, mode: 'open_dev' });
  }

  /**
   * The gate an Aster server runs from its trust settings:
   * `allowAllConsumers` → `'open_dev'`, otherwise `'protected'`.
   */
  static fromConfig(config: Pick<AsterConfig, 'allowAllConsumers'>, options: Omit<GatePolicyOptions, 'mode'> = {}): GatePolicy {
    return new GatePolicy({ ...options, mode: config.allowAllConsumers ? 'open_dev' : 'protected' });
  }

  get mode(): TrustMode {
    return this.native.mode;
  }

  set mode(mode: TrustMode) {
    this.native.mode = mode;
  }

  /** The admitted-peer store this gate consults (shared, not a copy). */
  get store(): PeerAdmissionStore {
    return new PeerAdmissionStore(this.native.store);
  }

  /** ALPNs open to un-admitted peers. */
  get admissionAlpns(): string[] {
    return this.native.admissionAlpns;
  }

  set admissionAlpns(alpns: Array<string | Uint8Array>) {
    this.native.admissionAlpns = alpns;
  }

  /** Set the close code and reason sent to rejected peers. */
  setReject(code: number, reason: string): void {
    this.native.setReject(code, reason);
  }

  /** Admit `peer` into this gate's store. */
  admit(peer: string, options?: AdmitOptions): boolean {
    return this.native.admit(peer, nativeAdmitOptions(options));
  }

  /** Revoke `peer`; new connections from it are gated. */
  revoke(peer: string): boolean {
    return this.native.revoke(peer);
  }

  /** Whether `peer` holds an unexpired admission. */
  isAdmitted(peer: string): boolean {
    return this.native.isAdmitted(peer);
  }

  /** Evaluate the gate for `(peer, alpn)` without connecting. */
  shouldAllow(peer: string, alpn: string | Uint8Array): GateDecision {
    const d = this.native.shouldAllow(peer, alpn);
    return {
      allow: d.allow,
      ...(d.errorCode != null ? { errorCode: d.errorCode } : {}),
      ...(d.reason != null ? { reason: d.reason } : {}),
    };
  }
}
