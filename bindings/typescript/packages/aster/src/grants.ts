/**
 * Sealed grants — per-recipient capability distribution over public docs
 * (Rust `aster::grants`).
 *
 * A root writes, in a publicly replicated policy doc, one record per
 * recipient at `<prefix>/grants/<nodeId>`: a secret (usually a
 * {@link NamespaceCapability}) HPKE-sealed to that node's identity-derived
 * key, with a {@link GrantContext} bound into the AEAD associated data so it
 * cannot be replayed at another node, resource, role or path. Recipients
 * read through a {@link PolicyDoc} pinned to the root author (HPKE Base has
 * no sender authenticity) and decide with {@link reconcileNamespaceGrant}:
 *
 * ```ts
 * // root
 * const ctx = { app: 'myapp/grant/v1', granter: root.id, recipient, resource: nsId,
 *               path: grantKey('/myapp/v1', recipient), role: 'write' };
 * const record = GrantRecord.active('write', 1, sealNamespaceGrant(ctx, capability));
 * await doc.setBytes(authorId, ctx.path, record.encode());
 *
 * // recipient
 * const policy = await PolicyDoc.importReadOnly(node.docs(), nsId, rootId);
 * const outcome = reconcileNamespaceGrant(node.exportSecretKey(), ctx,
 *   await policy.fetchGrant('/myapp/v1', node.id));
 * switch (outcome.kind) { case 'active': ...; case 'revoked': case 'unopenable': ...; case 'absent': ... }
 * ```
 *
 * Revoke by writing {@link GrantRecord.revoked} — never by deleting the key
 * (an absent key reads the same as "not synced yet").
 *
 * The AAD layout, seal/open, the role rule, reconcile and the grant key all
 * run in Rust (vectors: `conformance/vectors/grants.json`).
 */

import type {
  DocEntry,
  DocEvent,
  DocEventReceiver,
  DocHandle,
  DocsClient,
  GrantContext as NativeGrantContext,
  PolicyDoc as NativePolicyDoc,
} from '@aster-rpc/transport';
import type { KeyMaterial } from './config.js';
import { loadNative } from './native.js';
import type { NodeId } from './node.js';
import { NamespaceCapability } from './trust/config-primitives.js';

/** Domain label baked into every grant AAD. @group Grants */
export const GRANT_AAD_DOMAIN = 'aster/sealed-grant/v1' as const;
/** Grant record schema version. @group Grants */
export const GRANT_RECORD_VERSION = 1 as const;

/**
 * Everything a sealed grant is bound to. Sealing and opening must use an
 * identical context — any mismatch makes opening fail.
 * @group Grants
 */
export interface GrantContext {
  /** Versioned application label, e.g. `"portal-sync/tree-grant/v1"`. */
  app: string;
  /** The sealing authority (root) node id, bound into the AAD verbatim. */
  granter: NodeId | string;
  /** The one node this grant is addressed to, bound verbatim. */
  recipient: NodeId | string;
  /** What the capability unlocks — caller-chosen opaque bytes (e.g. the namespace id). */
  resource: Uint8Array;
  /** The exact doc key the record is written at. */
  path: string;
  /** Grant role, e.g. `'read'` / `'write'` (free-form for other schemes). */
  role: string;
}

function nativeCtx(ctx: GrantContext): NativeGrantContext {
  return {
    app: ctx.app,
    granter: ctx.granter,
    recipient: ctx.recipient,
    resource: ctx.resource,
    path: ctx.path,
    role: ctx.role,
  };
}

const u8 = (b: Uint8Array): Uint8Array => Uint8Array.from(b);

/** The canonical AAD for `ctx` (length-prefixed fields). @group Grants */
export function grantAad(ctx: GrantContext): Uint8Array {
  return u8(loadNative().grantAad(nativeCtx(ctx)));
}

/**
 * Seal `plaintext` to `ctx.recipient`'s identity-derived HPKE key. Returns
 * the Fory-encoded envelope to put in a {@link GrantRecord}.
 * @group Grants
 */
export function sealGrant(ctx: GrantContext, plaintext: Uint8Array): Uint8Array {
  return u8(loadNative().sealGrant(nativeCtx(ctx), plaintext));
}

/** Open a {@link sealGrant} envelope with the recipient's identity secret. @group Grants */
export function openGrant(identitySecret: KeyMaterial, ctx: GrantContext, envelope: Uint8Array): Uint8Array {
  return u8(loadNative().openGrant(identitySecret, nativeCtx(ctx), envelope));
}

