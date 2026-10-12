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
    policy::{ExecGrantStore, ExecGrantV1, ExecOperationV1, GrantError},
};
use aster::audit_outbox::{AuditEvent, AuditOutbox};
use aster::rpc::{Call, RpcStatus, ServiceDispatch, StatusCode, async_trait};
use aster_transport_core::CoreDoc;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const MAX_AUTHORITY_PEERS: usize = 1024;
pub const MAX_REFRESH_PEERS: usize = 128;
pub const AUTHORITY_FRESHNESS: Duration = Duration::from_secs(30);
const DENIAL_FRESHNESS: Duration = Duration::from_secs(1);
const AUTHORITY_READ_TIMEOUT: Duration = Duration::from_secs(5);
const WATCH_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_WATCHES_PER_CALLER: usize = 4;
const MAX_WATCHES: usize = 64;

struct PeerAuthority {
    refreshed_at: Instant,
    last_used: Instant,
    last_success: Option<Instant>,
    result: Result<ExecGrantV1, ExecErrorV1>,
}

#[derive(Default)]
struct AuthoritySnapshot {
    observed: BTreeSet<String>,
    refresh_offset: usize,
    peers: BTreeMap<String, PeerAuthority>,
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
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Admission and operation authority are independent checks.
#[async_trait]
pub trait EnrollmentAdmission: Send + Sync {
    async fn check(&self, caller: &[u8; 32]) -> Result<(), ExecErrorV1>;
}
#[async_trait]
impl EnrollmentAdmission for aster::enrollment::Enrollment {
    async fn check(&self, caller: &[u8; 32]) -> Result<(), ExecErrorV1> {
        self.check_peer(caller).await.map(|_| ()).map_err(|_| {
            error(
                ExecErrorCodeV1::PermissionDenied,
                "enrollment admission denied",
            )
        })
    }
}
/// Authenticate the current exact-root leaf on cold admission and background refresh.
#[async_trait]
pub trait GrantProvider: Send + Sync {
    async fn current(
        &self,
        caller: [u8; 32],
        target: [u8; 32],
        now_seconds: u64,
    ) -> Result<ExecGrantV1, ExecErrorV1>;
}
pub struct DocsGrantProvider {
    pub store: Mutex<ExecGrantStore>,
    pub document: CoreDoc,
}
#[async_trait]
impl GrantProvider for DocsGrantProvider {
    async fn current(
        &self,
        caller: [u8; 32],
        target: [u8; 32],
        now: u64,
    ) -> Result<ExecGrantV1, ExecErrorV1> {
        let mut store = self.store.lock().await;
        store
            .refresh_one(&self.document, &caller, &target)
            .await
            .map_err(grant_error)?;
        store.grant_for(&caller, &target, now).map_err(grant_error)
    }
}
fn grant_error(failure: GrantError) -> ExecErrorV1 {
    match failure {
        GrantError::PolicyUnavailable | GrantError::Storage(_) => error(
            ExecErrorCodeV1::PolicyUnavailable,
            "current Exec policy unavailable",
        ),
        GrantError::InvalidRecord
        | GrantError::WrongContext
        | GrantError::Stale
        | GrantError::Conflict
        | GrantError::ReauthorizationRequired
        | GrantError::Denied => error(ExecErrorCodeV1::PermissionDenied, "Exec grant denied"),
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
    started_at: Instant,
    inflight: Arc<StdMutex<BTreeSet<String>>>,
    watchers: Arc<StdMutex<BTreeMap<String, usize>>>,
    authority: Arc<RwLock<AuthoritySnapshot>>,
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
            started_at: Instant::now(),
            inflight: Arc::new(StdMutex::new(BTreeSet::new())),
            watchers: Arc::new(StdMutex::new(BTreeMap::new())),
            authority: Arc::new(RwLock::new(AuthoritySnapshot::default())),
            cold_refresh: Arc::new(Mutex::new(BTreeMap::new())),
            reconcile_lock: Arc::new(Mutex::new(())),
        }
    }
    /// Refresh an admitted peer outside the RPC hot path. Exact-root Docs and
    /// enrollment checks execute here; successful snapshots are bounded in age.
    pub async fn refresh_peer(&self, caller: &str) -> Result<(), ExecErrorV1> {
        let started = Instant::now();
        let decode = |id: &str| hex::decode(id).ok().and_then(|bytes| bytes.try_into().ok());
        let caller_bytes = decode(caller)
            .ok_or_else(|| error(ExecErrorCodeV1::Unauthenticated, "invalid node identity"))?;
        let target_bytes = decode(&self.ceiling.target_id)
            .ok_or_else(|| ExecErrorV1::invalid("invalid target identity"))?;
        let result = tokio::time::timeout(AUTHORITY_READ_TIMEOUT, async {
            self.enrollment.check(&caller_bytes).await?;
            self.grants
                .current(caller_bytes, target_bytes, now_ms() / 1000)
                .await
        })
        .await
        .unwrap_or_else(|_| {
            Err(error(
                ExecErrorCodeV1::PolicyUnavailable,
                "Exec authority refresh timed out",
            ))
        });
        let mut snapshot = self.authority.write().map_err(|_| {
            error(
                ExecErrorCodeV1::PolicyUnavailable,
                "authorization snapshot unavailable",
            )
        })?;
        if let Some(existing) = snapshot.peers.get(caller)
            && existing.refreshed_at > started
        {
            return existing.result.clone().map(|_| ());
        }
        if snapshot.peers.len() >= MAX_AUTHORITY_PEERS
            && !snapshot.peers.contains_key(caller)
            && let Some(victim) = snapshot
                .peers
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(peer, _)| peer.clone())
        {
            snapshot.peers.remove(&victim);
            snapshot.observed.remove(&victim);
        }
        if result.is_ok() {
            snapshot.observed.insert(caller.into());
        } else {
            snapshot.observed.remove(caller);
        }
        let last_used = snapshot
            .peers
            .get(caller)
            .map_or(started, |entry| entry.last_used);
        let last_success = if result.is_ok() {
            Some(started)
        } else {
            snapshot
                .peers
                .get(caller)
                .and_then(|entry| entry.last_success)
        };
        snapshot.peers.insert(
            caller.into(),
            PeerAuthority {
                refreshed_at: started,
                last_used,
                last_success,
                result: result.clone(),
            },
        );
        result.map(|_| ())
    }
    /// Immediately remove a revoked Enrollment session from usable authority.
    /// An older in-flight refresh cannot replace this denial.
    pub fn invalidate_peer(&self, caller: &str) -> Result<(), ExecErrorV1> {
        let mut snapshot = self.authority.write().map_err(|_| {
            error(
                ExecErrorCodeV1::PolicyUnavailable,
                "authorization snapshot unavailable",
            )
        })?;
        snapshot.observed.remove(caller);
        if snapshot.peers.len() >= MAX_AUTHORITY_PEERS
            && !snapshot.peers.contains_key(caller)
            && let Some(victim) = snapshot
                .peers
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(peer, _)| peer.clone())
        {
            snapshot.peers.remove(&victim);
            snapshot.observed.remove(&victim);
        }
        snapshot.peers.insert(
            caller.to_owned(),
            PeerAuthority {
                refreshed_at: Instant::now(),
                last_used: Instant::now(),
                last_success: None,
                result: Err(error(
                    ExecErrorCodeV1::PermissionDenied,
                    "enrollment admission revoked",
                )),
            },
        );
        Ok(())
    }
    fn defer_uncertain_cancel(&self, caller: &str, failure: &ExecErrorV1) -> bool {
        if failure.code != ExecErrorCodeV1::PolicyUnavailable {
            return false;
        }
        let last_success = self.authority.read().ok().and_then(|snapshot| {
            snapshot
                .peers
                .get(caller)
                .and_then(|entry| entry.last_success)
        });
        last_success.unwrap_or(self.started_at).elapsed() < AUTHORITY_FRESHNESS
    }
    /// Call on policy/enrollment change notifications and every one second.
    /// A helper failure must not stop enrollment and grant refresh for cached
    /// callers. Each peer read has its own deadline and bounded concurrency.
    pub async fn refresh_authority(&self) -> Result<(), ExecErrorV1> {
        let mut callers = self
            .authority
            .read()
            .map_err(|_| {
                error(
                    ExecErrorCodeV1::PolicyUnavailable,
                    "authorization snapshot unavailable",
                )
            })?
            .observed
            .clone();
        {
            let mut state = self.authority.write().map_err(|_| {
                error(
                    ExecErrorCodeV1::PolicyUnavailable,
                    "authorization snapshot unavailable",
                )
            })?;
            let all = callers.into_iter().collect::<Vec<_>>();
            callers = all
                .iter()
                .cycle()
                .skip(state.refresh_offset)
                .take(all.len().min(MAX_REFRESH_PEERS))
                .cloned()
                .collect();
            state.refresh_offset = if all.is_empty() {
                0
            } else {
                (state.refresh_offset + MAX_REFRESH_PEERS) % all.len()
            };
        }
        let jobs = self.runtime.list_jobs().await;
        if let Ok(jobs) = &jobs {
            callers.extend(
                jobs.iter()
                    .filter(|job| !job.state.is_terminal())
                    .map(|job| job.caller_id.clone()),
            );
        }
        let mut queue = callers.into_iter();
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            while tasks.len() < 16 {
                let Some(caller) = queue.next() else { break };
                let service = self.clone();
                tasks.spawn(async move { service.refresh_peer(&caller).await });
            }
            if tasks.join_next().await.is_none() {
                break;
            }
        }
        jobs.map(|_| ())
    }
    async fn authorize(
        &self,
        caller: &str,
        target: &str,
        op: ExecOperationV1,
    ) -> Result<ExecGrantV1, ExecErrorV1> {
        if caller.len() != 64 || hex::decode(caller).is_err() {
            return Err(error(
                ExecErrorCodeV1::Unauthenticated,
                "authenticated node identity required",
            ));
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
        let cold = self
            .authority
            .read()
            .map_err(|_| {
                error(
                    ExecErrorCodeV1::PolicyUnavailable,
                    "authorization snapshot unavailable",
                )
            })?
            .peers
            .get(caller)
            .is_none_or(|entry| {
                entry.result.is_err() && entry.refreshed_at.elapsed() > DENIAL_FRESHNESS
            });
        if cold {
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
            let missing = self
                .authority
                .read()
                .map_err(|_| {
                    error(
                        ExecErrorCodeV1::PolicyUnavailable,
                        "authorization snapshot unavailable",
                    )
                })?
                .peers
                .get(caller)
                .is_none_or(|entry| {
                    entry.result.is_err() && entry.refreshed_at.elapsed() > DENIAL_FRESHNESS
                });
            if missing {
                self.refresh_peer(caller).await?;
            }
        }
        let grant = {
            let mut snapshot = self.authority.write().map_err(|_| {
                error(
                    ExecErrorCodeV1::PolicyUnavailable,
                    "authorization snapshot unavailable",
                )
            })?;
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
            entry.result.clone()?
        };
        let now = now_ms() / 1000;
        if !grant.cancel_on_revocation
            || grant.version != 1
            || grant.status != crate::policy::GrantStatusV1::Active
            || grant.issued_at > now.saturating_add(60)
            || grant.not_before.is_some_and(|time| now < time)
            || grant.expires_at.is_some_and(|time| now >= time)
            || grant.caller != caller
            || grant.target != target
            || grant.account_alias != self.ceiling.account_alias
            || !grant.operations.contains(&op)
        {
            return Err(error(
                ExecErrorCodeV1::PermissionDenied,
                "Exec operation denied",
            ));
        }
        Ok(grant)
    }
    async fn owned(
        &self,
        caller: &str,
        job: &JobRefV1,
        op: ExecOperationV1,
    ) -> Result<JobStatusV1, ExecErrorV1> {
        self.authorize(caller, &job.target_id, op).await?;
        let status = self.runtime.status(job).await?;
        if status.caller_id != caller || status.job != *job {
            return Err(error(
                ExecErrorCodeV1::PermissionDenied,
                "job access denied",
            ));
        }
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
            failure.job_id = Some(old.job_id);
            failure.retryable = false;
            self.ledger
                .fail_admission(key, failure.clone(), now_ms())
                .map_err(ledger_error)?;
            return Err(failure);
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
    pub async fn submit(
        &self,
        caller: &str,
        mut request: ExecRequestV1,
    ) -> Result<JobStatusV1, ExecErrorV1> {
        let grant = self
            .authorize(caller, &request.target_id, ExecOperationV1::Submit)
            .await?;
        let authorized = self.ceiling.authorize_submit(caller, &grant, &request)?;
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
        let event = AuditEvent {
            timestamp_ms: now,
            kind: "exec.intent.v1".into(),
            payload: serde_json::json!({"caller":caller,"target":request.target_id,"request_id":request.request_id,"job_id":job_id,"request_digest":hex::encode(digest),"grant_revision":grant.revision,"profile":authorized.profile}),
        };
        if let Err(mut failure) = self.audit.admit(event).await {
            failure.request_id = Some(request.request_id.clone());
            failure.job_id = Some(job_id.clone());
            failure.retryable = false;
            self.ledger
                .fail_admission(&key, failure.clone(), now_ms())
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
    /// Policy unavailability fails closed: active jobs are cancelled and the
    /// lifetime owner durably retains the terminal audit outcome even when
    /// the outbox cannot admit the best-effort revocation intent.
    pub async fn reconcile_policy(&self) -> Result<usize, ExecErrorV1> {
        let _reconcile = self.reconcile_lock.lock().await;
        let mut count = 0;
        let reservations = self.ledger.entries().map_err(ledger_error)?;
        let mut first_error = None;
        let jobs = match self.runtime.list_jobs().await {
            Ok(jobs) => jobs,
            Err(failure) => {
                // One unreadable helper folder must not hide every other job
                // from revocation. Accepted reservations retain the job IDs
                // needed to recover each status independently.
                first_error = Some(failure);
                let mut recovered = Vec::new();
                for (_, reservation) in &reservations {
                    if reservation.phase == ReservationPhase::Terminal {
                        continue;
                    }
                    let job = JobRefV1 {
                        job_id: reservation.job_id.clone(),
                        target_id: self.ceiling.target_id.clone(),
                    };
                    match self.runtime.status(&job).await {
                        Ok(status) => recovered.push(status),
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                }
                recovered
            }
        };
        let inflight = self
            .inflight
            .lock()
            .map_err(|_| error(ExecErrorCodeV1::Internal, "admission state unavailable"))?
            .clone();
        for (key, reservation) in &reservations {
            if reservation.phase == ReservationPhase::Preparing
                && !inflight.contains(&reservation.job_id)
            {
                let mut failure = error(
                    ExecErrorCodeV1::AuditUnavailable,
                    "interrupted before launch; execution will not be retried automatically",
                );
                failure.job_id = Some(reservation.job_id.clone());
                failure.retryable = false;
                self.ledger
                    .fail_admission(key, failure, now_ms())
                    .map_err(ledger_error)?;
                continue;
            }
            // A missing Accepted job is not proof of non-execution. The helper
            // may be restarting or a launch response may have been lost.
            if reservation.phase == ReservationPhase::Accepted
                && jobs
                    .iter()
                    .any(|job| job.job.job_id == reservation.job_id && job.state.is_terminal())
            {
                self.ledger
                    .advance(key, ReservationPhase::Terminal, now_ms())
                    .map_err(ledger_error)?;
            }
        }
        for status in jobs {
            if status.state.is_terminal() {
                continue;
            }
            if let Err(denial) = self
                .authorize(
                    &status.caller_id,
                    &status.job.target_id,
                    ExecOperationV1::Submit,
                )
                .await
            {
                if self.defer_uncertain_cancel(&status.caller_id, &denial) {
                    continue;
                }
                let event = AuditEvent {
                    timestamp_ms: now_ms(),
                    kind: "exec.revocation.cancel.v1".into(),
                    payload: serde_json::json!({
                        "caller": status.caller_id,
                        "job_id": status.job.job_id,
                        "target": status.job.target_id,
                    }),
                };
                if let Err(failure) = self.audit.admit(event).await {
                    eprintln!(
                        "Exec revocation intent audit unavailable for {}",
                        status.job.job_id
                    );
                    first_error.get_or_insert(failure);
                }
                match self
                    .runtime
                    .cancel(&CancelRequestV1 {
                        job: status.job,
                        grace_ms: Some(DEFAULT_CANCEL_GRACE_MS),
                    })
                    .await
                {
                    Ok(_) => count += 1,
                    Err(failure) => {
                        first_error.get_or_insert(failure);
                    }
                }
            }
        }
        if let Err(failure) = self.ledger.prune(now_ms()).map_err(ledger_error) {
            first_error.get_or_insert(failure);
        }
        if let Err(failure) = self.replay_outcomes().await {
            first_error.get_or_insert(failure);
        }
        first_error.map_or(Ok(count), Err)
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
    async fn watch(&self, caller: String, mut call: Call) {
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
        let _permit = match self.watch_permit(&caller) {
            Ok(permit) => permit,
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
                self.authorize(&caller, &request.job.target_id, ExecOperationV1::Output)
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
        caller: &str,
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
                self.authorize(caller, &self.ceiling.target_id, ExecOperationV1::Discover)
                    .await?;
                encode(vec![TargetRefV1 {
                    target_id: self.ceiling.target_id.clone(),
                }])
            }
            "capabilities" => {
                let req: TargetRefV1 = decode(raw)?;
                let grant = self
                    .authorize(caller, &req.target_id, ExecOperationV1::Discover)
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
            "submit" => encode(self.submit(caller, decode(raw)?).await?.job),
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
                self.owned(caller, &req.job, ExecOperationV1::Cancel)
                    .await?;
                if req.grace_ms.is_some_and(|v| v > MAX_CANCEL_GRACE_MS) {
                    return Err(ExecErrorV1::invalid("invalid cancellation grace"));
                }
                self.audit.admit(AuditEvent{timestamp_ms:now_ms(),kind:"exec.cancel.intent.v1".into(),payload:serde_json::json!({"caller":caller,"job_id":req.job.job_id,"target":req.job.target_id})}).await?;
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
        Some(MAX_REQUEST_BYTES as u32)
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        let caller = call.peer().to_string();
        if method == "watch" {
            self.watch(caller, call).await;
            return;
        }
        let result = match call.recv_request().await {
            Some(raw) if raw.len() <= MAX_REQUEST_BYTES => self.run(method, &caller, &raw).await,
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
        async fn check(&self, _: &[u8; 32]) -> Result<(), ExecErrorV1> {
            Ok(())
        }
    }
    struct NotEnrolled;
    #[async_trait]
    impl EnrollmentAdmission for NotEnrolled {
        async fn check(&self, _: &[u8; 32]) -> Result<(), ExecErrorV1> {
            Err(error(ExecErrorCodeV1::PermissionDenied, "not enrolled"))
        }
    }
    struct CountAdmission(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl EnrollmentAdmission for CountAdmission {
        async fn check(&self, _: &[u8; 32]) -> Result<(), ExecErrorV1> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
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
                tokio::spawn(async move { service.refresh_peer(&caller).await })
            };
            tokio::time::timeout(Duration::from_secs(2), grants.entered.notified())
                .await
                .unwrap();
            if invalidate {
                service.invalidate_peer(&caller).unwrap();
            } else {
                assert_eq!(
                    service.refresh_peer(&caller).await.unwrap_err().code,
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
        service.invalidate_peer(&"11".repeat(32)).unwrap();
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
        service.invalidate_peer(&"11".repeat(32)).unwrap();
        let failure = service.reconcile_policy().await.unwrap_err();
        assert_eq!(failure.code, ExecErrorCodeV1::SupervisorUnavailable);
        assert_eq!(failure.safe_message, "list failed");
        assert_eq!(
            *runtime.recovered.lock().unwrap(),
            ["unreadable", "readable"]
        );
        assert_eq!(*runtime.attempted.lock().unwrap(), ["readable"]);
    }

    #[tokio::test]
    async fn helper_list_failure_still_refreshes_cached_authority() {
        let temp = tempfile::tempdir().unwrap();
        let mut service = authority_test_service(&temp);
        let caller = "11".repeat(32);
        service.refresh_peer(&caller).await.unwrap();
        service.runtime = Arc::new(OfflineRuntime);
        service.enrollment = Arc::new(NotEnrolled);
        assert_eq!(
            service.refresh_authority().await.unwrap_err().code,
            ExecErrorCodeV1::SupervisorUnavailable
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
            })),
            Arc::new(AdmitAudit),
            Arc::new(NeverLaunch),
            RequestLedger::open(&temp.path().join("requests.redb")).unwrap(),
        );
        for index in 0..=MAX_AUTHORITY_PEERS {
            service
                .refresh_peer(&format!("{index:064x}"))
                .await
                .unwrap();
        }
        let snapshot = service.authority.read().unwrap();
        assert_eq!(snapshot.peers.len(), MAX_AUTHORITY_PEERS);
        assert!(!snapshot.peers.contains_key(&format!("{:064x}", 0)));
        drop(snapshot);
        service.invalidate_peer(&"ff".repeat(32)).unwrap();
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
        };
        service.enrollment = Arc::new(NotEnrolled);
        assert!(service.refresh_peer(&caller).await.is_err());
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
        service.refresh_peer(&caller).await.unwrap();
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
