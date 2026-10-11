/**
 * R6 upcall payloads (UCv1) through the TypeScript binding's codec —
 * conformance/vectors/upcalls.json, also checked by Rust (`aster/tests/
 * upcall_vectors.rs`). The binding decodes every request into the object a
 * JavaScript handler receives and encodes every handler answer with the
 * same `upcallCodecDecode` / `upcallCodecEncode` the runtime uses, so a
 * pass here means JS handlers see and produce exactly Rust's bytes.
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import * as path from 'node:path';
import { UPCALL_KINDS, upcallCodecDecode, upcallCodecEncode } from '@aster-rpc/aster';
import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

interface Vectors {
  kinds: Array<{ code: number; name: string; decision: boolean }>;
  statuses: Array<{ code: number; name: string }>;
  cancel_reasons: Array<{ code: number; name: string }>;
  valid: Array<{ name: string; type: string; hex: string; value: unknown }>;
  invalid: Array<{ name: string; type: string; hex: string }>;
}

const VECTORS: Vectors = JSON.parse(
  readFileSync(path.resolve(__dirname, '../../../conformance/vectors/upcalls.json'), 'utf8'),
);

type Obj = Record<string, any>;
const hex = (b: Uint8Array | Buffer) => Buffer.from(b).toString('hex');
const nul = <T>(v: T | undefined | null): T | null => (v === undefined ? null : v);

function opJson(op: Obj): unknown {
  switch (op.kind) {
    case 'acquire':
      return { acquire: { candidate: op.actor, ttl_ms: op.ttlMs } };
    case 'renew':
    case 'release':
      return { [op.kind]: { holder: op.actor, epoch: op.epoch } };
    default:
      return { revoke: { authority: op.actor } };
  }
}

function snapJson(s: Obj): unknown {
  return {
    resource: s.resource,
    epoch: s.epoch,
    holder: nul(s.holder),
    held: s.held,
    remaining_ms: nul(s.remainingMs),
  };
}

function seedJson(s: Obj | null | undefined): unknown {
  if (!s) return null;
  return {
    node_id: s.nodeId,
    ticket: s.ticket,
    signed_join_permit: s.signedJoinPermit ? hex(s.signedJoinPermit) : null,
  };
}

const HTTP_VERSIONS = ['HTTP/0.9', 'HTTP/1.0', 'HTTP/1.1', 'HTTP/2', 'HTTP/3'];

/** Header values render as `value` (UTF-8) or `value_hex`. */
function headersJson(headers: Obj[]): unknown {
  return headers.map((h) => {
    const v = Buffer.from(h.value);
    const text = new TextDecoder('utf-8', { fatal: true });
    try {
      return { name: h.name, value: text.decode(v) };
    } catch {
      return { name: h.name, value_hex: hex(v) };
    }
  });
}

/** `body` absent = streamed through the exchange's body handle. */
function bodyJson(body: Buffer | undefined | null): unknown {
  return body == null ? 'stream' : { inline: Buffer.from(body).toString('utf8') };
}

