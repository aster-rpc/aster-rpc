use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;

use super::{TypedAttribute, VerifiedAdmissionEvidence};

pub const MAX_POLICY_DEPTH: usize = 32;
pub const MAX_POLICY_NODES: usize = 256;

/// These observations come from the consumer's trusted synchronization path.
/// A recent replica sync does not prove the root has published nothing newer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynchronizationSource {
    Root,
    Replica([u8; 32]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySynchronization {
    pub completed_at: u64,
    pub source: SynchronizationSource,
    pub observed_revision: Option<u64>,
}

/// Unknown freshness is represented by `None`, including at first startup.
#[derive(Debug, Clone, Default)]
pub struct AdmissionContext {
    /// A trusted transport observation; unavailable and relayed paths stay unknown.
    pub observed_source: Option<ObservedSourceAddress>,
    pub policy_sync: Option<PolicySynchronization>,
    pub pending_policy_content: bool,
}

/// How the transport obtained an address. Relay and peer-provided addresses
/// cannot satisfy an originating-peer CIDR restriction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAddressProvenance {
    DirectPeer,
    Relay,
    PeerReported,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedSourceAddress {
    pub address: IpAddr,
    pub provenance: SourceAddressProvenance,
}
/// IPv4 or IPv6 network predicate. This is checked at evaluation time only;
/// consumers must reevaluate admission/operation policy after path changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCidr {
    network: IpAddr,
    prefix: u8,
}
impl SourceCidr {
    pub fn new(network: IpAddr, prefix: u8) -> Result<Self, PolicyError> {
        if prefix > if network.is_ipv4() { 32 } else { 128 } {
            return Err(PolicyError::InvalidPredicate);
        }
        Ok(Self { network, prefix })
    }
    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.network, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(network) & mask == u32::from(address) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(network) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}
impl FromStr for SourceCidr {
    type Err = PolicyError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (network, prefix) = value.split_once('/').ok_or(PolicyError::InvalidPredicate)?;
        Self::new(
            network.parse().map_err(|_| PolicyError::InvalidPredicate)?,
            prefix.parse().map_err(|_| PolicyError::InvalidPredicate)?,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchState {
    Match,
    NoMatch,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    EmptyCombinator,
    DepthLimit,
    NodeLimit,
    InvalidPredicate,
    Handler(String),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "admission policy failed: {self:?}")
    }
}
impl std::error::Error for PolicyError {}

#[derive(Debug, Clone, PartialEq)]
pub enum PolicyPredicate {
    SourceCidr(SourceCidr),
    Audience(String),
    Scope(String),
    Tag(String),
    AttributeEquals {
        key: String,
        value: TypedAttribute,
    },
    /// Explicitly opt into cached/offline policy, even with unknown freshness.
    /// Known revocation still denies outside the expression.
    TolerateStalePolicy,
    /// Require a completed synchronization with a source chosen by the
    /// consumer. Pending content and missing/future timestamps are unknown.
    PolicySynchronizedWithin {
        max_age: Duration,
        require_root: bool,
    },
}

/// A separate versioned enrollment tree; the existing flat RPC capability
/// format and authorization semantics are unchanged. All empty combinators
/// are rejected. Depth/node limits cover the whole tree before evaluation,
/// including branches that could otherwise short-circuit.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyExpressionV1 {
    Predicate(PolicyPredicate),
    AllOf(Vec<Self>),
    AnyOf(Vec<Self>),
    NoneOf(Vec<Self>),
}

