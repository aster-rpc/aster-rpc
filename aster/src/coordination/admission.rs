//! The §8.4 verified admission source: who may speak Raft to this voter.
//!
//! Admission answers exactly one question — *has the application's own
//! enrollment layer verified this node as a producer?* — and it answers it
//! about a `CoordinationNodeId` that the transport authenticated, never about a
//! string a request supplied.
//!
//! ## What it is not
//!
//! Not Raft membership and not Store permission, in **either** direction. A
//! producer is not thereby a voter, and a voter is not thereby admitted; §2.1
//! of the contract matrix lists substituting one for the other as forbidden
//! both ways. Nothing in `state.redb` records admission, no committed entry
//! grants it, and no request field carries it. It is host-local policy the
//! application owns, checked before consensus work, and its only power is to
//! **deny**.
//!
//! ## Five denials, and `Err` is two of them
//!
//! [`VerifiedAdmissionSource::get`] is fallible because §8.4's own prose
//! requires four outcomes an `Option` cannot express. `Ok(None)` is *this node
//! was never admitted*; `Err(Unavailable)` is *the source is having an outage*.
//! Both deny, which is why the difference is easy to lose — but they are
//! different operational facts, and a signature that could not tell them apart
//! would hide a later change that made one of them admit.
//!
//! [`decide`] is the single recognizer, so the network layer, the bootstrap
//! corridor and the join corridor cannot drift into three readings of the same
//! five cases.
//!
//! ## Unavailable denies
//!
//! It does not fall back, cache through, or admit. A source that cannot answer
//! is not a source that says yes. This can reduce availability during a policy
//! outage; §8.4 accepts that explicitly, because the alternative is a voter
//! that admits strangers precisely when its policy layer is broken.

use std::sync::Arc;

use crate::coordination::ids::CoordinationNodeId;

/// The class an admission record carries.
///
/// Two variants, and consensus accepts only one of them. `Consumer` exists so
/// an application can use one admission directory for both its clients and its
/// voters without the Raft path silently widening to cover clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedAdmissionClass {
    /// May participate in consensus: send Raft RPCs, be dialled for them.
    Producer,
    /// May be a coordination *client*. Never accepted for Raft work.
    Consumer,
}

/// One verified admission fact, with the provenance that produced it.
///
/// `authority_fingerprint` and `evidence_digest` are carried rather than
/// dropped because an operator asking "why is this node admitted?" needs to
/// know which authority said so and against what evidence. They are recorded
/// and reported; no rule in this subsystem branches on them, and none may — a
/// voter that decided policy from them would be duplicating the application's
/// enrollment logic with worse inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedAdmission {
    /// The node this record is about. Compared for **exact** equality with the
    /// node that was asked about; see [`AdmissionDenial::WrongSubject`].
    pub subject: CoordinationNodeId,
    pub class: VerifiedAdmissionClass,
    /// Which admitting authority produced this record.
    pub authority_fingerprint: [u8; 32],
    /// A digest of the evidence the authority verified.
    pub evidence_digest: [u8; 32],
    /// Monotone within one authority, so successive policies are orderable.
    pub revision: u64,
}

/// Why a source could not answer.
///
/// `#[non_exhaustive]` because a future source kind may fail in a way neither
/// variant describes, and callers must already treat an unrecognised failure as
/// a denial rather than as an admission.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionLookupError {
    /// The source exists but cannot answer now.
    Unavailable {
        /// Operator-facing detail. Never returned to a peer.
        detail: String,
    },
    /// The source answered with a record it could not interpret.
    Malformed {
        /// Operator-facing detail. Never returned to a peer.
        detail: String,
    },
}

impl std::fmt::Display for AdmissionLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionLookupError::Unavailable { detail } => {
                write!(f, "admission source unavailable: {detail}")
            }
            AdmissionLookupError::Malformed { detail } => {
                write!(f, "admission source returned a malformed record: {detail}")
            }
        }
    }
}

impl std::error::Error for AdmissionLookupError {}

