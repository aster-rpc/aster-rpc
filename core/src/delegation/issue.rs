//! Issuance. Every `issue_*` signs one link, prepends it to the issuer's own
//! chain, and then runs the verifier's gates over the result. It refuses to
//! return anything `verify_chain` would reject for that subject, audience and
//! time (INV-A15). Only restriction support, which is the eventual verifier's
//! decision, and a future `nbf` on the new link are exempt.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

use super::jws;
use super::links::{
    decode_link, valid_key, ActionGrant, Audience, Confirmation, DelegationGrant,
    EnrollmentExtensionV1, LinkKind, LinkPayload,
};
use super::verify::{gates, Mode};
use super::{split, Chain, ChainError, NoRevocations, TrustContext, MAX_LINKS, MAX_LINK_BYTES};

/// Signed validity of a new link, in UTC seconds. `expires_at` is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    pub issued_at: u64,
    pub not_before: Option<u64>,
    pub expires_at: u64,
}

/// Delegate bounded issuing authority to `subject`. `parent: None` means the
/// signer is an anchor; otherwise the signer must be the parent's leaf subject.
pub fn issue_delegation(
    signer: &[u8; 32],
    parent: Option<&Chain>,
    subject: &[u8; 32],
    audience: &Audience,
    grant: &DelegationGrant,
    validity: Validity,
) -> Result<Chain, ChainError> {
    issue(
        signer,
        parent,
        subject,
        audience,
        validity,
        LinkKind::Delegation,
        |payload| payload.aster_delegation_v1 = Some(grant.clone()),
    )
}

/// Enroll `subject` with the given claims. Malformed reserved locality
/// attributes (`aster.locality.*`) are refused as [`ChainError::Malformed`]:
/// readers would ignore them, so signing them would only hide the mistake.
pub fn issue_enrollment(
    signer: &[u8; 32],
    parent: Option<&Chain>,
    subject: &[u8; 32],
    audience: &Audience,
    claims: &EnrollmentExtensionV1,
    validity: Validity,
) -> Result<Chain, ChainError> {
    if claims.claims.declared_locality().is_err() {
        return Err(ChainError::Malformed);
    }
    issue(
        signer,
        parent,
        subject,
        audience,
        validity,
        LinkKind::Enrollment,
        |payload| payload.aster_enrollment_v1 = Some(claims.clone()),
    )
}

/// Let `subject` act for the anchor principal within `grant`.
pub fn issue_action(
    signer: &[u8; 32],
    parent: Option<&Chain>,
    subject: &[u8; 32],
    audience: &Audience,
    grant: &ActionGrant,
    validity: Validity,
) -> Result<Chain, ChainError> {
    issue(
        signer,
        parent,
        subject,
        audience,
        validity,
        LinkKind::Action,
        |payload| payload.aster_action_v1 = Some(grant.clone()),
    )
}

fn issue(
    signer: &[u8; 32],
    parent: Option<&Chain>,
    subject: &[u8; 32],
    audience: &Audience,
    validity: Validity,
    kind: LinkKind,
    extension: impl FnOnce(&mut LinkPayload),
) -> Result<Chain, ChainError> {
    if !valid_key(subject) {
        return Err(ChainError::Malformed);
    }
    let issuer = jws::public_key(signer).map_err(|_| ChainError::Issuance)?;
    let mut jti = [0; 16];
    getrandom::getrandom(&mut jti).map_err(|_| ChainError::Issuance)?;
    let mut payload = LinkPayload {
        iss: hex::encode(issuer),
        sub: hex::encode(subject),
        cnf: Confirmation::ed25519(subject),
        aud: audience.clone(),
        jti: URL_SAFE_NO_PAD.encode(jti),
        iat: validity.issued_at,
        nbf: validity.not_before,
        exp: validity.expires_at,
        aster_delegation_v1: None,
        aster_enrollment_v1: None,
        aster_action_v1: None,
    };
    extension(&mut payload);
    let payload = serde_json::to_vec(&payload).map_err(|_| ChainError::Malformed)?;
    // Bound the complete link before signing.
    if jws::encoded_len(kind.jwt_type(), payload.len()) > MAX_LINK_BYTES {
        return Err(ChainError::Size);
    }
    let link = jws::sign(kind.jwt_type(), &payload, signer).map_err(|_| ChainError::Issuance)?;
    let mut links = vec![link];
    links.extend(parent.into_iter().flat_map(Chain::links).map(str::to_owned));
    let chain = Chain::from_links(&links)?;

    // The anchor is the parent chain's top issuer, or this signer.
    let texts = split(chain.to_text().as_bytes(), MAX_LINKS)?;
    let top = jws::decode(texts[texts.len() - 1]).map_err(|_| ChainError::Malformed)?;
    let anchor = [decode_link(&top.typ, &top.payload)?.iss];
    let revoked = NoRevocations;
    for value in audience.values() {
        let trust = TrustContext::new(&anchor, value, validity.issued_at, &revoked);
        let texts = split(chain.to_text().as_bytes(), trust.max_links)?;
        gates(&texts, kind, subject, &trust, Mode::Issue)?;
    }
    Ok(chain)
}