/**
 * Seal to an explicit, published X25519 public key (the rotation escape
 * hatch). Keep the two schemes explicit.
 * @group Grants
 */
export function sealGrantToKey(recipientPublicKey: Uint8Array, ctx: GrantContext, plaintext: Uint8Array): Uint8Array {
  return u8(loadNative().sealGrantToKey(recipientPublicKey, nativeCtx(ctx), plaintext));
}

/** Open a {@link sealGrantToKey} envelope with an explicit X25519 private key. @group Grants */
export function openGrantWithKey(recipientPrivateKey: Uint8Array, ctx: GrantContext, envelope: Uint8Array): Uint8Array {
  return u8(loadNative().openGrantWithKey(recipientPrivateKey, nativeCtx(ctx), envelope));
}

/** Seal a namespace capability; checks the role rule first. @group Grants */
export function sealNamespaceGrant(ctx: GrantContext, capability: NamespaceCapability): Uint8Array {
  return u8(loadNative().sealNamespaceGrant(nativeCtx(ctx), capability.encodeFory()));
}

/** Open a namespace-capability grant, re-checking the role rule. @group Grants */
export function openNamespaceGrant(
  identitySecret: KeyMaterial,
  ctx: GrantContext,
  envelope: Uint8Array,
): NamespaceCapability {
  return NamespaceCapability.decodeFory(loadNative().openNamespaceGrant(identitySecret, nativeCtx(ctx), envelope));
}

/**
 * The role ↔ capability rule: `'read'` needs a read capability, `'write'` a
 * write one; other roles carry no implication. Throws on a mismatch.
 * @group Grants
 */
export function checkGrantRole(role: string, canWrite: boolean): void {
  loadNative().checkGrantRole(role, canWrite);
}

/**
 * The conventional grant doc key, `<prefix>/grants/<recipient>` — an
 * address, not key material.
 * @group Grants
 */
export function grantKey(prefix: string, recipient: NodeId | string): string {
  return loadNative().grantKey(prefix, recipient);
}

/**
 * What a reconcile loop should do with the record (or absence of one) at a
 * grant key. `revoked` and `unopenable` both drop the capability; `absent`
 * is **not** revocation — keep waiting and keep what you hold.
 * @group Grants
 */
export type GrantOutcome<T> =
  | { kind: 'active'; value: T; dropsCapability: false }
  | { kind: 'revoked'; reason: string; role: string; dropsCapability: true }
  | { kind: 'unopenable'; error: Error; dropsCapability: true }
  | { kind: 'absent'; dropsCapability: false };

function outcome<T>(
  identitySecret: KeyMaterial,
  ctx: GrantContext,
  record: Uint8Array | null | undefined,
  payload: 'raw' | 'namespace',
  value: (o: { plaintext?: Uint8Array; capability?: Uint8Array }) => T,
): GrantOutcome<T> {
  const o = loadNative().reconcileGrant(identitySecret, nativeCtx(ctx), record ?? null, payload);
  switch (o.kind) {
    case 'active':
      return { kind: 'active', value: value(o), dropsCapability: false };
    case 'revoked':
      return { kind: 'revoked', reason: o.reason ?? '', role: o.role ?? '', dropsCapability: true };
    case 'unopenable':
      return { kind: 'unopenable', error: new Error(o.error ?? 'grant unopenable'), dropsCapability: true };
    default:
      return { kind: 'absent', dropsCapability: false };
  }
}

/**
 * Decide on the record at `ctx.path` (`null`/`undefined` = no record),
 * opening the raw payload when active. With more than one AAD version,
 * decode the record first ({@link GrantRecord.decode}), pick the context
 * from `contextVersion`, then pass the same bytes — never trial-decrypt.
 * @group Grants
 */
export function reconcileGrant(
  identitySecret: KeyMaterial,
  ctx: GrantContext,
  record: Uint8Array | null | undefined,
): GrantOutcome<Uint8Array> {
  return outcome(identitySecret, ctx, record, 'raw', (o) => u8(o.plaintext!));
}

/** {@link reconcileGrant} for a namespace-capability payload. @group Grants */
export function reconcileNamespaceGrant(
  identitySecret: KeyMaterial,
  ctx: GrantContext,
  record: Uint8Array | null | undefined,
): GrantOutcome<NamespaceCapability> {
  return outcome(identitySecret, ctx, record, 'namespace', (o) => NamespaceCapability.decodeFory(o.capability!));
}