/// The application's verified admission directory.
///
/// **Synchronous and local.** This runs on the Raft hot path — before every
/// unary call, before every stream, and again at bounded intervals inside a
/// snapshot transfer — so an implementation that does network or disk I/O turns
/// every Raft message into a round trip through the application's policy store.
/// Populate an in-memory snapshot of the directory and swap it; do not look
/// through to a remote service here.
///
/// The application populates it **only after cryptographic verification against
/// its own pinned trust directory**. Request attributes and `NodeInfo`
/// self-description are not inputs: both are things a peer says about itself.
pub trait VerifiedAdmissionSource: Send + Sync + 'static {
    /// Look up `node`.
    ///
    /// `Ok(None)` is a missing record; `Err` is a source that could not answer.
    /// Both deny, and they are distinguishable because an outage and a
    /// non-member are different operational facts.
    fn get(
        &self,
        node: CoordinationNodeId,
    ) -> Result<Option<VerifiedAdmission>, AdmissionLookupError>;
}

/// Why one admission check denied.
///
/// Exhaustive on purpose, and it is the reason [`decide`] exists rather than a
/// boolean helper: five distinct facts collapse to "no", and an operator
/// debugging a partition needs to know which one. A `bool` would make the
/// difference between "your directory is down" and "that node was never
/// enrolled" invisible at exactly the moment it matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionDenial {
    /// `Ok(None)`: the source has no record for this node.
    Missing {
        /// The node that was asked about.
        node: CoordinationNodeId,
    },
    /// `Err(Unavailable)`: the source could not answer.
    Unavailable {
        /// Operator-facing detail from the source.
        detail: String,
    },
    /// `Err(Malformed)`: the source answered with an uninterpretable record.
    Malformed {
        /// Operator-facing detail from the source.
        detail: String,
    },
    /// The record is for a different node than the one asked about.
    ///
    /// A source is not required to be correct, only to be consulted. A record
    /// keyed loosely — by a prefix, by a stale alias, by a hash collision in
    /// the application's own index — would otherwise admit whichever node the
    /// directory happened to return.
    WrongSubject {
        /// The node that was asked about.
        asked: CoordinationNodeId,
        /// The node the record is actually about.
        returned: CoordinationNodeId,
    },
    /// The record is a `Consumer`, and this is producer work.
    NotProducer {
        /// The node that was asked about.
        node: CoordinationNodeId,
    },
}

impl std::fmt::Display for AdmissionDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionDenial::Missing { node } => {
                write!(f, "no admission record for {node}")
            }
            AdmissionDenial::Unavailable { detail } => {
                write!(f, "admission source unavailable: {detail}")
            }
            AdmissionDenial::Malformed { detail } => {
                write!(f, "admission source returned a malformed record: {detail}")
            }
            AdmissionDenial::WrongSubject { asked, returned } => write!(
                f,
                "admission record for {asked} is about {returned} instead"
            ),
            AdmissionDenial::NotProducer { node } => {
                write!(f, "{node} is admitted as a consumer, not a producer")
            }
        }
    }
}

impl std::error::Error for AdmissionDenial {}

/// The one place the five denials of §8.4 are decided.
///
/// Returns the record on success so a caller that needs the provenance for a
/// log line has it without a second lookup — and, more importantly, so a caller
/// cannot call this for its verdict and then read a *different* record for its
/// fields.
pub fn decide(
    source: &dyn VerifiedAdmissionSource,
    node: CoordinationNodeId,
) -> Result<VerifiedAdmission, AdmissionDenial> {
    let record = match source.get(node) {
        Ok(Some(record)) => record,
        Ok(None) => return Err(AdmissionDenial::Missing { node }),
        Err(AdmissionLookupError::Unavailable { detail }) => {
            return Err(AdmissionDenial::Unavailable { detail })
        }
        Err(AdmissionLookupError::Malformed { detail }) => {
            return Err(AdmissionDenial::Malformed { detail })
        }
    };
    if record.subject != node {
        return Err(AdmissionDenial::WrongSubject {
            asked: node,
            returned: record.subject,
        });
    }
    match record.class {
        VerifiedAdmissionClass::Producer => Ok(record),
        VerifiedAdmissionClass::Consumer => Err(AdmissionDenial::NotProducer { node }),
    }
}

