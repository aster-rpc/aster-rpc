//! The issuer service and credential renewal (enrollment §4, B11).
//!
//! An [`Issuer`] runs on a node that holds a delegation chain from the root.
//! It enrolls nodes for an orchestrator with [`Issuer::issue_for`] and
//! serves `aster.enrollment.IssuerV1`, whose `renew` call gives an admitted
//! node a fresh credential with the same claims before its current one
//! expires. A node renews automatically with [`Enrollment::start_renewal`]
//! until the root promotes it to a registration. Issuance semantics live in
//! [`aster_transport_core::enrollment::issuer`].

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster_transport_core::enrollment::issuer as core_issuer;
use serde::{Deserialize, Serialize};

use super::{
    AdmissionClaims, DelegatedIssuer, Enrollment, IssuanceError, IssuedEnrollmentCredential,
    Registration, SubjectStanding, MAX_CHAIN_BYTES,
};
use crate::delegation::Chain;
use crate::rpc::{
    async_trait, Call, CallOptions, RpcStatus, ServiceDispatch, StatusCode, StreamHeader,
};

pub const ISSUER_SERVICE: &str = "aster.enrollment.IssuerV1";
pub const ISSUER_SERVICE_VERSION: i32 = 1;
/// Default lifetime of a credential an issuer signs (enrollment D4).
pub const DEFAULT_ISSUED_LIFETIME: Duration = Duration::from_secs(24 * 3600);

