//! Admission observations and promotion (enrollment §4–§5, B10).
//!
//! An observation is a compact JWT signed by the observer: "I issued, or
//! admitted, this subject on this chain". Observers write them into the
//! observations namespace; the root reads them when it comes online and may
//! promote a persistent node to a root-authored registration. Observations
//! confer no authority: nothing admits a peer because of one, and the root
//! re-verifies the carried chain against its own records before promoting.
//! Only persistent credentials are observed or promoted.

use serde::{Deserialize, Serialize};

use super::records::{observation_key, NodeRegistrationStatus, NodeRegistrationV1};
use super::{
    AdmissionClaims, AdmissionEvidenceSource, Audience, EnrollmentError, JwtEnrollmentVerifier,
    PolicyStores, Registration, VerifiedAdmissionEvidence, MAX_CHAIN_BYTES,
};
use crate::delegation::{jws, Chain};
use crate::CoreDoc;

pub const OBSERVATION_JWT_TYPE: &str = "aster-observation-v1+jwt";
/// `exp - iat` of an observation.
pub const MAX_OBSERVATION_LIFETIME: u64 = 7 * 86_400;
pub const MAX_OBSERVATION_BYTES: usize = 2 * MAX_CHAIN_BYTES;
/// Bound on observation entries one review reads.
pub const MAX_OBSERVATIONS: usize = 4096;
const OBSERVATION_PREFIX: &[u8] = b"aster/enrollment/v1/observations/";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// Written by the issuer that signed the subject's credential.
    Issued,
    /// Written by a replica with the observer scope that admitted the subject.
    Admitted,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationClaims {
    iss: String,
    sub: String,
    chain: String,
    digest: String,
    kind: ObservationKind,
    first_seen: u64,
    last_seen: u64,
    iat: u64,
    exp: u64,
}

/// A verified observation: its signature, framing and lifetime are valid.
/// That says only what the observer claims; see [`review_observations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationV1 {
    pub observer: [u8; 32],
    pub subject: [u8; 32],
    pub chain: String,
    pub digest: [u8; 32],
    pub kind: ObservationKind,
    pub first_seen: u64,
    pub last_seen: u64,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationError {
    Size,
    Malformed,
    Signature,
    /// `exp` passed, `iat` in the future, or the lifetime exceeds the limit.
    Validity,
    /// The chain digest does not match the carried chain.
    Digest,
    /// The entry key does not name the observation's observer and subject.
    KeyMismatch,
    SigningFailed,
}

impl std::fmt::Display for ObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "observation rejected: {self:?}")
    }
}
impl std::error::Error for ObservationError {}

/// Sign an observation as the observer. `issued_at` is trusted UTC seconds;
/// the observation expires [`MAX_OBSERVATION_LIFETIME`] later.
pub fn issue_observation(
    observer_secret: &[u8; 32],
    subject: &[u8; 32],
    chain: &str,
    kind: ObservationKind,
    first_seen: u64,
    last_seen: u64,
    issued_at: u64,
) -> Result<String, ObservationError> {
    let chain = Chain::from_text(chain).map_err(|_| ObservationError::Malformed)?;
    let observer = jws::public_key(observer_secret).map_err(|_| ObservationError::SigningFailed)?;
    if first_seen > last_seen || last_seen > issued_at {
        return Err(ObservationError::Validity);
    }
    let claims = ObservationClaims {
        iss: hex::encode(observer),
        sub: hex::encode(subject),
        digest: hex::encode(chain.digest()),
        chain: chain.into_text(),
        kind,
        first_seen,
        last_seen,
        iat: issued_at,
        exp: issued_at + MAX_OBSERVATION_LIFETIME,
    };
    let payload = serde_json::to_vec(&claims).map_err(|_| ObservationError::Malformed)?;
    let token = jws::sign(OBSERVATION_JWT_TYPE, &payload, observer_secret)
        .map_err(|_| ObservationError::SigningFailed)?;
    if token.len() > MAX_OBSERVATION_BYTES {
        return Err(ObservationError::Size);
    }
    Ok(token)
}

