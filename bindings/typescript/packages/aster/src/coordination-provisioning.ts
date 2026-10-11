import { loadNative } from './native.js';

/** A 32-byte id/hash/nonce, supplied as raw bytes or hexadecimal text. */
export type ProvisioningBytes = string | Uint8Array;
export interface ProvisioningVoterSeed { readonly nodeId: ProvisioningBytes; readonly ticket: string; }
/** Version is fixed to 1; Rust validates widths and does not sort or choose policy. */
export interface GenesisInput {
  readonly groupId: ProvisioningBytes;
  readonly authorityDomain: string;
  readonly genesisNonce: ProvisioningBytes;
  readonly deploymentProfileCode: number;
  readonly initialVoters: readonly ProvisioningVoterSeed[];
  readonly initialAdmins: readonly ProvisioningBytes[];
  readonly recoveryExportHash?: ProvisioningBytes;
}
/** The canonical candidate ticket is derived in Rust, never accepted from the caller. */
export interface JoinPermitInput {
  readonly groupId: ProvisioningBytes;
  readonly authorityDomain: string;
  readonly genesisHash: ProvisioningBytes;
  readonly candidateNodeId: ProvisioningBytes;
  readonly bootstrapSenders: readonly ProvisioningBytes[];
  readonly joinNonce: ProvisioningBytes;
}
/** Verified local metadata, not a transferable authorization capability or Rust sealed value. */
export interface VerifiedGenesis {
  readonly documentBytes: Uint8Array;
  readonly genesisHash: string;
  readonly groupId: string;
  readonly authorityPublicKey: string;
  readonly authorityDomain: string;
  readonly deploymentProfileCode: number;
  readonly initialVoters: readonly { readonly nodeId: string; readonly ticket: string }[];
  readonly initialAdmins: readonly string[];
  readonly recoveryExportHash?: string;
}
/** The group/genesis are extracted, not compared to independently expected values. */
export interface VerifiedJoinPermit {
  readonly documentBytes: Uint8Array;
  readonly groupId: string;
  readonly genesisHash: string;
  readonly candidateNodeId: string;
  readonly bootstrapSenders: readonly string[];
}
export type ProvisioningErrorCode = 'invalid_input' | 'encoding' | 'malformed' | 'unknown_format'
  | 'non_canonical' | 'wrong_authority_key' | 'bad_signature' | 'wrong_domain' | 'field_width'
  | 'unknown_profile' | 'voter_count' | 'initial_voters' | 'initial_admins' | 'root_is_not_delegable'
  | 'root_is_not_a_member' | 'voter_seed_ticket' | 'wrong_group' | 'wrong_genesis' | 'wrong_node'
  | 'ticket_mismatch' | 'bootstrap_senders';
export class ProvisioningError extends Error {
  constructor(readonly code: ProvisioningErrorCode, readonly detail: string, cause?: unknown) {
    super(`${code}: ${detail}`, { cause }); this.name = 'ProvisioningError';
  }
}
function call<T>(work: () => T): T {
  try { return work(); }
  catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (message.startsWith('ASTER_PROVISIONING:')) {
      const value = JSON.parse(message.slice('ASTER_PROVISIONING:'.length)) as { code: ProvisioningErrorCode; detail: string };
      throw new ProvisioningError(value.code, value.detail, error);
    }
    throw error;
  }
}
const hex = (value: ProvisioningBytes): string => typeof value === 'string' ? value : Buffer.from(value).toString('hex');
const bytes = (value: Uint8Array): Buffer => Buffer.from(value);
function genesisJSON(value: GenesisInput): string {
  return JSON.stringify({ group_id: hex(value.groupId), authority_domain: value.authorityDomain,
    genesis_nonce: hex(value.genesisNonce), deployment_profile_code: value.deploymentProfileCode,
    initial_voters: value.initialVoters.map(seed => ({ node_id: hex(seed.nodeId), ticket: seed.ticket })),
    initial_admins: value.initialAdmins.map(hex), recovery_export_hash: value.recoveryExportHash == null ? null : hex(value.recoveryExportHash) });
}
function joinJSON(value: JoinPermitInput): string {
  return JSON.stringify({ group_id: hex(value.groupId), authority_domain: value.authorityDomain,
    genesis_hash: hex(value.genesisHash), candidate_node_id: hex(value.candidateNodeId),
    bootstrap_senders: value.bootstrapSenders.map(hex), join_nonce: hex(value.joinNonce) });
}