impl PolicyExpressionV1 {
    pub fn validate(&self) -> Result<(), PolicyError> {
        let mut pending = vec![(self, 1usize)];
        let mut nodes = 0;
        while let Some((node, depth)) = pending.pop() {
            nodes += 1;
            if nodes > MAX_POLICY_NODES {
                return Err(PolicyError::NodeLimit);
            }
            if depth > MAX_POLICY_DEPTH {
                return Err(PolicyError::DepthLimit);
            }
            match node {
                Self::Predicate(predicate) => {
                    if matches!(predicate, PolicyPredicate::AttributeEquals { value: TypedAttribute::Float(v), .. } if !v.is_finite())
                    {
                        return Err(PolicyError::InvalidPredicate);
                    }
                }
                Self::AllOf(children) | Self::AnyOf(children) | Self::NoneOf(children) => {
                    if children.is_empty() {
                        return Err(PolicyError::EmptyCombinator);
                    }
                    // Bound pending memory as well as visited nodes. No large
                    // fanout is allocated merely to discover it is too large.
                    if children.len() > MAX_POLICY_NODES - nodes
                        || pending.len() + children.len() > MAX_POLICY_NODES - nodes
                    {
                        return Err(PolicyError::NodeLimit);
                    }
                    pending.extend(children.iter().map(|child| (child, depth + 1)));
                }
            }
        }
        Ok(())
    }

    pub fn evaluate(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        context: &AdmissionContext,
        now: u64,
    ) -> Result<MatchState, PolicyError> {
        self.validate()?;
        Ok(self.evaluate_validated(evidence, context, now))
    }

    fn evaluate_validated(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        context: &AdmissionContext,
        now: u64,
    ) -> MatchState {
        match self {
            Self::Predicate(predicate) => predicate.evaluate(evidence, context, now),
            Self::AllOf(children) | Self::AnyOf(children) | Self::NoneOf(children) => {
                let mut matched = false;
                let mut unmatched = false;
                let mut unknown = false;
                for child in children {
                    match child.evaluate_validated(evidence, context, now) {
                        MatchState::Match => matched = true,
                        MatchState::NoMatch => unmatched = true,
                        MatchState::Unknown => unknown = true,
                    }
                }
                match self {
                    Self::AnyOf(_) if matched => MatchState::Match,
                    Self::NoneOf(_) if matched => MatchState::NoMatch,
                    // Preserve uncertainty through all_of/none_of rather than
                    // permitting missing data through nested negation.
                    _ if unknown => MatchState::Unknown,
                    Self::AllOf(_) if unmatched => MatchState::NoMatch,
                    Self::AllOf(_) | Self::NoneOf(_) => MatchState::Match,
                    _ => MatchState::NoMatch,
                }
            }
        }
    }
}