/// Verify an observation's own signature and framing at `now` (with
/// `leeway` for an observer clock ahead of ours on `iat`, never on `exp`).
pub fn verify_observation(
    token: &str,
    now: u64,
    leeway: u64,
) -> Result<ObservationV1, ObservationError> {
    if token.len() > MAX_OBSERVATION_BYTES {
        return Err(ObservationError::Size);
    }
    let compact = jws::decode(token).map_err(|_| ObservationError::Malformed)?;
    if compact.typ != OBSERVATION_JWT_TYPE {
        return Err(ObservationError::Malformed);
    }
    let claims: ObservationClaims =
        serde_json::from_slice(&compact.payload).map_err(|_| ObservationError::Malformed)?;
    let observer = key(&claims.iss)?;
    let subject = key(&claims.sub)?;
    let digest = key_bytes(&claims.digest)?;
    if claims.exp <= claims.iat
        || claims.exp - claims.iat > MAX_OBSERVATION_LIFETIME
        || claims.iat > now.saturating_add(leeway)
        || now >= claims.exp
        || claims.first_seen > claims.last_seen
        || claims.last_seen > claims.iat
    {
        return Err(ObservationError::Validity);
    }
    let chain = Chain::from_text(&claims.chain).map_err(|_| ObservationError::Malformed)?;
    if chain.digest() != digest {
        return Err(ObservationError::Digest);
    }
    compact
        .verify(&observer)
        .map_err(|_| ObservationError::Signature)?;
    Ok(ObservationV1 {
        observer,
        subject,
        chain: claims.chain,
        digest,
        kind: claims.kind,
        first_seen: claims.first_seen,
        last_seen: claims.last_seen,
        issued_at: claims.iat,
        expires_at: claims.exp,
    })
}

fn key(value: &str) -> Result<[u8; 32], ObservationError> {
    super::canonical_identity(value).map_err(|_| ObservationError::Malformed)
}
fn key_bytes(value: &str) -> Result<[u8; 32], ObservationError> {
    let bytes: [u8; 32] = hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(ObservationError::Malformed)?;
    if hex::encode(bytes) != value {
        return Err(ObservationError::Malformed);
    }
    Ok(bytes)
}

/// Why the root ignores an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationRejection {
    /// Unreadable, unsigned, expired or at the wrong key.
    Invalid(ObservationError),
    /// Content not yet downloaded; retry after the next sync.
    Pending,
    /// An `issued` observation from a key that did not sign the chain's leaf,
    /// or an `admitted` one from a node that is not a current observer.
    ObserverNotQualified,
    /// The carried chain does not verify against the root's records at
    /// `last_seen` (for example, its issuer is revoked).
    ChainInvalid(EnrollmentError),
    /// The credential is ephemeral; it is never promoted.
    Ephemeral,
    /// The subject already has a registration (or denial) in the root's
    /// records; promotion is for chain-only nodes.
    AlreadyRegistered,
}

/// An observation whose chain the root verified at `last_seen`.
#[derive(Debug, Clone)]
pub struct ReviewedObservation {
    pub observation: ObservationV1,
    /// The chain's verified evidence: leaf claims within its ancestors'
    /// bounds, and the issuer path.
    pub evidence: VerifiedAdmissionEvidence,
}