/// A shareable source, as [`CoordinationConfig`](super::host::CoordinationConfig)
/// holds it.
pub type SharedAdmissionSource = Arc<dyn VerifiedAdmissionSource>;

// ── the built-in signed source ───────────────────────────────────────────────

/// §8.4's admission-set signing domain. The trailing NUL is part of it.
///
/// A fourth domain beside genesis, join and recovery, and distinct for the same
/// reason they are distinct from each other: a signature over one document kind
/// must never verify as another.
pub const ADMISSION_DOMAIN: &[u8] = b"aster-coordination/admission/v1\0";

/// The §12.1 cap on one admission set.
///
/// Sized to the largest group v1 permits plus room for the clients an operator
/// may enroll alongside it. This is a *host-local* document rather than a wire
/// message, so the bound exists to keep a corrupt or hostile file from
/// allocating without limit, not to negotiate anything with a peer.
pub const MAX_ADMISSION_ENTRIES: usize = 1024;

/// One entry of a signed admission set.
#[derive(
    fory_derive::ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq, Eq,
)]
#[aster(wire = "aster.coordination.v1/AdmissionEntry")]
pub struct AdmissionEntryV1 {
    /// Exactly 32 bytes.
    #[fory(id = 1, bytes)]
    pub node_id: Vec<u8>,
    /// `1` Producer | `2` Consumer. An unrecognised code is refused rather
    /// than defaulted: an entry this build cannot classify must not fall
    /// through to the permissive class.
    #[fory(id = 2)]
    pub class_code: i32,
}

/// A signed, exact admission set (§8.4's built-in source).
///
/// The document an operator mints for a test or small deployment. Sorted and
/// unique by node id before signing, for the same reason every other §7.1 set
/// is: an unsorted spelling is a different document with the same meaning, and
/// only one of the two is covered by the signature.
#[derive(
    fory_derive::ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq, Eq,
)]
#[aster(wire = "aster.coordination.v1/AdmissionSet")]
pub struct AdmissionSetV1 {
    #[fory(id = 1)]
    pub format_version: u32,
    /// 1..=128 ASCII, compared against the configured authority domain for the
    /// same reason a genesis's is: one authority legitimately signs for several
    /// deployments.
    #[fory(id = 2)]
    pub authority_domain: String,
    /// Monotone within one authority. Carried into every record this set
    /// produces so successive policies are orderable.
    #[fory(id = 3)]
    pub revision: u64,
    /// Sorted by `node_id`, unique, non-empty.
    #[fory(id = 4)]
    pub entries: Vec<AdmissionEntryV1>,
}

/// The class code a `Producer` entry carries.
pub const ADMISSION_CLASS_PRODUCER: i32 = 1;
/// The class code a `Consumer` entry carries.
pub const ADMISSION_CLASS_CONSUMER: i32 = 2;

/// Why a signed admission set was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionSetError {
    /// The envelope or the set inside it would not decode.
    Malformed,
    /// A `format_version` this build does not implement.
    UnknownFormat(u32),
    /// Re-encoding the decoded set did not reproduce the signed bytes.
    NonCanonical,
    /// The envelope names a public key other than the configured authority.
    WrongAuthorityKey,
    /// The signature does not verify under the configured authority key.
    BadSignature,
    /// The set's `authority_domain` is not this deployment's.
    WrongDomain,
    /// A fixed-width field was the wrong width, or a bounded one too long.
    FieldWidth,
    /// The entries are empty, over [`MAX_ADMISSION_ENTRIES`], unsorted, or
    /// duplicated.
    Entries,
    /// An entry carries a class code this build does not implement.
    UnknownClass(i32),
}

impl std::fmt::Display for AdmissionSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionSetError::Malformed => f.write_str("the admission set would not decode"),
            AdmissionSetError::UnknownFormat(v) => {
                write!(f, "admission set format version {v} is not implemented")
            }
            AdmissionSetError::NonCanonical => {
                f.write_str("the admission set is not its own canonical encoding")
            }
            AdmissionSetError::WrongAuthorityKey => {
                f.write_str("the admission set names another authority key")
            }
            AdmissionSetError::BadSignature => {
                f.write_str("the admission set signature does not verify")
            }
            AdmissionSetError::WrongDomain => {
                f.write_str("the admission set is for another deployment")
            }
            AdmissionSetError::FieldWidth => {
                f.write_str("an admission set field is the wrong size")
            }
            AdmissionSetError::Entries => f.write_str(
                "the admission set entries are empty, oversized, unsorted, or duplicated",
            ),
            AdmissionSetError::UnknownClass(c) => {
                write!(f, "admission class code {c} is not implemented")
            }
        }
    }
}

