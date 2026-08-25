//! §7.1 signed-document verification, as an apply-time decision.
//!
//! A membership plan's seeds carry signed join permits, and §9 requires apply to
//! check "the current group administrator, expected membership revision, sizes,
//! **signatures**, and exact request hash" before storing the intent. An earlier
//! revision checked only that a permit was present and under its size cap, which
//! made `Some(vec![])` a valid permit and accepted a permit minted for another
//! node, another group, another genesis, or another deployment entirely.
//!
//! **This has to run in apply, not only in preflight.** Preflight runs on one
//! leader; apply runs on every voter, and only what apply decides is replicated.
//! A leader-stamped "I checked it" boolean would make the group's membership
//! depend on the honesty of whichever node happened to be leader.
//!
//! Everything here is deterministic: the verification inputs are the group's own
//! replicated authority material (§11's `CoordinationMetaV1`), and Ed25519
//! verification reads no clock and no filesystem.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use super::state::{GroupAuthority, NodeId};
use crate::coordination::contract::{payload_fory, MAX_WIRE_STRING};
use crate::coordination::raft::{GenesisV1, JoinPermitV1, SignedCoordinationDocumentV1};

/// §7.1's join-permit signing domain. The trailing NUL is part of it.
pub const JOIN_DOMAIN: &[u8] = b"aster-coordination/join/v1\0";

/// §7.1's genesis signing domain. The trailing NUL is part of it.
///
/// Distinct from [`JOIN_DOMAIN`] so one authority's signature over a join
/// permit can never be replayed as a signature over a genesis document, even
/// if the two encodings ever coincided.
pub const GENESIS_DOMAIN: &[u8] = b"aster-coordination/genesis/v1\0";

/// The `format_version` a v1 permit and its envelope must declare.
pub const PERMIT_FORMAT_VERSION: u32 = 1;

/// The protocol limit on an authority domain: 128 ASCII bytes.
///
/// Named because it is a *protocol* bound rather than the generic
/// [`MAX_WIRE_STRING`] ceiling that applies to opaque strings. Storage sizing
/// needs the real one — using the generic 4095 to build a "maximal legal"
/// value overstates legal state by a factor of thirty, which is exactly the
/// mistake that produced a bogus worst-case measurement.
pub const MAX_AUTHORITY_DOMAIN_BYTES: usize = 128;

/// Why a signed join permit was refused.
///
/// One variant per reason, not a single `Invalid`: an operator holding a permit
/// that will not verify needs to know whether it was minted for another group,
/// another node, or another deployment -- those have different fixes, and
/// collapsing them turns a provisioning mistake into a debugging session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermitError {
    /// The envelope or the permit inside it would not decode.
    Malformed,
    /// A `format_version` this build does not implement.
    UnknownFormat(u32),
    /// Re-encoding the decoded permit did not reproduce the signed bytes.
    ///
    /// §7.1 requires this explicitly: verification signs exact bytes, so a
    /// permit that decodes to something whose canonical form differs has a
    /// second valid spelling, and the signature covers only one of them.
    NonCanonical,
    /// The envelope names a public key other than the group's pinned authority.
    WrongAuthorityKey,
    /// The signature does not verify under the group's authority key.
    BadSignature,
    /// The permit's `authority_domain` is not this deployment's.
    ///
    /// Separate from a signature failure because it is the opposite problem: the
    /// signature is *good*, and that is exactly why the domain matters. One
    /// authority legitimately signs for several deployments, so a permit minted
    /// for staging verifies cleanly against production's pinned key.
    WrongDomain,
    /// The permit names another group.
    WrongGroup,
    /// The permit names another genesis of this group id -- a replacement group
    /// reusing an id, which §7.1 makes a hard failure rather than a match.
    WrongGenesis,
    /// The permit was minted for a different candidate node.
    WrongNode,
    /// The permit's `candidate_ticket` is not the canonical addressless ticket
    /// derived from the candidate's NodeId — see [`canonical_candidate_ticket`].
    /// Address material is refused even when it names the right candidate.
    TicketMismatch,
    /// The bootstrap-sender list is empty, unsorted, or has duplicates.
    BootstrapSenders,
    /// A fixed-width field was the wrong width, or a bounded one too long.
    FieldWidth,
}