impl PolicyPredicate {
    fn evaluate(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        context: &AdmissionContext,
        now: u64,
    ) -> MatchState {
        let result = match self {
            Self::SourceCidr(cidr) => context
                .observed_source
                .filter(|source| source.provenance == SourceAddressProvenance::DirectPeer)
                .map(|source| cidr.contains(source.address)),
            Self::Audience(value) => Some(evidence.audience().contains(value)),
            Self::Scope(value) => evidence
                .claims()
                .scopes
                .as_ref()
                .map(|set| set.contains(value)),
            Self::Tag(value) => evidence
                .claims()
                .tags
                .as_ref()
                .map(|set| set.contains(value)),
            Self::AttributeEquals { key, value } => evidence
                .claims()
                .attributes
                .as_ref()
                .map(|attrs| attrs.get(key) == Some(value)),
            Self::TolerateStalePolicy => Some(true),
            Self::PolicySynchronizedWithin {
                max_age,
                require_root,
            } => {
                if context.pending_policy_content {
                    None
                } else {
                    context.policy_sync.as_ref().and_then(|sync| {
                        now.checked_sub(sync.completed_at).map(|age| {
                            (!require_root || sync.source == SynchronizationSource::Root)
                                && Duration::from_secs(age) <= *max_age
                        })
                    })
                }
            }
        };
        match result {
            Some(true) => MatchState::Match,
            Some(false) => MatchState::NoMatch,
            None => MatchState::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenialReason {
    NodeRevoked,
    RegistrationSuperseded,
    PolicyStateUnavailable,
    CredentialRevoked,
    InvalidValidity,
    PolicyDidNotMatch,
    MissingEvidence,
    PolicyError(PolicyError),
    PolicyTimeout,
    Consumer(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    Allow,
    Deny(DenialReason),
}

/// Consumer extension point. Invoke through [`admit`] to enforce validity,
/// known revocation and timeout checks outside consumer-controlled logic.
#[async_trait]
pub trait AdmissionPolicy: Send + Sync {
    async fn evaluate(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        context: &AdmissionContext,
        now: u64,
    ) -> Result<AdmissionDecision, PolicyError>;
}

#[async_trait]
impl AdmissionPolicy for PolicyExpressionV1 {
    async fn evaluate(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        context: &AdmissionContext,
        now: u64,
    ) -> Result<AdmissionDecision, PolicyError> {
        Ok(
            match PolicyExpressionV1::evaluate(self, evidence, context, now)? {
                MatchState::Match => AdmissionDecision::Allow,
                MatchState::NoMatch => AdmissionDecision::Deny(DenialReason::PolicyDidNotMatch),
                MatchState::Unknown => AdmissionDecision::Deny(DenialReason::MissingEvidence),
            },
        )
    }
}

#[derive(Default)]
struct RevocationState {
    nodes: BTreeSet<([u8; 32], [u8; 32])>,
    credentials: BTreeSet<([u8; 32], String)>,
}

/// In-memory monotonic denial set, scoped by issuer. Insert only after the
/// consumer has authenticated the root's revocation statement. There is no
/// removal API: reauthorization/revisions and durable storage need a separate
/// verified policy-store implementation. An empty set makes no freshness claim.
#[derive(Default)]
pub struct KnownRevocations(RwLock<RevocationState>);

impl KnownRevocations {
    pub fn revoke_node(&self, issuer: [u8; 32], subject: [u8; 32]) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .nodes
            .insert((issuer, subject));
    }
    pub fn revoke_credential(&self, issuer: [u8; 32], credential_id: impl Into<String>) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .credentials
            .insert((issuer, credential_id.into()));
    }
}

/// Mandatory denial state checked before and after the consumer handler.
/// Implementations must be trusted local policy, never supplied by a peer.
pub trait AdmissionGuard: Send + Sync {
    fn denial(&self, evidence: &VerifiedAdmissionEvidence, now: u64) -> Option<DenialReason>;
}

impl AdmissionGuard for KnownRevocations {
    fn denial(&self, evidence: &VerifiedAdmissionEvidence, _now: u64) -> Option<DenialReason> {
        let state = self.0.read().unwrap_or_else(|e| e.into_inner());
        if state.nodes.contains(&(evidence.issuer, evidence.subject)) {
            Some(DenialReason::NodeRevoked)
        } else if evidence.credential_id().is_some_and(|id| {
            state
                .credentials
                .contains(&(evidence.issuer, id.to_owned()))
        }) {
            Some(DenialReason::CredentialRevoked)
        } else {
            None
        }
    }
}

/// Mandatory checks always surround the explicitly selected policy. Timeout
/// bounds async/cooperative handlers, not a handler that blocks the executor;
/// consumers must move blocking work out of their future. The trusted clock is
/// sampled before and after the handler so cached evidence cannot outlive exp.
pub async fn admit(
    evidence: &VerifiedAdmissionEvidence,
    revocations: &dyn AdmissionGuard,
    context: &AdmissionContext,
    policy: &dyn AdmissionPolicy,
    timeout: Duration,
    clock: impl Fn() -> u64,
) -> AdmissionDecision {
    let now = clock();
    if let Some(reason) = mandatory_denial(evidence, revocations, now) {
        return AdmissionDecision::Deny(reason);
    }
    let result = tokio::time::timeout(timeout, policy.evaluate(evidence, context, now)).await;
    // A revocation learned while the handler awaited cannot be overridden by
    // its allow, nor can the handler's delay extend signed validity.
    if let Some(reason) = mandatory_denial(evidence, revocations, clock()) {
        return AdmissionDecision::Deny(reason);
    }
    match result {
        Ok(Ok(decision)) => decision,
        Ok(Err(error)) => AdmissionDecision::Deny(DenialReason::PolicyError(error)),
        Err(_) => AdmissionDecision::Deny(DenialReason::PolicyTimeout),
    }
}

fn mandatory_denial(
    evidence: &VerifiedAdmissionEvidence,
    revocations: &dyn AdmissionGuard,
    now: u64,
) -> Option<DenialReason> {
    revocations
        .denial(evidence, now)
        .or_else(|| (!evidence.valid_at(now)).then_some(DenialReason::InvalidValidity))
}
