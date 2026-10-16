//! Authenticated Exec admission. Requests contain no caller identity.
//!
//! Wire contract: `aster.exec.Exec`, version 1, over authenticated Iroh RPC.
//! Unary request frames contain UTF-8 JSON of the versioned contract structs;
//! responses contain serde JSON `Result<T, ExecErrorV1>` (`{"Ok": ...}` or
//! `{"Err": ...}`). `submit` returns `JobRefV1`. `watch` emits one such result
//! per `JobEventV1` frame and ends with an OK RPC trailer. `targets` is an
//! additive discovery convenience. Business errors use the typed JSON envelope;
//! malformed framing/transport failures use RPC status. This explicitly uses
//! JSON payloads rather than generated Fory serialization.
use crate::{
    authorization::{AuthorizedSubmit, LocalCeilingV1},
    contract::*,
    ledger::{LedgerError, RequestLedger, ReservationPhase, request_key},
    policy::{
        ExecGrantStore, ExecGrantV1, ExecOperationV1, GrantError, MAX_ISSUED_AT_SKEW_SECONDS,
        grant_subject, is_user_principal, parse_node_id as identity_bytes,
    },
    unix_millis as now_ms,
};
use aster::audit_outbox::{AuditEvent, AuditOutbox};
use aster::delegation::{
    LeafClaims, LeafKind, RevokedDelegates, TrustContext, VerifiedChain, verify_chain,
};
use aster::enrollment::{AdmissionError, EnrollmentRevoked};
use aster::rpc::{Call, CallContext, RpcStatus, ServiceDispatch, StatusCode, async_trait};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast};

pub const MAX_AUTHORITY_PEERS: usize = 1024;
pub const MAX_REFRESH_PEERS: usize = 128;
/// A positive authority snapshot older than this denies calls.
pub const AUTHORITY_FRESHNESS: Duration = Duration::from_secs(30);
/// Running jobs survive this long while their owner's policy cannot be
/// evaluated. A definite denial cancels them at once.
pub const UNKNOWN_STANDING_GRACE: Duration = Duration::from_secs(300);
const DENIAL_FRESHNESS: Duration = Duration::from_secs(1);
const AUTHORITY_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_REFRESHES: usize = 16;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);
const RECONCILE_DEADLINE: Duration = Duration::from_secs(60);
/// After this long, an accepted launch that is no longer in flight has
/// either created its job or never will.
const LAUNCH_SETTLE: Duration = Duration::from_secs(120);
const WATCH_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_WATCHES_PER_CALLER: usize = 4;
const MAX_WATCHES: usize = 64;
/// The action-chain operation that allows an on-behalf-of submission.
pub const EXEC_SUBMIT_OPERATION: &str = "exec.submit";

/// A caller's authority as of its latest refresh.
#[derive(Clone, Debug)]
enum Standing {
    /// Enrollment and the grant both allow.
    Allowed(ExecGrantV1),
    /// No enrollment session, for example after an API restart, and nothing
    /// definitely denies the caller. Calls fail until it enrolls again; its
    /// running jobs continue while the grant stays valid.
    NotEnrolled(ExecGrantV1),
    /// A definite enrollment or grant denial. Running jobs are cancelled.
    Denied(ExecErrorV1),
    /// Policy could not be evaluated. Running jobs are cancelled only after
    /// [`UNKNOWN_STANDING_GRACE`] without a settled result.
    Unknown(ExecErrorV1),
}

impl Standing {
    /// The result an RPC receives.
    fn call_result(&self) -> Result<ExecGrantV1, ExecErrorV1> {
        match self {
            Self::Allowed(grant) => Ok(grant.clone()),
            Self::NotEnrolled(_) => Err(not_enrolled()),
            Self::Denied(failure) | Self::Unknown(failure) => Err(failure.clone()),
        }
    }

    fn is_settled(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

fn not_enrolled() -> ExecErrorV1 {
    let mut failure = error(
        ExecErrorCodeV1::Unauthenticated,
        "enrollment session required; reconnect to enroll and retry",
    );
    failure.retryable = true;
    failure
}

struct PeerAuthority {
    refreshed_at: Instant,
    last_used: Instant,
    standing: Standing,
}

#[derive(Default)]
struct AuthoritySnapshot {
    observed: BTreeSet<String>,
    refresh_offset: usize,
    peers: BTreeMap<String, PeerAuthority>,
}

impl AuthoritySnapshot {
    /// Make room for `caller` by evicting the least recently used peer.
    fn reserve(&mut self, caller: &str) {
        if self.peers.len() >= MAX_AUTHORITY_PEERS
            && !self.peers.contains_key(caller)
            && let Some(victim) = self
                .peers
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(peer, _)| peer.clone())
        {
            self.peers.remove(&victim);
            self.observed.remove(&victim);
        }
    }
}

struct InflightAdmission {
    jobs: Arc<StdMutex<BTreeSet<String>>>,
    job_id: String,
}

impl Drop for InflightAdmission {
    fn drop(&mut self) {
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.remove(&self.job_id);
        }
    }
}

/// Who made an Exec call, read from the call context: a node calling as
/// itself, or a user whose identity a gateway edge forwarded in
/// `aster-on-behalf-of`. The core verified the forwarded token and the
/// edge's `aster.gateway` role before dispatch; Exec then authorizes the user
/// (`iss|sub`) under the grant naming it, with the edge as the actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecCaller {
    id: String,
    forwarded: Option<Forwarded>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Forwarded {
    actor: String,
    issuer: String,
    jti: String,
    auth_methods: Vec<String>,
}

fn unauthenticated() -> ExecErrorV1 {
    error(
        ExecErrorCodeV1::Unauthenticated,
        "authenticated node identity required",
    )
}

impl ExecCaller {
    /// A node calling as itself; `id` is its 64-hex identity.
    pub fn node(id: impl Into<String>) -> Result<Self, ExecErrorV1> {
        let id = id.into();
        if identity_bytes(&id).is_none() {
            return Err(unauthenticated());
        }
        Ok(Self {
            id,
            forwarded: None,
        })
    }

    /// A user principal (`iss|sub`) forwarded by the gateway edge `actor`,
    /// with the token's issuer, `jti` and `amr` for the audit.
    pub fn forwarded(
        principal: impl Into<String>,
        actor: impl Into<String>,
        issuer: impl Into<String>,
        jti: impl Into<String>,
        auth_methods: Vec<String>,
    ) -> Result<Self, ExecErrorV1> {
        let (id, actor) = (principal.into(), actor.into());
        if !is_user_principal(&id) || identity_bytes(&actor).is_none() {
            return Err(error(
                ExecErrorCodeV1::Unauthenticated,
                "forwarded identity is not usable for Exec",
            ));
        }
        Ok(Self {
            id,
            forwarded: Some(Forwarded {
                actor,
                issuer: issuer.into(),
                jti: jti.into(),
                auth_methods,
            }),
        })
    }

    /// The caller of the call `context` describes. Without a forwarded
    /// identity it must be a node: an authenticator-resolved principal is
    /// never taken for a user.
    pub fn from_context(context: &CallContext) -> Result<Self, ExecErrorV1> {
        match context.on_behalf_of() {
            Some(user) => Self::forwarded(
                user.principal(),
                user.actor(),
                user.issuer(),
                user.jti(),
                user.auth_methods().to_vec(),
            ),
            None => Self::node(context.principal()),
        }
    }

    /// The node id, or the forwarded user principal (`iss|sub`).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The forwarding edge, when the call was forwarded.
    pub fn edge(&self) -> Option<&str> {
        self.forwarded.as_ref().map(|f| f.actor.as_str())
    }

    /// The call's own authority: the node itself, or the edge acting for
    /// the user, which needs the edge enrolled and the user's grant.
    fn authority(&self) -> Authority {
        match &self.forwarded {
            None => Authority::own(&self.id),
            Some(forwarded) => Authority {
                actor: forwarded.actor.clone(),
                principal: self.id.clone(),
            },
        }
    }
}

/// Whose grant an operation runs under. A caller acting for itself is its own
/// principal; an on-behalf-of submission names another principal, whose grant
/// applies while the caller (the actor) stays enrolled. A forwarded call is
/// the edge (actor) acting for the user (principal). Cached standings are
/// keyed by [`Authority::key`]. The actor of a two-party authority is always
/// a node.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Authority {
    actor: String,
    principal: String,
}

impl Authority {
    fn own(caller: &str) -> Self {
        Self {
            actor: caller.into(),
            principal: caller.into(),
        }
    }

    /// The authority a job's follow-up calls and revocation checks use. A
    /// job submitted through a gateway is its user's own (`caller_id`): the
    /// edge was a conduit, so only the user's grant keeps it running.
    fn of_job(job: &JobStatusV1) -> Self {
        Self {
            actor: job.caller_id.clone(),
            principal: job
                .principal_id
                .clone()
                .unwrap_or_else(|| job.caller_id.clone()),
        }
    }

    /// The caller's hex identity alone, or `actor/principal`.
    fn key(&self) -> String {
        if self.actor == self.principal {
            self.actor.clone()
        } else {
            format!("{}/{}", self.actor, self.principal)
        }
    }

    /// Inverse of [`key`](Self::key). The actor is a node, so a two-party key
    /// is a node id, `/`, then the principal (which may contain `/`, as an
    /// issuer URL does). A lone user principal never has that shape
    /// ([`is_user_principal`]).
    fn parse(key: &str) -> Self {
        if key.len() > 65 && key.as_bytes()[64] == b'/' && identity_bytes(&key[..64]).is_some() {
            return Self {
                actor: key[..64].into(),
                principal: key[65..].into(),
            };
        }
        Self::own(key)
    }
}

struct WatchPermit {
    watchers: Arc<StdMutex<BTreeMap<String, usize>>>,
    caller: String,
}

impl Drop for WatchPermit {
    fn drop(&mut self) {
        if let Ok(mut watchers) = self.watchers.lock()
            && let Some(count) = watchers.get_mut(&self.caller)
        {
            *count -= 1;
            if *count == 0 {
                watchers.remove(&self.caller);
            }
        }
    }
}

pub fn error(code: ExecErrorCodeV1, message: &str) -> ExecErrorV1 {
    ExecErrorV1 {
        code,
        safe_message: message.into(),
        request_id: None,
        job_id: None,
        retryable: matches!(
            code,
            ExecErrorCodeV1::PolicyUnavailable
                | ExecErrorCodeV1::AuditUnavailable
                | ExecErrorCodeV1::SupervisorUnavailable
        ),
        retry_after_ms: None,
    }
}

/// Admission and operation authority are independent checks. Only
/// [`AdmissionError::Denied`] is a definite denial.
#[async_trait]
pub trait EnrollmentAdmission: Send + Sync {
    async fn check(&self, caller: &[u8; 32]) -> Result<(), AdmissionError>;

    /// Whether the root has revoked `key` as a delegate (`IssuerStatusV1`).
    /// Delegation links in on-behalf-of action chains are withdrawn by the
    /// same records as enrollment issuers.
    fn delegate_revoked(&self, key: &[u8; 32]) -> bool;
}
#[async_trait]
impl EnrollmentAdmission for aster::enrollment::Enrollment {
    async fn check(&self, caller: &[u8; 32]) -> Result<(), AdmissionError> {
        self.check_peer(caller).await.map(|_| ())
    }

    fn delegate_revoked(&self, key: &[u8; 32]) -> bool {
        aster::delegation::RevokedDelegates::is_revoked(self, key)
    }
}

/// The enrollment view of delegate revocations, as `verify_chain` takes it.
struct AdmissionRevocations<'a>(&'a dyn EnrollmentAdmission);

impl RevokedDelegates for AdmissionRevocations<'_> {
    fn is_revoked(&self, key: &[u8; 32]) -> bool {
        self.0.delegate_revoked(key)
    }
}
/// Authenticate the current exact-root leaf on cold admission and background
/// refresh. `PermissionDenied` is a definite denial; any other error means the
/// grant could not be evaluated.
#[async_trait]
pub trait GrantProvider: Send + Sync {
    async fn current(
        &self,
        caller: [u8; 32],
        target: [u8; 32],
        now_seconds: u64,
    ) -> Result<ExecGrantV1, ExecErrorV1>;