impl std::error::Error for AdmissionSetError {}

/// A [`VerifiedAdmissionSource`] backed by one authority-signed exact set.
///
/// **Not an allow-all closure**, which is the whole reason §8.4 names it: a
/// convenience source that admitted everything would make every negative
/// admission test in the suite pass vacuously, and would ship as the default
/// for anyone who did not write their own.
///
/// The verification happens once, at construction, against the same pinned key
/// §7.1 uses for genesis and join. After that every lookup is a map read, which
/// is what the hot-path rule in [`VerifiedAdmissionSource`] requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedAdmissionSet {
    entries: std::collections::BTreeMap<CoordinationNodeId, VerifiedAdmission>,
}

impl SignedAdmissionSet {
    /// Verify a signed admission set against the configured authority.
    ///
    /// Takes the same two pins as [`verify_genesis`](super::model::authority::verify_genesis)
    /// and applies them identically. The key inside the envelope is compared to
    /// the configured one and never verified against.
    pub fn verify(
        authority_public_key: &[u8; 32],
        authority_domain: &str,
        signed_bytes: &[u8],
    ) -> Result<Self, AdmissionSetError> {
        use crate::coordination::contract::payload_fory;
        use crate::coordination::model::authority::MAX_AUTHORITY_DOMAIN_BYTES;
        use crate::coordination::raft::SignedCoordinationDocumentV1;
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        let envelope: SignedCoordinationDocumentV1 = payload_fory::<SignedCoordinationDocumentV1>()
            .deserialize(signed_bytes)
            .map_err(|_| AdmissionSetError::Malformed)?;

        if envelope.format_version != 1 {
            return Err(AdmissionSetError::UnknownFormat(envelope.format_version));
        }
        if envelope.authority_public_key.len() != 32 || envelope.signature.len() != 64 {
            return Err(AdmissionSetError::FieldWidth);
        }
        if envelope.authority_public_key != authority_public_key.to_vec() {
            return Err(AdmissionSetError::WrongAuthorityKey);
        }

        let key = VerifyingKey::from_bytes(authority_public_key)
            .map_err(|_| AdmissionSetError::Malformed)?;
        let signature =
            Signature::from_slice(&envelope.signature).map_err(|_| AdmissionSetError::Malformed)?;
        key.verify(
            &admission_signing_bytes(&envelope.document_bytes),
            &signature,
        )
        .map_err(|_| AdmissionSetError::BadSignature)?;

        let set: AdmissionSetV1 = payload_fory::<AdmissionSetV1>()
            .deserialize(&envelope.document_bytes)
            .map_err(|_| AdmissionSetError::Malformed)?;
        let reencoded = payload_fory::<AdmissionSetV1>()
            .serialize(&set)
            .map_err(|_| AdmissionSetError::Malformed)?;
        if reencoded != envelope.document_bytes {
            return Err(AdmissionSetError::NonCanonical);
        }

        if set.format_version != 1 {
            return Err(AdmissionSetError::UnknownFormat(set.format_version));
        }
        if set.authority_domain.is_empty()
            || set.authority_domain.len() > MAX_AUTHORITY_DOMAIN_BYTES
            || !set.authority_domain.is_ascii()
        {
            return Err(AdmissionSetError::FieldWidth);
        }
        if set.authority_domain != authority_domain {
            return Err(AdmissionSetError::WrongDomain);
        }
        if set.entries.is_empty() || set.entries.len() > MAX_ADMISSION_ENTRIES {
            return Err(AdmissionSetError::Entries);
        }
        if set.entries.windows(2).any(|w| w[0].node_id >= w[1].node_id) {
            return Err(AdmissionSetError::Entries);
        }

        let authority_fingerprint = authority_fingerprint(authority_public_key);
        let evidence_digest = admission_set_digest(&envelope.document_bytes);
        let mut entries = std::collections::BTreeMap::new();
        for entry in &set.entries {
            let subject = CoordinationNodeId::from_slice(&entry.node_id)
                .map_err(|_| AdmissionSetError::FieldWidth)?;
            let class = match entry.class_code {
                ADMISSION_CLASS_PRODUCER => VerifiedAdmissionClass::Producer,
                ADMISSION_CLASS_CONSUMER => VerifiedAdmissionClass::Consumer,
                other => return Err(AdmissionSetError::UnknownClass(other)),
            };
            entries.insert(
                subject,
                VerifiedAdmission {
                    subject,
                    class,
                    authority_fingerprint,
                    evidence_digest,
                    revision: set.revision,
                },
            );
        }
        Ok(Self { entries })
    }