/** The binding's native object form → the vectors' JSON rendering. */
function toVectorJson(type: string, v: Obj): unknown {
  switch (type) {
    case 'LeaseTransitionRequest':
      return { resource: v.resource, op: opJson(v.op) };
    case 'TransitionOutcome':
      switch (v.kind) {
        case 'granted':
          return { granted: { resource: v.fence.resource, epoch: v.fence.epoch, holder: v.fence.holder } };
        case 'denied':
          return { denied: { reason: v.denyReason, snapshot: snapJson(v.snapshot) } };
        default:
          return v.kind;
      }
    case 'LeaseSnapshotRequest':
      return { resource: v.resource };
    case 'LeaseSnapshot':
      return snapJson(v);
    case 'LeaseAuthorizeRequest':
      return { op: opJson(v.op) };
    case 'FencedMutateRequest':
      return {
        fence: { resource: v.fence.resource, epoch: v.fence.epoch, holder: v.fence.holder },
        writer: v.writer,
        op_hex: hex(v.op),
      };
    case 'FencedMutateResponse':
      return v.kind === 'applied'
        ? { applied: { output_hex: hex(v.output) } }
        : { rejected: { reason: v.kind, carried: v.carried, current: v.current } };
    case 'Decision':
      return v.allowed;
    case 'TopologyAdmittedRequest':
      return { node_ids: v.nodeIds };
    case 'TopologyAdmittedResponse':
      return { admitted: v.admitted };
    case 'TopologySnapshotRequest':
    case 'PlacementPolicyRequest':
      return {};
    case 'TopologySnapshot':
      return {
        clusters: v.clusters.map((c: Obj) => ({ bridge: c.bridge, members: c.members, witnesses: c.witnesses })),
        far_pairs: v.farPairs,
        separation_coverage: v.separationCoverage,
      };
    case 'PlacementPolicy':
      return {
        allow_healthy_rebalance: v.allowHealthyRebalance,
        authority_domain: v.authorityDomain,
        authority_public_key: v.authorityPublicKey,
        digest: v.digest,
        eligible: v.eligible.map((c: Obj) => ({ member_seed: seedJson(c.memberSeed), node_id: c.nodeId })),
        excluded: v.excluded,
        minimum_separated_voter_pairs: v.minimumSeparatedVoterPairs,
        pinned_voters: v.pinnedVoters,
        replacement_grace_ms: v.replacementGraceMs,
        revision: v.revision,
        target_retained_learners: v.targetRetainedLearners,
        target_voters: v.targetVoters,
      };
    case 'PlacementProvisionRequest':
      return {
        group_id: v.groupId,
        membership_log_id: v.membershipLogId
          ? { index: v.membershipLogId.index, leader_node_id: v.membershipLogId.leaderNodeId, term: v.membershipLogId.term }
          : null,
        node_id: v.nodeId,
        policy_digest: v.policyDigest,
      };
    case 'MemberSeed':
      return seedJson(v);
    case 'ExposeAuthorizeRequest':
      return { service_id: v.serviceId, peer_id: v.peerId, metadata_hex: hex(v.metadata) };
    case 'ExposeHttpRequest':
      return {
        body: bodyJson(v.body),
        headers: headersJson(v.headers),
        method: v.method,
        peer_id: nul(v.peerId),
        service_id: v.serviceId,
        stream_id: v.streamId,
        uri: v.uri,
        version: HTTP_VERSIONS.indexOf(v.version),
      };
    case 'ExposeHttpResponse':
      return { body: bodyJson(v.body), headers: headersJson(v.headers), status: v.status };
    case 'ExposeHttpTrailers':
      return headersJson(v as unknown as Obj[]);
    case 'ExposeRoutePolicyRequest':
      return {
        metadata_hex: hex(v.metadata),
        peer_id: v.peerId,
        specs: v.specs.map((s: Obj) => ({ host: s.host, port: s.port, protocol: s.protocol, service_id: s.serviceId })),
      };
    case 'ExposeRouteGrant':
      return { granted: v.granted };
    case 'AdmissionConnectRequest':
      return { peer: v.peer, alpn: Buffer.from(v.alpn).toString('utf8') };
    case 'AdmissionHandshakeRequest':
      return { peer: v.peer, alpn: Buffer.from(v.alpn).toString('utf8'), is_alive: v.isAlive };
    case 'AdmissionHandshakeResponse':
      return { accept: v.accept, code: v.code, reason: Buffer.from(v.reason).toString('utf8') };
    case 'RpcAuthRequest':
      return { peer_id: v.peerId, service: v.service, method: v.method, session_id: v.sessionId, metadata: v.metadata };
    case 'RpcAuthResponse':
      return v.accepted ? { accepted: true, principal: v.principal, attributes: v.attributes } : { accepted: false };
    default:
      throw new Error(`no JSON rendering for ${type}`);
  }
}

describe('R6 upcall vectors (UCv1)', () => {
  it('the kind table matches the vectors and the binding constants', () => {
    const native = loadNative().upcallKinds();
    expect(native.map((k) => ({ code: k.code, name: k.name, decision: k.decision }))).toEqual(VECTORS.kinds);
    for (const k of VECTORS.kinds) {
      expect(UPCALL_KINDS[k.name as keyof typeof UPCALL_KINDS]).toBe(k.code);
    }
    expect(VECTORS.statuses.map((s) => [s.code, s.name])).toEqual([
      [0, 'ok'],
      [1, 'error'],
      [2, 'unimplemented'],
    ]);
  });

  it('every request and response kind has a vectors type', () => {
    const types = new Set(VECTORS.valid.map((v) => v.type));
    for (const k of loadNative().upcallKinds()) {
      if (k.requestType) expect(types.has(k.requestType)).toBe(true);
      if (k.responseType) expect(types.has(k.responseType)).toBe(true);
    }
  });

  for (const c of VECTORS.valid) {
    it(`valid: ${c.name} (${c.type})`, () => {
      const decoded = upcallCodecDecode(c.type, Buffer.from(c.hex, 'hex')) as Obj;
      expect(toVectorJson(c.type, decoded)).toEqual(c.value);
      expect(hex(upcallCodecEncode(c.type, decoded))).toBe(c.hex);
    });
  }

  for (const c of VECTORS.invalid) {
    it(`invalid: ${c.name} (${c.type}) is refused`, () => {
      expect(() => upcallCodecDecode(c.type, Buffer.from(c.hex, 'hex'))).toThrow(/upcall codec/);
    });
  }

  it('R9 encoders normalise what JavaScript hands them', () => {
    // Header names are lowercased, a missing body is a streamed one, grants are sorted and de-duplicated.
    const resp = upcallCodecEncode('ExposeHttpResponse', {
      status: 204,
      headers: [{ name: 'X-A', value: Buffer.from('b') }],
    } as never);
    const back = upcallCodecDecode('ExposeHttpResponse', resp) as Obj;
    expect(back.headers.map((h: Obj) => [h.name, Buffer.from(h.value).toString()])).toEqual([['x-a', 'b']]);
    expect(back.body == null).toBe(true);
    const grant = upcallCodecEncode('ExposeRouteGrant', { granted: [2, 0, 2] });
    expect((upcallCodecDecode('ExposeRouteGrant', grant) as Obj).granted).toEqual([0, 2]);
    for (const status of [99, 1000]) {
      expect(() => upcallCodecEncode('ExposeHttpResponse', { status, headers: [], body: Buffer.alloc(0) } as never)).toThrow();
    }
    expect(() =>
      upcallCodecEncode('ExposeHttpResponse', { status: 200, headers: [{ name: '', value: Buffer.alloc(0) }] } as never),
    ).toThrow();
  });
});