/// `renew`: the caller's current credential chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenewRequestV1 {
    pub chain: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenewedV1 {
    pub chain: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssuerErrorCodeV1 {
    /// The caller has no admission session with the issuer; enroll first.
    NotAdmitted,
    /// The root has denied the subject.
    SubjectDenied,
    /// A registration admits the subject; stop renewing.
    Registered,
    /// The issuer could not evaluate root policy now; retry.
    PolicyUnavailable,
    /// Another issuer (or the root) signed the presented credential.
    ForeignCredential,
    /// The root has revoked this issuer.
    IssuerRevoked,
    /// The presented credential does not verify, for example it expired.
    InvalidCredential,
    /// The request was malformed.
    InvalidArgument,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssuerErrorV1 {
    pub code: IssuerErrorCodeV1,
    pub message: String,
}

impl IssuerErrorV1 {
    fn new(code: IssuerErrorCodeV1, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Whether a later attempt can succeed without a change of policy.
    pub fn retryable(&self) -> bool {
        matches!(
            self.code,
            IssuerErrorCodeV1::NotAdmitted | IssuerErrorCodeV1::PolicyUnavailable
        )
    }
}

impl From<IssuanceError> for IssuerErrorV1 {
    fn from(error: IssuanceError) -> Self {
        let code = match &error {
            IssuanceError::SubjectDenied(_) => IssuerErrorCodeV1::SubjectDenied,
            IssuanceError::Registered => IssuerErrorCodeV1::Registered,
            IssuanceError::PolicyUnavailable => IssuerErrorCodeV1::PolicyUnavailable,
            IssuanceError::ForeignCredential => IssuerErrorCodeV1::ForeignCredential,
            IssuanceError::IssuerRevoked => IssuerErrorCodeV1::IssuerRevoked,
            IssuanceError::Credential(_) => IssuerErrorCodeV1::InvalidCredential,
        };
        Self::new(code, error.to_string())
    }
}

impl std::fmt::Display for IssuerErrorV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}
impl std::error::Error for IssuerErrorV1 {}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// An online issuer: this node, signing under its delegation from the root.
/// Register it with an RPC [`Server`](crate::rpc::Server) to serve renewals.
#[derive(Clone)]
pub struct Issuer {
    enrollment: Enrollment,
    issuer: Arc<DelegatedIssuer>,
}

impl Issuer {
    /// `delegation` is this node's chain from the root; its leaf must name
    /// this node's key. Credentials last `lifetime`, shortened to fit the
    /// delegation's `max_child_lifetime` and expiry.
    pub fn new(
        node: &crate::Node,
        enrollment: Enrollment,
        delegation: Chain,
        lifetime: Duration,
    ) -> anyhow::Result<Self> {
        let secret = node.export_secret_key()?.to_bytes();
        let issuer = DelegatedIssuer::new(
            &secret,
            delegation,
            enrollment.inner.audience(),
            lifetime.as_secs(),
        )?;
        Ok(Self {
            enrollment,
            issuer: Arc::new(issuer),
        })
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.issuer.public_key()
    }

    /// When this issuer's delegation expires; only the root renews it.
    pub fn delegation_expires_at(&self) -> u64 {
        self.issuer.delegation_expires_at()
    }

    /// Enroll `subject` for an orchestrator's provisioning channel. Refuses a
    /// subject the root denied or registered; a persistent credential is
    /// observed for the root when this node is an observer.
    pub fn issue_for(
        &self,
        subject: &[u8; 32],
        claims: AdmissionClaims,
        registration: Registration,
    ) -> Result<IssuedEnrollmentCredential, IssuanceError> {
        core_issuer::issue_for(
            &self.enrollment.inner,
            &self.issuer,
            subject,
            claims,
            registration,
            unix_now(),
        )
    }

    async fn renew(&self, caller: &str, raw: &[u8]) -> Result<RenewedV1, IssuerErrorV1> {
        let caller: [u8; 32] = hex::decode(caller)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| IssuerErrorV1::new(IssuerErrorCodeV1::InvalidArgument, "caller"))?;
        // Gate 0 admitted the connection; recheck the session now.
        if let Err(error) = self.enrollment.check_peer(&caller).await {
            return Err(match error {
                super::AdmissionError::Denied(reason) => IssuerErrorV1::new(
                    IssuerErrorCodeV1::SubjectDenied,
                    format!("caller denied: {reason:?}"),
                ),
                other => IssuerErrorV1::new(IssuerErrorCodeV1::NotAdmitted, other.to_string()),
            });
        }
        let request: RenewRequestV1 = serde_json::from_slice(raw).map_err(|_| {
            IssuerErrorV1::new(IssuerErrorCodeV1::InvalidArgument, "invalid renew request")
        })?;
        let issued = core_issuer::renew(
            &self.enrollment.inner,
            &self.issuer,
            &caller,
            &request.chain,
            unix_now(),
        )?;
        let expires_at = self
            .enrollment
            .inner
            .verify_credential(&caller, issued.chain())
            .ok()
            .and_then(|evidence| evidence.expires_at())
            .unwrap_or_default();
        Ok(RenewedV1 {
            chain: issued.into_chain(),
            expires_at,
        })
    }
}

#[async_trait]
impl ServiceDispatch for Issuer {
    fn name(&self) -> &str {
        ISSUER_SERVICE
    }
    fn version(&self) -> i32 {
        ISSUER_SERVICE_VERSION
    }
    fn methods(&self) -> &[&str] {
        &["renew"]
    }
    fn iroh_only(&self) -> bool {
        true
    }
    fn method_timeout(&self, _: &str) -> Option<Duration> {
        Some(Duration::from_secs(30))
    }
    fn max_frame_size(&self, _: &str) -> Option<u32> {
        Some((MAX_CHAIN_BYTES + 1024) as u32)
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let caller = call.peer().to_string();
        let result = match call.recv_request().await {
            Some(raw) if raw.len() <= MAX_CHAIN_BYTES + 1024 => self.renew(&caller, &raw).await,
            _ => Err(IssuerErrorV1::new(
                IssuerErrorCodeV1::InvalidArgument,
                "invalid renew frame",
            )),
        };
        match serde_json::to_vec(&result) {
            Ok(bytes) => {
                let _ = call.respond(bytes, &RpcStatus::ok()).await;
            }
            Err(_) => {
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::Internal,
                        "issuer encoding failed",
                    ))
                    .await;
            }
        }
    }
}

/// Ask `issuer` to renew this node's current credential. The caller must
/// already hold an admission session with the issuer.
pub async fn renew_from(
    node: &crate::Node,
    issuer: &crate::NodeId,
    chain: &str,
    timeout: Duration,
) -> anyhow::Result<Result<RenewedV1, IssuerErrorV1>> {
    let header = StreamHeader {
        service: ISSUER_SERVICE.into(),
        method: "renew".into(),
        version: ISSUER_SERVICE_VERSION,
        ..Default::default()
    };
    let request = serde_json::to_vec(&RenewRequestV1 {
        chain: chain.to_owned(),
    })?;
    let reply = node
        .rpc_connect(issuer)
        .await?
        .unary_with(&header, request, &CallOptions::new().deadline(timeout))
        .await?;
    Ok(serde_json::from_slice(&reply)?)
}