    /// How many nodes this set names.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether this set names no node. Never true for a verified set — an empty
    /// set is refused — but `clippy` asks for it beside `len`.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl VerifiedAdmissionSource for SignedAdmissionSet {
    fn get(
        &self,
        node: CoordinationNodeId,
    ) -> Result<Option<VerifiedAdmission>, AdmissionLookupError> {
        Ok(self.entries.get(&node).cloned())
    }
}

/// The exact preimage an authority signs for an admission set.
pub fn admission_signing_bytes(document_bytes: &[u8]) -> Vec<u8> {
    let mut signed = Vec::with_capacity(ADMISSION_DOMAIN.len() + document_bytes.len());
    signed.extend_from_slice(ADMISSION_DOMAIN);
    signed.extend_from_slice(document_bytes);
    signed
}

/// `BLAKE3(ADMISSION_DOMAIN || document_bytes)` — the evidence digest every
/// record from that set carries.
pub fn admission_set_digest(document_bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(&admission_signing_bytes(document_bytes)).as_bytes()
}

/// `BLAKE3` of an authority public key: which authority admitted a node.
///
/// A fingerprint rather than the key itself, so a record that leaks into a log
/// line names the authority without republishing its key material.
pub fn authority_fingerprint(authority_public_key: &[u8; 32]) -> [u8; 32] {
    *blake3::hash(authority_public_key).as_bytes()
}

#[cfg(test)]
pub(crate) mod testing {
    //! Minting real signed admission sets, so a negative test differs from the
    //! positive one by exactly the field under test rather than by being
    //! unsigned.

    use super::*;
    use crate::coordination::model::authority::{
        canonical_document_bytes, signed_document_from_parts, testing::signing_key,
    };
    use ed25519_dalek::Signer;

    /// A set naming `producers` as producers and `consumers` as consumers.
    pub fn set_of(producers: &[[u8; 32]], consumers: &[[u8; 32]], revision: u64) -> AdmissionSetV1 {
        let mut entries: Vec<AdmissionEntryV1> = producers
            .iter()
            .map(|n| AdmissionEntryV1 {
                node_id: n.to_vec(),
                class_code: ADMISSION_CLASS_PRODUCER,
            })
            .chain(consumers.iter().map(|n| AdmissionEntryV1 {
                node_id: n.to_vec(),
                class_code: ADMISSION_CLASS_CONSUMER,
            }))
            .collect();
        entries.sort_by(|l, r| l.node_id.cmp(&r.node_id));
        AdmissionSetV1 {
            format_version: 1,
            authority_domain: "aster0.net".into(),
            revision,
            entries,
        }
    }

    /// Sign a set under the test authority.
    pub fn sign_set(set: &AdmissionSetV1) -> Vec<u8> {
        let document_bytes = canonical_document_bytes(set).expect("encode");
        let signature = signing_key().sign(&admission_signing_bytes(&document_bytes));
        signed_document_from_parts(
            document_bytes,
            &signing_key().verifying_key().to_bytes(),
            &signature.to_bytes(),
        )
        .expect("encode")
    }

