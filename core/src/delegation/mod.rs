//! Delegation chains: Aster's one credential primitive.
//!
//! A chain is a list of compact Ed25519 JWTs, leaf first, each link issued by
//! the subject of the next and the last issued by a trusted anchor. One call,
//! [`verify_chain`], checks a whole chain offline: size gates before parsing,
//! structure and attenuation before any signature, signatures last. There is
//! no other path to a [`VerifiedChain`].
//!
//! Three link kinds cover three jobs: a root delegates bounded issuing
//! authority to an online issuer (`delegation`), an issuer enrolls a node
//! (`enrollment`), and a principal lets an actor act on its behalf (`action`).
//! Long-lived authority is not a chain: it is a root-authored record that a
//! consumer reads from Docs. Design: `docs/_internal/ownership-attestations.md`.

mod issue;
pub(crate) mod jws;
pub(crate) mod links;
mod rotation;
mod verify;

use std::fmt;

pub use issue::{issue_action, issue_delegation, issue_enrollment, Validity};
pub use links::{
    ActionGrant, AdmissionClaims, Audience, Confirmation, DelegationGrant, EnrollmentExtensionV1,
    LinkKind, PublicConfirmationKey, Registration, TypedAttribute, ACTION_JWT_TYPE,
    DELEGATION_JWT_TYPE, ENROLLMENT_LINK_TYPE,
};
pub use rotation::{
    issue_root_rotation, next_root_commitment, verify_root_rotation, RootRotation, RootState,
    RotationReason, ROOT_ROTATION_JWT_TYPE,
};
pub(crate) use verify::{claimed_start, leaf_jti};
pub use verify::{verify_chain, LeafClaims, VerifiedChain};

pub const CHAIN_PREFIX: &str = "aster-chain-v1";
pub const MAX_CHAIN_BYTES: usize = 24 * 1024;
pub const MAX_LINK_BYTES: usize = 8 * 1024;
pub const DEFAULT_MAX_LINKS: usize = 4;
/// Hard cap on links regardless of configuration.
pub const MAX_LINKS: usize = 8;
/// The longest `exp - iat` any link may carry; configurable down only.
pub const MAX_LINK_LIFETIME: u64 = 30 * 24 * 60 * 60;
pub const DEFAULT_LEEWAY_SECONDS: u64 = 60;
pub const MAX_LEEWAY_SECONDS: u64 = 300;
pub const MAX_LIST_ENTRIES: usize = 64;
pub const MAX_STRING_BYTES: usize = 128;

const DIGEST_CONTEXT: &[u8] = b"aster.chain.v1\0";

/// Which leaf a caller expects; every other link must be a delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafKind {
    Enrollment,
    Action,
}

impl LeafKind {
    fn link_kind(self) -> LinkKind {
        match self {
            Self::Enrollment => LinkKind::Enrollment,
            Self::Action => LinkKind::Action,
        }
    }
}

/// Keys withdrawn by root-authored records (issuer status, in enrollment).
pub trait RevokedDelegates {
    fn is_revoked(&self, key: &[u8; 32]) -> bool;
}

/// No revocations: for issuance self-checks and tests.
pub struct NoRevocations;

impl RevokedDelegates for NoRevocations {
    fn is_revoked(&self, _: &[u8; 32]) -> bool {
        false
    }
}

/// Everything a verifier trusts. Construct with [`TrustContext::new`] and
/// lower the limits if needed; [`verify_chain`] rejects values above the caps.
pub struct TrustContext<'a> {
    /// The current trusted-anchor set. Empty means verification is refused.
    pub anchors: &'a [[u8; 32]],
    /// This verifier's audience; every link's `aud` must contain it.
    pub audience: &'a str,
    /// Trusted UTC seconds.
    pub now: u64,
    /// Applies to `iat` and `nbf` only, never to `exp`. At most 300.
    pub leeway: u64,
    pub revoked: &'a dyn RevokedDelegates,
    /// 1..=8; default 4.
    pub max_links: usize,
    /// At most [`MAX_LINK_LIFETIME`].
    pub max_link_lifetime: u64,
    /// `mandatory` restrictions this consumer implements. Any other one
    /// anywhere in the chain rejects it.
    pub supported_restrictions: &'a [&'a str],
}

impl<'a> TrustContext<'a> {
    pub fn new(
        anchors: &'a [[u8; 32]],
        audience: &'a str,
        now: u64,
        revoked: &'a dyn RevokedDelegates,
    ) -> Self {
        Self {
            anchors,
            audience,
            now,
            leeway: DEFAULT_LEEWAY_SECONDS,
            revoked,
            max_links: DEFAULT_MAX_LINKS,
            max_link_lifetime: MAX_LINK_LIFETIME,
            supported_restrictions: &[],
        }
    }
}

