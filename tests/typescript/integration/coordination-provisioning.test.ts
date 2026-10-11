import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { createPrivateKey, sign } from 'node:crypto';
import { CoordinationProvisioning as provisioning, ProvisioningError, type GenesisInput, type JoinPermitInput } from '@aster-rpc/aster';

const vectors = JSON.parse(readFileSync(new URL('../../fixtures/coordination_provisioning_v1.json', import.meta.url), 'utf8'));
const b = (value: string): Buffer => Buffer.from(value, 'hex');
const authority = b(vectors.authority_public_key);
const externalKey = createPrivateKey({
  key: Buffer.concat([b('302e020100300506032b657004220420'), b(vectors.test_only_authority_secret)]),
  format: 'der', type: 'pkcs8',
});
function genesis(value: any): GenesisInput {
  return { groupId: value.group_id, authorityDomain: value.authority_domain, genesisNonce: value.genesis_nonce,
    deploymentProfileCode: value.deployment_profile_code,
    initialVoters: value.initial_voters.map((seed: any) => ({ nodeId: seed.node_id, ticket: seed.ticket })),
    initialAdmins: value.initial_admins,
    ...(value.recovery_export_hash == null ? {} : { recoveryExportHash: value.recovery_export_hash }) };
}
function join(value: any): JoinPermitInput {
  return { groupId: value.group_id, authorityDomain: value.authority_domain, genesisHash: value.genesis_hash,
    candidateNodeId: value.candidate_node_id, bootstrapSenders: value.bootstrap_senders, joinNonce: value.join_nonce };
}
function error(work: () => unknown, code: string): void {
  try { work(); throw new Error('expected provisioning failure'); }
  catch (failure) { expect(failure).toBeInstanceOf(ProvisioningError); expect(failure).toMatchObject({ code }); }
}

describe('native coordination authority provisioning', () => {
  it('matches shared genesis bytes and a real external Ed25519 signer', () => {
    const vector = vectors.genesis;
    const input = genesis(vector.request);
    const document = provisioning.canonicalGenesis(input);
    expect(Buffer.from(document)).toEqual(b(vector.document_bytes));
    const preimage = provisioning.genesisSigningBytes(document);
    expect(Buffer.from(preimage)).toEqual(b(vector.signing_bytes));
    const signature = sign(null, preimage, externalKey);
    expect(signature).toEqual(b(vector.signature));
    const envelope = provisioning.signedDocumentFromParts(document, authority, signature);
    expect(Buffer.from(envelope)).toEqual(b(vector.signed_document));
    expect(Buffer.from(provisioning.genesisHash(document))).toEqual(b(vector.genesis_hash));
    const verified = provisioning.verifyGenesis(authority, input.authorityDomain, envelope);
    const expected = vector.verified;
    expect(Buffer.from(verified.documentBytes)).toEqual(b(expected.document_bytes));
    expect(verified).toMatchObject({ genesisHash: expected.genesis_hash, groupId: expected.group_id,
      authorityPublicKey: expected.authority_public_key, authorityDomain: expected.authority_domain,
      deploymentProfileCode: expected.deployment_profile_code, initialAdmins: expected.initial_admins,
      initialVoters: expected.initial_voters.map((seed: any) => ({ nodeId: seed.node_id, ticket: seed.ticket })) });
    expect(verified.recoveryExportHash).toBe(expected.recovery_export_hash ?? undefined);
    expect(Object.isFrozen(verified.initialVoters)).toBe(true);
  });

  it('matches shared join bytes, derives its addressless ticket and verifies candidate pins', () => {
    const vector = vectors.join;
    const input = join(vector.request);
    const document = provisioning.canonicalJoinPermit(input);
    expect(Buffer.from(document)).toEqual(b(vector.document_bytes));
    expect(provisioning.canonicalCandidateTicket(b(vector.request.candidate_node_id))).toBe(vector.candidate_ticket);
    const preimage = provisioning.joinSigningBytes(document);
    expect(Buffer.from(preimage)).toEqual(b(vector.signing_bytes));
    const signature = sign(null, preimage, externalKey);
    expect(signature).toEqual(b(vector.signature));
    const envelope = provisioning.signedDocumentFromParts(document, authority, signature);
    expect(Buffer.from(envelope)).toEqual(b(vector.signed_document));
    const verified = provisioning.verifyJoinPermitForStart(authority, input.authorityDomain, b(vector.request.candidate_node_id), envelope);
    expect(Buffer.from(verified.documentBytes)).toEqual(b(vector.verified.document_bytes));
    expect(verified).toMatchObject({ groupId: vector.verified.group_id, genesisHash: vector.verified.genesis_hash,
      candidateNodeId: vector.verified.candidate_node_id, bootstrapSenders: vector.verified.bootstrap_senders });
    const wrong = b(vector.request.candidate_node_id); wrong[0] ^= 1;
    error(() => provisioning.verifyJoinPermitForStart(authority, input.authorityDomain, wrong, envelope), 'wrong_node');
  });

  it('rejects wrong pins, tampering, malformed envelopes and cross-domain signatures', () => {
    const vector = vectors.genesis;
    const domain = vector.request.authority_domain;
    const document = b(vector.document_bytes);
    const envelope = b(vector.signed_document);
    error(() => provisioning.verifyGenesis(authority, 'elsewhere', envelope), 'wrong_domain');
    const wrong = Buffer.from(authority); wrong[0] ^= 1;
    error(() => provisioning.verifyGenesis(wrong, domain, envelope), 'wrong_authority_key');
    const bad = b(vector.signature); bad[0] ^= 1;
    error(() => provisioning.verifyGenesis(authority, domain, provisioning.signedDocumentFromParts(document, authority, bad)), 'bad_signature');
    const cross = sign(null, provisioning.joinSigningBytes(document), externalKey);
    error(() => provisioning.verifyGenesis(authority, domain, provisioning.signedDocumentFromParts(document, authority, cross)), 'bad_signature');
    error(() => provisioning.verifyGenesis(authority, domain, new Uint8Array()), 'malformed');
  });

  it('preserves explicit profile selection and returns typed width/profile errors', () => {
    const empty = new Uint8Array();
    error(() => provisioning.signedDocumentFromParts(empty, new Uint8Array(31), new Uint8Array(64)), 'invalid_input');
    error(() => provisioning.signedDocumentFromParts(empty, new Uint8Array(32), new Uint8Array(63)), 'invalid_input');
    error(() => provisioning.canonicalCandidateTicket(new Uint8Array(31)), 'invalid_input');
    const input = { ...genesis(vectors.genesis.request), deploymentProfileCode: 999 };
    const document = provisioning.canonicalGenesis(input);
    const envelope = provisioning.signedDocumentFromParts(document, authority, sign(null, provisioning.genesisSigningBytes(document), externalKey));
    error(() => provisioning.verifyGenesis(authority, input.authorityDomain, envelope), 'unknown_profile');
  });
});
