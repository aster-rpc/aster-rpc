//! `verify_chain`: gates G0–G7 in the documented order. Any failure rejects
//! the whole chain; misordered input is never reordered or repaired.

use std::collections::BTreeSet;

use super::jws::{self, Compact, JwsError};
use super::links::{
    decode_link, ActionGrant, Audience, DecodedLink, EnrollmentExtensionV1, LinkBody,
    LocalityClaims,
};
use super::{
    digest, split, ChainError, LeafKind, LinkKind, TrustContext, MAX_LEEWAY_SECONDS, MAX_LINKS,
    MAX_LINK_LIFETIME,
};

/// The leaf's kind-specific claims.
#[derive(Debug, Clone, PartialEq)]
pub enum LeafClaims {
    Enrollment(EnrollmentExtensionV1),
    Action(ActionGrant),
    Locality(LocalityClaims),
}

/// A chain that passed every gate. Only [`verify_chain`] constructs one; the
/// fields are private and there is no other constructor.
///
/// ```compile_fail,E0451
/// use aster_transport_core::delegation::VerifiedChain;
/// let forged = VerifiedChain { anchor: [0; 32] };
/// ```
#[derive(Debug, Clone)]
pub struct VerifiedChain {
    anchor: [u8; 32],
    issuers: Vec<[u8; 32]>,
    subject: [u8; 32],
    audience: Audience,
    leaf: LeafClaims,
    mandatory: BTreeSet<String>,
    issued_at: u64,
    not_before: Option<u64>,
    not_after: u64,
    jtis: Vec<String>,
    digest: [u8; 32],
}

impl VerifiedChain {
    /// The trusted key the chain is rooted at.
    pub fn anchor(&self) -> &[u8; 32] {
        &self.anchor
    }
    /// Each link's issuer, leaf first; the last is the anchor.
    pub fn issuers(&self) -> &[[u8; 32]] {
        &self.issuers
    }
    /// The leaf subject: the authenticated key the chain was verified for.
    pub fn subject(&self) -> &[u8; 32] {
        &self.subject
    }
    /// The leaf's `aud`.
    pub fn audience(&self) -> &Audience {
        &self.audience
    }
    /// The leaf's claims. Effective authority is these, never a union with
    /// ancestors; ancestors only bound them.
    pub fn leaf(&self) -> &LeafClaims {
        &self.leaf
    }
    /// Every link's `mandatory` restrictions. A consumer enforces all of them.
    pub fn mandatory(&self) -> &BTreeSet<String> {
        &self.mandatory
    }
    /// The leaf's `iat`.
    pub fn issued_at(&self) -> u64 {
        self.issued_at
    }
    /// The latest `nbf` of any link.
    pub fn not_before(&self) -> Option<u64> {
        self.not_before
    }
    /// The earliest `exp` across links: when the chain stops being valid.
    pub fn not_after(&self) -> u64 {
        self.not_after
    }
    /// Every link's `jti`, leaf first.
    pub fn jtis(&self) -> &[String] {
        &self.jtis
    }
    /// `BLAKE3("aster.chain.v1\0" || text)`.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

/// Verify a chain's text form, as received, for the key the transport
/// authenticated. This is the only way to obtain a [`VerifiedChain`].
pub fn verify_chain(
    chain: &[u8],
    expect: LeafKind,
    leaf_key: &[u8; 32],
    trust: &TrustContext<'_>,
) -> Result<VerifiedChain, ChainError> {
    if trust.anchors.is_empty()
        || trust.leeway > MAX_LEEWAY_SECONDS
        || !(1..=MAX_LINKS).contains(&trust.max_links)
        || !(1..=MAX_LINK_LIFETIME).contains(&trust.max_link_lifetime)
    {
        return Err(ChainError::Configuration);
    }
    // G0 Size, before any parsing.
    let texts = split(chain, trust.max_links)?;
    let (links, mandatory) = gates(&texts, expect.link_kind(), leaf_key, trust, Mode::Verify)?;
    let leaf = &links[0];
    let leaf_claims = match &leaf.body {
        LinkBody::Enrollment(extension) => LeafClaims::Enrollment(extension.clone()),
        LinkBody::Action(grant) => LeafClaims::Action(grant.clone()),
        LinkBody::Locality(claims) => LeafClaims::Locality(claims.clone()),
        LinkBody::Delegation(_) => return Err(ChainError::Grammar),
    };
    Ok(VerifiedChain {
        anchor: links[links.len() - 1].iss,
        issuers: links.iter().map(|link| link.iss).collect(),
        subject: leaf.sub,
        audience: leaf.aud.clone(),
        leaf: leaf_claims,
        mandatory,
        issued_at: leaf.iat,
        not_before: links.iter().filter_map(|link| link.nbf).max(),
        not_after: links.iter().map(|link| link.exp).min().unwrap_or(leaf.exp),
        jtis: links.iter().map(|link| link.jti.clone()).collect(),
        digest: digest(chain),
    })
}

/// The latest `iat`/`nbf` any link of `chain` claims, decoded without trust.
/// Only for diagnosing a [`ChainError::NotYetValid`]: callers re-verify at
/// that time before reporting clock skew rather than a definite failure.
#[cfg(feature = "enrollment")]
pub(crate) fn claimed_start(chain: &[u8]) -> Option<u64> {
    let texts = split(chain, MAX_LINKS).ok()?;
    texts
        .iter()
        .map(|text| {
            let compact = jws::decode(text).ok()?;
            let link = decode_link(&compact.typ, &compact.payload).ok()?;
            Some(link.nbf.map_or(link.iat, |nbf| nbf.max(link.iat)))
        })
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .max()
}

/// The leaf link's `jti`, decoded without trust. For reporting what an
/// issuer just signed.
#[cfg(feature = "enrollment")]
pub(crate) fn leaf_jti(chain: &super::Chain) -> Option<String> {
    let leaf = chain.links().next()?;
    let compact = jws::decode(leaf).ok()?;
    Some(decode_link(&compact.typ, &compact.payload).ok()?.jti)
}

/// What an issuer's own delegation chain allows it to sign, decoded without
/// trust from its first (delegation) link. Only for sizing new leaves:
/// issuance re-checks every bound against the chain.
#[cfg(feature = "enrollment")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DelegationLimits {
    pub subject: [u8; 32],
    pub expires_at: u64,
    pub max_child_lifetime: u64,
}