/// Each variant names the failing gate, so tests and logs can tell why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainError {
    /// The trust context is outside the documented caps, or has no anchor.
    Configuration,
    /// G0: too large, too many links, or a link too large.
    Size,
    /// G0/G1: not a well-formed chain or link.
    Malformed,
    /// G1: a header that is not exactly `alg`/`typ`, or an unknown `typ`.
    Header,
    /// G2: wrong leaf kind, or a non-delegation link above the leaf.
    Grammar,
    /// G3: `iat` or `nbf` later than `now + leeway`.
    NotYetValid,
    /// G3: `now >= exp`.
    Expired,
    /// G3: `exp <= iat` or a lifetime above the limit.
    Lifetime,
    /// G4: a link's issuer is not the next link's subject, a non-leaf
    /// confirmation key differs from its subject, or a key repeats.
    Bridge,
    /// G4: the leaf does not name the authenticated key.
    LeafMismatch,
    /// G4: the top issuer is not a trusted anchor.
    UntrustedAnchor,
    /// G4: a link's `aud` lacks the verifier's audience.
    Audience,
    /// G5: a child exceeds its parent's bounds.
    Attenuation,
    /// G6: a delegate key is revoked.
    Revoked,
    /// A `mandatory` restriction the consumer does not implement.
    UnsupportedRestriction,
    /// G7: a signature does not verify.
    Signature,
    /// A rotation statement's sequence is not exactly the next one.
    Sequence,
    /// Issuance could not sign or draw randomness.
    Issuance,
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "delegation chain rejected: {self:?}")
    }
}

impl std::error::Error for ChainError {}

/// A chain's text form, parsed only as far as G0. Holding one confers nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    text: String,
}

impl Chain {
    /// Parse the text form: exact prefix, at least one link, no empty
    /// segments, within the size caps. No link is decoded.
    pub fn from_text(text: &str) -> Result<Self, ChainError> {
        split(text.as_bytes(), MAX_LINKS)?;
        Ok(Self { text: text.into() })
    }

    pub(crate) fn from_links(links: &[String]) -> Result<Self, ChainError> {
        let mut text = String::from(CHAIN_PREFIX);
        for link in links {
            text.push('~');
            text.push_str(link);
        }
        Self::from_text(&text)
    }

    pub fn to_text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }

    /// Compact JWTs, leaf first.
    pub fn links(&self) -> impl Iterator<Item = &str> {
        self.text.split('~').skip(1)
    }

    /// `BLAKE3("aster.chain.v1\0" || text)`, for audit and observations.
    pub fn digest(&self) -> [u8; 32] {
        digest(self.text.as_bytes())
    }
}

fn digest(text: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_CONTEXT);
    hasher.update(text);
    *hasher.finalize().as_bytes()
}

/// G0: bounds and framing only, before any decoding.
fn split(chain: &[u8], max_links: usize) -> Result<Vec<&str>, ChainError> {
    if chain.len() > MAX_CHAIN_BYTES {
        return Err(ChainError::Size);
    }
    let text = std::str::from_utf8(chain).map_err(|_| ChainError::Malformed)?;
    let rest = text
        .strip_prefix(CHAIN_PREFIX)
        .and_then(|rest| rest.strip_prefix('~'))
        .ok_or(ChainError::Malformed)?;
    let links: Vec<&str> = rest.split('~').collect();
    if links.len() > max_links || links.iter().any(|link| link.len() > MAX_LINK_BYTES) {
        return Err(ChainError::Size);
    }
    if links.iter().any(|link| link.is_empty()) {
        return Err(ChainError::Malformed);
    }
    Ok(links)
}

/// Test-only work counters, per thread, proving the size gate runs before
/// any parsing or signature verification.
#[cfg(feature = "test-hooks")]
pub mod test_counters {
    use std::cell::Cell;

    thread_local! {
        static DECODED: Cell<usize> = const { Cell::new(0) };
        static VERIFIED: Cell<usize> = const { Cell::new(0) };
    }

    /// `(links decoded, signatures verified)` on this thread so far.
    pub fn snapshot() -> (usize, usize) {
        (DECODED.with(Cell::get), VERIFIED.with(Cell::get))
    }

    pub(crate) fn decoded() {
        DECODED.with(|count| count.set(count.get() + 1));
    }

    pub(crate) fn verified() {
        VERIFIED.with(|count| count.set(count.get() + 1));
    }
}