/// Where and how a node renews its credential.
#[derive(Debug, Clone)]
pub struct RenewalConfig {
    /// The issuer that signed this node's credential.
    pub issuer: crate::NodeAddr,
    /// First retry delay after a failed attempt; doubles up to `max_retry`.
    pub retry: Duration,
    pub max_retry: Duration,
    /// Per-call deadline.
    pub call_timeout: Duration,
}

impl RenewalConfig {
    pub fn new(issuer: crate::NodeAddr) -> Self {
        Self {
            issuer,
            retry: Duration::from_secs(1),
            max_retry: Duration::from_secs(60),
            call_timeout: Duration::from_secs(30),
        }
    }
}

/// Why renewal stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewalOutcome {
    /// The root registered this node; the record is its authority now.
    Registered,
    /// The issuer refused for a reason a retry cannot fix.
    Refused(IssuerErrorV1),
    /// The credential expired before a renewal succeeded.
    Expired,
    /// This node holds no credential to renew.
    NoCredential,
}

/// The running renewal task. Dropping it stops renewal.
pub struct RenewalHandle {
    task: tokio::task::JoinHandle<RenewalOutcome>,
}

impl RenewalHandle {
    /// Wait for renewal to stop.
    pub async fn finished(mut self) -> RenewalOutcome {
        (&mut self.task)
            .await
            .unwrap_or(RenewalOutcome::NoCredential)
    }
}

impl Drop for RenewalHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Longest sleep between checks, so a promotion is noticed promptly.
const RENEWAL_CHECK: Duration = Duration::from_secs(5);

impl Enrollment {
    /// Renew this node's credential from `config.issuer` whenever a third of
    /// its lifetime remains, retrying with backoff until it expires. The new
    /// credential is presented to every admitted peer. Renewal stops when the
    /// root registers this node: the record replaces the credential.
    pub fn start_renewal(&self, config: RenewalConfig) -> RenewalHandle {
        let enrollment = self.clone();
        RenewalHandle {
            task: tokio::spawn(async move { enrollment.renew_until_done(config).await }),
        }
    }

    async fn renew_until_done(&self, config: RenewalConfig) -> RenewalOutcome {
        let own = self.node.id();
        let Ok(own_key) = hex::decode(own.as_str())
            .map_err(drop)
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).map_err(drop))
        else {
            return RenewalOutcome::NoCredential;
        };
        let mut retry = config.retry;
        loop {
            if self.inner.subject_standing(&own_key) == SubjectStanding::Registered {
                return RenewalOutcome::Registered;
            }
            let Some((chain, evidence)) = self.inner.credential() else {
                return RenewalOutcome::NoCredential;
            };
            let Some(expires_at) = evidence.expires_at() else {
                return RenewalOutcome::NoCredential;
            };
            let now = unix_now();
            if now >= expires_at {
                return RenewalOutcome::Expired;
            }
            let lifetime = expires_at.saturating_sub(evidence.issued_at());
            let due = expires_at.saturating_sub(lifetime / 3);
            if now < due {
                let wait = Duration::from_secs(due - now).min(RENEWAL_CHECK);
                tokio::time::sleep(wait.max(Duration::from_millis(200))).await;
                continue;
            }
            match self.renew_once(&config, &chain).await {
                Ok(Ok(renewed)) => {
                    retry = config.retry;
                    if let Err(error) = self.inner.replace_credential(renewed.chain).await {
                        tracing::warn!(%error, "enrollment: renewed credential rejected");
                    }
                    continue;
                }
                Ok(Err(refused)) if refused.code == IssuerErrorCodeV1::Registered => {
                    return RenewalOutcome::Registered;
                }
                Ok(Err(refused)) if !refused.retryable() => {
                    return RenewalOutcome::Refused(refused);
                }
                Ok(Err(refused)) => {
                    tracing::debug!(%refused, "enrollment: renewal will retry");
                }
                Err(error) => {
                    tracing::debug!(%error, "enrollment: renewal will retry");
                }
            }
            let left = Duration::from_secs(expires_at.saturating_sub(unix_now()));
            tokio::time::sleep(retry.min(left).max(Duration::from_millis(100))).await;
            retry = (retry * 2).min(config.max_retry);
        }
    }

    async fn renew_once(
        &self,
        config: &RenewalConfig,
        chain: &str,
    ) -> anyhow::Result<Result<RenewedV1, IssuerErrorV1>> {
        // Enroll with the issuer first: renewal runs on an admitted session.
        self.connect(config.issuer.clone()).await?;
        renew_from(
            &self.node,
            &config.issuer.node_id,
            chain,
            config.call_timeout,
        )
        .await
    }
}