impl ReviewedObservation {
    /// The leaf's issuer: the root, or the delegate that signed it.
    pub fn chain_issuer(&self) -> Option<[u8; 32]> {
        match self.evidence.source() {
            AdmissionEvidenceSource::PresentedChain { issuers, .. } => issuers.first().copied(),
            AdmissionEvidenceSource::RootAuthoredRecord { .. } => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ObservationReview {
    /// The latest accepted observation per subject.
    pub accepted: Vec<ReviewedObservation>,
    /// Ignored entries: `(entry key, author, reason)`.
    pub rejected: Vec<(Vec<u8>, String, ObservationRejection)>,
}

/// Read every observation entry (any author) in `doc`, the observations
/// namespace, and keep only those the root can rely on:
/// - the JWT verifies, is unexpired and sits at its own key;
/// - an `issued` observation is signed by the chain leaf's issuer; an
///   `admitted` one by a current observer: a key the root's sealed observer
///   grant in `fleet` is still active for, with no registration denial;
/// - the chain verifies against the root's records (`stores`) at
///   `last_seen`, and is persistent;
/// - the subject has no registration or denial yet.
pub async fn review_observations(
    doc: &CoreDoc,
    fleet: &CoreDoc,
    verifier: &JwtEnrollmentVerifier,
    stores: &PolicyStores,
    now: u64,
) -> anyhow::Result<ObservationReview> {
    let entries = doc
        .query_key_prefix(
            OBSERVATION_PREFIX.to_vec(),
            Some(MAX_OBSERVATIONS as u64 + 1),
        )
        .await?;
    anyhow::ensure!(
        entries.len() <= MAX_OBSERVATIONS,
        "too many observation entries to review"
    );
    let mut review = ObservationReview::default();
    let mut latest: std::collections::BTreeMap<[u8; 32], ReviewedObservation> = Default::default();
    for entry in entries {
        let reject = |review: &mut ObservationReview, reason| {
            review
                .rejected
                .push((entry.key.clone(), entry.author_id.clone(), reason));
        };
        if entry.content_len > MAX_OBSERVATION_BYTES as u64 {
            reject(
                &mut review,
                ObservationRejection::Invalid(ObservationError::Size),
            );
            continue;
        }
        let Some(bytes) = doc
            .read_entry_contents_if_complete(vec![entry.content_hash.clone()])
            .await?
            .pop()
            .flatten()
        else {
            reject(&mut review, ObservationRejection::Pending);
            continue;
        };
        let observation = match std::str::from_utf8(&bytes)
            .map_err(|_| ObservationError::Malformed)
            .and_then(|token| verify_observation(token.trim(), now, verifier.activation_leeway()))
        {
            Ok(observation) => observation,
            Err(error) => {
                reject(&mut review, ObservationRejection::Invalid(error));
                continue;
            }
        };
        if entry.key != observation_key(&observation.observer, &observation.subject) {
            reject(
                &mut review,
                ObservationRejection::Invalid(ObservationError::KeyMismatch),
            );
            continue;
        }
        match judge(&observation, fleet, verifier, stores, now).await {
            Ok(evidence) => {
                let reviewed = ReviewedObservation {
                    observation,
                    evidence,
                };
                let subject = reviewed.observation.subject;
                if latest.get(&subject).is_none_or(|known| {
                    known.observation.last_seen < reviewed.observation.last_seen
                }) {
                    latest.insert(subject, reviewed);
                }
            }
            Err(reason) => reject(&mut review, reason),
        }
    }
    review.accepted = latest.into_values().collect();
    Ok(review)
}

async fn judge(
    observation: &ObservationV1,
    fleet: &CoreDoc,
    verifier: &JwtEnrollmentVerifier,
    stores: &PolicyStores,
    now: u64,
) -> Result<VerifiedAdmissionEvidence, ObservationRejection> {
    if matches!(
        stores.select_evidence(&observation.subject, None, now),
        Ok(_) | Err(super::RecordError::Denied(_))
    ) {
        return Err(ObservationRejection::AlreadyRegistered);
    }
    let evidence = verifier
        .verify_with(
            &observation.chain,
            &observation.subject,
            observation.last_seen,
            stores,
        )
        .map_err(ObservationRejection::ChainInvalid)?;
    if evidence.registration() != Some(Registration::Persistent) {
        return Err(ObservationRejection::Ephemeral);
    }
    let qualified = match observation.kind {
        ObservationKind::Issued => {
            let issuer = match evidence.source() {
                AdmissionEvidenceSource::PresentedChain { issuers, .. } => issuers.first().copied(),
                AdmissionEvidenceSource::RootAuthoredRecord { .. } => None,
            };
            issuer == Some(observation.observer) && observation.observer != *verifier.trusted_root()
        }
        ObservationKind::Admitted => {
            !matches!(
                stores.select_evidence(&observation.observer, None, now),
                Err(super::RecordError::Denied(_))
            ) && observer_granted(fleet, verifier.trusted_root(), &observation.observer).await
        }
    };
    if !qualified {
        return Err(ObservationRejection::ObserverNotQualified);
    }
    Ok(evidence)
}

/// Whether the root's grant record for `observer` in the fleet namespace is
/// active. The root reads only the record's state; the sealed capability is
/// the observer's to open.
async fn observer_granted(fleet: &CoreDoc, root: &[u8; 32], observer: &[u8; 32]) -> bool {
    let key = crate::grants::grant_key(super::OBSERVER_GRANT_PREFIX, &hex::encode(observer));
    let view = super::RootAuthoredDocView::new(*fleet.doc.id().as_bytes(), *root, 64 * 1024, 1);
    matches!(
        view.read_exact(fleet, key.into_bytes()).await,
        Ok(super::RootRecordRead::Available(bytes))
            if crate::grant_record::GrantRecord::decode_fory(&bytes)
                .is_ok_and(|record| record.is_active())
    )
}

/// The state of an observer's sealed write grant in the fleet namespace.
#[derive(Debug)]
pub enum ObserverGrant {
    /// The observations namespace write secret.
    Active([u8; 32]),
    /// The root withdrew the grant; stop writing. A secret already learned
    /// cannot be unlearned, which is why observations confer no authority.
    Revoked,
    /// No grant yet, or its content has not arrived.
    Absent,
}

/// Open the root's sealed grant of the observations namespace's write
/// capability to `observer` (enrollment D2). Only the exact root author's
/// record at the observer's grant key counts, and it must open with the
/// observer's identity secret to a write capability for `observations`.
pub async fn open_observer_grant(
    fleet: &CoreDoc,
    root: &[u8; 32],
    observer_secret: &[u8; 32],
    observations: &[u8; 32],
) -> anyhow::Result<ObserverGrant> {
    use crate::grants::{grant_key, reconcile_namespace_grant, GrantContext, GrantOutcome};
    use crate::namespace::CoreNamespaceCapability;
    let namespace: [u8; 32] = *fleet.doc.id().as_bytes();
    let observer =
        jws::public_key(observer_secret).map_err(|_| anyhow::anyhow!("invalid observer key"))?;
    let recipient = hex::encode(observer);
    let key = grant_key(super::OBSERVER_GRANT_PREFIX, &recipient);
    let view = super::RootAuthoredDocView::new(namespace, *root, 64 * 1024, 1);
    let record = match view.read_exact(fleet, key.clone().into_bytes()).await? {
        super::RootRecordRead::Available(bytes) => bytes,
        super::RootRecordRead::Absent | super::RootRecordRead::Pending => {
            return Ok(ObserverGrant::Absent)
        }
    };
    let granter = hex::encode(root);
    let context = GrantContext {
        app: super::OBSERVER_GRANT_APP,
        granter: &granter,
        recipient: &recipient,
        resource: observations,
        path: &key,
        role: "write",
    };
    Ok(
        match reconcile_namespace_grant(observer_secret, &context, Some(&record)) {
            GrantOutcome::Active(CoreNamespaceCapability::Write(secret))
                if CoreNamespaceCapability::Write(secret).namespace_id() == *observations =>
            {
                ObserverGrant::Active(secret)
            }
            GrantOutcome::Revoked { .. } => ObserverGrant::Revoked,
            GrantOutcome::Absent => ObserverGrant::Absent,
            GrantOutcome::Active(_) | GrantOutcome::Unopenable(_) => {
                anyhow::bail!("observer grant does not open to this namespace's write capability")
            }
        },
    )
}

/// The root-authored registration that promotes a reviewed node. Claims are
/// the verified leaf's unless the root supplies its own; the record has no
/// expiry and outlives the chain. The caller writes it as the root author.
pub fn promotion_record(
    root: &[u8; 32],
    reviewed: &ReviewedObservation,
    audience: &str,
    revision: u64,
    claims: Option<AdmissionClaims>,
    issued_at: u64,
) -> NodeRegistrationV1 {
    NodeRegistrationV1 {
        version: super::POLICY_RECORD_VERSION,
        issuer: hex::encode(root),
        subject: hex::encode(reviewed.observation.subject),
        audience: Audience::One(audience.to_owned()),
        revision,
        issued_at,
        not_before: None,
        expires_at: None,
        status: NodeRegistrationStatus::Active,
        claims: claims.unwrap_or_else(|| reviewed.evidence.claims().clone()),
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    }
}