    /// A verified source admitting `producers`, for tests that need a source
    /// rather than a document.
    pub fn source_admitting(producers: &[[u8; 32]]) -> SignedAdmissionSet {
        let key = signing_key().verifying_key().to_bytes();
        SignedAdmissionSet::verify(&key, "aster0.net", &sign_set(&set_of(producers, &[], 1)))
            .expect("a freshly minted set verifies")
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::coordination::model::authority::testing::signing_key;

    const NODE: CoordinationNodeId = CoordinationNodeId::from_array([1u8; 32]);
    const OTHER: CoordinationNodeId = CoordinationNodeId::from_array([2u8; 32]);

    fn key() -> [u8; 32] {
        signing_key().verifying_key().to_bytes()
    }

    /// A source that answers however the test says, so the five denials can be
    /// driven independently of the signed-set implementation.
    struct Scripted(Result<Option<VerifiedAdmission>, AdmissionLookupError>);

    impl VerifiedAdmissionSource for Scripted {
        fn get(
            &self,
            _node: CoordinationNodeId,
        ) -> Result<Option<VerifiedAdmission>, AdmissionLookupError> {
            self.0.clone()
        }
    }

    fn record(subject: CoordinationNodeId, class: VerifiedAdmissionClass) -> VerifiedAdmission {
        VerifiedAdmission {
            subject,
            class,
            authority_fingerprint: [7u8; 32],
            evidence_digest: [8u8; 32],
            revision: 3,
        }
    }

    /// Every one of §8.4's five denials, each reached on its own, plus the one
    /// admission. Written as a table so a sixth outcome added to `decide`
    /// without a case here is visible as an untested branch.
    #[test]
    fn each_of_the_five_denials_is_reached_on_its_own() {
        let admitted = decide(
            &Scripted(Ok(Some(record(NODE, VerifiedAdmissionClass::Producer)))),
            NODE,
        );
        assert_eq!(
            admitted.expect("an exact-subject producer is the only admission"),
            record(NODE, VerifiedAdmissionClass::Producer)
        );

        assert_eq!(
            decide(&Scripted(Ok(None)), NODE),
            Err(AdmissionDenial::Missing { node: NODE })
        );
        assert_eq!(
            decide(
                &Scripted(Err(AdmissionLookupError::Unavailable {
                    detail: "directory reload in progress".into()
                })),
                NODE
            ),
            Err(AdmissionDenial::Unavailable {
                detail: "directory reload in progress".into()
            })
        );
        assert_eq!(
            decide(
                &Scripted(Err(AdmissionLookupError::Malformed {
                    detail: "row 4 has no class".into()
                })),
                NODE
            ),
            Err(AdmissionDenial::Malformed {
                detail: "row 4 has no class".into()
            })
        );
        assert_eq!(
            decide(
                &Scripted(Ok(Some(record(OTHER, VerifiedAdmissionClass::Producer)))),
                NODE
            ),
            Err(AdmissionDenial::WrongSubject {
                asked: NODE,
                returned: OTHER
            })
        );
        assert_eq!(
            decide(
                &Scripted(Ok(Some(record(NODE, VerifiedAdmissionClass::Consumer)))),
                NODE
            ),
            Err(AdmissionDenial::NotProducer { node: NODE })
        );
    }

    /// An outage and a non-member deny identically but are *distinguishable*.
    /// The point of the fallible signature: if these ever collapsed, a change
    /// that made one of them admit would be invisible.
    #[test]
    fn an_outage_and_a_non_member_deny_differently() {
        let outage = decide(
            &Scripted(Err(AdmissionLookupError::Unavailable {
                detail: "down".into(),
            })),
            NODE,
        );
        let missing = decide(&Scripted(Ok(None)), NODE);
        assert!(outage.is_err() && missing.is_err());
        assert_ne!(outage, missing);
    }

    /// The built-in source admits exactly the nodes its signed set names, and
    /// nothing else. The property that distinguishes it from an allow-all
    /// closure, asserted against a node the set does not contain.
    #[test]
    fn the_built_in_source_admits_exactly_its_signed_set() {
        let source = source_admitting(&[[1u8; 32], [3u8; 32]]);
        assert_eq!(source.len(), 2);

        assert!(decide(&source, NODE).is_ok());
        assert!(decide(&source, CoordinationNodeId::from_array([3u8; 32])).is_ok());
        assert_eq!(
            decide(&source, OTHER),
            Err(AdmissionDenial::Missing { node: OTHER })
        );
    }

    /// A consumer in the signed set is denied for producer work — the class is
    /// carried through verification rather than flattened to "present".
    #[test]
    fn a_consumer_in_the_signed_set_is_not_a_producer() {
        let signed = sign_set(&set_of(&[[1u8; 32]], &[[2u8; 32]], 9));
        let source =
            SignedAdmissionSet::verify(&key(), "aster0.net", &signed).expect("a signed set");
        assert!(decide(&source, NODE).is_ok());
        assert_eq!(
            decide(&source, OTHER),
            Err(AdmissionDenial::NotProducer { node: OTHER })
        );
    }

    /// Provenance survives verification: the fingerprint names the configured
    /// authority and the digest names the document that admitted the node.
    ///
    /// Both oracles are computed here from the signed bytes rather than read
    /// back from the source, so a record carrying zeroed provenance fails.
    #[test]
    fn a_verified_record_carries_the_provenance_that_produced_it() {
        let set = set_of(&[[1u8; 32]], &[], 42);
        let signed = sign_set(&set);
        let source =
            SignedAdmissionSet::verify(&key(), "aster0.net", &signed).expect("a signed set");
        let admitted = decide(&source, NODE).expect("admitted");

        use crate::coordination::raft::SignedCoordinationDocumentV1;
        let envelope: SignedCoordinationDocumentV1 =
            crate::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&signed)
                .expect("decode");
        let mut preimage = Vec::from(&b"aster-coordination/admission/v1\0"[..]);
        preimage.extend_from_slice(&envelope.document_bytes);

        assert_eq!(
            admitted.authority_fingerprint,
            *blake3::hash(&key()).as_bytes()
        );
        assert_eq!(
            admitted.evidence_digest,
            *blake3::hash(&preimage).as_bytes()
        );
        assert_eq!(admitted.revision, 42);
    }