/// The one ticket a permit's `candidate_ticket` field may carry: the canonical,
/// credential-free, **addressless** Aster ticket for `candidate`.
///
/// A permit binds identity, and an Aster node's identity is its NodeId. Its
/// address is not: a node binds an ephemeral UDP port and is resolved from
/// NodeId to a current address by discovery, so a fully rendered address ticket
/// names something that changes on every restart. Binding one would mean a
/// candidate could never restart under the permit it was given — which is
/// exactly what happened, and what `S3-JOIN-TICKET` records.
///
/// Field 6 of `JoinPermitV1` is frozen, so it stays; what it *means* is fixed
/// here instead. Derived, never accepted: nothing outside this function decides
/// what the field should contain, so no caller can reintroduce address material
/// by handing in a ticket it rendered itself. Current addresses remain advisory
/// discovery hints — see the "Route hint" row of the authority matrix, which
/// already said they are advisory only.
pub fn canonical_candidate_ticket(candidate: &NodeId) -> Result<String, PermitError> {
    aster_transport_core::ticket::AsterTicket {
        endpoint_id: *candidate,
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .map_err(|_| PermitError::TicketMismatch)
}

/// Verify one seed's signed join permit against the group's authority.
///
/// The permit's ticket is checked against [`canonical_candidate_ticket`] for
/// `candidate` rather than against a ticket the caller supplies, so a permit
/// binds the candidate's identity and nothing transient.
pub fn verify_join_permit(
    authority: &GroupAuthority,
    permit_bytes: &[u8],
    candidate: &NodeId,
) -> Result<(), PermitError> {
    let (permit, _) = open_signed_permit(&authority.public_key, permit_bytes)?;
    check_permit(authority, &permit, candidate)
}

/// Open a signed permit envelope: everything up to and including "these bytes
/// are the authority's, and this is what they decode to".
///
/// Shared by the apply-time [`verify_join_permit`] and the startup
/// [`verify_join_permit_for_start`] so the signature half has one
/// implementation. What the two do *afterwards* differs, and that difference is
/// the point — see [`VerifiedJoinPermit`].
///
/// Returns the decoded permit and the exact signed bytes, because a caller that
/// seals the result must carry the bytes whose signature was checked rather
/// than a re-encoding of the value.
fn open_signed_permit(
    authority_public_key: &[u8; 32],
    permit_bytes: &[u8],
) -> Result<(JoinPermitV1, Vec<u8>), PermitError> {
    let envelope: SignedCoordinationDocumentV1 = payload_fory::<SignedCoordinationDocumentV1>()
        .deserialize(permit_bytes)
        .map_err(|_| PermitError::Malformed)?;

    if envelope.format_version != PERMIT_FORMAT_VERSION {
        return Err(PermitError::UnknownFormat(envelope.format_version));
    }
    if envelope.authority_public_key.len() != 32 || envelope.signature.len() != 64 {
        return Err(PermitError::FieldWidth);
    }

    // §7.1: the envelope's key must **equal** the pinned one. It exists so a
    // mismatch is a clear diagnostic, never as a key to verify against -- a
    // signature checked with the key that came with it proves nothing.
    if envelope.authority_public_key != authority_public_key.to_vec() {
        return Err(PermitError::WrongAuthorityKey);
    }

    let key = VerifyingKey::from_bytes(authority_public_key).map_err(|_| PermitError::Malformed)?;
    let signature =
        Signature::from_slice(&envelope.signature).map_err(|_| PermitError::Malformed)?;
    key.verify(&join_signing_bytes(&envelope.document_bytes), &signature)
        .map_err(|_| PermitError::BadSignature)?;

    let permit: JoinPermitV1 = payload_fory::<JoinPermitV1>()
        .deserialize(&envelope.document_bytes)
        .map_err(|_| PermitError::Malformed)?;

    // §7.1: decode then re-encode must reproduce the signed bytes exactly.
    let reencoded = payload_fory::<JoinPermitV1>()
        .serialize(&permit)
        .map_err(|_| PermitError::Malformed)?;
    if reencoded != envelope.document_bytes {
        return Err(PermitError::NonCanonical);
    }

    Ok((permit, envelope.document_bytes))
}

/// The bindings a verified permit must carry, checked after the signature.
///
/// Shape first, then bindings. Order matters only for which error an operator
/// sees first, and a malformed field must not be reported as a *mismatched*
/// one: a permit whose `group_id` is 31 bytes long does not name another group,
/// it names nothing, and "WrongGroup" would send the operator to reissue a
/// permit when the encoder is what is broken. Within the bindings the order
/// goes from "this permit is for someone else entirely" to "this permit is for
/// you but does not match this seed".
fn check_permit(
    authority: &GroupAuthority,
    permit: &JoinPermitV1,
    candidate: &NodeId,
) -> Result<(), PermitError> {
    check_permit_shape(permit)?;

    if permit.authority_domain != authority.domain {
        return Err(PermitError::WrongDomain);
    }
    if permit.group_id != authority.group_id.to_vec() {
        return Err(PermitError::WrongGroup);
    }
    if permit.genesis_hash != authority.genesis_hash.to_vec() {
        return Err(PermitError::WrongGenesis);
    }
    if permit.candidate_node_id != candidate.to_vec() {
        return Err(PermitError::WrongNode);
    }
    if permit.candidate_ticket != canonical_candidate_ticket(candidate)? {
        return Err(PermitError::TicketMismatch);
    }
    Ok(())
}

/// The width and ordering rules a permit's own fields must satisfy, with no
/// reference to a group.
///
/// Split out of [`check_permit`] because the startup verifier needs exactly
/// these and none of the group comparisons — it is *learning* the group, so it
/// has nothing to compare against yet.
fn check_permit_shape(permit: &JoinPermitV1) -> Result<(), PermitError> {
    if permit.format_version != PERMIT_FORMAT_VERSION {
        return Err(PermitError::UnknownFormat(permit.format_version));
    }
    if permit.group_id.len() != 32
        || permit.genesis_hash.len() != 32
        || permit.candidate_node_id.len() != 32
        || permit.join_nonce.len() != 32
        || permit.authority_domain.is_empty()
        || permit.authority_domain.len() > MAX_AUTHORITY_DOMAIN_BYTES
        || !permit.authority_domain.is_ascii()
        || permit.candidate_ticket.is_empty()
        || permit.candidate_ticket.len() > MAX_WIRE_STRING
    {
        return Err(PermitError::FieldWidth);
    }

    // §7.1: sorted, non-empty. Until the candidate applies a membership
    // containing itself, this list is the only set of peers it accepts Raft
    // traffic from, so an empty one is a node that can never be reached and a
    // duplicated one is a list whose meaning depends on how it is read.
    if permit.bootstrap_senders.is_empty() {
        return Err(PermitError::BootstrapSenders);
    }
    if permit
        .bootstrap_senders
        .iter()
        .any(|n| n.node_id.len() != 32)
    {
        return Err(PermitError::FieldWidth);
    }
    if permit
        .bootstrap_senders
        .windows(2)
        .any(|w| w[0].node_id >= w[1].node_id)
    {
        return Err(PermitError::BootstrapSenders);
    }
    Ok(())
}

/// A join permit verified by the **candidate at startup**, and the group
/// identity it establishes.
///
/// This is a different question from [`verify_join_permit`]'s, about the same
/// document, and the difference is exactly which values the caller already
/// holds.
///
/// At apply time every voter already knows the group's authority, so the check
/// is "does this seed's permit match the group we are?" — nothing is extracted,
/// the caller hands in the candidate and ticket it intends to use, and a bare
/// `Result<(), PermitError>` is the honest return.
///
/// At startup the candidate is *learning* the group. It has its own NodeId, its
/// own ticket, and the configured authority key and domain — and it has no
/// independent expectation for the group id or the genesis hash at all, because
/// the permit is what tells it. Those two are therefore **extracted rather than
/// compared**, and saying so plainly matters: a reader who assumed they were
/// checked would believe this rejects a permit for the wrong group, and there
/// is no "wrong group" for a node that belongs to none yet. What binds the
/// permit to *this* node is the signature, the configured domain, the candidate
/// NodeId and the candidate ticket.
///
/// Extraction is why this seals and [`verify_join_permit`] does not: a bare
/// `(group_id, genesis_hash)` return would leave the caller free to decode the
/// permit again and act on a different one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedJoinPermit {
    document_bytes: Vec<u8>,
    group_id: [u8; 32],
    genesis_hash: [u8; 32],
    candidate: NodeId,
    bootstrap_senders: Vec<NodeId>,
}

impl VerifiedJoinPermit {
    /// The exact bytes whose signature was checked.
    pub fn document_bytes(&self) -> &[u8] {
        &self.document_bytes
    }

    /// The group this permit admits the candidate to speak to.
    pub fn group_id(&self) -> &[u8; 32] {
        &self.group_id
    }

    /// The genesis the permit names. Compared against the derived hash of a
    /// genesis this node later obtains — never trusted as a hash on its own.
    pub fn genesis_hash(&self) -> &[u8; 32] {
        &self.genesis_hash
    }

    /// The candidate the permit was minted for, already checked to be the
    /// local node.
    pub fn candidate(&self) -> &NodeId {
        &self.candidate
    }

    /// The only senders this node accepts Raft traffic from until a committed
    /// membership contains it. Sorted and non-empty.
    pub fn bootstrap_senders(&self) -> &[NodeId] {
        &self.bootstrap_senders
    }

    /// Whether `node` is one of the bootstrap senders.
    pub fn permits_sender(&self, node: &NodeId) -> bool {
        // Sorted by construction, so this is a binary search rather than a
        // scan -- it runs on the Raft admission path for every call until
        // membership lands.
        self.bootstrap_senders.binary_search(node).is_ok()
    }
}

/// Verify a join permit as the **candidate**, at startup.
///
/// See [`VerifiedJoinPermit`] for why this shape differs from
/// [`verify_join_permit`], and for exactly which bindings it does and does not
/// establish.
pub fn verify_join_permit_for_start(
    authority_public_key: &[u8; 32],
    authority_domain: &str,
    permit_bytes: &[u8],
    candidate: &NodeId,
) -> Result<VerifiedJoinPermit, PermitError> {
    let (permit, document_bytes) = open_signed_permit(authority_public_key, permit_bytes)?;
    check_permit_shape(&permit)?;

    // The three bindings the candidate *can* check, and does.
    if permit.authority_domain != authority_domain {
        return Err(PermitError::WrongDomain);
    }
    if permit.candidate_node_id != candidate.to_vec() {
        return Err(PermitError::WrongNode);
    }
    if permit.candidate_ticket != canonical_candidate_ticket(candidate)? {
        return Err(PermitError::TicketMismatch);
    }

    let mut bootstrap_senders = Vec::with_capacity(permit.bootstrap_senders.len());
    for sender in &permit.bootstrap_senders {
        bootstrap_senders.push(
            NodeId::try_from(sender.node_id.as_slice()).map_err(|_| PermitError::FieldWidth)?,
        );
    }

    Ok(VerifiedJoinPermit {
        group_id: NodeId::try_from(permit.group_id.as_slice())
            .map_err(|_| PermitError::FieldWidth)?,
        genesis_hash: NodeId::try_from(permit.genesis_hash.as_slice())
            .map_err(|_| PermitError::FieldWidth)?,
        candidate: *candidate,
        bootstrap_senders,
        document_bytes,
    })
}

// ── §7.1's genesis document ──────────────────────────────────────────────────

