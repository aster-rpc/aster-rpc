/**
 * Gateway identity: calling Aster services on behalf of a user
 * (`docs/_internal/web_gateway/gateway-identity.md`). Everything runs in Rust
 * on the config-driven path; this module is the TypeScript surface:
 *
 * - server: `config.identity` / `config.remoteConfig`, and in handlers
 *   `ctx.actor`, `ctx.authMethods`, `ctx.requireAuthMethod(..)`,
 *   `ctx.onBehalfOf`, `ctx.nodeProperties`, `ctx.nodeAttribute(..)`;
 * - edge: a call's `onBehalfOf` option, `AsterClientWrapper.mintUserToken`,
 *   {@link bearerToken}, the publisher notices;
 * - admins: {@link NodeConfigClient} (`aster.ops.NodeConfig`).
 *
 * @module
 */

import type { IrohConnection } from '@aster-rpc/transport';
import { loadNative } from './native.js';
import { mapTransportError } from './transport/iroh.js';
import type { NodeAttribute } from './interceptors/base.js';
import type { TrustedIssuer } from './config.js';

/** The call metadata entry carrying a user's JWT (the `onBehalfOf` call option). @group Gateway identity */
export const ON_BEHALF_OF_METADATA = 'aster-on-behalf-of';
/** The role that lets a node forward users' identities. @group Gateway identity */
export const GATEWAY_ROLE = 'aster.gateway';
/** The role `aster.ops.NodeConfig` requires. @group Gateway identity */
export const CONFIG_ADMIN_ROLE = 'aster.config-admin';
/** The role a node needs to publish issuer keys into the fleet namespace. @group Gateway identity */
export const IDENTITY_PUBLISHER_ROLE = 'aster.identity-publisher';
/** The credential attribute naming the roles a gateway may put in tokens it mints. @group Gateway identity */
export const ISSUABLE_ROLES_ATTRIBUTE = 'aster.identity.issuable_roles';
/** `amr`: a passkey (WebAuthn) sign-in. @group Gateway identity */
export const AMR_WEBAUTHN = 'webauthn';
/** `amr`: a password sign-in. @group Gateway identity */
export const AMR_PASSWORD = 'pwd';
/** `amr`: an API key. @group Gateway identity */
export const AMR_API_KEY = 'apikey';

/**
 * The token of an `Authorization: Bearer <token>` header value (Rust
 * `rpc::bearer_token`); `undefined` for any other scheme or an empty token.
 * @group Gateway identity
 */
export function bearerToken(authorization: string): string | undefined {
  return loadNative().bearerToken(authorization) ?? undefined;
}

/**
 * An override of `[identity]` set through `aster.ops.NodeConfig`: only the
 * fields given override; `trustedIssuers` replaces the whole list. The
 * section that results is validated as a whole before anything is stored.
 * @group Gateway identity
 */
export interface IdentityOverride {
  acceptOnBehalfOf?: boolean;
  trustedIssuers?: TrustedIssuer[];
  edgeTokenMaxLifetimeSecs?: number;
  clockLeewaySecs?: number;
  maxKeySetAgeSecs?: number;
}

/**
 * `aster.ops.NodeConfig` `set`: replaces the node's whole record.
 * @group Gateway identity
 */
export interface NodeConfigSetRequest {
  /** `0` / unset: unconditional; otherwise refused with `ABORTED` unless the current generation equals it. */
  expectedGeneration?: number;
  name?: string;
  tags?: string[];
  /** Typed attributes; keys may not start with `aster.`. They never authorise. */
  attributes?: Record<string, NodeAttribute>;
  /** The `[identity]` override. */
  identity?: IdentityOverride;
  /** Other sections by name, as their JSON objects (a section that is not overridable is refused). */
  sections?: Record<string, Record<string, unknown>>;
}

/** One overridable setting's effective value and where it came from. @group Gateway identity */
export interface EffectiveSetting {
  /** `section.field`, e.g. `identity.accept_on_behalf_of`. */
  key: string;
  value: unknown;
  /** `default`, `file`, `env`, `code` or `override`. */
  source: string;
}

/** `aster.ops.NodeConfig` `get`. @group Gateway identity */
export interface NodeConfigView {
  nodeId: string;
  /** The stored record's generation; `0` when there has never been one. */
  generation: number;
  appliedGeneration: number;
  /** `set`, `clear`, or empty. */
  change: string;
  changedBy: string;
  changedAtMs: number;
  name: string;
  tags: string[];
  attributes: Record<string, NodeAttribute>;
  /** The section overrides in force, by section. */
  sections: Record<string, unknown>;
  ignoredSections: string[];
  settings: EffectiveSetting[];
  overridableSections: string[];
  lockedSections: string[];
}