    /// A set signed by another key, and a set for another deployment signed by
    /// the *right* key. The second is the case a signature check alone misses.
    #[test]
    fn a_set_from_another_authority_or_deployment_is_refused() {
        use crate::coordination::model::authority::{
            canonical_document_bytes, signed_document_from_parts,
        };
        use ed25519_dalek::{Signer, SigningKey};

        let impostor = SigningKey::from_bytes(&[0x99u8; 32]);
        let set = set_of(&[[1u8; 32]], &[], 1);
        let document_bytes = canonical_document_bytes(&set).expect("encode");
        let signature = impostor.sign(&admission_signing_bytes(&document_bytes));
        let bytes = signed_document_from_parts(
            document_bytes,
            &impostor.verifying_key().to_bytes(),
            &signature.to_bytes(),
        )
        .expect("encode");
        assert_eq!(
            SignedAdmissionSet::verify(&key(), "aster0.net", &bytes),
            Err(AdmissionSetError::WrongAuthorityKey)
        );

        let elsewhere = sign_set(&AdmissionSetV1 {
            authority_domain: "aster1.net".into(),
            ..set_of(&[[1u8; 32]], &[], 1)
        });
        assert_eq!(
            SignedAdmissionSet::verify(&key(), "aster0.net", &elsewhere),
            Err(AdmissionSetError::WrongDomain)
        );
    }

    /// The admission domain is its own. A signature minted over the genesis
    /// preimage must not verify as an admission set.
    #[test]
    fn a_genesis_signature_does_not_verify_as_an_admission_set() {
        use crate::coordination::model::authority::{
            canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
        };
        use ed25519_dalek::Signer;

        let set = set_of(&[[1u8; 32]], &[], 1);
        let document_bytes = canonical_document_bytes(&set).expect("encode");
        let signature = signing_key().sign(&genesis_signing_bytes(&document_bytes));
        let bytes = signed_document_from_parts(
            document_bytes,
            &signing_key().verifying_key().to_bytes(),
            &signature.to_bytes(),
        )
        .expect("encode");
        assert_eq!(
            SignedAdmissionSet::verify(&key(), "aster0.net", &bytes),
            Err(AdmissionSetError::BadSignature)
        );
    }