/// Why a signed genesis document was refused.
///
/// Deliberately a separate enum from [`PermitError`] rather than a shared one.
/// The two documents share an envelope and a verification shape, but not their
/// failure sets: a genesis has a deployment profile, an initial voter set and an
/// admin set, and a permit has a candidate, a ticket and a sender list. A merged
/// enum would carry variants that are unreachable for one of the two, and an
/// operator reading "WrongNode" from a genesis failure would be chasing a
/// concept the document does not contain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenesisError {
    /// The envelope or the genesis inside it would not decode.
    Malformed,
    /// A `format_version` this build does not implement.
    UnknownFormat(u32),
    /// Re-encoding the decoded genesis did not reproduce the signed bytes.
    NonCanonical,
    /// The envelope names a public key other than the configured authority.
    WrongAuthorityKey,
    /// The signature does not verify under the configured authority key.
    BadSignature,
    /// The document's `authority_domain` is not this deployment's.
    WrongDomain,
    /// A fixed-width field was the wrong width, or a bounded one too long.
    FieldWidth,
    /// The `deployment_profile_code` is not one this build implements.
    ///
    /// Checked by membership in [`shape::PROFILES`], which is a different
    /// question from whether a voter *count* is permitted: an unknown profile
    /// permits no count, so a count check alone would report the voter set as
    /// the problem and leave the operator editing the wrong field.
    UnknownProfile(i32),
    /// The initial voter count is not one this profile permits.
    VoterCount {
        /// The profile the genesis declares.
        profile: i32,
        /// The number of initial voters it names.
        voters: usize,
    },
    /// The initial voters are empty, unsorted, or duplicated.
    InitialVoters,
    /// The genesis names **ROOT** among its initial administrators.
    ///
    /// §8.1: ROOT is immutable and is never a row in the mutable delegated
    /// table. A document that would put it there is refused rather than
    /// filtered, so no group can be founded holding one.
    RootIsNotDelegable,
    /// The genesis names **ROOT** among its initial voters.
    ///
    /// A voter runs its identity's secret as its Aster endpoint secret on a
    /// live mesh node. ROOT's secret is the deployment's offline trust anchor,
    /// and a permanent always-on holder of it is exactly the exposure keeping
    /// it offline exists to avoid. A short-lived administration endpoint is
    /// fine; a voter is not.
    RootIsNotAMember,
    /// The initial admins are empty, over the §12.1 cap, unsorted, or
    /// duplicated.
    InitialAdmins,
    /// A voter seed's ticket is not a canonical, credential-free Aster ticket
    /// for exactly that seed's node.
    VoterSeedTicket {
        /// Which of the §12.1 request-shape rules the ticket broke.
        limit: super::shape::ShapeLimit,
    },
}

/// One initial voter, validated: a 32-byte node id and the canonical Open
/// ticket that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedVoterSeed {
    node_id: NodeId,
    ticket: String,
}

impl VerifiedVoterSeed {
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    pub fn ticket(&self) -> &str {
        &self.ticket
    }
}

/// A genesis document that has been verified, together with everything derived
/// from the bytes that were verified.
///
/// **There is no public constructor, and no accessor returns anything
/// [`verify_genesis`] did not itself prove.** That is the whole point of the
/// type, and it is the same argument
/// [`PreparedInstall`](crate::coordination::storage::snapshot) makes about a
/// staged body and its candidate.
///
/// If verification returned only a hash, the caller would still be holding the
/// *presented* bytes and would decode them itself — so bootstrap could act on a
/// document that is not the one that was verified. A second decode under
/// different bounds, a re-read of the file between the two, or simply the wrong
/// variable in scope: every check would pass and the group would be founded on
/// unverified content. Returning the verified content instead removes the
/// second decode from the design.
///
/// [`verify_join_permit`] is deliberately **not** this shape, and the contrast
/// is the point: it is handed the candidate and ticket the caller intends to
/// use and checks *those*. Nothing is extracted, so nothing can be substituted
/// afterwards, and sealing it would add a capability with nothing to protect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedGenesis {
    document_bytes: Vec<u8>,
    genesis_hash: [u8; 32],
    group_id: [u8; 32],
    authority_domain: String,
    deployment_profile_code: i32,
    initial_voters: Vec<VerifiedVoterSeed>,
    initial_admins: Vec<NodeId>,
    recovery_export_hash: Option<[u8; 32]>,
}

impl VerifiedGenesis {
    /// The exact bytes whose signature was checked.
    ///
    /// Exposed because §11 persists them and §7.2 forwards them to peers, and
    /// both must carry the verified spelling rather than a re-encoding of the
    /// decoded value.
    pub fn document_bytes(&self) -> &[u8] {
        &self.document_bytes
    }

    /// `BLAKE3(GENESIS_DOMAIN || document_bytes)`, derived here rather than
    /// read from the document.
    ///
    /// `GenesisV1` carries no genesis-hash field precisely so there is nothing
    /// inside it for a verifier to disagree with. The two-input comparison §7.1
    /// requires is between *this* value and an independently held expectation —
    /// the bound store's `genesis_hash`, `Existing`'s `expected_genesis_hash`,
    /// or the `genesis_hash` a `JoinPermitV1` names — and it lives at the call
    /// site that holds that expectation, not here.
    pub fn genesis_hash(&self) -> &[u8; 32] {
        &self.genesis_hash
    }

    pub fn group_id(&self) -> &[u8; 32] {
        &self.group_id
    }

    pub fn authority_domain(&self) -> &str {
        &self.authority_domain
    }

    /// The replicated deployment profile, already checked for membership in
    /// [`shape::PROFILES`].
    pub fn deployment_profile_code(&self) -> i32 {
        self.deployment_profile_code
    }

    /// The initial voters, sorted by node id and each bound to a validated
    /// ticket.
    pub fn initial_voters(&self) -> &[VerifiedVoterSeed] {
        &self.initial_voters
    }

    /// The initial group administrators, sorted and unique.
    pub fn initial_admins(&self) -> &[NodeId] {
        &self.initial_admins
    }

    /// The §13 recovery export this genesis binds, if any.
    pub fn recovery_export_hash(&self) -> Option<&[u8; 32]> {
        self.recovery_export_hash.as_ref()
    }

    /// Whether `node` is one of the initial voters.
    ///
    /// The §7.2 bootstrap allowlist reads this rather than iterating the seeds
    /// at the call site, so "signed initial voter" has one definition.
    pub fn is_initial_voter(&self, node: &NodeId) -> bool {
        self.initial_voters.iter().any(|v| &v.node_id == node)
    }
}

/// Verify a signed genesis document against the **configured** authority.
///
/// The key and domain are the single top-level pins from §3.3 and apply
/// identically in all three start modes. The key inside the envelope is
/// compared to the configured one and never verified against — a signature
/// checked with the key that arrived beside it proves nothing.
///
/// The order mirrors [`verify_join_permit`] exactly, because that ordering is
/// the one §7.1 already reasoned about: nothing in the document is interpreted
/// until its signature has been checked, and the re-encode comparison
/// necessarily follows the decode it validates.
pub fn verify_genesis(
    authority_public_key: &[u8; 32],
    authority_domain: &str,
    signed_bytes: &[u8],
) -> Result<VerifiedGenesis, GenesisError> {
    let envelope: SignedCoordinationDocumentV1 = payload_fory::<SignedCoordinationDocumentV1>()
        .deserialize(signed_bytes)
        .map_err(|_| GenesisError::Malformed)?;

    if envelope.format_version != PERMIT_FORMAT_VERSION {
        return Err(GenesisError::UnknownFormat(envelope.format_version));
    }
    if envelope.authority_public_key.len() != 32 || envelope.signature.len() != 64 {
        return Err(GenesisError::FieldWidth);
    }
    if envelope.authority_public_key != authority_public_key.to_vec() {
        return Err(GenesisError::WrongAuthorityKey);
    }

    let key =
        VerifyingKey::from_bytes(authority_public_key).map_err(|_| GenesisError::Malformed)?;
    let signature =
        Signature::from_slice(&envelope.signature).map_err(|_| GenesisError::Malformed)?;
    let mut signed = Vec::with_capacity(GENESIS_DOMAIN.len() + envelope.document_bytes.len());
    signed.extend_from_slice(GENESIS_DOMAIN);
    signed.extend_from_slice(&envelope.document_bytes);
    key.verify(&signed, &signature)
        .map_err(|_| GenesisError::BadSignature)?;

    let genesis: GenesisV1 = payload_fory::<GenesisV1>()
        .deserialize(&envelope.document_bytes)
        .map_err(|_| GenesisError::Malformed)?;

    let reencoded = payload_fory::<GenesisV1>()
        .serialize(&genesis)
        .map_err(|_| GenesisError::Malformed)?;
    if reencoded != envelope.document_bytes {
        return Err(GenesisError::NonCanonical);
    }

    seal_genesis(
        authority_public_key,
        authority_domain,
        genesis,
        envelope.document_bytes,
    )
}