/** A part of a record not (yet) in effect. @group Gateway identity */
export interface NodeConfigOutstanding {
  /** `record`, `properties` or a section name. */
  part: string;
  /** `pending`, `superseded`, `not-applied` or `not-in-effect`. */
  state: string;
  detail: string;
}

/** The answer of `set`, `clear` and `ack`: `complete` once the generation is stored and in effect. @group Gateway identity */
export interface NodeConfigAck {
  complete: boolean;
  generation: number;
  appliedGeneration: number;
  outstanding: NodeConfigOutstanding[];
}

/** Per-call options of {@link NodeConfigClient}. @group Gateway identity */
export interface NodeConfigCallOptions {
  /** Call deadline in milliseconds (0 / unset: none). */
  deadlineMs?: number;
}

function identityJson(o: IdentityOverride): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  if (o.acceptOnBehalfOf !== undefined) out.accept_on_behalf_of = o.acceptOnBehalfOf;
  if (o.trustedIssuers !== undefined) {
    out.trusted_issuers = o.trustedIssuers.map((t) => ({
      iss: t.iss,
      audiences: t.audiences,
      ...(t.rolesClaim !== undefined ? { roles_claim: t.rolesClaim } : {}),
      ...(t.roleMap !== undefined ? { role_map: t.roleMap } : {}),
      ...(t.maxTokenLifetimeSecs !== undefined ? { max_token_lifetime_secs: Number(t.maxTokenLifetimeSecs) } : {}),
    }));
  }
  if (o.edgeTokenMaxLifetimeSecs !== undefined) out.edge_token_max_lifetime_secs = o.edgeTokenMaxLifetimeSecs;
  if (o.clockLeewaySecs !== undefined) out.clock_leeway_secs = o.clockLeewaySecs;
  if (o.maxKeySetAgeSecs !== undefined) out.max_key_set_age_secs = o.maxKeySetAgeSecs;
  return out;
}

function ackOf(j: any): NodeConfigAck {
  return {
    complete: j.complete,
    generation: j.generation,
    appliedGeneration: j.applied_generation,
    outstanding: j.outstanding ?? [],
  };
}

function viewOf(j: any): NodeConfigView {
  return {
    nodeId: j.node_id,
    generation: j.generation,
    appliedGeneration: j.applied_generation,
    change: j.change,
    changedBy: j.changed_by,
    changedAtMs: j.changed_at_ms,
    name: j.name,
    tags: j.tags,
    attributes: j.attributes,
    sections: j.sections,
    ignoredSections: j.ignored_sections,
    settings: j.settings,
    overridableSections: j.overridable_sections,
    lockedSections: j.locked_sections,
  };
}

/**
 * The `aster.ops.NodeConfig` admin client (Rust `NodeConfigClient`): reads
 * and changes a server's remote overrides over an `aster/1` connection. The
 * calling node needs `aster.config-admin` from its own Enrollment
 * credential; Gate 3 checks it on every method. Get one from
 * `AsterClientWrapper.nodeConfig()`.
 *
 * `set`, `clear` and `ack` answer only once the change is in effect.
 * @group Gateway identity
 */
export class NodeConfigClient {
  constructor(private readonly connection: IrohConnection) {}

  private async call(method: string, request: Record<string, unknown>, opts?: NodeConfigCallOptions): Promise<any> {
    const body = { ...request, ...(opts?.deadlineMs ? { deadline_ms: opts.deadlineMs } : {}) };
    try {
      return JSON.parse(await loadNative().nodeConfigCall(this.connection, method, JSON.stringify(body)));
    } catch (e) {
      throw mapTransportError(e);
    }
  }

  /** The record and every overridable setting's effective value and source. */
  async get(opts?: NodeConfigCallOptions): Promise<NodeConfigView> {
    return viewOf(await this.call('get', {}, opts));
  }

  /** Replace the record; answers once the new generation is in effect. */
  async set(request: NodeConfigSetRequest, opts?: NodeConfigCallOptions): Promise<NodeConfigAck> {
    const sections: Record<string, unknown> = { ...request.sections };
    if (request.identity) sections.identity = identityJson(request.identity);
    return ackOf(await this.call('set', {
      expected_generation: request.expectedGeneration ?? 0,
      name: request.name ?? '',
      tags: request.tags ?? [],
      attributes: request.attributes ?? {},
      sections,
    }, opts));
  }

  /** Remove every override and property, reverting the node to its own configuration. */
  async clear(expectedGeneration = 0, opts?: NodeConfigCallOptions): Promise<NodeConfigAck> {
    return ackOf(await this.call('clear', { expected_generation: expectedGeneration }, opts));
  }

  /** Is `generation` in effect? Waits up to `timeoutMs` (0: 10 s; at most 60 s). */
  async ack(generation: number, timeoutMs = 0, opts?: NodeConfigCallOptions): Promise<NodeConfigAck> {
    return ackOf(await this.call('ack', { generation, timeout_ms: timeoutMs }, opts));
  }
}