    /// Each malformed-set field on its own, correctly signed.
    #[test]
    fn each_malformed_set_field_is_refused_on_its_own() {
        let cases: Vec<(&str, AdmissionSetV1, AdmissionSetError)> = vec![
            (
                "a future format",
                AdmissionSetV1 {
                    format_version: 2,
                    ..set_of(&[[1u8; 32]], &[], 1)
                },
                AdmissionSetError::UnknownFormat(2),
            ),
            (
                "no entries",
                AdmissionSetV1 {
                    entries: Vec::new(),
                    ..set_of(&[[1u8; 32]], &[], 1)
                },
                AdmissionSetError::Entries,
            ),
            (
                "unsorted entries",
                AdmissionSetV1 {
                    entries: vec![
                        AdmissionEntryV1 {
                            node_id: vec![2u8; 32],
                            class_code: ADMISSION_CLASS_PRODUCER,
                        },
                        AdmissionEntryV1 {
                            node_id: vec![1u8; 32],
                            class_code: ADMISSION_CLASS_PRODUCER,
                        },
                    ],
                    ..set_of(&[[1u8; 32]], &[], 1)
                },
                AdmissionSetError::Entries,
            ),
            (
                "duplicated entries",
                AdmissionSetV1 {
                    entries: vec![
                        AdmissionEntryV1 {
                            node_id: vec![1u8; 32],
                            class_code: ADMISSION_CLASS_PRODUCER,
                        },
                        AdmissionEntryV1 {
                            node_id: vec![1u8; 32],
                            class_code: ADMISSION_CLASS_CONSUMER,
                        },
                    ],
                    ..set_of(&[[1u8; 32]], &[], 1)
                },
                AdmissionSetError::Entries,
            ),
            (
                "a short node id",
                AdmissionSetV1 {
                    entries: vec![AdmissionEntryV1 {
                        node_id: vec![1u8; 31],
                        class_code: ADMISSION_CLASS_PRODUCER,
                    }],
                    ..set_of(&[[1u8; 32]], &[], 1)
                },
                AdmissionSetError::FieldWidth,
            ),
            (
                "an unrecognised class",
                AdmissionSetV1 {
                    entries: vec![AdmissionEntryV1 {
                        node_id: vec![1u8; 32],
                        class_code: 7,
                    }],
                    ..set_of(&[[1u8; 32]], &[], 1)
                },
                AdmissionSetError::UnknownClass(7),
            ),
            (
                "an empty domain",
                AdmissionSetV1 {
                    authority_domain: String::new(),
                    ..set_of(&[[1u8; 32]], &[], 1)
                },
                AdmissionSetError::FieldWidth,
            ),
        ];

        for (name, set, expected) in cases {
            assert_eq!(
                SignedAdmissionSet::verify(&key(), "aster0.net", &sign_set(&set)),
                Err(expected),
                "{name}"
            );
        }
    }

    /// An unrecognised class code is refused rather than treated as a consumer.
    /// Refusing the whole set is the safe direction: a build that silently
    /// dropped an entry it could not classify would admit a *narrower* set than
    /// the operator signed, which is a different policy than the one in force.
    #[test]
    fn an_unrecognised_class_refuses_the_whole_set_rather_than_the_entry() {
        let set = AdmissionSetV1 {
            entries: vec![
                AdmissionEntryV1 {
                    node_id: vec![1u8; 32],
                    class_code: ADMISSION_CLASS_PRODUCER,
                },
                AdmissionEntryV1 {
                    node_id: vec![2u8; 32],
                    class_code: 99,
                },
            ],
            ..set_of(&[[1u8; 32]], &[], 1)
        };
        assert_eq!(
            SignedAdmissionSet::verify(&key(), "aster0.net", &sign_set(&set)),
            Err(AdmissionSetError::UnknownClass(99))
        );
    }

    #[test]
    fn an_empty_document_is_not_an_admission_set() {
        assert_eq!(
            SignedAdmissionSet::verify(&key(), "aster0.net", &[]),
            Err(AdmissionSetError::Malformed)
        );
    }
}