#[cfg(feature = "enrollment")]
pub(crate) fn delegation_limits(chain: &super::Chain) -> Option<DelegationLimits> {
    let first = chain.links().next()?;
    let compact = jws::decode(first).ok()?;
    let link = decode_link(&compact.typ, &compact.payload).ok()?;
    let grant = link.delegation()?;
    Some(DelegationLimits {
        subject: link.sub,
        expires_at: link.exp,
        max_child_lifetime: grant.max_child_lifetime,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Verify,
    /// Issuance self-check: a leaf may legitimately start in the future, and
    /// restriction support is the eventual verifier's decision.
    Issue,
}

/// G1–G7 over framed links. Returns the decoded links and the combined
/// `mandatory` set.
pub(super) fn gates(
    texts: &[&str],
    expect: LinkKind,
    leaf_key: &[u8; 32],
    trust: &TrustContext<'_>,
    mode: Mode,
) -> Result<(Vec<DecodedLink>, BTreeSet<String>), ChainError> {
    // G1 Decode.
    let mut compacts: Vec<Compact<'_>> = Vec::with_capacity(texts.len());
    let mut links: Vec<DecodedLink> = Vec::with_capacity(texts.len());
    for text in texts {
        #[cfg(feature = "test-hooks")]
        super::test_counters::decoded();
        let compact = jws::decode(text).map_err(|error| match error {
            JwsError::Header => ChainError::Header,
            _ => ChainError::Malformed,
        })?;
        links.push(decode_link(&compact.typ, &compact.payload)?);
        compacts.push(compact);
    }
    let leaf = &links[0];
    let top = &links[links.len() - 1];

    // G2 Grammar.
    if leaf.kind != expect
        || links[1..]
            .iter()
            .any(|link| link.kind != LinkKind::Delegation)
    {
        return Err(ChainError::Grammar);
    }

    // G3 Time. Static lifetime first, then the clock.
    let latest_start = trust.now.saturating_add(trust.leeway);
    for (index, link) in links.iter().enumerate() {
        if link.exp <= link.iat || link.exp - link.iat > trust.max_link_lifetime {
            return Err(ChainError::Lifetime);
        }
        let future_leaf = mode == Mode::Issue && index == 0;
        if link.iat > latest_start
            || (!future_leaf && link.nbf.is_some_and(|nbf| nbf > latest_start))
        {
            return Err(ChainError::NotYetValid);
        }
        // No leeway on expiry: a skewed clock never extends authority.
        if trust.now >= link.exp {
            return Err(ChainError::Expired);
        }
    }

    // G4 Structure.
    if leaf.sub != *leaf_key || leaf.cnf != Some(*leaf_key) {
        return Err(ChainError::LeafMismatch);
    }
    if links[1..].iter().any(|link| link.cnf != Some(link.sub))
        || links.windows(2).any(|pair| pair[0].iss != pair[1].sub)
    {
        return Err(ChainError::Bridge);
    }
    if !trust.anchors.contains(&top.iss) {
        return Err(ChainError::UntrustedAnchor);
    }
    // Every issuer and the leaf subject are distinct: no cyclic links. The
    // one exception is an anchor's own one-link credential (the root's
    // self-issued enrollment), which it still presents with its own key.
    let self_issued_anchor = links.len() == 1 && leaf.iss == leaf.sub;
    let mut keys = BTreeSet::from([leaf.sub]);
    if !self_issued_anchor && !links.iter().all(|link| keys.insert(link.iss)) {
        return Err(ChainError::Bridge);
    }
    if !links.iter().all(|link| link.aud.contains(trust.audience)) {
        return Err(ChainError::Audience);
    }

    // G5 Attenuation, for each parent delegation and its child.
    for pair in links.windows(2) {
        attenuate(&pair[1], &pair[0])?;
    }
    let mandatory: BTreeSet<String> = links
        .iter()
        .flat_map(DecodedLink::mandatory)
        .cloned()
        .collect();
    if mode == Mode::Verify
        && mandatory
            .iter()
            .any(|restriction| !trust.supported_restrictions.contains(&restriction.as_str()))
    {
        return Err(ChainError::UnsupportedRestriction);
    }

    // G6 Revocation of delegates, by root-authored records the verifier holds.
    if links[1..]
        .iter()
        .any(|link| trust.revoked.is_revoked(&link.iss) || trust.revoked.is_revoked(&link.sub))
    {
        return Err(ChainError::Revoked);
    }

    // G7 Signatures, over the bytes exactly as received.
    for (compact, link) in compacts.iter().zip(&links) {
        #[cfg(feature = "test-hooks")]
        super::test_counters::verified();
        compact
            .verify(&link.iss)
            .map_err(|_| ChainError::Signature)?;
    }
    Ok((links, mandatory))
}

/// G5 for one parent delegation P and its direct child C.
fn attenuate(parent: &DecodedLink, child: &DecodedLink) -> Result<(), ChainError> {
    let Some(bounds) = parent.delegation() else {
        return Err(ChainError::Grammar);
    };
    let within = |values: Box<dyn Iterator<Item = &String> + '_>, bound: &BTreeSet<String>| {
        values.into_iter().all(|value| bound.contains(value))
    };
    let claimed = child.claimed();
    let mut ok = bounds.may_issue.contains(&child.kind.authorized_by())
        && child
            .aud
            .values()
            .all(|value| bounds.audiences.contains(value))
        && bounds
            .subjects
            .as_ref()
            .is_none_or(|subjects| subjects.contains(&hex::encode(child.sub)))
        && child.exp - child.iat <= bounds.max_child_lifetime
        && child.exp <= parent.exp
        && parent.iat <= child.iat
        && child.iat < parent.exp
        && within(claimed.scopes, &bounds.scopes)
        && within(claimed.tags, &bounds.tags)
        && within(claimed.attribute_keys, &bounds.attribute_keys)
        && within(claimed.operations, &bounds.operations)
        // Persistent registration, or the right to grant it, needs the
        // parent's `may_register`.
        && (!child.registers() || bounds.may_register);
    if let Some(grant) = child.delegation() {
        // A sub-delegate may never grant more than it was granted.
        ok = ok
            && bounds.max_depth > 0
            && grant.max_depth < bounds.max_depth
            && grant.may_issue.is_subset(&bounds.may_issue)
            && grant.audiences.is_subset(&bounds.audiences);
    }
    ok.then_some(()).ok_or(ChainError::Attenuation)
}