/** Grant record lifecycle state. @group Grants */
export type GrantState = 'active' | 'revoked';

/**
 * The plaintext framing a sealed grant is published in: explicit state
 * (revocation is a value, never a deleted key) and the context version.
 * @group Grants
 */
export class GrantRecord {
  private constructor(
    readonly state: GrantState,
    /** Which context template the envelope was sealed under. */
    readonly contextVersion: number,
    readonly role: string,
    private readonly _envelope: Uint8Array,
    /** Revocation reason (empty when active). */
    readonly reason: string,
  ) {}

  /** An active record carrying `envelope` (from {@link sealGrant} / {@link sealNamespaceGrant}). */
  static active(role: string, contextVersion: number, envelope: Uint8Array): GrantRecord {
    if (envelope.byteLength === 0) throw new Error('active grant record must carry an envelope');
    return new GrantRecord('active', contextVersion, role, u8(envelope), '');
  }

  /** A revocation record — write this at the grant key instead of deleting it. */
  static revoked(role: string, reason = ''): GrantRecord {
    return new GrantRecord('revoked', GRANT_RECORD_VERSION, role, new Uint8Array(0), reason);
  }

  /** Decode record bytes (validates version and the state/envelope rules). */
  static decode(bytes: Uint8Array): GrantRecord {
    const r = loadNative().grantRecordDecode(bytes);
    return new GrantRecord(r.state, r.contextVersion, r.role, u8(r.envelope), r.reason);
  }

  get isActive(): boolean {
    return this.state === 'active';
  }

  /** The sealed envelope (empty when revoked). */
  get envelope(): Uint8Array {
    return u8(this._envelope);
  }

  /** Fory XLANG bytes. */
  encode(): Uint8Array {
    return u8(
      loadNative().grantRecordEncode({
        state: this.state,
        contextVersion: this.contextVersion,
        role: this.role,
        envelope: this._envelope,
        reason: this.reason,
      }),
    );
  }

  toString(): string {
    return `GrantRecord(${this.state}, role=${this.role}, v${this.contextVersion}, envelope=${this._envelope.byteLength} bytes)`;
  }
}

/**
 * A replicated doc read through a **pinned author** (the root's node id —
 * Aster's docs author is the node identity). Records by any other author are
 * invisible, which is what makes sealed grants trustworthy.
 * @group Grants
 */
export class PolicyDoc {
  private constructor(
    /** @internal */
    readonly native: NativePolicyDoc,
  ) {}

  /** Import `namespace` read-only (idempotent) and pin `author`. */
  static async importReadOnly(
    docs: DocsClient,
    namespace: string | Uint8Array,
    author: NodeId | string,
  ): Promise<PolicyDoc> {
    return new PolicyDoc(await loadNative().PolicyDoc.importReadOnly(docs, namespace, author));
  }

  /** Wrap an already-opened doc (e.g. joined from a ticket) with a pinned author. */
  static wrap(doc: DocHandle, author: NodeId | string): PolicyDoc {
    return new PolicyDoc(loadNative().PolicyDoc.wrap(doc, author));
  }

  /** The pinned author. */
  get author(): string {
    return this.native.author;
  }

  /** The underlying doc (e.g. to start sync or share). */
  doc(): DocHandle {
    return this.native.doc();
  }

  /** The pinned author's record at `key`, or `undefined`. */
  async get(key: string | Uint8Array): Promise<Uint8Array | undefined> {
    return (await this.native.get(key)) ?? undefined;
  }

  /** The pinned author's entries under `prefix`. */
  listPrefix(prefix: string | Uint8Array): Promise<DocEntry[]> {
    return this.native.listPrefix(prefix);
  }

  /** Live doc events; keep only {@link isPolicyWrite} ones. */
  subscribe(): Promise<DocEventReceiver> {
    return this.native.subscribe();
  }

  /** Whether `event` is an insert by the pinned author. */
  isPolicyWrite(event: DocEvent): boolean {
    return this.native.isPolicyWrite(event);
  }

  /** The raw grant record for `recipient` at `<prefix>/grants/<recipient>`, or `undefined`. */
  async fetchGrant(prefix: string, recipient: NodeId | string): Promise<Uint8Array | undefined> {
    return (await this.native.fetchGrant(prefix, recipient)) ?? undefined;
  }
}