/** Offline provisioning; canonical bytes, domains, hashes, envelopes and verification all run in Rust.
 * Sign the returned preimage externally. No authority secret is accepted by these helpers. */
export const CoordinationProvisioning = Object.freeze({
  canonicalGenesis(input: GenesisInput): Uint8Array {
    return call(() => Uint8Array.from(loadNative().coordinationCanonicalGenesis(genesisJSON(input))));
  },
  canonicalJoinPermit(input: JoinPermitInput): Uint8Array {
    return call(() => Uint8Array.from(loadNative().coordinationCanonicalJoinPermit(joinJSON(input))));
  },
  genesisSigningBytes(document: Uint8Array): Uint8Array {
    return call(() => Uint8Array.from(loadNative().coordinationGenesisSigningBytes(bytes(document))));
  },
  joinSigningBytes(document: Uint8Array): Uint8Array {
    return call(() => Uint8Array.from(loadNative().coordinationJoinSigningBytes(bytes(document))));
  },
  /** Hashes supplied bytes without verification. Only verifyGenesis establishes authority. */
  genesisHash(document: Uint8Array): Uint8Array {
    return call(() => Uint8Array.from(loadNative().coordinationGenesisHash(bytes(document))));
  },
  signedDocumentFromParts(document: Uint8Array, authorityKey: Uint8Array, externalSignature: Uint8Array): Uint8Array {
    return call(() => Uint8Array.from(loadNative().coordinationSignedDocumentFromParts(bytes(document), bytes(authorityKey), bytes(externalSignature))));
  },
  canonicalCandidateTicket(candidate: Uint8Array): string {
    return call(() => loadNative().coordinationCanonicalCandidateTicket(bytes(candidate)));
  },
  /** Verify with independently trusted key/domain pins, never just the envelope's own key. */
  verifyGenesis(authorityKey: Uint8Array, domain: string, envelope: Uint8Array): VerifiedGenesis {
    return call(() => {
      const value = JSON.parse(loadNative().coordinationVerifyGenesis(bytes(authorityKey), domain, bytes(envelope)));
      return Object.freeze({ documentBytes: Uint8Array.from(Buffer.from(value.document_bytes, 'hex')),
        genesisHash: value.genesis_hash, groupId: value.group_id, authorityPublicKey: value.authority_public_key,
        authorityDomain: value.authority_domain, deploymentProfileCode: value.deployment_profile_code,
        initialVoters: Object.freeze(value.initial_voters.map((seed: { node_id: string; ticket: string }) => Object.freeze({ nodeId: seed.node_id, ticket: seed.ticket }))),
        initialAdmins: Object.freeze(value.initial_admins),
        ...(value.recovery_export_hash == null ? {} : { recoveryExportHash: value.recovery_export_hash }) });
    });
  },
  /** Checks key/domain/candidate; group and genesis are learned from the permit. */
  verifyJoinPermitForStart(authorityKey: Uint8Array, domain: string, candidate: Uint8Array, envelope: Uint8Array): VerifiedJoinPermit {
    return call(() => {
      const value = JSON.parse(loadNative().coordinationVerifyJoinPermitForStart(bytes(authorityKey), domain, bytes(candidate), bytes(envelope)));
      return Object.freeze({ documentBytes: Uint8Array.from(Buffer.from(value.document_bytes, 'hex')),
        groupId: value.group_id, genesisHash: value.genesis_hash, candidateNodeId: value.candidate_node_id,
        bootstrapSenders: Object.freeze(value.bootstrap_senders) });
    });
  },
});