/// The bindings a verified genesis must carry, checked after the signature, and
/// the derivation of everything the sealed value exposes.
///
/// Separate from the signature half so the two halves are readable apart: above
/// this line is "these bytes are the authority's"; below it is "and this is
/// what they say".
fn seal_genesis(
    authority_public_key: &[u8; 32],
    authority_domain: &str,
    genesis: GenesisV1,
    document_bytes: Vec<u8>,
) -> Result<VerifiedGenesis, GenesisError> {
    use super::limits::{MAX_ADMINS_PER_GROUP, MAX_TICKET};
    use super::shape;

    if genesis.format_version != PERMIT_FORMAT_VERSION {
        return Err(GenesisError::UnknownFormat(genesis.format_version));
    }
    if genesis.group_id.len() != 32
        || genesis.genesis_nonce.len() != 32
        || genesis.authority_domain.is_empty()
        || genesis.authority_domain.len() > MAX_AUTHORITY_DOMAIN_BYTES
        || !genesis.authority_domain.is_ascii()
    {
        return Err(GenesisError::FieldWidth);
    }
    // `Option<Vec<u8>>`: absent is legal and means "no recovery export". A
    // present one that is not a 32-byte digest is a width failure rather than
    // an absent hash, because §13 compares it and a truncated digest would
    // compare unequal for the wrong reason.
    let recovery_export_hash = match &genesis.recovery_export_hash {
        None => None,
        Some(bytes) => {
            Some(<[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| GenesisError::FieldWidth)?)
        }
    };

    // §7.1: the document's domain must equal the **configured** expectation.
    // The signature being good is exactly why this matters — one authority
    // legitimately signs for several deployments, so a staging genesis verifies
    // cleanly against production's pinned key.
    if genesis.authority_domain != authority_domain {
        return Err(GenesisError::WrongDomain);
    }

    // Two separate checks, and neither substitutes for the other. Membership in
    // the declared set rejects a profile this build cannot evaluate;
    // `profile_permits_voters` then rejects a count the profile forbids. Its
    // predicate is phrased as "not permitted", matching the `Option` idiom of
    // the other shape checks.
    if !shape::PROFILES.contains(&genesis.deployment_profile_code) {
        return Err(GenesisError::UnknownProfile(
            genesis.deployment_profile_code,
        ));
    }
    if shape::profile_permits_voters(
        genesis.deployment_profile_code,
        genesis.initial_voters.len(),
    ) {
        return Err(GenesisError::VoterCount {
            profile: genesis.deployment_profile_code,
            voters: genesis.initial_voters.len(),
        });
    }

    let mut initial_voters = Vec::with_capacity(genesis.initial_voters.len());
    for seed in &genesis.initial_voters {
        let node_id =
            NodeId::try_from(seed.node_id.as_slice()).map_err(|_| GenesisError::FieldWidth)?;
        if seed.ticket.len() > MAX_TICKET {
            return Err(GenesisError::VoterSeedTicket {
                limit: shape::ShapeLimit::MemberSeedTicket,
            });
        }
        // The same validator §9 applies to a member seed's ticket, so a genesis
        // seed and a membership seed cannot disagree about what a route is.
        // (`shape` is the owner; the voter-gated `membership` module re-exports
        // it, and this module compiles for the client graph too.)
        super::shape::check_seed_ticket(seed.ticket.as_bytes(), &node_id)
            .map_err(|limit| GenesisError::VoterSeedTicket { limit })?;
        initial_voters.push(VerifiedVoterSeed {
            node_id,
            ticket: seed.ticket.clone(),
        });
    }
    // Sorted and unique, checked on the derived ids rather than on the wire
    // vectors: §7.1 declares the set sorted before signing, so an unsorted one
    // is a different document from its sorted spelling and must be refused
    // rather than normalised.
    if initial_voters.is_empty()
        || initial_voters
            .windows(2)
            .any(|w| w[0].node_id >= w[1].node_id)
    {
        return Err(GenesisError::InitialVoters);
    }

    let mut initial_admins = Vec::with_capacity(genesis.initial_admins.len());
    for admin in &genesis.initial_admins {
        initial_admins.push(
            NodeId::try_from(admin.node_id.as_slice()).map_err(|_| GenesisError::FieldWidth)?,
        );
    }
    if initial_admins.is_empty()
        || initial_admins.len() as u64 > MAX_ADMINS_PER_GROUP
        || initial_admins.windows(2).any(|w| w[0] >= w[1])
    {
        return Err(GenesisError::InitialAdmins);
    }

    // **ROOT is neither a delegate nor a member**, and both are refused here so
    // no group can be founded holding either.
    //
    // A delegated ROOT would be a mutable row: `set_admin` could remove it and
    // the final-delegate rule would start counting it, so a group could "remove
    // ROOT" and reach a state where the key that founded it authorizes nothing.
    // §8.1 makes ROOT immutable, and the way to keep an immutable thing out of
    // a mutable table is to refuse the document that would put it there —
    // checking at apply would leave a signed genesis that verifies everywhere
    // and installs nowhere.
    //
    // A ROOT **voter or learner** is worse. ROOT's secret is the deployment's
    // offline trust anchor; a voter runs it as its Aster endpoint secret on a
    // live mesh node, which is exactly the exposure keeping ROOT offline
    // exists to avoid. A short-lived administration endpoint holding the secret
    // is fine — it is brought online deliberately, does one thing and stops. A
    // voter is a permanent, dialable, always-on holder of it.
    if initial_admins.contains(authority_public_key) {
        return Err(GenesisError::RootIsNotDelegable);
    }
    if initial_voters
        .iter()
        .any(|seed| &seed.node_id == authority_public_key)
    {
        return Err(GenesisError::RootIsNotAMember);
    }

    let group_id =
        <[u8; 32]>::try_from(genesis.group_id.as_slice()).map_err(|_| GenesisError::FieldWidth)?;

    Ok(VerifiedGenesis {
        genesis_hash: genesis_hash(&document_bytes),
        document_bytes,
        group_id,
        authority_domain: genesis.authority_domain,
        deployment_profile_code: genesis.deployment_profile_code,
        initial_voters,
        initial_admins,
        recovery_export_hash,
    })
}

/// `BLAKE3(GENESIS_DOMAIN || document_bytes)`.
///
/// Public so provisioning tooling can name a genesis before a voter has ever
/// opened storage, and so the §13 recovery flow can bind one. It is a *hash of
/// supplied bytes* and asserts nothing about them; only [`verify_genesis`]
/// produces a hash the group may act on, which is why it returns the digest
/// inside a sealed value rather than as a bare array.
pub fn genesis_hash(document_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(GENESIS_DOMAIN);
    hasher.update(document_bytes);
    *hasher.finalize().as_bytes()
}

// ── §7.1 provisioning ────────────────────────────────────────────────────────
//
// The voter runtime never holds an authority secret, so signing lives outside
// it. What Aster owns is the two byte-exact questions an external signer cannot
// answer for itself: what the canonical document encoding is, and what the
// domain-separated preimage is. Getting either wrong produces a signature that
// verifies nowhere, and a control tool that re-derived them by hand would be a
// second implementation of the rule this module enforces.

/// Canonically encode a signed-document root — the `document_bytes` an
/// authority signs and an envelope carries.
///
/// The re-encode check in [`verify_genesis`] compares against exactly this, so
/// a provisioning tool that uses it cannot mint a document that is refused as
/// non-canonical.
pub fn canonical_document_bytes<T>(document: &T) -> Result<Vec<u8>, EncodeError>
where
    T: crate::rpc::contract::WireField + fory_core::serializer::Serializer + fory_core::ForyDefault,
{
    payload_fory::<T>()
        .serialize(document)
        .map_err(|_| EncodeError)
}

/// The exact preimage an authority signs for a genesis document.
pub fn genesis_signing_bytes(document_bytes: &[u8]) -> Vec<u8> {
    let mut signed = Vec::with_capacity(GENESIS_DOMAIN.len() + document_bytes.len());
    signed.extend_from_slice(GENESIS_DOMAIN);
    signed.extend_from_slice(document_bytes);
    signed
}

/// The exact preimage an authority signs for a join permit.
pub fn join_signing_bytes(document_bytes: &[u8]) -> Vec<u8> {
    let mut signed = Vec::with_capacity(JOIN_DOMAIN.len() + document_bytes.len());
    signed.extend_from_slice(JOIN_DOMAIN);
    signed.extend_from_slice(document_bytes);
    signed
}