    /// The audience every grant carries, when fixed. On-behalf-of chains are
    /// then authenticated before any grant is read for the principal they
    /// name; otherwise after, against the grant's audience.
    fn audience(&self) -> Option<String> {
        None
    }
}
pub struct DocsGrantProvider {
    pub store: Arc<ExecGrantStore>,
    pub document: aster::Doc,
}
#[async_trait]
impl GrantProvider for DocsGrantProvider {
    async fn current(
        &self,
        caller: [u8; 32],
        target: [u8; 32],
        now: u64,
    ) -> Result<ExecGrantV1, ExecErrorV1> {
        // The Docs read runs concurrently across callers; applying it may
        // fsync, so it runs off the async workers.
        let read = self.store.read_leaf(&self.document, &caller, &target).await;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            match read.and_then(|read| store.apply_leaf(&caller, &target, read)) {
                Err(GrantError::Storage(detail)) => Err(GrantError::Storage(detail)),
                // A content failure recorded any denial barrier for grant_for.
                _ => store.grant_for(&caller, &target, now),
            }
        })
        .await
        .map_err(|_| GrantError::PolicyUnavailable)
        .and_then(|result| result)
        .map_err(grant_error)
    }

    fn audience(&self) -> Option<String> {
        Some(self.store.audience().to_owned())
    }
}
fn grant_error(failure: GrantError) -> ExecErrorV1 {
    match failure {
        GrantError::Denied => error(ExecErrorCodeV1::PermissionDenied, "Exec grant denied"),
        _ => error(
            ExecErrorCodeV1::PolicyUnavailable,
            "current Exec policy unavailable",
        ),
    }
}
/// Production admission uses the shipped durable Aster outbox.
#[async_trait]
pub trait ExecAudit: Send + Sync {
    async fn admit(&self, event: AuditEvent) -> Result<(), ExecErrorV1>;
}
#[async_trait]
impl ExecAudit for AuditOutbox {
    async fn admit(&self, event: AuditEvent) -> Result<(), ExecErrorV1> {
        self.submit(event).await.map(|_| ()).map_err(|_| {
            error(
                ExecErrorCodeV1::AuditUnavailable,
                "durable Exec audit unavailable",
            )
        })
    }
}
/// Lifetime owner owns jobs beyond the lifetime of RPC futures.
#[async_trait]
pub trait ExecRuntime: Send + Sync {
    async fn capabilities(&self) -> Result<TargetCapabilitiesV1, ExecErrorV1>;
    async fn launch(
        &self,
        request: ExecRequestV1,
        authorized: AuthorizedSubmit,
        job_id: String,
        digest: [u8; 32],
        caller: String,
        accepted_at_ms: u64,
    ) -> Result<JobStatusV1, ExecErrorV1>;
    async fn status(&self, job: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1>;
    async fn output(&self, request: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1>;
    async fn cancel(&self, request: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1>;
    /// Jobs that are not terminal. May include terminal jobs; callers filter.
    async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1>;
    async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1>;
    async fn ack_outcome(&self, job_id: &str) -> Result<(), ExecErrorV1>;
}
#[derive(Clone)]
pub struct ExecService {
    ceiling: LocalCeilingV1,
    enrollment: Arc<dyn EnrollmentAdmission>,
    grants: Arc<dyn GrantProvider>,
    audit: Arc<dyn ExecAudit>,
    runtime: Arc<dyn ExecRuntime>,
    ledger: Arc<RequestLedger>,
    inflight: Arc<StdMutex<BTreeSet<String>>>,
    watchers: Arc<StdMutex<BTreeMap<String, usize>>>,
    authority: Arc<RwLock<AuthoritySnapshot>>,
    /// When each live job owner last had a settled standing, for the
    /// [`UNKNOWN_STANDING_GRACE`] decision. Pruned to current owners.
    owners_settled: Arc<StdMutex<BTreeMap<String, Instant>>>,
    cold_refresh: Arc<Mutex<BTreeMap<String, Weak<Mutex<()>>>>>,
    reconcile_lock: Arc<Mutex<()>>,
}
impl ExecService {
    pub fn new(
        ceiling: LocalCeilingV1,
        enrollment: Arc<dyn EnrollmentAdmission>,
        grants: Arc<dyn GrantProvider>,
        audit: Arc<dyn ExecAudit>,
        runtime: Arc<dyn ExecRuntime>,
        ledger: RequestLedger,
    ) -> Self {
        Self {
            ceiling,
            enrollment,
            grants,
            audit,
            runtime,
            ledger: Arc::new(ledger),
            inflight: Arc::new(StdMutex::new(BTreeSet::new())),
            watchers: Arc::new(StdMutex::new(BTreeMap::new())),
            authority: Arc::new(RwLock::new(AuthoritySnapshot::default())),
            owners_settled: Arc::new(StdMutex::new(BTreeMap::new())),
            cold_refresh: Arc::new(Mutex::new(BTreeMap::new())),
            reconcile_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Run the background work a serving Exec node needs until one part
    /// stops, and return why. The host should treat a return as fatal.
    ///
    /// - every `refresh_interval`: refresh cached caller authority;
    /// - every ten seconds: reconcile running jobs with policy, prune the
    ///   ledger and replay audit outcomes;
    /// - on each enrollment revocation: invalidate the caller and reconcile.
    ///   A lagged receiver missed revocations, so every cached caller is
    ///   rechecked before its next call.
    pub async fn maintain(
        &self,
        refresh_interval: Duration,
        mut revocations: broadcast::Receiver<EnrollmentRevoked>,
    ) -> ExecErrorV1 {
        let refresh = async {
            let mut tick = tokio::time::interval(refresh_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                self.refresh_authority().await;
            }
        };
        let reconcile = async {
            let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                self.reconcile_logged("Exec reconciliation").await;
            }
        };
        let revoke = async {
            loop {
                match revocations.recv().await {
                    Ok(event) => self.invalidate_peer(&hex::encode(event.peer)),
                    Err(broadcast::error::RecvError::Lagged(_)) => self.invalidate_all(),
                    Err(broadcast::error::RecvError::Closed) => {
                        return error(
                            ExecErrorCodeV1::Internal,
                            "enrollment revocation stream closed",
                        );
                    }
                }
                self.reconcile_logged("Exec revocation reconciliation")
                    .await;
            }
        };
        tokio::select! {
            failure = revoke => failure,
            never = refresh => never,
            never = reconcile => never,
        }
    }

    async fn reconcile_logged(&self, context: &str) {
        match tokio::time::timeout(RECONCILE_DEADLINE, self.reconcile_policy()).await {
            Ok(Ok(_)) => {}
            Ok(Err(failure)) => eprintln!("{context}: {:?} {}", failure.code, failure.safe_message),
            Err(_) => eprintln!("{context}: deadline exceeded"),
        }
    }

    /// Read the actor's enrollment and the principal's grant (the same node
    /// unless acting on another's behalf). A definite denial from either
    /// wins; otherwise any unevaluated part leaves the standing unknown. A
    /// user principal acting as itself (the owner of a job submitted through
    /// a gateway) has no enrollment: its grant alone decides.
    async fn read_standing(&self, authority: &Authority, target: [u8; 32]) -> Standing {
        let Some(principal) = grant_subject(&authority.principal) else {
            return Standing::Denied(unauthenticated());
        };
        let enrollment = match identity_bytes(&authority.actor) {
            Some(actor) => self.enrollment.check(&actor).await,
            None if authority.actor == authority.principal => Ok(()),
            None => return Standing::Denied(unauthenticated()),
        };
        if let Err(AdmissionError::Denied(_)) = enrollment {
            return Standing::Denied(error(
                ExecErrorCodeV1::PermissionDenied,
                "enrollment admission revoked",
            ));
        }
        let grant = match self
            .grants
            .current(principal, target, now_ms() / 1000)
            .await
        {
            Ok(grant) => grant,
            Err(failure) if failure.code == ExecErrorCodeV1::PermissionDenied => {
                return Standing::Denied(failure);
            }
            Err(failure) => return Standing::Unknown(failure),
        };
        match enrollment {
            Ok(()) => Standing::Allowed(grant),
            Err(AdmissionError::NotAdmitted) => Standing::NotEnrolled(grant),
            Err(_) => Standing::Unknown(error(
                ExecErrorCodeV1::PolicyUnavailable,
                "enrollment policy unavailable",
            )),
        }
    }

    /// Refresh one caller now, outside its next call, and return what that
    /// call would receive.
    pub async fn refresh_peer(&self, caller: &str) -> Result<(), ExecErrorV1> {
        self.refresh_standing(caller)
            .await
            .call_result()
            .map(|_| ())
    }

    /// Refresh one authority key (see [`Authority::key`]) outside the RPC hot
    /// path and record the result. A refresh that started before a newer
    /// recorded one is discarded.
    async fn refresh_standing(&self, caller: &str) -> Standing {
        let started = Instant::now();
        let authority = Authority::parse(caller);
        let Some(target_bytes) = identity_bytes(&self.ceiling.target_id) else {
            return Standing::Unknown(ExecErrorV1::invalid("invalid target identity"));
        };
        let standing = tokio::time::timeout(
            AUTHORITY_READ_TIMEOUT,
            self.read_standing(&authority, target_bytes),
        )
        .await
        .unwrap_or_else(|_| {
            Standing::Unknown(error(
                ExecErrorCodeV1::PolicyUnavailable,
                "Exec authority refresh timed out",
            ))
        });
        let mut snapshot = self.authority.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = snapshot.peers.get(caller)
            && existing.refreshed_at > started
        {
            return existing.standing.clone();
        }
        snapshot.reserve(caller);
        if matches!(standing, Standing::Allowed(_)) {
            snapshot.observed.insert(caller.into());
        } else {
            snapshot.observed.remove(caller);
        }
        let last_used = snapshot
            .peers
            .get(caller)
            .map_or(started, |entry| entry.last_used);
        snapshot.peers.insert(
            caller.into(),
            PeerAuthority {
                refreshed_at: started,
                last_used,
                standing: standing.clone(),
            },
        );
        standing
    }

    /// Immediately deny a revoked caller, including where it acts for another
    /// principal. An older in-flight refresh cannot replace this denial.
    pub fn invalidate_peer(&self, caller: &str) {
        let mut snapshot = self.authority.write().unwrap_or_else(|e| e.into_inner());
        let mut keys: Vec<String> = snapshot
            .peers
            .keys()
            .filter(|key| Authority::parse(key).actor == caller)
            .cloned()
            .collect();
        if !keys.iter().any(|key| key == caller) {
            keys.push(caller.to_owned());
        }
        let now = Instant::now();
        for key in keys {
            snapshot.observed.remove(&key);
            snapshot.reserve(&key);
            snapshot.peers.insert(
                key,
                PeerAuthority {
                    refreshed_at: now,
                    last_used: now,
                    standing: Standing::Denied(error(
                        ExecErrorCodeV1::PermissionDenied,
                        "enrollment admission revoked",
                    )),
                },
            );
        }
    }

    /// Force every cached caller to be read again before its next call.
    pub fn invalidate_all(&self) {
        let mut snapshot = self.authority.write().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        for entry in snapshot.peers.values_mut() {
            entry.refreshed_at = now;
            entry.standing = Standing::Unknown(error(
                ExecErrorCodeV1::PolicyUnavailable,
                "Exec authority is being rechecked",
            ));
        }
    }

    /// Refresh a rotating window of recently admitted callers so warm calls
    /// stay within [`AUTHORITY_FRESHNESS`]. Each read has its own deadline
    /// and reads run with bounded concurrency.
    pub async fn refresh_authority(&self) {
        let callers: Vec<String> = {
            let mut snapshot = self.authority.write().unwrap_or_else(|e| e.into_inner());
            let all: Vec<String> = snapshot.observed.iter().cloned().collect();
            let window = all
                .iter()
                .cycle()
                .skip(snapshot.refresh_offset)
                .take(all.len().min(MAX_REFRESH_PEERS))
                .cloned()
                .collect();
            snapshot.refresh_offset = if all.is_empty() {
                0
            } else {
                (snapshot.refresh_offset + MAX_REFRESH_PEERS) % all.len()
            };
            window
        };
        self.refresh_many(callers).await;
    }

    async fn refresh_many(&self, callers: Vec<String>) -> BTreeMap<String, Standing> {
        let mut queue = callers.into_iter();
        let mut tasks = tokio::task::JoinSet::new();
        let mut standings = BTreeMap::new();
        loop {
            while tasks.len() < MAX_CONCURRENT_REFRESHES {
                let Some(caller) = queue.next() else { break };
                let service = self.clone();
                tasks.spawn(async move {
                    let standing = service.refresh_standing(&caller).await;
                    (caller, standing)
                });
            }
            match tasks.join_next().await {
                Some(Ok((caller, standing))) => {
                    standings.insert(caller, standing);
                }
                Some(Err(panic)) => eprintln!("Exec authority refresh task failed: {panic}"),
                None => break,
            }
        }
        standings
    }

    /// Authorize a caller under its own grant.
    #[cfg(test)]
    async fn authorize(
        &self,
        caller: &str,
        target: &str,
        op: ExecOperationV1,
    ) -> Result<ExecGrantV1, ExecErrorV1> {
        self.authorize_as(&Authority::own(caller), target, op).await
    }

    /// The authority a follow-up call on `job` runs under: a forwarded user
    /// acts through the edge of this call (which must be enrolled), a node
    /// under the job's own authority.
    fn job_authority(caller: &ExecCaller, job: &JobStatusV1) -> Authority {
        match caller.forwarded {
            Some(_) => caller.authority(),
            None => Authority::of_job(job),
        }
    }

    /// Checks that need no policy read: the caller's identity, the target,
    /// the owner's local ceiling and its denied callers.
    /// A user principal reaches here only through [`ExecCaller::forwarded`]
    /// or as the owner of a job submitted that way.
    fn precheck(&self, caller: &str, target: &str, op: ExecOperationV1) -> Result<(), ExecErrorV1> {
        if identity_bytes(caller).is_none() && !is_user_principal(caller) {
            return Err(unauthenticated());
        }
        if target != self.ceiling.target_id
            || self.ceiling.version != 1
            || self.ceiling.denied_callers.contains(caller)
            || !self.ceiling.enabled_operations.contains(&op)
        {
            return Err(error(
                ExecErrorCodeV1::PermissionDenied,
                "Exec operation denied",
            ));
        }
        Ok(())
    }

    /// Authorize `op` under the principal's grant for the actor. Both must
    /// pass the local ceiling; only the actor's enrollment is checked.
    async fn authorize_as(
        &self,
        authority: &Authority,
        target: &str,
        op: ExecOperationV1,
    ) -> Result<ExecGrantV1, ExecErrorV1> {
        self.precheck(&authority.actor, target, op)?;
        if authority.principal != authority.actor {
            self.precheck(&authority.principal, target, op)?;
        }
        let key = authority.key();
        let caller = key.as_str();
        if self.needs_cold_refresh(caller) {
            // Only first calls for the same peer share an authority read.
            // A slow peer cannot serialize cold admission for other peers.
            let peer_lock = {
                let mut locks = self.cold_refresh.lock().await;
                if locks.len() >= MAX_AUTHORITY_PEERS {
                    locks.retain(|_, lock| lock.strong_count() > 0);
                }
                if let Some(lock) = locks.get(caller).and_then(Weak::upgrade) {
                    lock
                } else {
                    if locks.len() >= MAX_AUTHORITY_PEERS {
                        return Err(error(
                            ExecErrorCodeV1::ResourceExhausted,
                            "too many concurrent authority refreshes",
                        ));
                    }
                    let lock = Arc::new(Mutex::new(()));
                    locks.insert(caller.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            };
            let _refresh = peer_lock.lock().await;
            if self.needs_cold_refresh(caller) {
                self.refresh_standing(caller).await;
            }
        }
        let grant = {
            let mut snapshot = self.authority.write().unwrap_or_else(|e| e.into_inner());
            let entry = snapshot.peers.get_mut(caller).ok_or_else(|| {
                error(
                    ExecErrorCodeV1::PolicyUnavailable,
                    "Exec authority snapshot warming",
                )
            })?;
            if entry.refreshed_at.elapsed() > AUTHORITY_FRESHNESS {
                return Err(error(
                    ExecErrorCodeV1::PolicyUnavailable,
                    "Exec authority snapshot expired",
                ));
            }
            entry.last_used = Instant::now();
            entry.standing.call_result()?
        };
        self.check_grant(&grant, &authority.principal, op)?;
        Ok(grant)
    }

    /// A caller with no entry, or whose last result was a failure more than
    /// [`DENIAL_FRESHNESS`] ago, is read again before this call.
    fn needs_cold_refresh(&self, caller: &str) -> bool {
        self.authority
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .peers
            .get(caller)
            .is_none_or(|entry| {
                !matches!(entry.standing, Standing::Allowed(_))
                    && entry.refreshed_at.elapsed() > DENIAL_FRESHNESS
            })
    }

    /// Time-dependent grant checks, applied on every use of a snapshot.
    /// `caller` is the grant holder: the principal of the authority.
    fn check_grant(
        &self,
        grant: &ExecGrantV1,
        caller: &str,
        op: ExecOperationV1,
    ) -> Result<(), ExecErrorV1> {
        let now = now_ms() / 1000;
        if !grant.cancel_on_revocation
            || grant.version != 1
            || grant.status != crate::policy::GrantStatusV1::Active
            || grant.issued_at > now.saturating_add(MAX_ISSUED_AT_SKEW_SECONDS)
            || grant.not_before.is_some_and(|time| now < time)
            || grant.expires_at.is_some_and(|time| now >= time)
            || grant.caller != caller
            || grant.target != self.ceiling.target_id
            || grant.account_alias != self.ceiling.account_alias
            || !grant.operations.contains(&op)
        {
            return Err(error(
                ExecErrorCodeV1::PermissionDenied,
                "Exec operation denied",
            ));
        }
        Ok(())
    }

    /// Whether a live job's owner has lost authority, from a fresh read.
    fn should_cancel(&self, owner: &str, standing: &Standing) -> bool {
        let settled_at = {
            let mut settled = self
                .owners_settled
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let entry = settled.entry(owner.to_owned()).or_insert_with(Instant::now);
            if standing.is_settled() {
                *entry = Instant::now();
            }
            *entry
        };
        match standing {
            Standing::Allowed(grant) | Standing::NotEnrolled(grant) => self
                .check_grant(
                    grant,
                    &Authority::parse(owner).principal,
                    ExecOperationV1::Submit,
                )
                .is_err(),
            Standing::Denied(_) => true,
            Standing::Unknown(_) => settled_at.elapsed() >= UNKNOWN_STANDING_GRACE,
        }
    }
    /// A job's submitter may use it under the job's authority: its own grant,
    /// or the principal's grant for an on-behalf-of job. The submitter's own
    /// grant never authorizes another principal's job.
    async fn owned(
        &self,
        caller: &ExecCaller,
        job: &JobRefV1,
        op: ExecOperationV1,
    ) -> Result<JobStatusV1, ExecErrorV1> {
        self.precheck(caller.id(), &job.target_id, op)?;
        let status = match self.runtime.status(job).await {
            Ok(status) => status,
            Err(failure) => {
                // Only an authorized caller learns why the lookup failed.
                self.authorize_as(&caller.authority(), &job.target_id, op)
                    .await?;
                return Err(failure);
            }
        };
        if status.caller_id != caller.id() || status.job != *job {
            return Err(error(
                ExecErrorCodeV1::PermissionDenied,
                "job access denied",
            ));
        }
        self.authorize_as(&Self::job_authority(caller, &status), &job.target_id, op)
            .await?;
        Ok(status)
    }
    async fn existing_submission(
        &self,
        key: &str,
        old: crate::ledger::Reservation,
        request: &ExecRequestV1,
        digest: [u8; 32],
    ) -> Result<JobStatusV1, ExecErrorV1> {
        if old.digest != digest {
            return Err(error(
                ExecErrorCodeV1::RequestConflict,
                "request ID content conflict",
            ));
        }
        if let Some(failure) = old.admission_error {
            return Err(failure);
        }
        let active_admission = self
            .inflight
            .lock()
            .map_err(|_| error(ExecErrorCodeV1::Internal, "admission state unavailable"))?
            .contains(&old.job_id);
        if old.phase == ReservationPhase::Preparing {
            if active_admission {
                let mut pending = error(
                    ExecErrorCodeV1::SupervisorUnavailable,
                    "request admission is still in progress; retry with the same request ID",
                );
                pending.request_id = Some(request.request_id.clone());
                pending.job_id = Some(old.job_id);
                return Err(pending);
            }
            let mut failure = error(
                ExecErrorCodeV1::AuditUnavailable,
                "previous admission was interrupted before launch; use a new request ID",
            );
            failure.request_id = Some(request.request_id.clone());
            failure.job_id = Some(old.job_id.clone());
            failure.retryable = false;
            return match self.ledger.fail_preparing(key, failure.clone(), now_ms()) {
                Ok(_) => Err(failure),
                // Another admission advanced it meanwhile; report its state.
                Err(LedgerError::InvalidTransition) => {
                    let mut pending = error(
                        ExecErrorCodeV1::SupervisorUnavailable,
                        "request admission changed; retry with the same request ID",
                    );
                    pending.request_id = Some(request.request_id.clone());
                    pending.job_id = Some(old.job_id);
                    Err(pending)
                }
                Err(other) => Err(ledger_error(other)),
            };
        }
        // Accepted launch outcomes are ambiguous across a helper restart or
        // lost response. Keep the durable reservation and its capacity until
        // the lifetime owner proves a terminal result; never claim failure.
        self.runtime
            .status(&JobRefV1 {
                job_id: old.job_id.clone(),
                target_id: request.target_id.clone(),
            })
            .await
            .map_err(|_| {
                let mut unresolved = error(
                    ExecErrorCodeV1::SupervisorUnavailable,
                    "previous launch outcome is unresolved; retry this request ID or inspect the job ID",
                );
                unresolved.request_id = Some(request.request_id.clone());
                unresolved.job_id = Some(old.job_id);
                unresolved
            })
    }
    /// Submit for the node `caller` (see [`submit_as`](Self::submit_as)).
    pub async fn submit(
        &self,
        caller: &str,
        request: ExecRequestV1,
    ) -> Result<JobStatusV1, ExecErrorV1> {
        self.submit_as(&ExecCaller::node(caller)?, request).await
    }

    /// Submit for `caller`: a node under its own grant, a node acting for a
    /// principal under that principal's action chain, or a gateway-forwarded
    /// user under the grant naming it (`iss|sub`).
    pub async fn submit_as(
        &self,
        exec_caller: &ExecCaller,
        mut request: ExecRequestV1,
    ) -> Result<JobStatusV1, ExecErrorV1> {
        request.validate()?;
        let caller = exec_caller.id();
        let authority = match (&exec_caller.forwarded, &request.on_behalf_of) {
            (None, None) => Authority::own(caller),
            (Some(_), None) => exec_caller.authority(),
            // An action chain names its actor's key; a forwarded user has none.
            (Some(_), Some(_)) => {
                return Err(ExecErrorV1::invalid(
                    "a call forwarded by a gateway cannot carry on_behalf_of",
                ));
            }
            (None, Some(on_behalf)) if on_behalf.principal == caller => {
                return Err(ExecErrorV1::invalid(
                    "on_behalf_of names the caller; submit without it",
                ));
            }
            (None, Some(on_behalf)) => Authority {
                actor: caller.into(),
                principal: on_behalf.principal.clone(),
            },
        };
        // Bound to the request exactly as sent, before defaults are resolved.
        let request_digest = request.on_behalf_digest()?;
        // Authenticate the chain before reading the grant of the principal it
        // names, so an actor cannot make this node read arbitrary grants.
        let early = match (&request.on_behalf_of, self.grants.audience()) {
            (Some(on_behalf), Some(audience)) => Some((
                self.verify_on_behalf_chain(
                    caller,
                    &on_behalf.chain,
                    &authority.principal,
                    &audience,
                )?,
                audience,
            )),
            _ => None,
        };
        let grant = self
            .authorize_as(&authority, &request.target_id, ExecOperationV1::Submit)
            .await?;
        let authorized = self
            .ceiling
            .authorize_submit(&authority.principal, &grant, &request)?;
        let delegation = match &request.on_behalf_of {
            // A forwarded token is not bound to this request, so it cannot
            // meet a grant that demands single-use, request-bound authority.
            None if exec_caller.forwarded.is_some() && grant.require_request_digest => {
                return Err(error(
                    ExecErrorCodeV1::PermissionDenied,
                    "the principal's grant requires a chain bound to this request",
                ));
            }
            None => None,
            Some(on_behalf) => {
                let verified = match early {
                    Some((verified, audience)) if audience == grant.audience => verified,
                    Some(_) => {
                        return Err(error(
                            ExecErrorCodeV1::PermissionDenied,
                            "on-behalf-of chain audience differs from the grant",
                        ));
                    }
                    None => self.verify_on_behalf_chain(
                        caller,
                        &on_behalf.chain,
                        &authority.principal,
                        &grant.audience,
                    )?,
                };
                self.check_action(&verified, &grant, &authorized, &request_digest)?;
                Some(verified)
            }
        };
        request.cwd = authorized.canonical_cwd.to_string_lossy().into_owned();
        request.profile = Some(authorized.profile.clone());
        request.execution_timeout_ms = Some(authorized.timeout_ms);
        request.output_limit_bytes = Some(authorized.output_limit_bytes);
        let digest = request.canonical_digest(&authorized.profile)?;
        let key = request_key(caller, &request.target_id, &request.request_id);
        if let Some(old) = self.ledger.lookup(&key).map_err(ledger_error)? {
            return self.existing_submission(&key, old, &request, digest).await;
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        self.inflight
            .lock()
            .map_err(|_| error(ExecErrorCodeV1::Internal, "admission state unavailable"))?
            .insert(job_id.clone());
        let _inflight = InflightAdmission {
            jobs: self.inflight.clone(),
            job_id: job_id.clone(),
        };
        let reservation = self
            .ledger
            .reserve_with_output(&key, digest, &job_id, now, authorized.output_limit_bytes)
            .map_err(ledger_error)?;
        if reservation.job_id != job_id {
            return self
                .existing_submission(&key, reservation, &request, digest)
                .await;
        }
        // One shape for every submission: whose grant applies (principal)
        // and who acted (actor; the caller itself unless delegated or
        // forwarded), then the evidence for the delegation, if any.
        let mut payload = serde_json::json!({
            "caller": caller,
            "principal": authority.principal,
            "actor": authority.actor,
            "target": request.target_id,
            "request_id": request.request_id,
            "job_id": job_id,
            "request_digest": hex::encode(digest),
            "grant_revision": grant.revision,
            "profile": authorized.profile,
        });
        if let Some(chain) = &delegation {
            payload["on_behalf_of"] = serde_json::json!({
                "chain_digest": hex::encode(chain.digest()),
                "chain_jtis": chain.jtis(),
                "issuers": chain.issuers().iter().map(hex::encode).collect::<Vec<_>>(),
            });
        }
        if let Some(forwarded) = &exec_caller.forwarded {
            payload["forwarded"] = serde_json::json!({
                "issuer": forwarded.issuer,
                "jti": forwarded.jti,
                "auth_methods": forwarded.auth_methods,
            });
        }
        let event = AuditEvent {
            timestamp_ms: now,
            kind: "exec.intent.v1".into(),
            payload,
        };
        if let Err(mut failure) = self.audit.admit(event).await {
            failure.request_id = Some(request.request_id.clone());
            failure.job_id = Some(job_id.clone());
            failure.retryable = false;
            self.ledger
                .fail_preparing(&key, failure.clone(), now_ms())
                .map_err(ledger_error)?;
            return Err(failure);
        }
        self.ledger
            .advance(&key, ReservationPhase::Accepted, now)
            .map_err(ledger_error)?;
        let request_id = request.request_id.clone();
        match self
            .runtime
            .launch(
                request,
                authorized,
                job_id.clone(),
                digest,
                caller.into(),
                now,
            )
            .await
        {
            Ok(status) => Ok(status),
            Err(mut failure) => {
                failure.request_id = Some(request_id);
                failure.job_id = Some(job_id);
                failure.code = ExecErrorCodeV1::SupervisorUnavailable;
                failure.safe_message =
                    "launch outcome is unresolved; retry this request ID or inspect the job ID"
                        .into();
                failure.retryable = true;
                Err(failure)
            }
        }
    }
    /// Verify an on-behalf-of action chain for the authenticated actor: it
    /// must be anchored at exactly the named principal, for this audience.
    fn verify_on_behalf_chain(
        &self,
        actor: &str,
        chain: &str,
        principal: &str,
        audience: &str,
    ) -> Result<VerifiedChain, ExecErrorV1> {
        let denied = |message: &str| error(ExecErrorCodeV1::PermissionDenied, message);
        let (Some(actor_key), Some(principal_key)) =
            (identity_bytes(actor), identity_bytes(principal))
        else {
            return Err(denied("invalid on-behalf-of identity"));
        };
        let anchors = [principal_key];
        // Gate G6: a delegate the root revoked withdraws every chain through
        // it. A job already running stays under the principal's grant.
        let revoked = AdmissionRevocations(self.enrollment.as_ref());
        let trust = TrustContext::new(&anchors, audience, now_ms() / 1000, &revoked);
        verify_chain(chain.as_bytes(), LeafKind::Action, &actor_key, &trust)
            .map_err(|failure| denied(&format!("on-behalf-of chain rejected: {failure:?}")))
    }

    /// The verified leaf must allow `exec.submit`, and every resource
    /// constraint must be one this target understands and satisfies. A leaf
    /// `request` digest must match, and the grant can require one.
    fn check_action(
        &self,
        verified: &VerifiedChain,
        grant: &ExecGrantV1,
        authorized: &AuthorizedSubmit,
        request_digest: &[u8; 32],
    ) -> Result<(), ExecErrorV1> {
        let denied = |message: &str| error(ExecErrorCodeV1::PermissionDenied, message);
        let LeafClaims::Action(action) = verified.leaf() else {
            return Err(denied("on-behalf-of chain is not an action"));
        };
        if !action.operations.contains(EXEC_SUBMIT_OPERATION) {
            return Err(denied("on-behalf-of chain does not allow exec.submit"));
        }
        for resource in &action.resources {
            self.check_resource(resource, authorized)?;
        }
        let digest = hex::encode(request_digest);
        match &action.request {
            Some(bound) if *bound != digest => {
                return Err(denied("on-behalf-of chain authorizes a different request"));
            }
            None if grant.require_request_digest => {
                return Err(denied(
                    "the principal's grant requires a chain bound to this request",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// One `resources` constraint from an action leaf. Constraints this
    /// target does not understand deny rather than being ignored.
    fn check_resource(
        &self,
        resource: &str,
        authorized: &AuthorizedSubmit,
    ) -> Result<(), ExecErrorV1> {
        let denied = |message: &str| error(ExecErrorCodeV1::PermissionDenied, message);
        let satisfied = match resource.split_once('=') {
            Some(("exec:target", target)) => target == self.ceiling.target_id,
            Some(("exec:profile", profile)) => profile == authorized.profile,
            Some(("exec:cwd_root", root)) => {
                std::path::Path::new(root).is_absolute()
                    && std::fs::canonicalize(root)
                        .is_ok_and(|root| authorized.canonical_cwd.starts_with(root))
            }
            _ => return Err(denied("on-behalf-of chain has an unsupported resource")),
        };
        if !satisfied {
            return Err(denied("request is outside the on-behalf-of resources"));
        }
        Ok(())
    }

    /// Replays supervisor-owned terminal outcomes; acknowledgement follows
    /// durable outbox admission, so a crash causes a safe duplicate audit.
    pub async fn replay_outcomes(&self) -> Result<usize, ExecErrorV1> {
        let mut count = 0;
        let mut first_error = None;
        for (job_id, bytes) in self.runtime.pending_outcomes().await? {
            let event: AuditEvent = match serde_json::from_slice(&bytes) {
                Ok(event) => event,
                Err(_) => {
                    eprintln!(
                        "Exec audit outcome for job {job_id} is corrupt; preserving it for repair"
                    );
                    first_error.get_or_insert_with(|| {
                        error(
                            ExecErrorCodeV1::BackendFailure,
                            "invalid stored audit outcome",
                        )
                    });
                    continue;
                }
            };
            if let Err(failure) = self.audit.admit(event).await {
                first_error.get_or_insert(failure);
                continue;
            }
            if let Err(failure) = self.runtime.ack_outcome(&job_id).await {
                first_error.get_or_insert(failure);
                continue;
            }
            count += 1;
        }
        first_error.map_or(Ok(count), Err)
    }
    /// Call on startup and periodically, independently of incoming RPCs.
    ///
    /// Settles ledger reservations against the lifetime owner, cancels live
    /// jobs whose owner has lost authority, prunes expired tombstones and
    /// replays audit outcomes. Every item is handled independently: a failure
    /// is reported in the result but never stops the remaining work.
    pub async fn reconcile_policy(&self) -> Result<usize, ExecErrorV1> {
        let _reconcile = self.reconcile_lock.lock().await;
        let mut first_error = None;
        // Copy in-flight admissions before reading the ledger, so a submit
        // that completes in between is seen as in flight rather than stale.
        let inflight = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let reservations = self.ledger.entries().unwrap_or_else(|failure| {
            first_error.get_or_insert(ledger_error(failure));
            Vec::new()
        });
        let jobs = match self.runtime.list_jobs().await {
            Ok(jobs) => jobs,
            Err(failure) => {
                // One unreadable helper folder must not hide every other job
                // from revocation. Accepted reservations retain the job IDs
                // needed to recover each status independently.
                first_error.get_or_insert(failure);
                self.recover_jobs(&reservations, &mut first_error).await
            }
        };
        for (key, reservation) in &reservations {
            if let Err(failure) = self
                .settle_reservation(key, reservation, &inflight, &jobs)
                .await
            {
                first_error.get_or_insert(failure);
            }
        }
        let live: Vec<&JobStatusV1> = jobs.iter().filter(|job| !job.state.is_terminal()).collect();
        let owners: BTreeSet<String> = live
            .iter()
            .map(|job| Authority::of_job(job).key())
            .collect();
        self.owners_settled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|owner, _| owners.contains(owner));
        let standings = self.refresh_many(owners.into_iter().collect()).await;
        let mut cancelled = 0;
        for job in live {
            let owner = Authority::of_job(job).key();
            let Some(standing) = standings.get(&owner) else {
                continue;
            };
            if !self.should_cancel(&owner, standing) {
                continue;
            }
            match self.cancel_for_revocation(job).await {
                Ok(()) => cancelled += 1,
                Err(failure) => {
                    first_error.get_or_insert(failure);
                }
            }
        }
        if let Err(failure) = self.ledger.prune(now_ms()) {
            first_error.get_or_insert(ledger_error(failure));
        }
        if let Err(failure) = self.replay_outcomes().await {
            first_error.get_or_insert(failure);
        }
        first_error.map_or(Ok(cancelled), Err)
    }

    async fn recover_jobs(
        &self,
        reservations: &[(String, crate::ledger::Reservation)],
        first_error: &mut Option<ExecErrorV1>,
    ) -> Vec<JobStatusV1> {
        let mut recovered = Vec::new();
        for (_, reservation) in reservations {
            if reservation.phase == ReservationPhase::Terminal {
                continue;
            }
            let job = JobRefV1 {
                job_id: reservation.job_id.clone(),
                target_id: self.ceiling.target_id.clone(),
            };
            match self.runtime.status(&job).await {
                Ok(status) => recovered.push(status),
                Err(failure) => {
                    first_error.get_or_insert(failure);
                }
            }
        }
        recovered
    }

    /// Move one reservation forward when the lifetime owner proves its fate.
    /// A missing Accepted job is not proof of non-execution until no launch
    /// can still be in flight and the owner definitely has no such job.
    async fn settle_reservation(
        &self,
        key: &str,
        reservation: &crate::ledger::Reservation,
        inflight: &BTreeSet<String>,
        live: &[JobStatusV1],
    ) -> Result<(), ExecErrorV1> {
        if inflight.contains(&reservation.job_id) {
            return Ok(());
        }
        let settled = |result: Result<_, LedgerError>| match result {
            // The reservation moved on concurrently; nothing to settle.
            Ok(_) | Err(LedgerError::InvalidTransition) => Ok(()),
            Err(other) => Err(ledger_error(other)),
        };
        match reservation.phase {
            ReservationPhase::Terminal => Ok(()),
            ReservationPhase::Preparing => {
                let mut failure = error(
                    ExecErrorCodeV1::AuditUnavailable,
                    "interrupted before launch; execution will not be retried automatically",
                );
                failure.job_id = Some(reservation.job_id.clone());
                failure.retryable = false;
                settled(self.ledger.fail_preparing(key, failure, now_ms()))
            }
            ReservationPhase::Accepted => {
                let running = live
                    .iter()
                    .any(|job| job.job.job_id == reservation.job_id && !job.state.is_terminal());
                if running {
                    return Ok(());
                }
                let status = self
                    .runtime
                    .status(&JobRefV1 {
                        job_id: reservation.job_id.clone(),
                        target_id: self.ceiling.target_id.clone(),
                    })
                    .await;
                let launch_settled = now_ms().saturating_sub(reservation.accepted_at_ms)
                    >= LAUNCH_SETTLE.as_millis() as u64;
                match status {
                    Ok(status) if status.state.is_terminal() => settled(self.ledger.advance(
                        key,
                        ReservationPhase::Terminal,
                        now_ms(),
                    )),
                    Err(failure)
                        if failure.code == ExecErrorCodeV1::JobNotFound && launch_settled =>
                    {
                        let mut failure = error(
                            ExecErrorCodeV1::SupervisorUnavailable,
                            "launch did not start; use a new request ID",
                        );
                        failure.job_id = Some(reservation.job_id.clone());
                        failure.retryable = false;
                        settled(self.ledger.abandon_accepted(key, failure, now_ms()))
                    }
                    _ => Ok(()),
                }
            }
        }
    }

    async fn cancel_for_revocation(&self, job: &JobStatusV1) -> Result<(), ExecErrorV1> {
        let event = AuditEvent {
            timestamp_ms: now_ms(),
            kind: "exec.revocation.cancel.v1".into(),
            payload: serde_json::json!({
                "caller": job.caller_id,
                "principal": job.principal_id,
                "job_id": job.job.job_id,
                "target": job.job.target_id,
            }),
        };
        // The cancellation proceeds even if its intent cannot be audited; the
        // lifetime owner durably records the terminal outcome either way.
        let audited = self.audit.admit(event).await;
        if audited.is_err() {
            eprintln!(
                "Exec revocation intent audit unavailable for {}",
                job.job.job_id
            );
        }
        self.runtime
            .cancel(&CancelRequestV1 {
                job: job.job.clone(),
                grace_ms: Some(DEFAULT_CANCEL_GRACE_MS),
            })
            .await?;
        audited
    }

    fn watch_permit(&self, caller: &str) -> Result<WatchPermit, ExecErrorV1> {
        let mut watchers = self.watchers.lock().map_err(|_| {
            error(
                ExecErrorCodeV1::Internal,
                "watch admission state unavailable",
            )
        })?;
        let total: usize = watchers.values().sum();
        if total >= MAX_WATCHES
            || watchers.get(caller).copied().unwrap_or(0) >= MAX_WATCHES_PER_CALLER
        {
            return Err(error(
                ExecErrorCodeV1::ResourceExhausted,
                "too many concurrent Exec watches",
            ));
        }
        *watchers.entry(caller.to_owned()).or_default() += 1;
        Ok(WatchPermit {
            watchers: self.watchers.clone(),
            caller: caller.to_owned(),
        })
    }
    async fn watch(&self, caller: Result<ExecCaller, ExecErrorV1>, mut call: Call) {
        let request: WatchRequestV1 = match call
            .recv_request()
            .await
            .and_then(|raw| serde_json::from_slice(&raw).ok())
        {
            Some(v) => v,
            None => {
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::InvalidArgument,
                        "invalid watch request",
                    ))
                    .await;
                return;
            }
        };
        let (caller, _permit) = match caller.and_then(|caller| {
            self.watch_permit(caller.id())
                .map(|permit| (caller, permit))
        }) {
            Ok(admitted) => admitted,
            Err(failure) => {
                if let Ok(bytes) = serde_json::to_vec(&Err::<JobEventV1, _>(failure)) {
                    let _ = call.send(bytes).await;
                }
                let _ = call.finish(&RpcStatus::ok()).await;
                return;
            }
        };
        let mut cursor = request.cursor;
        let mut previous = None;
        let mut last_progress = Instant::now();
        loop {
            if last_progress.elapsed() >= WATCH_IDLE_TIMEOUT {
                let failure = error(
                    ExecErrorCodeV1::DeadlineExceeded,
                    "Exec watch idle; resume from the last cursor",
                );
                if let Ok(bytes) = serde_json::to_vec(&Err::<JobEventV1, _>(failure)) {
                    let _ = call.send(bytes).await;
                }
                let _ = call.finish(&RpcStatus::ok()).await;
                return;
            }
            let step = async {
                let status = self
                    .owned(&caller, &request.job, ExecOperationV1::Watch)
                    .await?;
                // Output authority is independently required for output bytes.
                self.authorize_as(
                    &Self::job_authority(&caller, &status),
                    &request.job.target_id,
                    ExecOperationV1::Output,
                )
                .await?;
                let page = self
                    .runtime
                    .output(&OutputRequestV1 {
                        job: request.job.clone(),
                        cursor,
                        max_bytes: MAX_PAGE_BYTES,
                    })
                    .await?;
                Ok::<_, ExecErrorV1>((status, page))
            }
            .await;
            let (status, page) = match step {
                Ok(v) => v,
                Err(e) => {
                    if let Ok(bytes) = serde_json::to_vec(&Err::<JobEventV1, _>(e)) {
                        let _ = call.send(bytes).await;
                    }
                    let _ = call.finish(&RpcStatus::ok()).await;
                    return;
                }
            };
            for chunk in page.chunks {
                cursor = chunk.sequence.saturating_add(1);
                let event = Ok::<_, ExecErrorV1>(JobEventV1::OutputChunk(chunk));
                let Ok(bytes) = serde_json::to_vec(&event) else {
                    let _ = call
                        .finish(&RpcStatus::error(
                            StatusCode::Internal,
                            "Exec encoding failed",
                        ))
                        .await;
                    return;
                };
                if call.send(bytes).await.is_err() {
                    return;
                }
                last_progress = Instant::now();
            }
            if previous.as_ref() != Some(&status) {
                let event =
                    Ok::<_, ExecErrorV1>(JobEventV1::StateChanged(Box::new(status.clone())));
                let Ok(bytes) = serde_json::to_vec(&event) else {
                    let _ = call
                        .finish(&RpcStatus::error(
                            StatusCode::Internal,
                            "Exec encoding failed",
                        ))
                        .await;
                    return;
                };
                if call.send(bytes).await.is_err() {
                    return;
                }
                last_progress = Instant::now();
                previous = Some(status.clone());
            }
            if status.state.is_terminal() && cursor >= status.output_cursor {
                let _ = call.finish(&RpcStatus::ok()).await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    async fn run(
        &self,
        method: &str,
        caller: &ExecCaller,
        raw: &[u8],
    ) -> Result<serde_json::Value, ExecErrorV1> {
        fn decode<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T, ExecErrorV1> {
            serde_json::from_slice(raw).map_err(|_| ExecErrorV1::invalid("invalid Exec request"))
        }
        fn encode<T: serde::Serialize>(v: T) -> Result<serde_json::Value, ExecErrorV1> {
            serde_json::to_value(v)
                .map_err(|_| error(ExecErrorCodeV1::Internal, "Exec response encoding failed"))
        }
        match method {
            "targets" => {
                self.authorize_as(
                    &caller.authority(),
                    &self.ceiling.target_id,
                    ExecOperationV1::Discover,
                )
                .await?;
                encode(vec![TargetRefV1 {
                    target_id: self.ceiling.target_id.clone(),
                }])
            }
            "capabilities" => {
                let req: TargetRefV1 = decode(raw)?;
                let grant = self
                    .authorize_as(
                        &caller.authority(),
                        &req.target_id,
                        ExecOperationV1::Discover,
                    )
                    .await?;
                let mut caps = self.runtime.capabilities().await?;
                caps.profiles.retain(|p| {
                    self.ceiling.enabled_profiles.contains(&p.id) && grant.profiles.contains(&p.id)
                });
                caps.max_execution_timeout_ms = caps
                    .max_execution_timeout_ms
                    .min(grant.max_timeout_ms)
                    .min(self.ceiling.max_timeout_ms);
                caps.max_output_bytes = caps
                    .max_output_bytes
                    .min(grant.max_output_bytes)
                    .min(self.ceiling.max_output_bytes);
                encode(caps)
            }
            "submit" => encode(self.submit_as(caller, decode(raw)?).await?.job),
            "status" => {
                let req: JobRefV1 = decode(raw)?;
                encode(self.owned(caller, &req, ExecOperationV1::Status).await?)
            }
            "output" => {
                let req: OutputRequestV1 = decode(raw)?;
                self.owned(caller, &req.job, ExecOperationV1::Output)
                    .await?;
                if !(MIN_PAGE_BYTES..=MAX_PAGE_BYTES).contains(&req.max_bytes) {
                    return Err(ExecErrorV1::invalid("invalid output page size"));
                }
                encode(self.runtime.output(&req).await?)
            }
            "cancel" => {
                let req: CancelRequestV1 = decode(raw)?;
                let job = self
                    .owned(caller, &req.job, ExecOperationV1::Cancel)
                    .await?;
                if req.grace_ms.is_some_and(|v| v > MAX_CANCEL_GRACE_MS) {
                    return Err(ExecErrorV1::invalid("invalid cancellation grace"));
                }
                let intent = AuditEvent {
                    timestamp_ms: now_ms(),
                    kind: "exec.cancel.intent.v1".into(),
                    payload: serde_json::json!({
                        "caller": caller.id(),
                        "principal": job.principal_id.as_deref().unwrap_or(caller.id()),
                        "actor": caller.edge().unwrap_or(caller.id()),
                        "job_id": req.job.job_id,
                        "target": req.job.target_id,
                    }),
                };
                self.audit.admit(intent).await?;
                encode(self.runtime.cancel(&req).await?)
            }
            _ => Err(error(
                ExecErrorCodeV1::UnsupportedOperation,
                "unknown Exec method",
            )),
        }
    }
}
fn ledger_error(e: LedgerError) -> ExecErrorV1 {
    match e {
        LedgerError::Capacity => error(
            ExecErrorCodeV1::ResourceExhausted,
            "Exec admission capacity exhausted",
        ),
        LedgerError::Conflict => error(
            ExecErrorCodeV1::RequestConflict,
            "request ID content conflict",
        ),
        _ => error(
            ExecErrorCodeV1::BackendFailure,
            "request ledger unavailable",
        ),
    }
}
#[async_trait]
impl ServiceDispatch for ExecService {
    fn name(&self) -> &str {
        SERVICE_NAME
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &[
            "targets",
            "capabilities",
            "submit",
            "status",
            "output",
            "cancel",
            "watch",
        ]
    }
    fn iroh_only(&self) -> bool {
        true
    }
    fn method_timeout(&self, method: &str) -> Option<Duration> {
        (method != "watch").then_some(Duration::from_secs(30))
    }
    fn max_frame_size(&self, _: &str) -> Option<u32> {
        Some(MAX_REQUEST_FRAME_BYTES as u32)
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        // The node itself, or the user a gateway forwarded (the core has
        // verified the token and the edge's `aster.gateway` role).
        let caller = ExecCaller::from_context(call.context());
        if method == "watch" {
            self.watch(caller, call).await;
            return;
        }
        let result = match (call.recv_request().await, caller) {
            (Some(raw), Ok(caller)) if raw.len() <= MAX_REQUEST_FRAME_BYTES => {
                self.run(method, &caller, &raw).await
            }
            (Some(_), Err(failure)) => Err(failure),
            _ => Err(ExecErrorV1::invalid("invalid Exec frame")),
        };
        match serde_json::to_vec(&result) {
            Ok(bytes) => {
                let _ = call.respond(bytes, &RpcStatus::ok()).await;
            }
            Err(_) => {
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::Internal,
                        "Exec encoding failed",
                    ))
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::GrantStatusV1;
    struct Grants(ExecGrantV1);
    #[async_trait]
    impl GrantProvider for Grants {
        async fn current(
            &self,
            _: [u8; 32],
            _: [u8; 32],
            _: u64,
        ) -> Result<ExecGrantV1, ExecErrorV1> {
            Ok(self.0.clone())
        }
    }
    struct Admitted;
    #[async_trait]
    impl EnrollmentAdmission for Admitted {
        async fn check(&self, _: &[u8; 32]) -> Result<(), AdmissionError> {
            Ok(())
        }
        fn delegate_revoked(&self, _: &[u8; 32]) -> bool {
            false
        }
    }
    /// Enrollment answers every check with this fixed error.
    struct Enrollment(AdmissionError);
    #[async_trait]
    impl EnrollmentAdmission for Enrollment {
        async fn check(&self, _: &[u8; 32]) -> Result<(), AdmissionError> {
            Err(self.0.clone())
        }
        fn delegate_revoked(&self, _: &[u8; 32]) -> bool {
            false
        }
    }
    fn revoked() -> Arc<Enrollment> {
        Arc::new(Enrollment(AdmissionError::Denied(
            aster::enrollment::DenialReason::NodeRevoked,
        )))
    }
    struct CountAdmission(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl EnrollmentAdmission for CountAdmission {
        async fn check(&self, _: &[u8; 32]) -> Result<(), AdmissionError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn delegate_revoked(&self, _: &[u8; 32]) -> bool {
            false
        }
    }
    struct CountGrants {
        inner: Arc<dyn GrantProvider>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl GrantProvider for CountGrants {
        async fn current(
            &self,
            c: [u8; 32],
            t: [u8; 32],
            now: u64,
        ) -> Result<ExecGrantV1, ExecErrorV1> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.current(c, t, now).await
        }
    }
    struct NoAudit;
    #[async_trait]
    impl ExecAudit for NoAudit {
        async fn admit(&self, _: AuditEvent) -> Result<(), ExecErrorV1> {
            Err(error(ExecErrorCodeV1::AuditUnavailable, "audit offline"))
        }
    }
    struct AdmitAudit;
    #[async_trait]
    impl ExecAudit for AdmitAudit {
        async fn admit(&self, _: AuditEvent) -> Result<(), ExecErrorV1> {
            Ok(())
        }
    }
    struct FailLaunch(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl ExecRuntime for FailLaunch {
        async fn capabilities(&self) -> Result<TargetCapabilitiesV1, ExecErrorV1> {
            NeverLaunch.capabilities().await
        }
        async fn launch(
            &self,
            _: ExecRequestV1,
            _: AuthorizedSubmit,
            _: String,
            _: [u8; 32],
            _: String,
            _: u64,
        ) -> Result<JobStatusV1, ExecErrorV1> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(error(
                ExecErrorCodeV1::SupervisorUnavailable,
                "helper launch failed",
            ))
        }
        async fn status(&self, _: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1> {
            Err(error(ExecErrorCodeV1::JobNotFound, "no helper store"))
        }
        async fn output(&self, r: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1> {
            NeverLaunch.output(r).await
        }
        async fn cancel(&self, r: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1> {
            NeverLaunch.cancel(r).await
        }
        async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1> {
            Ok(vec![])
        }
        async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1> {
            Ok(vec![])
        }
        async fn ack_outcome(&self, _: &str) -> Result<(), ExecErrorV1> {
            Ok(())
        }
    }
    struct NeverLaunch;
    #[async_trait]
    impl ExecRuntime for NeverLaunch {
        async fn capabilities(&self) -> Result<TargetCapabilitiesV1, ExecErrorV1> {
            panic!("unexpected capabilities")
        }
        async fn launch(
            &self,
            _: ExecRequestV1,
            _: AuthorizedSubmit,
            _: String,
            _: [u8; 32],
            _: String,
            _: u64,
        ) -> Result<JobStatusV1, ExecErrorV1> {
            panic!("must not launch without durable audit")
        }
        async fn status(&self, _: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1> {
            Err(error(
                ExecErrorCodeV1::SupervisorUnavailable,
                "unresolved admission",
            ))
        }
        async fn output(&self, _: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1> {
            panic!("unexpected output")
        }
        async fn cancel(&self, _: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1> {
            panic!("unexpected cancel")
        }
        async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1> {
            Ok(vec![])
        }
        async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1> {
            Ok(vec![])
        }
        async fn ack_outcome(&self, _: &str) -> Result<(), ExecErrorV1> {
            Ok(())
        }
    }
    fn authority_test_service(temp: &tempfile::TempDir) -> ExecService {
        ExecService::new(
            LocalCeilingV1 {
                version: 1,
                target_id: "22".repeat(32),
                account_alias: "builder".into(),
                default_profile: "open".into(),
                enabled_profiles: ["open".into()].into(),
                enabled_operations: [ExecOperationV1::Submit].into(),
                cwd_roots: vec![temp.path().into()],
                max_timeout_ms: DEFAULT_TIMEOUT_MS,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
                denied_callers: Default::default(),
            },
            Arc::new(Admitted),
            Arc::new(Grants(ExecGrantV1 {
                version: 1,
                issuer: "33".repeat(32),
                audience: "fleet".into(),
                caller: "11".repeat(32),
                target: "22".repeat(32),
                revision: 1,
                issued_at: 1,
                not_before: None,
                expires_at: None,
                status: GrantStatusV1::Active,
                account_alias: "builder".into(),
                profiles: vec!["open".into()],
                operations: vec![ExecOperationV1::Submit],
                cwd_roots: vec![temp.path().to_string_lossy().into()],
                max_timeout_ms: DEFAULT_TIMEOUT_MS,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
                cancel_on_revocation: true,
                reauthorizes_revision: None,
                require_request_digest: false,
            })),
            Arc::new(AdmitAudit),
            Arc::new(NeverLaunch),
            RequestLedger::open(&temp.path().join("requests.redb")).unwrap(),
        )
    }

    struct SlowFirstGrant {
        inner: Arc<dyn GrantProvider>,
        calls: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl GrantProvider for SlowFirstGrant {
        async fn current(
            &self,
            c: [u8; 32],
            t: [u8; 32],
            now: u64,
        ) -> Result<ExecGrantV1, ExecErrorV1> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
                self.inner.current(c, t, now).await
            } else {
                Err(error(ExecErrorCodeV1::PermissionDenied, "newer denial"))
            }
        }
    }

    #[tokio::test]
    async fn older_refresh_cannot_overwrite_newer_denial_or_revocation() {
        for invalidate in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut service = authority_test_service(&temp);
            let grants = Arc::new(SlowFirstGrant {
                inner: service.grants.clone(),
                calls: std::sync::atomic::AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            service.grants = grants.clone();
            let caller = "11".repeat(32);
            let older = {
                let service = service.clone();
                let caller = caller.clone();
                tokio::spawn(async move { service.refresh_standing(&caller).await.call_result() })
            };
            tokio::time::timeout(Duration::from_secs(2), grants.entered.notified())
                .await
                .unwrap();
            if invalidate {
                service.invalidate_peer(&caller);
            } else {
                assert_eq!(
                    service
                        .refresh_standing(&caller)
                        .await
                        .call_result()
                        .unwrap_err()
                        .code,
                    ExecErrorCodeV1::PermissionDenied
                );
            }
            grants.release.notify_one();
            assert_eq!(
                older.await.unwrap().unwrap_err().code,
                ExecErrorCodeV1::PermissionDenied
            );
            assert_eq!(
                service
                    .authorize(&caller, &service.ceiling.target_id, ExecOperationV1::Submit)
                    .await
                    .unwrap_err()
                    .code,
                ExecErrorCodeV1::PermissionDenied
            );
            assert!(!service.authority.read().unwrap().observed.contains(&caller));
        }
    }

    struct OfflineRuntime;
    #[async_trait]
    impl ExecRuntime for OfflineRuntime {
        async fn capabilities(&self) -> Result<TargetCapabilitiesV1, ExecErrorV1> {
            NeverLaunch.capabilities().await
        }
        async fn launch(
            &self,
            r: ExecRequestV1,
            a: AuthorizedSubmit,
            id: String,
            d: [u8; 32],
            c: String,
            at: u64,
        ) -> Result<JobStatusV1, ExecErrorV1> {
            NeverLaunch.launch(r, a, id, d, c, at).await
        }
        async fn status(&self, r: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1> {
            NeverLaunch.status(r).await
        }
        async fn output(&self, r: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1> {
            NeverLaunch.output(r).await
        }
        async fn cancel(&self, r: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1> {
            NeverLaunch.cancel(r).await
        }
        async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1> {
            Err(error(
                ExecErrorCodeV1::SupervisorUnavailable,
                "helper offline",
            ))
        }
        async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1> {
            Ok(vec![])
        }
        async fn ack_outcome(&self, _: &str) -> Result<(), ExecErrorV1> {
            Ok(())
        }
    }

    fn revocation_job() -> JobStatusV1 {
        JobStatusV1 {
            job: JobRefV1 {
                job_id: "job".into(),
                target_id: "22".repeat(32),
            },
            state: JobStateV1::Accepted,
            caller_id: "11".repeat(32),
            principal_id: None,
            resolved_target: "22".repeat(32),
            execution_identity: "runner".into(),
            accepted_at_ms: 1,
            started_at_ms: None,
            finished_at_ms: None,
            exit_code: None,
            signal: None,
            reason_code: None,
            output_cursor: 0,
            guarantees: GuaranteesV1 {
                profile: "open".into(),
                provider: "native".into(),
                network: NetworkGuaranteeV1::Inherited,
                proxy_id: None,
                filesystem: FilesystemGuaranteeV1::Inherited,
                filesystem_policy_id: None,
                execution_identity: "runner".into(),
                resource_limits: ResourceLimitsV1 {
                    cpu_millis_per_second: None,
                    memory_bytes: None,
                    processes: None,
                    file_bytes: None,
                },
                descendant_scope: DescendantScopeV1::Cgroup,
                restart_survival: RestartSurvivalV1::ApiRestart,
            },
        }
    }

    struct CancelFailures {
        attempted: StdMutex<Vec<String>>,
        recovered: StdMutex<Vec<String>>,
        fail_list: bool,
    }
    #[async_trait]
    impl ExecRuntime for CancelFailures {
        async fn capabilities(&self) -> Result<TargetCapabilitiesV1, ExecErrorV1> {
            NeverLaunch.capabilities().await
        }
        async fn launch(
            &self,
            r: ExecRequestV1,
            a: AuthorizedSubmit,
            id: String,
            d: [u8; 32],
            c: String,
            at: u64,
        ) -> Result<JobStatusV1, ExecErrorV1> {
            NeverLaunch.launch(r, a, id, d, c, at).await
        }
        async fn status(&self, r: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1> {
            self.recovered.lock().unwrap().push(r.job_id.clone());
            if r.job_id == "unreadable" {
                return Err(error(
                    ExecErrorCodeV1::SupervisorUnavailable,
                    "unreadable reservation",
                ));
            }
            let mut status = revocation_job();
            status.job = r.clone();
            status.state = JobStateV1::Running;
            Ok(status)
        }
        async fn output(&self, r: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1> {
            NeverLaunch.output(r).await
        }
        async fn cancel(&self, r: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1> {
            self.attempted.lock().unwrap().push(r.job.job_id.clone());
            if r.job.job_id == "first" {
                Err(error(
                    ExecErrorCodeV1::SupervisorUnavailable,
                    "first cancellation failed",
                ))
            } else {
                Ok(CancelReceiptV1 {
                    job: r.job.clone(),
                    accepted_at_ms: 1,
                    already_terminal: false,
                })
            }
        }
        async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1> {
            if self.fail_list {
                return Err(error(ExecErrorCodeV1::SupervisorUnavailable, "list failed"));
            }
            Ok([
                ("first", JobStateV1::Running),
                ("terminal", JobStateV1::Succeeded),
                ("second", JobStateV1::Starting),
                ("third", JobStateV1::Accepted),
            ]
            .into_iter()
            .map(|(id, state)| {
                let mut job = revocation_job();
                job.job.job_id = id.into();
                job.state = state;
                job
            })
            .collect())
        }
        async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1> {
            Ok(vec![])
        }
        async fn ack_outcome(&self, _: &str) -> Result<(), ExecErrorV1> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn definite_revocation_attempts_every_live_job_after_cancel_failure() {
        let temp = tempfile::tempdir().unwrap();
        let mut service = authority_test_service(&temp);
        let runtime = Arc::new(CancelFailures {
            attempted: StdMutex::new(vec![]),
            recovered: StdMutex::new(vec![]),
            fail_list: false,
        });
        service.runtime = runtime.clone();
        service.enrollment = revoked();
        assert_eq!(
            service.reconcile_policy().await.unwrap_err().code,
            ExecErrorCodeV1::SupervisorUnavailable
        );
        assert_eq!(
            *runtime.attempted.lock().unwrap(),
            ["first", "second", "third"]
        );
    }

    #[tokio::test]
    async fn unreadable_reservation_does_not_block_revocation_after_list_failure() {
        let temp = tempfile::tempdir().unwrap();
        let mut service = authority_test_service(&temp);
        let runtime = Arc::new(CancelFailures {
            attempted: StdMutex::new(vec![]),
            recovered: StdMutex::new(vec![]),
            fail_list: true,
        });
        service.runtime = runtime.clone();
        // Ledger iteration visits the unreadable reservation before the live
        // job, so an early return on status failure would miss cancellation.
        for (key, job_id) in [("a", "unreadable"), ("b", "readable")] {
            service
                .ledger
                .reserve(key, [1; 32], job_id, now_ms())
                .unwrap();
            service
                .ledger
                .advance(key, ReservationPhase::Accepted, now_ms())
                .unwrap();
        }
        service.enrollment = revoked();
        let failure = service.reconcile_policy().await.unwrap_err();
        assert_eq!(failure.code, ExecErrorCodeV1::SupervisorUnavailable);
        assert_eq!(failure.safe_message, "list failed");
        // Recovery reads every reservation in ledger order; settling then
        // retries the unreadable one without blocking the readable job.
        assert!(
            runtime
                .recovered
                .lock()
                .unwrap()
                .starts_with(&["unreadable".to_string(), "readable".to_string()])
        );
        assert_eq!(*runtime.attempted.lock().unwrap(), ["readable"]);
    }

    #[tokio::test]
    async fn background_refresh_applies_definite_denial() {
        let temp = tempfile::tempdir().unwrap();
        let mut service = authority_test_service(&temp);
        let caller = "11".repeat(32);
        assert!(matches!(
            service.refresh_standing(&caller).await,
            Standing::Allowed(_)
        ));
        // The helper is not consulted: an offline helper cannot stall refresh.
        service.runtime = Arc::new(OfflineRuntime);
        service.enrollment = revoked();
        service.refresh_authority().await;
        assert_eq!(
            service
                .authorize(&caller, &service.ceiling.target_id, ExecOperationV1::Submit)
                .await
                .unwrap_err()
                .code,
            ExecErrorCodeV1::PermissionDenied
        );
        assert!(!service.authority.read().unwrap().observed.contains(&caller));
    }

    fn running_jobs_service(temp: &tempfile::TempDir) -> (ExecService, Arc<CancelFailures>) {
        let mut service = authority_test_service(temp);
        let runtime = Arc::new(CancelFailures {
            attempted: StdMutex::new(vec![]),
            recovered: StdMutex::new(vec![]),
            fail_list: false,
        });
        service.runtime = runtime.clone();
        (service, runtime)
    }

    #[tokio::test]
    async fn caller_without_enrollment_session_keeps_jobs_and_must_reenroll() {
        let temp = tempfile::tempdir().unwrap();
        let (mut service, runtime) = running_jobs_service(&temp);
        // After an API restart no caller has an enrollment session yet.
        service.enrollment = Arc::new(Enrollment(AdmissionError::NotAdmitted));
        assert_eq!(service.reconcile_policy().await.unwrap(), 0);
        assert!(runtime.attempted.lock().unwrap().is_empty());
        let failure = service
            .authorize(
                &"11".repeat(32),
                &service.ceiling.target_id,
                ExecOperationV1::Submit,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, ExecErrorCodeV1::Unauthenticated);
        assert!(failure.retryable);
    }

    #[tokio::test]
    async fn unknown_standing_cancels_only_after_grace() {
        let temp = tempfile::tempdir().unwrap();
        let (mut service, runtime) = running_jobs_service(&temp);
        service.enrollment = Arc::new(Enrollment(AdmissionError::Unavailable(
            "policy store offline".into(),
        )));
        assert_eq!(service.reconcile_policy().await.unwrap(), 0);
        assert!(runtime.attempted.lock().unwrap().is_empty());
        *service
            .owners_settled
            .lock()
            .unwrap()
            .get_mut(&"11".repeat(32))
            .unwrap() = Instant::now() - UNKNOWN_STANDING_GRACE - Duration::from_secs(1);
        service.reconcile_policy().await.unwrap_err();
        assert_eq!(
            *runtime.attempted.lock().unwrap(),
            ["first", "second", "third"]
        );
    }

    #[tokio::test]
    async fn reconcile_settles_lost_launch_but_not_inflight_or_recent_ones() {
        let temp = tempfile::tempdir().unwrap();
        let mut service = authority_test_service(&temp);
        // The helper has no record of any job.
        service.runtime = Arc::new(FailLaunch(Arc::default()));
        let old = now_ms() - LAUNCH_SETTLE.as_millis() as u64 - 1;
        for (key, job_id, accepted_at) in [
            ("lost", "lost", old),
            ("recent", "recent", now_ms()),
            ("inflight", "inflight", old),
        ] {
            service
                .ledger
                .reserve(key, [1; 32], job_id, accepted_at)
                .unwrap();
            service
                .ledger
                .advance(key, ReservationPhase::Accepted, accepted_at)
                .unwrap();
        }
        service.inflight.lock().unwrap().insert("inflight".into());
        service.reconcile_policy().await.unwrap();
        let phase = |key| service.ledger.lookup(key).unwrap().unwrap();
        let lost = phase("lost");
        assert_eq!(lost.phase, ReservationPhase::Terminal);
        assert!(!lost.admission_error.unwrap().retryable);
        assert_eq!(lost.output_limit_bytes, 0);
        assert_eq!(phase("recent").phase, ReservationPhase::Accepted);
        assert_eq!(phase("inflight").phase, ReservationPhase::Accepted);
    }

    #[tokio::test]
    async fn authority_cache_evicts_idle_peers_and_revocation_stays_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let service = ExecService::new(
            LocalCeilingV1 {
                version: 1,
                target_id: "22".repeat(32),
                account_alias: "builder".into(),
                default_profile: "open".into(),
                enabled_profiles: ["open".into()].into(),
                enabled_operations: [ExecOperationV1::Discover].into(),
                cwd_roots: vec![temp.path().into()],
                max_timeout_ms: DEFAULT_TIMEOUT_MS,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
                denied_callers: Default::default(),
            },
            Arc::new(Admitted),
            Arc::new(Grants(ExecGrantV1 {
                version: 1,
                issuer: "33".repeat(32),
                audience: "fleet".into(),
                caller: "11".repeat(32),
                target: "22".repeat(32),
                revision: 1,
                issued_at: 1,
                not_before: None,
                expires_at: None,
                status: GrantStatusV1::Active,
                account_alias: "builder".into(),
                profiles: vec!["open".into()],
                operations: vec![ExecOperationV1::Discover],
                cwd_roots: vec![temp.path().to_string_lossy().into()],
                max_timeout_ms: DEFAULT_TIMEOUT_MS,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
                cancel_on_revocation: true,
                reauthorizes_revision: None,
                require_request_digest: false,
            })),
            Arc::new(AdmitAudit),
            Arc::new(NeverLaunch),
            RequestLedger::open(&temp.path().join("requests.redb")).unwrap(),
        );
        for index in 0..=MAX_AUTHORITY_PEERS {
            service
                .refresh_standing(&format!("{index:064x}"))
                .await
                .call_result()
                .unwrap();
        }
        let snapshot = service.authority.read().unwrap();
        assert_eq!(snapshot.peers.len(), MAX_AUTHORITY_PEERS);
        assert!(!snapshot.peers.contains_key(&format!("{:064x}", 0)));
        drop(snapshot);
        service.invalidate_peer(&"ff".repeat(32));
        assert_eq!(
            service.authority.read().unwrap().peers.len(),
            MAX_AUTHORITY_PEERS
        );
    }

    #[tokio::test]
    async fn audit_failure_cannot_launch_and_ambiguous_retry_keeps_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let caller = "11".repeat(32);
        let target = "22".repeat(32);
        let grant = ExecGrantV1 {
            version: 1,
            issuer: "33".repeat(32),
            audience: "fleet".into(),
            caller: caller.clone(),
            target: target.clone(),
            revision: 1,
            issued_at: 1,
            not_before: None,
            expires_at: None,
            status: GrantStatusV1::Active,
            account_alias: "builder".into(),
            profiles: vec!["open".into()],
            operations: vec![ExecOperationV1::Submit],
            cwd_roots: vec![temp.path().to_string_lossy().into()],
            max_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
            cancel_on_revocation: true,
            reauthorizes_revision: None,
            require_request_digest: false,
        };
        let ceiling = LocalCeilingV1 {
            version: 1,
            target_id: target.clone(),
            account_alias: "builder".into(),
            default_profile: "open".into(),
            enabled_profiles: ["open".into()].into(),
            enabled_operations: [ExecOperationV1::Submit].into(),
            cwd_roots: vec![temp.path().into()],
            max_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
            denied_callers: Default::default(),
        };
        let mut service = ExecService::new(
            ceiling,
            Arc::new(Admitted),
            Arc::new(Grants(grant)),
            Arc::new(NoAudit),
            Arc::new(NeverLaunch),
            RequestLedger::open(&temp.path().join("requests.redb")).unwrap(),
        );
        assert!(service.iroh_only());
        assert_eq!(
            service.method_timeout("submit"),
            Some(Duration::from_secs(30))
        );
        assert_eq!(service.method_timeout("watch"), None);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        service.grants = Arc::new(CountGrants {
            inner: service.grants.clone(),
            calls: calls.clone(),
        });
        let request = ExecRequestV1 {
            request_id: uuid::Uuid::new_v4().to_string(),
            target_id: target,
            profile: None,
            cwd: temp.path().to_string_lossy().into(),
            executable: "/bin/true".into(),
            argv: vec![],
            environment_overrides: Default::default(),
            stdin_bytes: None,
            execution_timeout_ms: None,
            output_limit_bytes: None,
            on_behalf_of: None,
        };
        service.enrollment = revoked();
        assert!(
            service
                .refresh_standing(&caller)
                .await
                .call_result()
                .is_err()
        );
        assert_eq!(
            service
                .submit(&caller, request.clone())
                .await
                .unwrap_err()
                .code,
            ExecErrorCodeV1::PermissionDenied
        );
        let enrollment_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        service.enrollment = Arc::new(CountAdmission(enrollment_calls.clone()));
        // A restarted API has no positive authority; the first call must
        // perform one cold verification before its 10k warm successors.
        service.authority.write().unwrap().peers.clear();
        let started = Instant::now();
        for _ in 0..10_000 {
            service
                .authorize(&caller, &request.target_id, ExecOperationV1::Submit)
                .await
                .unwrap();
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "warm calls must not read policy again"
        );
        assert_eq!(
            enrollment_calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "warm calls must not reevaluate enrollment"
        );
        eprintln!("10k warm local authority checks: {:?}", started.elapsed());
        service
            .authority
            .write()
            .unwrap()
            .peers
            .get_mut(&caller)
            .unwrap()
            .refreshed_at = Instant::now() - AUTHORITY_FRESHNESS - Duration::from_secs(1);
        assert_eq!(
            service
                .authorize(&caller, &request.target_id, ExecOperationV1::Submit)
                .await
                .unwrap_err()
                .code,
            ExecErrorCodeV1::PolicyUnavailable
        );
        service
            .refresh_standing(&caller)
            .await
            .call_result()
            .unwrap();
        assert_eq!(
            service
                .submit(&caller, request.clone())
                .await
                .unwrap_err()
                .code,
            ExecErrorCodeV1::AuditUnavailable
        );
        assert_eq!(
            service
                .submit(&caller, request.clone())
                .await
                .unwrap_err()
                .code,
            ExecErrorCodeV1::AuditUnavailable
        );
        let mut launch_request = request.clone();
        launch_request.request_id = uuid::Uuid::new_v4().to_string();
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        service.audit = Arc::new(AdmitAudit);
        service.runtime = Arc::new(FailLaunch(launches.clone()));
        let failed = service
            .submit(&caller, launch_request.clone())
            .await
            .unwrap_err();
        let retried = service.submit(&caller, launch_request).await.unwrap_err();
        assert_eq!(retried.code, ExecErrorCodeV1::SupervisorUnavailable);
        assert_eq!(retried.job_id, failed.job_id);
        assert_eq!(launches.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(failed.job_id.is_some());
        assert!(failed.retryable);
        assert!(
            service
                .ledger
                .entries()
                .unwrap()
                .iter()
                .any(|(_, entry)| entry.phase == ReservationPhase::Accepted
                    && entry.admission_error.is_none())
        );
        let mut interrupted = request.clone();
        interrupted.request_id = uuid::Uuid::new_v4().to_string();
        interrupted.profile = Some("open".into());
        interrupted.execution_timeout_ms = Some(DEFAULT_TIMEOUT_MS);
        interrupted.output_limit_bytes = Some(DEFAULT_OUTPUT_BYTES);
        interrupted.cwd = std::fs::canonicalize(&interrupted.cwd)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let interrupted_key = request_key(&caller, &interrupted.target_id, &interrupted.request_id);
        service
            .ledger
            .reserve(
                &interrupted_key,
                interrupted.canonical_digest("open").unwrap(),
                "interrupted",
                now_ms(),
            )
            .unwrap();
        let interrupted_error = service
            .submit(&caller, interrupted.clone())
            .await
            .unwrap_err();
        assert_eq!(interrupted_error.code, ExecErrorCodeV1::AuditUnavailable);
        assert_eq!(
            service.submit(&caller, interrupted).await.unwrap_err(),
            interrupted_error
        );
        assert_eq!(launches.load(std::sync::atomic::Ordering::Relaxed), 1);
        let mut orphan = request.clone();
        orphan.request_id = uuid::Uuid::new_v4().to_string();
        orphan.profile = Some("open".into());
        orphan.execution_timeout_ms = Some(DEFAULT_TIMEOUT_MS);
        orphan.output_limit_bytes = Some(DEFAULT_OUTPUT_BYTES);
        orphan.cwd = std::fs::canonicalize(&orphan.cwd)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let orphan_key = request_key(&caller, &orphan.target_id, &orphan.request_id);
        service
            .ledger
            .reserve(
                &orphan_key,
                orphan.canonical_digest("open").unwrap(),
                "orphan",
                now_ms(),
            )
            .unwrap();
        service
            .ledger
            .advance(&orphan_key, ReservationPhase::Accepted, now_ms())
            .unwrap();
        let orphan_error = service.submit(&caller, orphan.clone()).await.unwrap_err();
        assert_eq!(orphan_error.code, ExecErrorCodeV1::SupervisorUnavailable);
        assert_eq!(
            service.submit(&caller, orphan).await.unwrap_err(),
            orphan_error
        );
        assert_eq!(launches.load(std::sync::atomic::Ordering::Relaxed), 1);
        let mut changed = request;
        changed.argv.push("changed".into());
        assert_eq!(
            service.submit(&caller, changed).await.unwrap_err().code,
            ExecErrorCodeV1::RequestConflict
        );
    }
}

/// On-behalf-of submissions (enrollment design §9, INV-E21 and INV-E22).
#[cfg(test)]
mod on_behalf_tests {
    use super::*;
    use crate::policy::GrantStatusV1;
    use aster::delegation::{
        ActionGrant, Audience, Chain, DelegationGrant, LinkKind, Validity, issue_action,
        issue_delegation,
    };
    use aster::{PublicKey, SecretKey};

    const AUDIENCE: &str = "fleet";

    /// Admits every caller; `revoked` stands in for the root's issuer-status
    /// records in the fleet namespace.
    #[derive(Default)]
    struct Admitted {
        revoked: StdMutex<Vec<[u8; 32]>>,
        /// Nodes whose enrollment the root has revoked.
        denied: StdMutex<Vec<[u8; 32]>>,
    }
    #[async_trait]
    impl EnrollmentAdmission for Admitted {
        async fn check(&self, node: &[u8; 32]) -> Result<(), AdmissionError> {
            if self.denied.lock().unwrap().contains(node) {
                return Err(AdmissionError::Denied(
                    aster::enrollment::DenialReason::NodeRevoked,
                ));
            }
            Ok(())
        }
        fn delegate_revoked(&self, key: &[u8; 32]) -> bool {
            self.revoked.lock().unwrap().contains(key)
        }
    }

    fn key(seed: u8) -> SecretKey {
        SecretKey::from_bytes([seed; 32])
    }
    fn id(key: &SecretKey) -> String {
        key.public_key().to_hex()
    }
    fn node(key: &SecretKey) -> ExecCaller {
        ExecCaller::node(id(key)).unwrap()
    }
    fn now() -> u64 {
        now_ms() / 1000
    }
    fn validity() -> Validity {
        Validity {
            issued_at: now(),
            not_before: None,
            expires_at: now() + 3_600,
        }
    }

    /// Grants by holder, all for one audience like the Docs store; a missing
    /// holder is a definite denial. Records which holders were read.
    #[derive(Default)]
    struct GrantTable {
        grants: StdMutex<BTreeMap<[u8; 32], ExecGrantV1>>,
        reads: StdMutex<Vec<[u8; 32]>>,
    }
    #[async_trait]
    impl GrantProvider for GrantTable {
        async fn current(
            &self,
            caller: [u8; 32],
            _: [u8; 32],
            _: u64,
        ) -> Result<ExecGrantV1, ExecErrorV1> {
            self.reads.lock().unwrap().push(caller);
            self.grants
                .lock()
                .unwrap()
                .get(&caller)
                .cloned()
                .ok_or_else(|| error(ExecErrorCodeV1::PermissionDenied, "no grant"))
        }
        fn audience(&self) -> Option<String> {
            Some(AUDIENCE.into())
        }
    }
    impl GrantTable {
        fn insert(&self, grant: ExecGrantV1) {
            let holder = grant_subject(&grant.caller).unwrap();
            self.grants.lock().unwrap().insert(holder, grant);
        }
    }

    #[derive(Default)]
    struct RecordingAudit(StdMutex<Vec<AuditEvent>>);
    #[async_trait]
    impl ExecAudit for RecordingAudit {
        async fn admit(&self, event: AuditEvent) -> Result<(), ExecErrorV1> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    /// Launches into memory, deriving the job's principal from the request
    /// as the Linux adapter does.
    #[derive(Default)]
    struct MemoryRuntime {
        jobs: StdMutex<BTreeMap<String, JobStatusV1>>,
        cancelled: StdMutex<Vec<String>>,
    }
    #[async_trait]
    impl ExecRuntime for MemoryRuntime {
        async fn capabilities(&self) -> Result<TargetCapabilitiesV1, ExecErrorV1> {
            panic!("unexpected capabilities")
        }
        async fn launch(
            &self,
            request: ExecRequestV1,
            authorized: AuthorizedSubmit,
            job_id: String,
            _: [u8; 32],
            caller: String,
            accepted_at_ms: u64,
        ) -> Result<JobStatusV1, ExecErrorV1> {
            let status = JobStatusV1 {
                job: JobRefV1 {
                    job_id: job_id.clone(),
                    target_id: request.target_id.clone(),
                },
                state: JobStateV1::Running,
                caller_id: caller,
                principal_id: request
                    .on_behalf_of
                    .as_ref()
                    .map(|on_behalf| on_behalf.principal.clone()),
                resolved_target: request.target_id.clone(),
                execution_identity: authorized.account_alias,
                accepted_at_ms,
                started_at_ms: Some(accepted_at_ms),
                finished_at_ms: None,
                exit_code: None,
                signal: None,
                reason_code: None,
                output_cursor: 0,
                guarantees: GuaranteesV1 {
                    profile: authorized.profile,
                    provider: "memory".into(),
                    network: NetworkGuaranteeV1::Inherited,
                    proxy_id: None,
                    filesystem: FilesystemGuaranteeV1::Inherited,
                    filesystem_policy_id: None,
                    execution_identity: "runner".into(),
                    resource_limits: ResourceLimitsV1 {
                        cpu_millis_per_second: None,
                        memory_bytes: None,
                        processes: None,
                        file_bytes: None,
                    },
                    descendant_scope: DescendantScopeV1::Cgroup,
                    restart_survival: RestartSurvivalV1::ApiRestart,
                },
            };
            self.jobs.lock().unwrap().insert(job_id, status.clone());
            Ok(status)
        }
        async fn status(&self, job: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1> {
            self.jobs
                .lock()
                .unwrap()
                .get(&job.job_id)
                .cloned()
                .ok_or_else(|| error(ExecErrorCodeV1::JobNotFound, "no such job"))
        }
        async fn output(&self, _: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1> {
            panic!("unexpected output")
        }
        async fn cancel(&self, request: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1> {
            self.cancelled
                .lock()
                .unwrap()
                .push(request.job.job_id.clone());
            if let Some(job) = self.jobs.lock().unwrap().get_mut(&request.job.job_id) {
                job.state = JobStateV1::Cancelled;
            }
            Err(error(ExecErrorCodeV1::Internal, "receipt not modelled"))
        }
        async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1> {
            Ok(self.jobs.lock().unwrap().values().cloned().collect())
        }
        async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1> {
            Ok(vec![])
        }
        async fn ack_outcome(&self, _: &str) -> Result<(), ExecErrorV1> {
            Ok(())
        }
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        cwd: String,
        target: String,
        service: ExecService,
        grants: Arc<GrantTable>,
        audit: Arc<RecordingAudit>,
        runtime: Arc<MemoryRuntime>,
        admission: Arc<Admitted>,
    }

    fn grant(fixture_cwd: &str, holder: &str, target: &str) -> ExecGrantV1 {
        ExecGrantV1 {
            version: 1,
            issuer: "33".repeat(32),
            audience: AUDIENCE.into(),
            caller: holder.into(),
            target: target.into(),
            revision: 1,
            issued_at: 1,
            not_before: None,
            expires_at: None,
            status: GrantStatusV1::Active,
            account_alias: "builder".into(),
            profiles: vec!["open".into(), "build".into()],
            operations: vec![
                ExecOperationV1::Submit,
                ExecOperationV1::Status,
                ExecOperationV1::Cancel,
            ],
            cwd_roots: vec![fixture_cwd.into()],
            max_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
            cancel_on_revocation: true,
            reauthorizes_revision: None,
            require_request_digest: false,
        }
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(temp.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let target = id(&key(0x22));
        let grants = Arc::new(GrantTable::default());
        let audit = Arc::new(RecordingAudit::default());
        let runtime = Arc::new(MemoryRuntime::default());
        let admission = Arc::new(Admitted::default());
        let service = ExecService::new(
            LocalCeilingV1 {
                version: 1,
                target_id: target.clone(),
                account_alias: "builder".into(),
                default_profile: "open".into(),
                enabled_profiles: ["open".into(), "build".into()].into(),
                enabled_operations: [
                    ExecOperationV1::Submit,
                    ExecOperationV1::Status,
                    ExecOperationV1::Cancel,
                ]
                .into(),
                cwd_roots: vec![cwd.clone().into()],
                max_timeout_ms: DEFAULT_TIMEOUT_MS,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
                denied_callers: Default::default(),
            },
            admission.clone(),
            grants.clone(),
            audit.clone(),
            runtime.clone(),
            RequestLedger::open(&temp.path().join("requests.redb")).unwrap(),
        );
        Fixture {
            _temp: temp,
            cwd,
            target,
            service,
            grants,
            audit,
            runtime,
            admission,
        }
    }

    fn request(fixture: &Fixture) -> ExecRequestV1 {
        ExecRequestV1 {
            request_id: uuid::Uuid::new_v4().to_string(),
            target_id: fixture.target.clone(),
            profile: None,
            cwd: fixture.cwd.clone(),
            executable: "/bin/true".into(),
            argv: vec![],
            environment_overrides: Default::default(),
            stdin_bytes: None,
            execution_timeout_ms: None,
            output_limit_bytes: None,
            on_behalf_of: None,
        }
    }

    /// Principal H delegates action issuance to orchestrator O.
    fn principal_delegates(principal: &SecretKey, orchestrator: &SecretKey) -> Chain {
        issue_delegation(
            principal,
            None,
            &orchestrator.public_key(),
            &Audience::One(AUDIENCE.into()),
            &DelegationGrant {
                may_issue: [LinkKind::Action].into(),
                audiences: [AUDIENCE.to_string()].into(),
                scopes: Default::default(),
                tags: Default::default(),
                attribute_keys: Default::default(),
                max_child_lifetime: 3_600,
                max_depth: 0,
                subjects: None,
                operations: ["exec.submit".to_string()].into(),
                mandatory: Default::default(),
                may_register: false,
            },
            validity(),
        )
        .unwrap()
    }

    /// O lets actor A submit for H within `resources`.
    fn action(
        orchestrator: &SecretKey,
        parent: &Chain,
        actor: &PublicKey,
        resources: &[String],
        request: Option<String>,
    ) -> Chain {
        issue_action(
            orchestrator,
            Some(parent),
            actor,
            &Audience::One(AUDIENCE.into()),
            &ActionGrant {
                operations: ["exec.submit".to_string()].into(),
                resources: resources.iter().cloned().collect(),
                request,
                mandatory: Default::default(),
            },
            validity(),
        )
        .unwrap()
    }

    fn on_behalf(
        mut request: ExecRequestV1,
        principal: &SecretKey,
        chain: &Chain,
    ) -> ExecRequestV1 {
        request.on_behalf_of = Some(OnBehalfOfV1 {
            principal: id(principal),
            chain: chain.to_text().into(),
        });
        request
    }

    fn denied(result: Result<JobStatusV1, ExecErrorV1>) -> ExecErrorV1 {
        let failure = result.unwrap_err();
        assert_eq!(
            failure.code,
            ExecErrorCodeV1::PermissionDenied,
            "{failure:?}"
        );
        failure
    }

    /// INV-E21 and INV-E22: H → O (delegation) → A (action). A submits for
    /// H; the job runs under H's grant and its audit names H, A, the chain
    /// digest and every jti.
    #[tokio::test]
    async fn actor_submits_under_the_principals_grant_and_audit_names_both() {
        let f = fixture();
        let (h, o, a) = (key(1), key(2), key(3));
        f.grants.insert(grant(&f.cwd, &id(&h), &f.target));
        let chain = action(
            &o,
            &principal_delegates(&h, &o),
            &a.public_key(),
            &[
                format!("exec:target={}", f.target),
                "exec:profile=open".into(),
                format!("exec:cwd_root={}", f.cwd),
            ],
            None,
        );
        let status = f
            .service
            .submit(&id(&a), on_behalf(request(&f), &h, &chain))
            .await
            .unwrap();
        assert_eq!(status.caller_id, id(&a));
        assert_eq!(status.principal_id.as_deref(), Some(id(&h).as_str()));

        let intent = f.audit.0.lock().unwrap()[0].payload.clone();
        assert_eq!(intent["caller"], id(&a));
        assert_eq!(intent["principal"], id(&h));
        assert_eq!(intent["actor"], id(&a));
        assert!(intent.get("forwarded").is_none());
        let recorded = &intent["on_behalf_of"];
        assert_eq!(recorded["chain_digest"], hex::encode(chain.digest()));
        let jtis = recorded["chain_jtis"].as_array().unwrap();
        assert_eq!(jtis.len(), 2);
        assert!(
            jtis.iter()
                .all(|jti| jti.as_str().is_some_and(|v| !v.is_empty()))
        );

        // The actor follows its job under H's grant; it holds none itself.
        let job = status.job.clone();
        f.service
            .owned(&node(&a), &job, ExecOperationV1::Status)
            .await
            .unwrap();
        // H's revocation cancels the job: its authority is H's grant.
        let mut revoked = grant(&f.cwd, &id(&h), &f.target);
        revoked.status = GrantStatusV1::Revoked;
        f.grants.insert(revoked);
        f.service.invalidate_all();
        let _ = f.service.reconcile_policy().await;
        assert_eq!(*f.runtime.cancelled.lock().unwrap(), vec![job.job_id]);
    }

    /// Root-revoking delegate O (`IssuerStatusV1`) withdraws every action
    /// chain through it: A can no longer submit for H. A job A already
    /// started for H keeps running, because its authority is H's grant, which
    /// still stands; H's grant revocation is what cancels it.
    #[tokio::test]
    async fn a_revoked_delegate_withdraws_new_submissions_only() {
        let f = fixture();
        let (h, o, a) = (key(1), key(2), key(3));
        f.grants.insert(grant(&f.cwd, &id(&h), &f.target));
        let chain = action(&o, &principal_delegates(&h, &o), &a.public_key(), &[], None);
        let running = f
            .service
            .submit(&id(&a), on_behalf(request(&f), &h, &chain))
            .await
            .unwrap();

        f.admission
            .revoked
            .lock()
            .unwrap()
            .push(o.public_key().to_bytes());
        let failure = denied(
            f.service
                .submit(&id(&a), on_behalf(request(&f), &h, &chain))
                .await,
        );
        assert!(failure.safe_message.contains("Revoked"), "{failure:?}");

        f.service.invalidate_all();
        let _ = f.service.reconcile_policy().await;
        assert!(f.runtime.cancelled.lock().unwrap().is_empty());
        f.service
            .owned(&node(&a), &running.job, ExecOperationV1::Status)
            .await
            .unwrap();
    }

    /// INV-E21: without a chain the caller's own grant applies; with one, the
    /// caller's own grant never substitutes for the principal's.
    #[tokio::test]
    async fn own_grant_applies_only_without_a_chain() {
        let f = fixture();
        let (h, o, a) = (key(1), key(2), key(3));
        f.grants.insert(grant(&f.cwd, &id(&a), &f.target));
        let status = f.service.submit(&id(&a), request(&f)).await.unwrap();
        assert_eq!(status.principal_id, None);

        // H holds no grant: A's own grant must not authorize the chain.
        let chain = action(&o, &principal_delegates(&h, &o), &a.public_key(), &[], None);
        denied(
            f.service
                .submit(&id(&a), on_behalf(request(&f), &h, &chain))
                .await,
        );
        // Naming oneself as principal is refused rather than treated as own.
        let mut own = request(&f);
        own.on_behalf_of = Some(OnBehalfOfV1 {
            principal: id(&a),
            chain: chain.to_text().into(),
        });
        assert_eq!(
            f.service.submit(&id(&a), own).await.unwrap_err().code,
            ExecErrorCodeV1::InvalidArgument
        );
    }

    /// INV-E21: resources narrow the principal's grant; a different target
    /// or profile, an unknown resource, a missing `exec.submit`, a chain
    /// rooted elsewhere or issued to another actor are all denied.
    #[tokio::test]
    async fn chains_outside_their_resources_or_anchor_are_denied() {
        let f = fixture();
        let (h, o, a, other) = (key(1), key(2), key(3), key(4));
        f.grants.insert(grant(&f.cwd, &id(&h), &f.target));
        let parent = principal_delegates(&h, &o);
        for resources in [
            vec![format!("exec:target={}", id(&key(0x23)))],
            vec!["exec:profile=build".to_string()],
            vec!["exec:cwd_root=/nonexistent-aster-root".to_string()],
            vec!["exec:executable=/bin/true".to_string()],
            vec!["unscoped".to_string()],
        ] {
            let chain = action(&o, &parent, &a.public_key(), &resources, None);
            denied(
                f.service
                    .submit(&id(&a), on_behalf(request(&f), &h, &chain))
                    .await,
            );
        }
        // A grant for H, but the chain is rooted at another principal.
        let elsewhere = action(
            &o,
            &principal_delegates(&other, &o),
            &a.public_key(),
            &[],
            None,
        );
        let failure = denied(
            f.service
                .submit(&id(&a), on_behalf(request(&f), &h, &elsewhere))
                .await,
        );
        assert!(
            failure.safe_message.contains("UntrustedAnchor"),
            "{failure:?}"
        );
        // Named principal holds no grant.
        denied(
            f.service
                .submit(&id(&a), on_behalf(request(&f), &other, &elsewhere))
                .await,
        );
        // Issued to a different actor than the one submitting.
        let chain = action(&o, &parent, &other.public_key(), &[], None);
        let failure = denied(
            f.service
                .submit(&id(&a), on_behalf(request(&f), &h, &chain))
                .await,
        );
        assert!(failure.safe_message.contains("LeafMismatch"), "{failure:?}");
        // A leaf without exec.submit.
        let status_only = issue_action(
            &o,
            Some(&parent),
            &a.public_key(),
            &Audience::One(AUDIENCE.into()),
            &ActionGrant {
                operations: Default::default(),
                resources: Default::default(),
                request: None,
                mandatory: Default::default(),
            },
            validity(),
        )
        .unwrap();
        denied(
            f.service
                .submit(&id(&a), on_behalf(request(&f), &h, &status_only))
                .await,
        );
        assert!(f.runtime.jobs.lock().unwrap().is_empty());
    }

    /// D5: a grant can require single-use chains bound to the request digest;
    /// a bound digest must match whether or not the grant requires one.
    #[tokio::test]
    async fn request_digest_is_enforced_when_bound_or_required() {
        let f = fixture();
        let (h, o, a) = (key(1), key(2), key(3));
        let mut required = grant(&f.cwd, &id(&h), &f.target);
        required.require_request_digest = true;
        f.grants.insert(required);
        let parent = principal_delegates(&h, &o);

        let unbound = action(&o, &parent, &a.public_key(), &[], None);
        let failure = denied(
            f.service
                .submit(&id(&a), on_behalf(request(&f), &h, &unbound))
                .await,
        );
        assert!(failure.safe_message.contains("requires"), "{failure:?}");

        let submitted = request(&f);
        let digest = hex::encode(submitted.on_behalf_digest().unwrap());
        let other = hex::encode(request(&f).on_behalf_digest().unwrap());
        let wrong = action(&o, &parent, &a.public_key(), &[], Some(other));
        denied(
            f.service
                .submit(&id(&a), on_behalf(submitted.clone(), &h, &wrong))
                .await,
        );
        let bound = action(&o, &parent, &a.public_key(), &[], Some(digest));
        f.service
            .submit(&id(&a), on_behalf(submitted, &h, &bound))
            .await
            .unwrap();
    }

    /// A chain that does not authenticate for the named principal is
    /// rejected before Exec reads that principal's grant.
    #[tokio::test]
    async fn an_unauthenticated_chain_reads_no_principal_grant() {
        let f = fixture();
        let (h, o, a, other) = (key(1), key(2), key(3), key(4));
        f.grants.insert(grant(&f.cwd, &id(&h), &f.target));
        let foreign = action(
            &o,
            &principal_delegates(&other, &o),
            &a.public_key(),
            &[],
            None,
        );
        for principal in [&h, &key(5)] {
            denied(
                f.service
                    .submit(&id(&a), on_behalf(request(&f), principal, &foreign))
                    .await,
            );
        }
        let mut garbage = on_behalf(request(&f), &h, &foreign);
        garbage.on_behalf_of.as_mut().unwrap().chain = "aster-chain-v1~not.a.jwt".into();
        denied(f.service.submit(&id(&a), garbage).await);
        assert!(f.grants.reads.lock().unwrap().is_empty());
    }

    #[test]
    fn authority_keys_round_trip() {
        let own = Authority::own(&"11".repeat(32));
        assert_eq!(own.key(), "11".repeat(32));
        assert_eq!(Authority::parse(&own.key()), own);
        let acting = Authority {
            actor: "11".repeat(32),
            principal: "22".repeat(32),
        };
        assert_eq!(Authority::parse(&acting.key()), acting);
        // A user principal's issuer is usually a URL, with `/` in it.
        let user = "https://idp.example/realms/a|alice/b";
        let own_user = Authority::own(user);
        assert_eq!(Authority::parse(&own_user.key()), own_user);
        let forwarded = Authority {
            actor: "11".repeat(32),
            principal: user.into(),
        };
        assert_eq!(Authority::parse(&forwarded.key()), forwarded);
    }

    const ISSUER: &str = "https://idp.example";
    const ALICE: &str = "https://idp.example|alice";

    fn user(principal: &str, edge: &SecretKey) -> ExecCaller {
        ExecCaller::forwarded(
            principal,
            id(edge),
            ISSUER,
            "jti-0001",
            vec!["webauthn".into()],
        )
        .unwrap()
    }

    #[test]
    fn only_user_principals_and_node_actors_are_forwarded() {
        let edge = id(&key(9));
        assert!(ExecCaller::forwarded(ALICE, &edge, ISSUER, "j", vec![]).is_ok());
        for (principal, actor) in [
            (edge.as_str(), edge.as_str()),
            ("no-separator", edge.as_str()),
            ("|alice", edge.as_str()),
            (ALICE, "not-a-node"),
        ] {
            let failure = ExecCaller::forwarded(principal, actor, ISSUER, "j", vec![]).unwrap_err();
            assert_eq!(failure.code, ExecErrorCodeV1::Unauthenticated);
        }
        // A node calling as itself is never taken for a user.
        assert!(ExecCaller::node(ALICE).is_err());
    }

    /// Gateway identity, slice 7: an edge forwards alice (`iss|sub`); her
    /// job runs under the grant naming her, and the audit names alice as
    /// principal and the edge as actor, with the token's issuer and jti.
    #[tokio::test]
    async fn a_forwarded_user_submits_under_the_grant_naming_it() {
        let f = fixture();
        let (edge, other_edge) = (key(7), key(8));
        f.grants.insert(grant(&f.cwd, ALICE, &f.target));
        let status = f
            .service
            .submit_as(&user(ALICE, &edge), request(&f))
            .await
            .unwrap();
        assert_eq!(status.caller_id, ALICE);
        assert_eq!(status.principal_id, None);
        assert!(
            f.grants
                .reads
                .lock()
                .unwrap()
                .contains(&grant_subject(ALICE).unwrap())
        );

        let intent = f.audit.0.lock().unwrap()[0].payload.clone();
        assert_eq!(intent["caller"], ALICE);
        assert_eq!(intent["principal"], ALICE);
        assert_eq!(intent["actor"], id(&edge));
        assert_eq!(intent["forwarded"]["issuer"], ISSUER);
        assert_eq!(intent["forwarded"]["jti"], "jti-0001");
        assert_eq!(intent["forwarded"]["auth_methods"][0], "webauthn");
        assert!(intent.get("on_behalf_of").is_none());

        // Alice follows her job through any gateway; the edge itself and
        // another user cannot.
        let job = status.job.clone();
        f.service
            .owned(&user(ALICE, &other_edge), &job, ExecOperationV1::Status)
            .await
            .unwrap();
        denied(
            f.service
                .owned(&node(&edge), &job, ExecOperationV1::Status)
                .await,
        );
        denied(
            f.service
                .owned(
                    &user("https://idp.example|bob", &edge),
                    &job,
                    ExecOperationV1::Status,
                )
                .await,
        );
        // Bob holds no grant, and alice's grant is no use to the edge.
        denied(
            f.service
                .submit_as(&user("https://idp.example|bob", &edge), request(&f))
                .await,
        );
        denied(f.service.submit(&id(&edge), request(&f)).await);
    }

    /// A forwarded token is not bound to the request, so a grant that
    /// requires request-bound chains refuses it; and a forwarded call cannot
    /// also carry an action chain.
    #[tokio::test]
    async fn a_forwarded_user_meets_no_digest_bound_grant_and_carries_no_chain() {
        let f = fixture();
        let (h, o, edge) = (key(1), key(2), key(7));
        let mut required = grant(&f.cwd, ALICE, &f.target);
        required.require_request_digest = true;
        f.grants.insert(required);
        let failure = denied(f.service.submit_as(&user(ALICE, &edge), request(&f)).await);
        assert!(failure.safe_message.contains("requires"), "{failure:?}");

        f.grants.insert(grant(&f.cwd, &id(&h), &f.target));
        let chain = action(
            &o,
            &principal_delegates(&h, &o),
            &edge.public_key(),
            &[],
            None,
        );
        let failure = f
            .service
            .submit_as(&user(ALICE, &edge), on_behalf(request(&f), &h, &chain))
            .await
            .unwrap_err();
        assert_eq!(failure.code, ExecErrorCodeV1::InvalidArgument);
        assert!(f.runtime.jobs.lock().unwrap().is_empty());
    }

    /// A revoked edge can forward nothing more, but the job it submitted is
    /// alice's: it runs while her grant stands and is cancelled when it goes.
    #[tokio::test]
    async fn a_revoked_edge_stops_forwarding_and_the_users_grant_keeps_its_job() {
        let f = fixture();
        let edge = key(7);
        f.grants.insert(grant(&f.cwd, ALICE, &f.target));
        let running = f
            .service
            .submit_as(&user(ALICE, &edge), request(&f))
            .await
            .unwrap();

        f.admission
            .denied
            .lock()
            .unwrap()
            .push(edge.public_key().to_bytes());
        f.service.invalidate_peer(&id(&edge));
        denied(f.service.submit_as(&user(ALICE, &edge), request(&f)).await);
        let _ = f.service.reconcile_policy().await;
        assert!(f.runtime.cancelled.lock().unwrap().is_empty());

        let mut revoked = grant(&f.cwd, ALICE, &f.target);
        revoked.status = GrantStatusV1::Revoked;
        f.grants.insert(revoked);
        f.service.invalidate_all();
        let _ = f.service.reconcile_policy().await;
        assert_eq!(
            *f.runtime.cancelled.lock().unwrap(),
            vec![running.job.job_id]
        );
    }
}
