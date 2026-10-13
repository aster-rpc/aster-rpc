//! Delegation chains: bounded authority handed down as compact Ed25519 JWTs
//! and verified offline in one call.
//!
//! A root delegates to an online issuer ([`issue_delegation`]), the issuer
//! enrolls nodes ([`issue_enrollment`]), and a principal lets an actor act for
//! it ([`issue_action`]). [`verify_chain`] is the only way to obtain a
//! [`VerifiedChain`]: it checks size, structure, attenuation and revocation
//! before any signature. Root pre-rotation is in [`verify_root_rotation`].

pub use aster_transport_core::delegation::{
    next_root_commitment, verify_chain, verify_root_rotation, ActionGrant, AdmissionClaims,
    Audience, Chain, ChainError, DelegationGrant, EnrollmentExtensionV1, LeafClaims, LeafKind,
    LinkKind, NoRevocations, RevokedDelegates, RootRotation, RootState, RotationReason,
    TrustContext, TypedAttribute, Validity, VerifiedChain, ACTION_JWT_TYPE, CHAIN_PREFIX,
    DEFAULT_LEEWAY_SECONDS, DEFAULT_MAX_LINKS, DELEGATION_JWT_TYPE, ENROLLMENT_LINK_TYPE,
    MAX_CHAIN_BYTES, MAX_LEEWAY_SECONDS, MAX_LINKS, MAX_LINK_BYTES, MAX_LINK_LIFETIME,
    ROOT_ROTATION_JWT_TYPE,
};

use aster_transport_core::delegation as core;

use crate::{PublicKey, SecretKey};

/// Delegate bounded issuing authority to `subject`. `parent: None` means the
/// signer is an anchor; otherwise the signer must be the parent's leaf
/// subject. Refuses anything a verifier would reject.
pub fn issue_delegation(
    signer: &SecretKey,
    parent: Option<&Chain>,
    subject: &PublicKey,
    audience: &Audience,
    grant: &DelegationGrant,
    validity: Validity,
) -> Result<Chain, ChainError> {
    core::issue_delegation(
        &signer.to_bytes(),
        parent,
        &subject.to_bytes(),
        audience,
        grant,
        validity,
    )
}

/// Enroll `subject`, as an anchor (`parent: None`) or as a delegated issuer.
pub fn issue_enrollment(
    signer: &SecretKey,
    parent: Option<&Chain>,
    subject: &PublicKey,
    audience: &Audience,
    claims: &EnrollmentExtensionV1,
    validity: Validity,
) -> Result<Chain, ChainError> {
    core::issue_enrollment(
        &signer.to_bytes(),
        parent,
        &subject.to_bytes(),
        audience,
        claims,
        validity,
    )
}

/// Let `subject` act on the anchor principal's behalf within `grant`.
pub fn issue_action(
    signer: &SecretKey,
    parent: Option<&Chain>,
    subject: &PublicKey,
    audience: &Audience,
    grant: &ActionGrant,
    validity: Validity,
) -> Result<Chain, ChainError> {
    core::issue_action(
        &signer.to_bytes(),
        parent,
        &subject.to_bytes(),
        audience,
        grant,
        validity,
    )
}

/// Sign a root rotation with the committed next root's secret key.
pub fn issue_root_rotation(
    next_root: &SecretKey,
    previous_root: &PublicKey,
    seq: u64,
    following_commitment: &[u8; 32],
    namespaces: &[[u8; 32]],
    issued_at: u64,
    reason: RotationReason,
) -> Result<String, ChainError> {
    core::issue_root_rotation(
        &next_root.to_bytes(),
        &previous_root.to_bytes(),
        seq,
        following_commitment,
        namespaces,
        issued_at,
        reason,
    )
}
