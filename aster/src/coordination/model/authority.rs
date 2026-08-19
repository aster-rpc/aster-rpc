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
use crate::coordination::raft::{JoinPermitV1, SignedCoordinationDocumentV1};

/// §7.1's join-permit signing domain. The trailing NUL is part of it.
pub const JOIN_DOMAIN: &[u8] = b"aster-coordination/join/v1\0";

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
    /// The permit's ticket is not the ticket the seed carries.
    TicketMismatch,
    /// The bootstrap-sender list is empty, unsorted, or has duplicates.
    BootstrapSenders,
    /// A fixed-width field was the wrong width, or a bounded one too long.
    FieldWidth,
}

/// Verify one seed's signed join permit against the group's authority.
///
/// `ticket` is the seed's own ticket: §9 requires the permit to bind the route
/// material it authorises, so a permit for a node says nothing about a ticket it
/// was not minted with.
pub fn verify_join_permit(
    authority: &GroupAuthority,
    permit_bytes: &[u8],
    candidate: &NodeId,
    ticket: &[u8],
) -> Result<(), PermitError> {
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
    if envelope.authority_public_key != authority.public_key.to_vec() {
        return Err(PermitError::WrongAuthorityKey);
    }

    let key =
        VerifyingKey::from_bytes(&authority.public_key).map_err(|_| PermitError::Malformed)?;
    let signature =
        Signature::from_slice(&envelope.signature).map_err(|_| PermitError::Malformed)?;
    let mut signed = Vec::with_capacity(JOIN_DOMAIN.len() + envelope.document_bytes.len());
    signed.extend_from_slice(JOIN_DOMAIN);
    signed.extend_from_slice(&envelope.document_bytes);
    key.verify(&signed, &signature)
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

    check_permit(authority, &permit, candidate, ticket)
}

/// The bindings a verified permit must carry, checked after the signature.
///
/// Order matters only for which error an operator sees first, and it goes from
/// "this permit is for someone else entirely" to "this permit is for you but
/// does not match this seed".
fn check_permit(
    authority: &GroupAuthority,
    permit: &JoinPermitV1,
    candidate: &NodeId,
    ticket: &[u8],
) -> Result<(), PermitError> {
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
    if permit.candidate_ticket.as_bytes() != ticket {
        return Err(PermitError::TicketMismatch);
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
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const CANDIDATE: NodeId = [7u8; 32];
    const TICKET: &[u8] = b"aster1ticket";

    fn verify(bytes: &[u8]) -> Result<(), PermitError> {
        verify_join_permit(&authority(), bytes, &CANDIDATE, TICKET)
    }

    #[test]
    fn a_correctly_signed_permit_verifies() {
        assert_eq!(verify(&valid(&CANDIDATE, TICKET)), Ok(()));
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
        let permit = permit_for(&CANDIDATE, TICKET);
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
        let good = valid(&CANDIDATE, TICKET);
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
                    ..permit_for(&CANDIDATE, TICKET)
                },
                PermitError::WrongDomain,
            ),
            (
                "another group",
                JoinPermitV1 {
                    group_id: vec![0xCDu8; 32],
                    ..permit_for(&CANDIDATE, TICKET)
                },
                PermitError::WrongGroup,
            ),
            (
                "a replacement group reusing the id",
                JoinPermitV1 {
                    genesis_hash: vec![0x22u8; 32],
                    ..permit_for(&CANDIDATE, TICKET)
                },
                PermitError::WrongGenesis,
            ),
            (
                "another node",
                JoinPermitV1 {
                    candidate_node_id: vec![8u8; 32],
                    ..permit_for(&CANDIDATE, TICKET)
                },
                PermitError::WrongNode,
            ),
            (
                "another ticket",
                JoinPermitV1 {
                    candidate_ticket: "aster1other".into(),
                    ..permit_for(&CANDIDATE, TICKET)
                },
                PermitError::TicketMismatch,
            ),
            (
                "an empty ticket",
                JoinPermitV1 {
                    candidate_ticket: String::new(),
                    ..permit_for(&CANDIDATE, TICKET)
                },
                PermitError::FieldWidth,
            ),
            (
                "no bootstrap senders",
                JoinPermitV1 {
                    bootstrap_senders: Vec::new(),
                    ..permit_for(&CANDIDATE, TICKET)
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
                    ..permit_for(&CANDIDATE, TICKET)
                },
                PermitError::BootstrapSenders,
            ),
            (
                "a future format",
                JoinPermitV1 {
                    format_version: 2,
                    ..permit_for(&CANDIDATE, TICKET)
                },
                PermitError::UnknownFormat(2),
            ),
        ];

        for (name, permit, expected) in cases {
            assert_eq!(verify(&sign(&permit)), Err(expected), "{name}");
        }
    }

    /// The ticket binding is what stops a valid permit authorising route
    /// material it was not minted with.
    #[test]
    fn a_valid_permit_does_not_authorise_a_different_ticket() {
        let bytes = valid(&CANDIDATE, TICKET);
        assert_eq!(
            verify_join_permit(&authority(), &bytes, &CANDIDATE, b"aster1substituted"),
            Err(PermitError::TicketMismatch),
        );
    }
}