/// Assemble the signed envelope a voter is configured with.
///
/// Width-checked here rather than at verification time only, so a provisioning
/// mistake is caught by the tool that made it instead of by every voter at
/// start.
pub fn signed_document_from_parts(
    document_bytes: Vec<u8>,
    authority_public_key: &[u8; 32],
    signature: &[u8; 64],
) -> Result<Vec<u8>, EncodeError> {
    payload_fory::<SignedCoordinationDocumentV1>()
        .serialize(&SignedCoordinationDocumentV1 {
            format_version: PERMIT_FORMAT_VERSION,
            document_bytes,
            authority_public_key: authority_public_key.to_vec(),
            signature: signature.to_vec(),
        })
        .map_err(|_| EncodeError)
}

/// A canonical encoding failed.
///
/// `Fory::serialize` is fallible, so a provisioning helper that claimed an
/// infallible encode would be wrong in the same way §5.2.1 already records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeError;

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("canonical encoding failed")
    }
}

impl std::error::Error for EncodeError {}

#[cfg(test)]
pub(crate) mod testing {
    //! Minting real signed permits, so the negative tests differ from the
    //! positive one by exactly the field under test rather than by being
    //! unsigned.

    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// A deterministic authority. Not `rand`: a fixed seed keeps the golden
    /// bytes reproducible and the test independent of an RNG.
    pub fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[0x42u8; 32])
    }

    pub fn authority() -> GroupAuthority {
        GroupAuthority {
            public_key: signing_key().verifying_key().to_bytes(),
            domain: "aster0.net".into(),
            group_id: [0xABu8; 32],
            genesis_hash: [0x11u8; 32],
        }
    }

    pub fn permit_for(candidate: &NodeId, ticket: &[u8]) -> JoinPermitV1 {
        let a = authority();
        JoinPermitV1 {
            format_version: PERMIT_FORMAT_VERSION,
            group_id: a.group_id.to_vec(),
            authority_domain: a.domain.clone(),
            genesis_hash: a.genesis_hash.to_vec(),
            candidate_node_id: candidate.to_vec(),
            candidate_ticket: String::from_utf8_lossy(ticket).into_owned(),
            bootstrap_senders: vec![crate::coordination::raft::NodeIdV1 {
                node_id: vec![1u8; 32],
            }],
            join_nonce: vec![0x33u8; 32],
        }
    }

    /// Sign a permit under the test authority, producing the bytes a seed
    /// carries.
    pub fn sign(permit: &JoinPermitV1) -> Vec<u8> {
        let document_bytes = payload_fory::<JoinPermitV1>()
            .serialize(permit)
            .expect("encode");
        let mut signed = Vec::from(JOIN_DOMAIN);
        signed.extend_from_slice(&document_bytes);
        let signature = signing_key().sign(&signed);
        payload_fory::<SignedCoordinationDocumentV1>()
            .serialize(&SignedCoordinationDocumentV1 {
                format_version: PERMIT_FORMAT_VERSION,
                document_bytes,
                authority_public_key: signing_key().verifying_key().to_bytes().to_vec(),
                signature: signature.to_bytes().to_vec(),
            })
            .expect("encode")
    }

    /// A valid permit for `candidate` bound to `ticket`.
    pub fn valid(candidate: &NodeId, ticket: &[u8]) -> Vec<u8> {
        sign(&permit_for(candidate, ticket))
    }

    // ── genesis ─────────────────────────────────────────────────────────────

    /// A genesis for `voters` under the test authority, with real tickets.
    ///
    /// Real tickets rather than literals for the same reason
    /// [`membership::test_ticket`](super::super::membership::test_ticket) is:
    /// a fixture that could not pass the validator would let every negative
    /// test pass for the wrong reason.
    pub fn genesis_for(voters: &[NodeId], profile: i32) -> GenesisV1 {
        let a = authority();
        let mut initial_voters: Vec<_> = voters
            .iter()
            .map(|node| crate::coordination::raft::VoterSeedV1 {
                node_id: node.to_vec(),
                ticket: String::from_utf8(super::super::membership::test_ticket(node))
                    .expect("a ticket is ASCII base58"),
            })
            .collect();
        initial_voters.sort_by(|l, r| l.node_id.cmp(&r.node_id));
        let mut initial_admins: Vec<_> = voters
            .iter()
            .map(|node| crate::coordination::raft::NodeIdV1 {
                node_id: node.to_vec(),
            })
            .collect();
        initial_admins.sort_by(|l, r| l.node_id.cmp(&r.node_id));
        GenesisV1 {
            format_version: PERMIT_FORMAT_VERSION,
            group_id: a.group_id.to_vec(),
            authority_domain: a.domain.clone(),
            genesis_nonce: vec![0x5Au8; 32],
            deployment_profile_code: profile,
            initial_voters,
            initial_admins,
            recovery_export_hash: None,
        }
    }

    /// Sign a genesis under the test authority.
    pub fn sign_genesis(genesis: &GenesisV1) -> Vec<u8> {
        let document_bytes = canonical_document_bytes(genesis).expect("encode");
        let signature = signing_key().sign(&genesis_signing_bytes(&document_bytes));
        signed_document_from_parts(
            document_bytes,
            &signing_key().verifying_key().to_bytes(),
            &signature.to_bytes(),
        )
        .expect("encode")
    }

    /// Three production voters, signed. The common positive fixture.
    pub fn valid_genesis() -> Vec<u8> {
        sign_genesis(&genesis_for(
            &[[1u8; 32], [2u8; 32], [3u8; 32]],
            super::super::shape::PROFILE_PROD,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const CANDIDATE: NodeId = [7u8; 32];

    /// The one ticket a permit for `CANDIDATE` may carry. Derived, not spelled:
    /// a literal here would let the fixture and production disagree about what
    /// the field means, which is the defect `S3-JOIN-TICKET` records.
    fn ticket_bytes() -> Vec<u8> {
        canonical_candidate_ticket(&CANDIDATE)
            .expect("a canonical ticket")
            .into_bytes()
    }

    fn verify(bytes: &[u8]) -> Result<(), PermitError> {
        verify_join_permit(&authority(), bytes, &CANDIDATE)
    }

    #[test]
    fn a_correctly_signed_permit_verifies() {
        assert_eq!(verify(&valid(&CANDIDATE, &ticket_bytes())), Ok(()));
    }

    /// The case the old presence check accepted outright.
    #[test]
    fn an_empty_permit_is_not_a_permit() {
        assert_eq!(verify(&[]), Err(PermitError::Malformed));
        assert_eq!(verify(&[0u8; 4]), Err(PermitError::Malformed));
    }

    /// A permit signed by the wrong key. The envelope carries that key, so a
    /// verifier that trusted the envelope would accept it -- which is why §7.1
    /// pins the key and uses the envelope copy only as a diagnostic.
    #[test]
    fn a_permit_signed_by_another_authority_is_refused() {
        use ed25519_dalek::{Signer, SigningKey};

        let impostor = SigningKey::from_bytes(&[0x99u8; 32]);
        let permit = permit_for(&CANDIDATE, &ticket_bytes());
        let document_bytes = payload_fory::<JoinPermitV1>()
            .serialize(&permit)
            .expect("encode");
        let mut signed = Vec::from(JOIN_DOMAIN);
        signed.extend_from_slice(&document_bytes);
        let bytes = payload_fory::<SignedCoordinationDocumentV1>()
            .serialize(&SignedCoordinationDocumentV1 {
                format_version: PERMIT_FORMAT_VERSION,
                document_bytes,
                authority_public_key: impostor.verifying_key().to_bytes().to_vec(),
                signature: impostor.sign(&signed).to_bytes().to_vec(),
            })
            .expect("encode");

        assert_eq!(verify(&bytes), Err(PermitError::WrongAuthorityKey));
    }

    /// A permit whose contents were edited after signing.
    #[test]
    fn a_tampered_permit_fails_its_signature() {
        let good = valid(&CANDIDATE, &ticket_bytes());
        let mut envelope: SignedCoordinationDocumentV1 =
            payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&good)
                .expect("decode");

        // Re-point it at another candidate, keeping the original signature.
        let mut permit: JoinPermitV1 = payload_fory::<JoinPermitV1>()
            .deserialize(&envelope.document_bytes)
            .expect("decode");
        permit.candidate_node_id = vec![8u8; 32];
        envelope.document_bytes = payload_fory::<JoinPermitV1>()
            .serialize(&permit)
            .expect("encode");
        let bytes = payload_fory::<SignedCoordinationDocumentV1>()
            .serialize(&envelope)
            .expect("encode");

        assert_eq!(verify(&bytes), Err(PermitError::BadSignature));
    }

    /// Every binding, each broken on its own and **correctly signed**, so the
    /// test proves the binding is checked rather than the signature.
    #[test]
    fn each_mismatched_binding_is_refused_on_its_own() {
        let cases: Vec<(&str, JoinPermitV1, PermitError)> = vec![
            (
                "another deployment",
                JoinPermitV1 {
                    authority_domain: "aster1.net".into(),
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::WrongDomain,
            ),
            (
                "another group",
                JoinPermitV1 {
                    group_id: vec![0xCDu8; 32],
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::WrongGroup,
            ),
            (
                "a replacement group reusing the id",
                JoinPermitV1 {
                    genesis_hash: vec![0x22u8; 32],
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::WrongGenesis,
            ),
            (
                "another node",
                JoinPermitV1 {
                    candidate_node_id: vec![8u8; 32],
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::WrongNode,
            ),
            (
                "another ticket",
                JoinPermitV1 {
                    candidate_ticket: "aster1other".into(),
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::TicketMismatch,
            ),
            (
                "an empty ticket",
                JoinPermitV1 {
                    candidate_ticket: String::new(),
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::FieldWidth,
            ),
            (
                "no bootstrap senders",
                JoinPermitV1 {
                    bootstrap_senders: Vec::new(),
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::BootstrapSenders,
            ),
            (
                "unsorted bootstrap senders",
                JoinPermitV1 {
                    bootstrap_senders: vec![
                        crate::coordination::raft::NodeIdV1 {
                            node_id: vec![2u8; 32],
                        },
                        crate::coordination::raft::NodeIdV1 {
                            node_id: vec![1u8; 32],
                        },
                    ],
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::BootstrapSenders,
            ),
            (
                "a future format",
                JoinPermitV1 {
                    format_version: 2,
                    ..permit_for(&CANDIDATE, &ticket_bytes())
                },
                PermitError::UnknownFormat(2),
            ),
        ];

        for (name, permit, expected) in cases {
            assert_eq!(verify(&sign(&permit)), Err(expected), "{name}");
        }
    }

    /// **A permit may not carry address material, even for the right node.**
    ///
    /// This is the binding that changed. An Aster node binds an ephemeral UDP
    /// port and is resolved from its NodeId by discovery, so a permit that
    /// bound the candidate's rendered address would stop verifying the moment
    /// that candidate restarted — the candidate could never come back. The
    /// field now carries exactly one value, derived from the NodeId, and an
    /// address-bearing ticket *for this very candidate* is refused.
    #[test]
    fn a_permit_may_not_bind_the_candidates_address() {
        let addressed = aster_transport_core::ticket::AsterTicket {
            endpoint_id: CANDIDATE,
            relay: None,
            direct_addrs: vec!["127.0.0.1:47731".parse().expect("an address")],
            credential: Some(aster_transport_core::ticket::TicketCredential::Open),
        }
        .to_base58_string()
        .expect("a well-formed ticket");
        assert_ne!(
            addressed.as_bytes(),
            ticket_bytes(),
            "the fixture must actually differ from the canonical form"
        );

        let bytes = sign(&JoinPermitV1 {
            candidate_ticket: addressed,
            ..permit_for(&CANDIDATE, &ticket_bytes())
        });
        assert_eq!(verify(&bytes), Err(PermitError::TicketMismatch));
    }

    /// And a canonical ticket for a *different* node is refused too, which is
    /// what stops a permit authorising route material for someone else.
    #[test]
    fn a_valid_permit_does_not_authorise_another_nodes_ticket() {
        let bytes = sign(&JoinPermitV1 {
            candidate_ticket: canonical_candidate_ticket(&[9u8; 32]).expect("canonical"),
            ..permit_for(&CANDIDATE, &ticket_bytes())
        });
        assert_eq!(verify(&bytes), Err(PermitError::TicketMismatch));
    }

    // ── the candidate's own view of its permit ──────────────────────────────

    fn verify_start(bytes: &[u8]) -> Result<VerifiedJoinPermit, PermitError> {
        let a = authority();
        verify_join_permit_for_start(&a.public_key, &a.domain, bytes, &CANDIDATE)
    }

    /// The candidate learns the group from the permit, and what it learns is
    /// what was signed.
    ///
    /// The oracle is the permit fixture's own fields, read here rather than
    /// through the sealed value, so a verifier that returned defaults or
    /// swapped the two 32-byte identifiers fails.
    #[test]
    fn a_candidate_learns_its_group_from_a_permit_it_verified() {
        let verified = verify_start(&valid(&CANDIDATE, &ticket_bytes())).expect("verify");
        let expected = permit_for(&CANDIDATE, &ticket_bytes());

        assert_eq!(verified.group_id(), &authority().group_id);
        assert_eq!(verified.genesis_hash(), &authority().genesis_hash);
        assert_eq!(verified.group_id().to_vec(), expected.group_id);
        assert_eq!(verified.genesis_hash().to_vec(), expected.genesis_hash);
        assert_eq!(verified.candidate(), &CANDIDATE);
        assert_eq!(verified.bootstrap_senders(), &[[1u8; 32]]);
        assert!(verified.permits_sender(&[1u8; 32]));
        assert!(!verified.permits_sender(&[2u8; 32]));
    }

    /// The sealed value carries the bytes whose signature was checked, not a
    /// re-encoding.
    #[test]
    fn a_verified_permit_carries_the_exact_signed_bytes() {
        let bytes = valid(&CANDIDATE, &ticket_bytes());
        let verified = verify_start(&bytes).expect("verify");
        let envelope: SignedCoordinationDocumentV1 = payload_fory::<SignedCoordinationDocumentV1>()
            .deserialize(&bytes)
            .expect("decode");
        assert_eq!(
            verified.document_bytes(),
            envelope.document_bytes.as_slice()
        );
    }

    /// The three bindings a candidate *can* check, each broken on its own and
    /// correctly signed.
    ///
    /// The group id and genesis hash are deliberately absent from this list:
    /// they are extracted rather than compared, because a node that belongs to
    /// no group yet has no independent expectation to compare them against.
    /// See [`VerifiedJoinPermit`].
    #[test]
    fn the_startup_verifier_checks_domain_candidate_and_ticket() {
        let a = authority();

        let wrong_domain = sign(&JoinPermitV1 {
            authority_domain: "aster1.net".into(),
            ..permit_for(&CANDIDATE, &ticket_bytes())
        });
        assert_eq!(verify_start(&wrong_domain), Err(PermitError::WrongDomain));

        let for_another_node = valid(&[8u8; 32], &ticket_bytes());
        assert_eq!(verify_start(&for_another_node), Err(PermitError::WrongNode));

        assert_eq!(
            verify_start(&sign(&JoinPermitV1 {
                candidate_ticket: canonical_candidate_ticket(&[9u8; 32]).expect("canonical"),
                ..permit_for(&CANDIDATE, &ticket_bytes())
            })),
            Err(PermitError::TicketMismatch)
        );

        // And a permit minted by another authority never reaches any of them.
        assert_eq!(
            verify_join_permit_for_start(
                &[0u8; 32],
                &a.domain,
                &valid(&CANDIDATE, &ticket_bytes()),
                &CANDIDATE,
            ),
            Err(PermitError::WrongAuthorityKey)
        );
    }

    /// A permit naming another group verifies at startup — and that is correct,
    /// not a gap.
    ///
    /// The candidate has nothing to compare the group id against; the permit is
    /// what tells it which group it may speak to. This test exists so the
    /// property is stated rather than assumed, and so a later change that
    /// invented a comparison here has to justify what it compares against.
    #[test]
    fn a_permit_for_another_group_is_learned_rather_than_refused() {
        let elsewhere = sign(&JoinPermitV1 {
            group_id: vec![0xCDu8; 32],
            genesis_hash: vec![0x22u8; 32],
            ..permit_for(&CANDIDATE, &ticket_bytes())
        });
        let verified = verify_start(&elsewhere).expect("a signed permit for another group");
        assert_eq!(verified.group_id(), &[0xCDu8; 32]);
        assert_eq!(verified.genesis_hash(), &[0x22u8; 32]);

        // The apply-time verifier, which *does* hold an expectation, refuses
        // the same bytes. The two answers differ because the questions do.
        assert_eq!(
            verify_join_permit(&authority(), &elsewhere, &CANDIDATE),
            Err(PermitError::WrongGroup)
        );
    }

    /// The sender list is the candidate's whole admission policy until
    /// membership lands, so its shape is enforced before it is relied on.
    #[test]
    fn the_startup_verifier_enforces_the_sender_list_shape() {
        let empty = sign(&JoinPermitV1 {
            bootstrap_senders: Vec::new(),
            ..permit_for(&CANDIDATE, &ticket_bytes())
        });
        assert_eq!(verify_start(&empty), Err(PermitError::BootstrapSenders));

        let unsorted = sign(&JoinPermitV1 {
            bootstrap_senders: vec![
                crate::coordination::raft::NodeIdV1 {
                    node_id: vec![2u8; 32],
                },
                crate::coordination::raft::NodeIdV1 {
                    node_id: vec![1u8; 32],
                },
            ],
            ..permit_for(&CANDIDATE, &ticket_bytes())
        });
        assert_eq!(verify_start(&unsorted), Err(PermitError::BootstrapSenders));
    }

    // ── genesis ─────────────────────────────────────────────────────────────

    use super::super::shape::{PROFILE_DEV_EVEN, PROFILE_DEV_SINGLE, PROFILE_PROD};

    fn verify_g(bytes: &[u8]) -> Result<VerifiedGenesis, GenesisError> {
        let a = authority();
        verify_genesis(&a.public_key, &a.domain, bytes)
    }

    #[test]
    fn a_correctly_signed_genesis_verifies_and_carries_what_it_verified() {
        let bytes = valid_genesis();
        let verified = verify_g(&bytes).expect("a signed genesis verifies");

        assert_eq!(verified.group_id(), &authority().group_id);
        assert_eq!(verified.authority_domain(), authority().domain);
        assert_eq!(verified.deployment_profile_code(), PROFILE_PROD);
        assert_eq!(verified.initial_voters().len(), 3);
        assert_eq!(verified.initial_admins().len(), 3);
        assert_eq!(verified.recovery_export_hash(), None);
        assert!(verified.is_initial_voter(&[2u8; 32]));
        assert!(!verified.is_initial_voter(&[9u8; 32]));
    }

    /// The property the sealing exists for: what comes out is what was
    /// verified, byte for byte.
    ///
    /// The oracle is independent of the verifier — the envelope is decoded here
    /// and its `document_bytes` compared — so this fails if the type ever hands
    /// back a re-encoding rather than the bytes whose signature was checked.
    #[test]
    fn the_sealed_value_carries_the_exact_bytes_that_were_signed() {
        let bytes = valid_genesis();
        let verified = verify_g(&bytes).expect("verify");

        let envelope: SignedCoordinationDocumentV1 = payload_fory::<SignedCoordinationDocumentV1>()
            .deserialize(&bytes)
            .expect("decode");
        assert_eq!(
            verified.document_bytes(),
            envelope.document_bytes.as_slice()
        );
    }

    /// The hash is derived from the signed bytes and the domain, not read from
    /// the document — `GenesisV1` has no hash field to read.
    ///
    /// Computed here from an independently written preimage rather than by
    /// calling `genesis_hash`, so a change to the domain separator or the
    /// ordering fails rather than agreeing with itself.
    #[test]
    fn the_derived_hash_is_blake3_over_the_domain_and_the_signed_bytes() {
        let bytes = valid_genesis();
        let verified = verify_g(&bytes).expect("verify");

        let mut preimage = Vec::from(&b"aster-coordination/genesis/v1\0"[..]);
        preimage.extend_from_slice(verified.document_bytes());
        assert_eq!(verified.genesis_hash(), blake3::hash(&preimage).as_bytes());
    }

    /// Two genesis documents differing in one field have different hashes, so
    /// the hash actually identifies the document.
    #[test]
    fn a_different_genesis_derives_a_different_hash() {
        let a = verify_g(&valid_genesis()).expect("verify");
        let other = sign_genesis(&GenesisV1 {
            genesis_nonce: vec![0x77u8; 32],
            ..genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD)
        });
        let b = verify_g(&other).expect("verify");
        assert_ne!(a.genesis_hash(), b.genesis_hash());
    }

    /// The genesis domain is not the join domain. A signature minted over one
    /// preimage must not verify against the other, or an authority's permit
    /// could be replayed as a group's founding document.
    #[test]
    fn a_join_signature_does_not_verify_as_a_genesis() {
        use ed25519_dalek::Signer;

        // Same document bytes, signed under the *join* domain.
        let genesis = genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD);
        let document_bytes = canonical_document_bytes(&genesis).expect("encode");
        let signature = signing_key().sign(&join_signing_bytes(&document_bytes));
        let bytes = signed_document_from_parts(
            document_bytes,
            &signing_key().verifying_key().to_bytes(),
            &signature.to_bytes(),
        )
        .expect("encode");

        assert_eq!(verify_g(&bytes), Err(GenesisError::BadSignature));
    }

    #[test]
    fn a_genesis_signed_by_another_authority_is_refused() {
        use ed25519_dalek::{Signer, SigningKey};

        let impostor = SigningKey::from_bytes(&[0x99u8; 32]);
        let genesis = genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD);
        let document_bytes = canonical_document_bytes(&genesis).expect("encode");
        let signature = impostor.sign(&genesis_signing_bytes(&document_bytes));
        let bytes = signed_document_from_parts(
            document_bytes,
            &impostor.verifying_key().to_bytes(),
            &signature.to_bytes(),
        )
        .expect("encode");

        assert_eq!(verify_g(&bytes), Err(GenesisError::WrongAuthorityKey));
    }

    /// A genesis edited after signing.
    #[test]
    fn a_tampered_genesis_fails_its_signature() {
        let good = valid_genesis();
        let mut envelope: SignedCoordinationDocumentV1 =
            payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&good)
                .expect("decode");
        let mut genesis: GenesisV1 = payload_fory::<GenesisV1>()
            .deserialize(&envelope.document_bytes)
            .expect("decode");
        genesis.deployment_profile_code = PROFILE_DEV_SINGLE;
        envelope.document_bytes = canonical_document_bytes(&genesis).expect("encode");
        let bytes = payload_fory::<SignedCoordinationDocumentV1>()
            .serialize(&envelope)
            .expect("encode");

        assert_eq!(verify_g(&bytes), Err(GenesisError::BadSignature));
    }

    /// A correctly signed genesis for another deployment. The signature is
    /// *good*, which is exactly why the configured domain has to be compared.
    #[test]
    fn a_genesis_for_another_deployment_is_refused_under_the_same_key() {
        let bytes = sign_genesis(&GenesisV1 {
            authority_domain: "aster1.net".into(),
            ..genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD)
        });
        assert_eq!(verify_g(&bytes), Err(GenesisError::WrongDomain));
    }

    /// Every profile in the declared set admits its own voter count, and the
    /// cases are iterated from `shape::PROFILES` rather than matched against
    /// literals — a fourth profile added without a rule here fails the test
    /// rather than silently going unchecked.
    #[test]
    fn each_declared_profile_admits_exactly_the_counts_it_permits() {
        let nodes: Vec<NodeId> = (1..=6u8).map(|n| [n; 32]).collect();
        for profile in super::super::shape::PROFILES {
            let mut admitted = Vec::new();
            for count in 1..=6usize {
                let bytes = sign_genesis(&genesis_for(&nodes[..count], profile));
                if verify_g(&bytes).is_ok() {
                    admitted.push(count);
                }
            }
            // The independent oracle: the shape predicate, consulted directly.
            let expected: Vec<usize> = (1..=6usize)
                .filter(|c| !super::super::shape::profile_permits_voters(profile, *c))
                .collect();
            assert_eq!(
                admitted, expected,
                "profile {profile} admitted {admitted:?}, the shape rule permits {expected:?}"
            );
            assert!(
                !admitted.is_empty(),
                "profile {profile} admits no voter count at all"
            );
        }
    }

    /// A profile code outside the declared set is refused *as a profile*, not
    /// as a voter count. Those are different operator fixes.
    #[test]
    fn an_undeclared_profile_is_refused_by_name() {
        for code in [0, 4, -1, i32::MAX] {
            let bytes = sign_genesis(&genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], code));
            assert_eq!(verify_g(&bytes), Err(GenesisError::UnknownProfile(code)));
        }
    }

    /// The dev profiles are reachable — a rule that refused them would make the
    /// count test above pass while no dev group could ever start.
    #[test]
    fn the_development_profiles_verify_at_their_own_sizes() {
        let one = sign_genesis(&genesis_for(&[[1u8; 32]], PROFILE_DEV_SINGLE));
        assert_eq!(
            verify_g(&one)
                .expect("one voter is legal in profile 2")
                .initial_voters()
                .len(),
            1
        );
        let two = sign_genesis(&genesis_for(&[[1u8; 32], [2u8; 32]], PROFILE_DEV_EVEN));
        assert_eq!(
            verify_g(&two)
                .expect("two voters are legal in profile 3")
                .initial_voters()
                .len(),
            2
        );
    }

    /// Every binding broken on its own and **correctly signed**, so each case
    /// proves the binding is checked rather than the signature.
    #[test]
    fn each_malformed_genesis_field_is_refused_on_its_own() {
        let base = || genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD);
        let cases: Vec<(&str, GenesisV1, GenesisError)> = vec![
            (
                "a future format",
                GenesisV1 {
                    format_version: 2,
                    ..base()
                },
                GenesisError::UnknownFormat(2),
            ),
            (
                "a short group id",
                GenesisV1 {
                    group_id: vec![0u8; 31],
                    ..base()
                },
                GenesisError::FieldWidth,
            ),
            (
                "a short nonce",
                GenesisV1 {
                    genesis_nonce: vec![0u8; 16],
                    ..base()
                },
                GenesisError::FieldWidth,
            ),
            (
                "an empty domain",
                GenesisV1 {
                    authority_domain: String::new(),
                    ..base()
                },
                GenesisError::FieldWidth,
            ),
            (
                "a non-ASCII domain",
                GenesisV1 {
                    authority_domain: "astér.net".into(),
                    ..base()
                },
                GenesisError::FieldWidth,
            ),
            (
                "an over-long domain",
                GenesisV1 {
                    authority_domain: "a".repeat(MAX_AUTHORITY_DOMAIN_BYTES + 1),
                    ..base()
                },
                GenesisError::FieldWidth,
            ),
            (
                "a truncated recovery export hash",
                GenesisV1 {
                    recovery_export_hash: Some(vec![0u8; 16]),
                    ..base()
                },
                GenesisError::FieldWidth,
            ),
            (
                "no admins",
                GenesisV1 {
                    initial_admins: Vec::new(),
                    ..base()
                },
                GenesisError::InitialAdmins,
            ),
            (
                "unsorted admins",
                GenesisV1 {
                    initial_admins: vec![
                        crate::coordination::raft::NodeIdV1 {
                            node_id: vec![2u8; 32],
                        },
                        crate::coordination::raft::NodeIdV1 {
                            node_id: vec![1u8; 32],
                        },
                    ],
                    ..base()
                },
                GenesisError::InitialAdmins,
            ),
            (
                "duplicated admins",
                GenesisV1 {
                    initial_admins: vec![
                        crate::coordination::raft::NodeIdV1 {
                            node_id: vec![1u8; 32],
                        },
                        crate::coordination::raft::NodeIdV1 {
                            node_id: vec![1u8; 32],
                        },
                    ],
                    ..base()
                },
                GenesisError::InitialAdmins,
            ),
        ];

        for (name, genesis, expected) in cases {
            assert_eq!(verify_g(&sign_genesis(&genesis)), Err(expected), "{name}");
        }
    }

    /// The voter set's own ordering rules, separated from the field-width cases
    /// because an unsorted set is a *different document* from its sorted
    /// spelling rather than a malformed one.
    #[test]
    fn an_unsorted_or_duplicated_voter_set_is_refused() {
        let unsorted = sign_genesis(&GenesisV1 {
            initial_voters: vec![
                crate::coordination::raft::VoterSeedV1 {
                    node_id: vec![3u8; 32],
                    ticket: String::from_utf8(super::super::membership::test_ticket(&[3u8; 32]))
                        .expect("ascii"),
                },
                crate::coordination::raft::VoterSeedV1 {
                    node_id: vec![1u8; 32],
                    ticket: String::from_utf8(super::super::membership::test_ticket(&[1u8; 32]))
                        .expect("ascii"),
                },
                crate::coordination::raft::VoterSeedV1 {
                    node_id: vec![2u8; 32],
                    ticket: String::from_utf8(super::super::membership::test_ticket(&[2u8; 32]))
                        .expect("ascii"),
                },
            ],
            ..genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD)
        });
        assert_eq!(verify_g(&unsorted), Err(GenesisError::InitialVoters));
    }

    /// A seed ticket that names another node. Without this, a genesis may seat
    /// node A while installing a route that dials node B — and the whole group
    /// would agree on it, because the document is correctly signed.
    #[test]
    fn a_voter_seed_ticket_for_another_node_is_refused() {
        let mut genesis = genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD);
        genesis.initial_voters[0].ticket =
            String::from_utf8(super::super::membership::test_ticket(&[9u8; 32])).expect("ascii");
        assert_eq!(
            verify_g(&sign_genesis(&genesis)),
            Err(GenesisError::VoterSeedTicket {
                limit: super::super::shape::ShapeLimit::MemberSeedTicketNode
            })
        );
    }

    /// A credential-bearing ticket. A route row is replicated state every voter
    /// and every future snapshot reader holds, so a credential in one is a
    /// secret handed to everyone who can read the group.
    #[test]
    fn a_voter_seed_ticket_carrying_a_credential_is_refused() {
        let mut genesis = genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD);
        genesis.initial_voters[0].ticket = aster_transport_core::ticket::AsterTicket {
            endpoint_id: [1u8; 32],
            relay: None,
            direct_addrs: Vec::new(),
            credential: Some(aster_transport_core::ticket::TicketCredential::Enrollment(
                "secret".into(),
            )),
        }
        .to_base58_string()
        .expect("a well-formed ticket");
        assert_eq!(
            verify_g(&sign_genesis(&genesis)),
            Err(GenesisError::VoterSeedTicket {
                limit: super::super::shape::ShapeLimit::MemberSeedTicketCredential
            })
        );
    }

    #[test]
    fn an_empty_genesis_is_not_a_genesis() {
        assert_eq!(verify_g(&[]), Err(GenesisError::Malformed));
        assert_eq!(verify_g(&[0u8; 4]), Err(GenesisError::Malformed));
    }

    /// A recovery-bound genesis carries its export hash through verification —
    /// §7.2 makes the export path mandatory when it is present, so a verifier
    /// that dropped it would make that rule unenforceable.
    #[test]
    fn a_recovery_bound_genesis_carries_its_export_hash() {
        let bytes = sign_genesis(&GenesisV1 {
            recovery_export_hash: Some(vec![0xEEu8; 32]),
            ..genesis_for(&[[1u8; 32], [2u8; 32], [3u8; 32]], PROFILE_PROD)
        });
        let verified = verify_g(&bytes).expect("verify");
        assert_eq!(verified.recovery_export_hash(), Some(&[0xEEu8; 32]));
    }

    /// The provisioning helpers produce exactly what verification consumes.
    /// Written as a round trip through the *public* helpers so a control tool
    /// that uses them cannot mint a document every voter refuses.
    #[test]
    fn the_provisioning_helpers_round_trip_through_verification() {
        use ed25519_dalek::Signer;

        let genesis = genesis_for(&[[4u8; 32], [5u8; 32], [6u8; 32]], PROFILE_PROD);
        let document_bytes = canonical_document_bytes(&genesis).expect("encode");
        let preimage = genesis_signing_bytes(&document_bytes);
        assert!(preimage.starts_with(GENESIS_DOMAIN));
        assert_eq!(&preimage[GENESIS_DOMAIN.len()..], &document_bytes[..]);

        let signature = signing_key().sign(&preimage);
        let bytes = signed_document_from_parts(
            document_bytes,
            &signing_key().verifying_key().to_bytes(),
            &signature.to_bytes(),
        )
        .expect("encode");
        assert!(verify_g(&bytes).is_ok());
    }
}
