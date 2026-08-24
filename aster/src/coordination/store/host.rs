//! The live `aster.coordination.Store` v1 service (S4-B, S4-C).
//!
//! ## One pipeline, nineteen methods
//!
//! Every method runs the same staged boundary; the per-method handlers supply
//! only their decode, their permission row and their projection, and none may
//! reorder the gates:
//!
//! | Stage | Mutation | Read/status |
//! | ---: | --- | --- |
//! | 0 | frame/method/version/group/identity decode | same |
//! | 1 | follower → bounded authenticated redirect hint | same |
//! | 2 | leader linearizable **session resolution** | leader barrier `B`, one guarded read, transaction-local `A >= B` witness |
//! | 3 | reason-coded admission projection or status-only denial | authorize from that same transaction |
//! | 4 | leader-stamped command, propose | read/project from that one transaction |
//! | 5 | project the exact applied/saved reply | bounded unary/stream result |
//!
//! ## Where each stage's authority comes from
//!
//! **Mutations** resolve sessions and admission on the state actor
//! ([`StateHandle::preflight`]), which evaluates `model::apply::preflight` —
//! the *same function apply runs* — against the state machine's applied state.
//! The answer returns with the actor's applied log id, and the pipeline
//! requires it to be at or past the `ensure_linearizable` barrier with
//! complete log-id agreement at an equal index. That is the §8.2.1 witness:
//! the state consulted, proven against the barrier, not assumed from it.
//!
//! **Reads** take the barrier, open **one** [`GuardedRead`] transaction, read
//! its own pinned `last_applied` from inside it, require `A >= B`
//! ([`BarrierFit`]), and then authorize *and* project from that same
//! transaction. The §S4-STATUS-CUT rule is structural here: nothing on the
//! read path consults the actor's memory, and no second transaction is opened.
//! The one "cheap preliminary authorization" §8.2 step 3 allows is subsumed —
//! a read proposes nothing, so there is no log write for a pre-check to save,
//! and the transaction check is both the preliminary and the authoritative
//! one.
//!
//! `group_status` samples its advisory local fields (lifecycle, role, leader
//! belief, quorum evidence, fatal state) strictly **after** the replicated cut
//! and the reply marks them advisory by declaration (`store::wire`).

use std::sync::Arc;

use openraft::Raft;
use tokio::sync::RwLock;

use crate::coordination::contract::{
    payload_fory, ActionV1, CancelMembershipActionV1, ChangeMembershipActionV1, ClientCommandV1,
    ClientReplyV1, CommandReplyV1, CommandV1, CompareAndSetActionV1, LogIdV1, NodeIdV1,
    ReleaseActionV1, ReleaseWithValueActionV1, RetireClientCommandV1, RevokeActionV1,
    SetAdminActionV1, SetGrantActionV1,
};
use crate::coordination::hash;
use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId, ResourceKey};
use crate::coordination::model::apply::{Action, Admission, ClientRef, StatusDenial};
use crate::coordination::model::boundary::action_from_wire;
use crate::coordination::model::limits::RIGHT_READ;
use crate::coordination::model::retire::{self, RetireCommand};
use crate::coordination::model::state::LogId;
use crate::coordination::reason::{Outcome, Reason, RefusalReason};
use crate::coordination::runtime::readiness::Readiness;
use crate::coordination::service::{STORE_FRAME_MAX, STORE_METHODS, STORE_SERVICE, STORE_VERSION};
use crate::coordination::storage::machine::{PreflightAnswer, StateHandle};
use crate::coordination::storage::reads::{BarrierFit, GuardedRead};
use crate::coordination::storage::serve;
use crate::coordination::store::methods::{self, Class};
use crate::coordination::store::wire::{self, MutationPrefix, WireInvalid};
use crate::coordination::typeconfig::CoordinationTypeConfig;
use crate::rpc::{Call, RpcStatus, ServiceDispatch, StatusCode};

/// The runtime a live Store service serves, attached after the `Raft` exists
/// and detached before it shuts down — the same discipline as `RaftEndpoint`,
/// plus the §11.5 drain: detach refuses new calls, and shutdown then waits a
/// bounded five seconds for the calls already past the gate.
#[derive(Clone, Default)]
pub(crate) struct StoreEndpoint {
    inner: Arc<RwLock<Option<AttachedStore>>>,
    /// Calls currently inside a handler. Incremented only after attach was
    /// observed, so a refused call never counts.
    in_flight: Arc<std::sync::atomic::AtomicU64>,
}

/// Everything a live Store handler needs.
#[derive(Clone)]
pub(crate) struct AttachedStore {
    pub(crate) raft: Raft<CoordinationTypeConfig>,
    pub(crate) state: StateHandle,
    pub(crate) group: CoordinationGroupId,
    pub(crate) local: CoordinationNodeId,
    pub(crate) readiness: Readiness,
    /// The bounded operational counters (§14).
    pub(crate) metrics: Arc<StoreMetrics>,
    /// The §11.5 tiers, maintained by the runtime's pressure monitor.
    pub(crate) pressure: Arc<PressureState>,
    /// The §12.2 per-actor admission state.
    pub(crate) buckets: Arc<Buckets>,
}

/// Bounded Store counters (§14): what the service did, never what it carried.
///
/// Atomics, monotonic, sampled by [`crate::coordination::Coordination`]'s
/// public accessor. No value, resource key or actor ever enters a metric.
#[derive(Debug, Default)]
pub struct StoreMetrics {
    /// Mutations admitted and proposed through Raft.
    pub proposed: std::sync::atomic::AtomicU64,
    /// Requests answered from the session table (§8.2.1 stage 2).
    pub replayed: std::sync::atomic::AtomicU64,
    /// Typed reason-coded denials (stages 2–3).
    pub denied_typed: std::sync::atomic::AtomicU64,
    /// Status-only denials (stages 0 and 4), including the pressure gate.
    pub denied_status: std::sync::atomic::AtomicU64,
    /// Follower answers carrying the §5.4 redirect hint.
    pub redirected: std::sync::atomic::AtomicU64,
    /// Linearizable read/status/stream calls served.
    pub reads_served: std::sync::atomic::AtomicU64,
    /// Mutations refused by the §11.5 growth pressure gate.
    pub pressure_refusals: std::sync::atomic::AtomicU64,
}

impl StoreMetrics {
    pub(crate) fn bump(counter: &std::sync::atomic::AtomicU64) {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The §11.5 tiers, maintained by the runtime's pressure monitor and read on
/// the admission path. Host-local; never consulted by apply.
#[derive(Debug, Default)]
pub(crate) struct PressureState {
    /// Tier one: the state database is at its high-water mark. Growth-class
    /// mutations and new-session creation are refused; reads and
    /// safety-reducing operations continue.
    pub(crate) growth: std::sync::atomic::AtomicBool,
    /// Tier two: free space is below the larger of 1 GiB or twice the latest
    /// snapshot. **All** new Store mutations are refused and the runtime is
    /// marked draining so operators (and `wait_ready` callers) see it.
    pub(crate) hard: std::sync::atomic::AtomicBool,
    /// Test-seam overrides, OR'd into the tiers. Set only by the
    /// `coordination-testing` seam; the monitor never touches them, so a
    /// forced tier survives the next sampling pass.
    pub(crate) force_growth: std::sync::atomic::AtomicBool,
    pub(crate) force_hard: std::sync::atomic::AtomicBool,
}

impl PressureState {
    fn growth_hit(&self) -> bool {
        self.growth.load(std::sync::atomic::Ordering::Relaxed)
            || self.force_growth.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn hard_hit(&self) -> bool {
        self.hard.load(std::sync::atomic::Ordering::Relaxed)
            || self.force_hard.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// One refill-on-read token bucket (§12.2).
#[derive(Debug, Clone, Copy)]
struct TokenBucket {
    tokens: f64,
    last: std::time::Instant,
}

impl TokenBucket {
    fn new(burst: f64) -> Self {
        Self {
            tokens: burst,
            last: std::time::Instant::now(),
        }
    }

    /// Refill at `rate`/s up to `burst`, then try to take one token.
    fn take(&mut self, rate: f64, burst: f64) -> bool {
        let now = std::time::Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * rate).min(burst);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Whether a take would currently succeed, without consuming.
    fn would_take(&mut self, rate: f64, burst: f64) -> bool {
        let now = std::time::Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * rate).min(burst);
        self.last = now;
        self.tokens >= 1.0
    }
}

/// §12.2's per-actor admission state: in-flight calls, the mutating-request
/// bucket (20/s, burst 40) and the denied-request bucket (10/s, burst 20),
/// bounded to [`MAX_TRACKED_ACTORS`] actors with least-recently-touched
/// eviction so a rotating flood of identities cannot grow the table.
#[derive(Debug)]
struct ActorAdmission {
    in_flight: u32,
    mutate: TokenBucket,
    denied: TokenBucket,
    touched: std::time::Instant,
}

const MAX_TRACKED_ACTORS: usize = 1024;
const MAX_IN_FLIGHT_PER_ACTOR: u32 = 32;
const MAX_IN_FLIGHT_PER_GROUP: u64 = 256;
const MUTATE_RATE: f64 = 20.0;
const MUTATE_BURST: f64 = 40.0;
const DENIED_RATE: f64 = 10.0;
const DENIED_BURST: f64 = 20.0;

#[derive(Debug, Default)]
pub(crate) struct Buckets {
    actors: std::sync::Mutex<std::collections::HashMap<[u8; 32], ActorAdmission>>,
}

impl Buckets {
    fn with<T>(&self, actor: &[u8; 32], run: impl FnOnce(&mut ActorAdmission) -> T) -> T {
        let mut actors = self.actors.lock().expect("the admission table");
        if !actors.contains_key(actor) && actors.len() >= MAX_TRACKED_ACTORS {
            if let Some(oldest) = actors
                .iter()
                .min_by_key(|(_, a)| a.touched)
                .map(|(k, _)| *k)
            {
                actors.remove(&oldest);
            }
        }
        let entry = actors.entry(*actor).or_insert_with(|| ActorAdmission {
            in_flight: 0,
            mutate: TokenBucket::new(MUTATE_BURST),
            denied: TokenBucket::new(DENIED_BURST),
            touched: std::time::Instant::now(),
        });
        entry.touched = std::time::Instant::now();
        run(entry)
    }

    fn enter(&self, actor: &[u8; 32]) -> bool {
        self.with(actor, |a| {
            if a.in_flight >= MAX_IN_FLIGHT_PER_ACTOR {
                false
            } else {
                a.in_flight += 1;
                true
            }
        })
    }

    fn leave(&self, actor: &[u8; 32]) {
        self.with(actor, |a| a.in_flight = a.in_flight.saturating_sub(1));
    }

    fn take_mutate(&self, actor: &[u8; 32]) -> bool {
        self.with(actor, |a| a.mutate.take(MUTATE_RATE, MUTATE_BURST))
    }

    fn note_denied(&self, actor: &[u8; 32]) {
        self.with(actor, |a| {
            let _ = a.denied.take(DENIED_RATE, DENIED_BURST);
        });
    }

    fn denied_exhausted(&self, actor: &[u8; 32]) -> bool {
        self.with(actor, |a| !a.denied.would_take(DENIED_RATE, DENIED_BURST))
    }
}

/// Which decoded mutations the §11.5 growth tier refuses. Both tiers
/// themselves live on [`PressureState`], published by the runtime's
/// [`pressure`](crate::coordination::runtime::pressure) monitor.
///
/// Method identity is not enough: `set_grant` removing rights and `set_admin`
/// disabling a row are the ACL *removals* §11.5 keeps available, while the
/// same methods adding them are growth. The classification reads the decoded
/// action, never state — this is a host-local admission gate, and the
/// replicated byte budget in apply stays the exact authority.
fn is_growth_mutation(action: &ActionV1) -> bool {
    match action {
        ActionV1::Acquire(_) => true,
        ActionV1::CompareAndSet(_) => true,
        ActionV1::ReleaseWithValue(_) => true,
        // A membership change only grows state when it admits somebody new;
        // a cleanup plan (shrink, or reshuffle of already-admitted peers)
        // carries no seeds and must stay possible under pressure.
        ActionV1::ChangeMembership(change) => !change.member_seeds.is_empty(),
        ActionV1::SetGrant(grant) => grant.rights != 0,
        ActionV1::SetAdmin(admin) => admin.enabled,
        // Safety-reducing or size-preserving: release, revoke, cancel.
        ActionV1::Release(_) | ActionV1::Revoke(_) | ActionV1::CancelMembership(_) => false,
        ActionV1::Renew(_) => false,
        ActionV1::Unknown(_) => false,
    }
}

impl StoreEndpoint {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn attach(&self, attached: AttachedStore) {
        *self.inner.write().await = Some(attached);
    }

    pub(crate) async fn detach(&self) {
        *self.inner.write().await = None;
    }

    /// §11.5 step 2: after detach has stopped new mutations, wait up to five
    /// seconds for in-flight Store calls. Bounded and advisory by design — a
    /// call still running afterwards fails against the stopping Raft and its
    /// client resends the exact journaled bytes, which is the ambiguity path
    /// the protocol already covers.
    pub(crate) async fn drain(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.in_flight.load(std::sync::atomic::Ordering::SeqCst) > 0
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn attached(&self) -> Option<AttachedStore> {
        self.inner.read().await.clone()
    }
}

/// Holds the in-flight count for exactly one dispatched call.
struct InFlight(Arc<std::sync::atomic::AtomicU64>);

impl InFlight {
    fn enter(counter: &Arc<std::sync::atomic::AtomicU64>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(Arc::clone(counter))
    }
}

/// The per-actor §12.2 in-flight slot, held for the life of one call.
struct ActorInFlight {
    buckets: Arc<Buckets>,
    actor: [u8; 32],
}

impl ActorInFlight {
    fn enter(runtime: &AttachedStore, actor: [u8; 32]) -> Option<Self> {
        if runtime.buckets.enter(&actor) {
            Some(Self {
                buckets: Arc::clone(&runtime.buckets),
                actor,
            })
        } else {
            None
        }
    }
}

impl Drop for ActorInFlight {
    fn drop(&mut self) {
        self.buckets.leave(&self.actor);
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The status this node answers before its runtime attaches or after it
/// detaches: up, not serving coordination — transient to a client.
fn not_yet_serving() -> RpcStatus {
    RpcStatus::error(
        StatusCode::FailedPrecondition,
        "this node is not yet serving coordination",
    )
}

/// The live service, registered by `builtin_coordination` in place of the
/// reservation.
pub(crate) struct CoordinationStoreService {
    endpoint: StoreEndpoint,
}

impl CoordinationStoreService {
    pub(crate) fn new(endpoint: StoreEndpoint) -> Self {
        Self { endpoint }
    }
}

#[async_trait::async_trait]
impl ServiceDispatch for CoordinationStoreService {
    fn name(&self) -> &str {
        STORE_SERVICE
    }

    fn version(&self) -> i32 {
        STORE_VERSION
    }

    fn methods(&self) -> &[&str] {
        STORE_METHODS
    }

    /// A caller's identity **is** its authorization input, so the service is
    /// unreachable over a transport that cannot authenticate the peer.
    fn iroh_only(&self) -> bool {
        true
    }

    /// §12.3: 2 MiB for every method, including one not in the table, for the
    /// same reason the reservation gives.
    fn max_frame_size(&self, _method: &str) -> Option<u32> {
        Some(STORE_FRAME_MAX)
    }

    async fn dispatch(&self, method: &str, call: Call) {
        // The count is taken **before** the attachment is read, so a drain
        // that observes zero after detach cannot race a call that had already
        // cloned the runtime: any call that will serve is counted first, and
        // a call that finds no attachment drops its count on refusal.
        let _in_flight = InFlight::enter(&self.endpoint.in_flight);
        // §12.2: at most 256 Store calls in flight per group, counted by the
        // same counter drain waits on, checked after our own increment so
        // concurrent entries cannot all read the pre-increment value.
        if self
            .endpoint
            .in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            > MAX_IN_FLIGHT_PER_GROUP
        {
            let _ = call
                .finish(&RpcStatus::error(
                    StatusCode::ResourceExhausted,
                    "the group's in-flight admission ceiling is reached; retry later",
                ))
                .await;
            return;
        }
        let Some(runtime) = self.endpoint.attached().await else {
            let _ = call.finish(&not_yet_serving()).await;
            return;
        };
        serve_call(&runtime, method, call).await;
    }
}

// ── Stage 0: identity ───────────────────────────────────────────────────────

/// The authenticated actor, from the immutable Iroh transport peer and from
/// nothing else. Dispatch enforced `iroh_only` before the handler ran.
fn actor_of(call: &Call) -> Result<[u8; 32], RpcStatus> {
    let peer = CoordinationNodeId::parse_hex(call.peer()).map_err(|_| {
        RpcStatus::error(
            StatusCode::Unauthenticated,
            "the transport peer is not a 32-byte coordination identity",
        )
    })?;
    Ok(*peer.as_bytes())
}

fn invalid(invalid: WireInvalid) -> RpcStatus {
    RpcStatus::error(StatusCode::InvalidArgument, invalid.describe())
}

fn decode_failure() -> RpcStatus {
    RpcStatus::error(
        StatusCode::InvalidArgument,
        "the request does not decode as this method's §5.2 request type",
    )
}

fn wrong_group() -> RpcStatus {
    RpcStatus::error(
        StatusCode::InvalidArgument,
        "the request names a different coordination group",
    )
}

fn internal(what: &str) -> RpcStatus {
    RpcStatus::error(StatusCode::Internal, what.to_string())
}

/// Decode one unary request frame as `T`, or say why not (stage 0).
fn decode<T>(frame: &[u8]) -> Result<T, RpcStatus>
where
    T: crate::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
{
    payload_fory::<T>()
        .deserialize(frame)
        .map_err(|_| decode_failure())
}

// ── Stage 1: leader routing ─────────────────────────────────────────────────

/// The `UNAVAILABLE` a follower answers, with the §5.4 structured hint.
///
/// The hint names the last known leader and the term it was observed at — both
/// admitted-peer operational metadata. No resource existence, holder, ACL or
/// value ever rides in a status.
fn not_leader(runtime: &AttachedStore) -> RpcStatus {
    StoreMetrics::bump(&runtime.metrics.redirected);
    let metrics = runtime.raft.metrics().borrow().clone();
    let mut status = RpcStatus::error(
        StatusCode::Unavailable,
        "this node is not the coordination leader",
    );
    if let Some(leader) = metrics.current_leader {
        status
            .detail_keys
            .push(wire::DETAIL_LEADER_NODE_ID.to_string());
        status.detail_values.push(leader.to_hex());
        status
            .detail_keys
            .push(wire::DETAIL_LEADER_TERM.to_string());
        status
            .detail_values
            .push(metrics.vote.leader_id().get_term().to_string());
    }
    status
}

/// Take the linearizable barrier (stage 2's entry). A refusal is §5.4's
/// `UNAVAILABLE`: either this node is not the leader (redirect hint) or it
/// cannot currently prove a quorum.
async fn barrier(runtime: &AttachedStore) -> Result<Option<LogId>, RpcStatus> {
    use openraft::error::CheckIsLeaderError;
    match runtime.raft.ensure_linearizable().await {
        Ok(read_log_id) => Ok(read_log_id.map(|id| LogId::from(&id))),
        Err(err) => match err.api_error() {
            Some(CheckIsLeaderError::ForwardToLeader(_)) => Err(not_leader(runtime)),
            Some(CheckIsLeaderError::QuorumNotEnough(_)) => Err(RpcStatus::error(
                StatusCode::Unavailable,
                "the leader cannot currently reach a quorum",
            )),
            None => Err(RpcStatus::error(
                StatusCode::Unavailable,
                "the coordination runtime is shutting down",
            )),
        },
    }
}

/// Prove `applied` is at or past `barrier` — the §8.2.1 mutation witness.
///
/// Same three-way answer as [`PinnedVersion::against`], for the same reason:
/// behind is a retryable ordering surprise, but the same index under a
/// different log id is divergent history and must not be retried around.
fn witness(applied: Option<&LogId>, barrier: Option<&LogId>) -> BarrierFit {
    let Some(barrier) = barrier else {
        return BarrierFit::Satisfied;
    };
    let Some(applied) = applied else {
        return BarrierFit::Behind;
    };
    match applied.index.cmp(&barrier.index) {
        std::cmp::Ordering::Less => BarrierFit::Behind,
        std::cmp::Ordering::Greater => BarrierFit::Satisfied,
        std::cmp::Ordering::Equal if applied == barrier => BarrierFit::Satisfied,
        std::cmp::Ordering::Equal => BarrierFit::Diverged,
    }
}

fn diverged() -> RpcStatus {
    RpcStatus::error(
        StatusCode::DataLoss,
        "this voter's applied history disagrees with the leader barrier at an equal index",
    )
}

fn behind() -> RpcStatus {
    // `ensure_linearizable` waits for the state machine to reach the read log
    // id, so this is an internal ordering surprise, not an expected state.
    RpcStatus::error(
        StatusCode::Unavailable,
        "the applied state fell behind the linearizable barrier; retry",
    )
}

// ── The router ──────────────────────────────────────────────────────────────

async fn serve_call(runtime: &AttachedStore, method: &str, mut call: Call) {
    // One request frame for every method: the two streams are
    // server-streaming, not client-streaming.
    let Some(frame) = call.recv_request().await else {
        let _ = call
            .finish(&RpcStatus::error(
                StatusCode::InvalidArgument,
                "a Store call carries exactly one request frame",
            ))
            .await;
        return;
    };

    let actor = match actor_of(&call) {
        Ok(actor) => actor,
        Err(status) => {
            let _ = call.finish(&status).await;
            return;
        }
    };

    // The single declaration routes; an unknown method cannot reach here
    // because dispatch checked `methods()`, but it is answered rather than
    // panicked for the same reason the Raft service answers it.
    let Some(spec) = methods::spec(method) else {
        let _ = call
            .finish(&RpcStatus::error(
                StatusCode::Unimplemented,
                format!("{STORE_SERVICE} v1 has no method '{method}'"),
            ))
            .await;
        return;
    };

    // §12.2, before any barrier or storage work: at most 32 calls in flight
    // per actor, and an actor whose denied-request bucket is dry has its
    // *reads* short-circuited here. Mutations are exempt from the
    // short-circuit on purpose — a mutation must always reach session
    // resolution so a saved result can replay; its own bucket sits on the
    // propose path instead.
    let Some(_actor_slot) = ActorInFlight::enter(runtime, actor) else {
        let _ = call
            .finish(&RpcStatus::error(
                StatusCode::ResourceExhausted,
                "the actor's in-flight admission ceiling is reached; retry later",
            ))
            .await;
        return;
    };
    let mutating = matches!(
        spec.class,
        Class::SequencedMutation | Class::SessionlessMutation
    );
    if !mutating && runtime.buckets.denied_exhausted(&actor) {
        let _ = call
            .finish(&RpcStatus::error(
                StatusCode::ResourceExhausted,
                "the actor's denied-request budget is exhausted; retry later",
            ))
            .await;
        return;
    }

    match (spec.class, method) {
        (Class::SequencedMutation, _) => mutation(runtime, method, actor, &frame, call).await,
        (Class::SessionlessMutation, _) => retire_client(runtime, actor, &frame, call).await,
        (Class::LinearizableRead, "read_linearizable") => {
            read_linearizable(runtime, actor, &frame, call).await
        }
        (Class::LinearizableRead, "inspect_acl") => inspect_acl(runtime, actor, &frame, call).await,
        (Class::LinearizableRead, "inspect_admins") => {
            inspect_admins(runtime, actor, &frame, call).await
        }
        (Class::LinearizableRead, "membership_status") => {
            membership_status(runtime, actor, &frame, call).await
        }
        (Class::LinearizableRead, "probe_client") => {
            probe_client(runtime, actor, &frame, call).await
        }
        (Class::StatusCut, _) => group_status(runtime, actor, &frame, call).await,
        (Class::Stream, "audit_stream") => audit_stream(runtime, actor, &frame, call).await,
        (Class::Stream, _) => recovery_export(runtime, actor, &frame, call).await,
        (class, other) => {
            let _ = call
                .finish(&RpcStatus::error(
                    StatusCode::Internal,
                    format!("method '{other}' is classified {class:?} but has no handler"),
                ))
                .await;
        }
    }
}

// ── Mutations (stage 0 decode per method, then one shared path) ─────────────

/// A decoded sequenced mutation: the §5.2 prefix and the action it asks for.
struct DecodedMutation {
    prefix: MutationPrefix,
    /// The exact wire action — what is hashed, proposed and journaled.
    wire_action: ActionV1,
}

/// Decode one of the ten sequenced mutation requests (stage 0).
///
/// Only the *prefix* failures are stage 0. A request whose prefix narrows has
/// a mutation identity; anything wrong with the action body is discovered by
/// the shared preflight after session resolution, exactly as §10 orders it.
fn decode_mutation(method: &str, frame: &[u8]) -> Result<DecodedMutation, RpcStatus> {
    macro_rules! narrow {
        ($ty:ty, $build:expr) => {{
            let request: $ty = decode(frame)?;
            let prefix = wire::prefix(
                request.protocol_version,
                &request.group_id,
                &request.client_id,
                request.sequence,
            )
            .map_err(invalid)?;
            #[allow(clippy::redundant_closure_call)]
            let wire_action: ActionV1 = ($build)(request);
            Ok(DecodedMutation {
                prefix,
                wire_action,
            })
        }};
    }

    match method {
        "acquire" => narrow!(wire::AcquireRequestV1, |r: wire::AcquireRequestV1| {
            ActionV1::Acquire(crate::coordination::contract::AcquireActionV1 {
                resource: r.resource,
                ttl_ms: r.ttl_ms,
            })
        }),
        "renew" => narrow!(wire::RenewRequestV1, |r: wire::RenewRequestV1| {
            ActionV1::Renew(crate::coordination::contract::RenewActionV1 { fence: r.fence })
        }),
        "compare_and_set" => narrow!(
            wire::CompareAndSetRequestV1,
            |r: wire::CompareAndSetRequestV1| {
                ActionV1::CompareAndSet(CompareAndSetActionV1 {
                    fence: r.fence,
                    expected_value_revision: r.expected_value_revision,
                    new_value: r.new_value,
                })
            }
        ),
        "release" => narrow!(wire::ReleaseRequestV1, |r: wire::ReleaseRequestV1| {
            ActionV1::Release(ReleaseActionV1 { fence: r.fence })
        }),
        "release_with_value" => narrow!(
            wire::ReleaseWithValueRequestV1,
            |r: wire::ReleaseWithValueRequestV1| {
                ActionV1::ReleaseWithValue(ReleaseWithValueActionV1 {
                    fence: r.fence,
                    expected_value_revision: r.expected_value_revision,
                    final_value: r.final_value,
                })
            }
        ),
        "revoke" => narrow!(wire::RevokeRequestV1, |r: wire::RevokeRequestV1| {
            ActionV1::Revoke(RevokeActionV1 {
                resource: r.resource,
                expected_epoch: r.expected_epoch,
            })
        }),
        "set_grant" => narrow!(wire::SetGrantRequestV1, |r: wire::SetGrantRequestV1| {
            ActionV1::SetGrant(SetGrantActionV1 {
                resource: r.resource,
                target_node_id: r.target_node_id,
                rights: r.rights,
                expected_acl_revision: r.expected_acl_revision,
            })
        }),
        "set_admin" => narrow!(wire::SetAdminRequestV1, |r: wire::SetAdminRequestV1| {
            ActionV1::SetAdmin(SetAdminActionV1 {
                target_node_id: r.target_node_id,
                enabled: r.enabled,
                expected_admin_revision: r.expected_admin_revision,
            })
        }),
        "change_membership" => narrow!(
            wire::ChangeMembershipRequestV1,
            |r: wire::ChangeMembershipRequestV1| {
                ActionV1::ChangeMembership(ChangeMembershipActionV1 {
                    operation_id: r.operation_id,
                    expected_membership_log_id: r.expected_membership_log_id,
                    desired_voters: r.desired_voters,
                    retained_learners: r.retained_learners,
                    member_seeds: r.member_seeds,
                })
            }
        ),
        "cancel_membership" => narrow!(
            wire::CancelMembershipRequestV1,
            |r: wire::CancelMembershipRequestV1| {
                ActionV1::CancelMembership(CancelMembershipActionV1 {
                    operation_id: r.operation_id,
                    expected_phase_code: r.expected_phase_code,
                })
            }
        ),
        other => Err(RpcStatus::error(
            StatusCode::Internal,
            format!("'{other}' is not a sequenced mutation"),
        )),
    }
}

/// The one sequenced-mutation path (stages 1–5). No method-specific gate
/// reordering: every method decoded above goes through exactly this.
async fn mutation(
    runtime: &AttachedStore,
    method: &str,
    actor: [u8; 32],
    frame: &[u8],
    call: Call,
) {
    let decoded = match decode_mutation(method, frame) {
        Ok(decoded) => decoded,
        Err(status) => {
            let _ = call.finish(&status).await;
            return;
        }
    };
    if decoded.prefix.group_id != *runtime.group.as_bytes() {
        let _ = call.finish(&wrong_group()).await;
        return;
    }

    // The canonical hash, derived from the decoded action's canonical bytes —
    // never taken from the request.
    let request_hash = match hash::request_hash(&decoded.wire_action) {
        Ok(hash) => hash,
        Err(_) => {
            let _ = call.finish(&internal("the action does not hash")).await;
            return;
        }
    };

    // Stage 1.
    let barrier_id = match barrier(runtime).await {
        Ok(id) => id,
        Err(status) => {
            let _ = call.finish(&status).await;
            return;
        }
    };

    // Stage 2 + 3: session resolution first, then admission, on the state
    // apply runs against. A `None` action is a body that failed narrowing —
    // §10 stage 4, but only after the classifier had its chance.
    let narrowed: Option<Action> = action_from_wire(&decoded.wire_action).ok();
    let client = ClientRef {
        actor,
        client_id: decoded.prefix.client_id,
        sequence: decoded.prefix.sequence,
        request_hash,
    };
    let (answer, applied, creates_session) = match runtime.state.preflight(client, narrowed).await {
        Ok(result) => result,
        Err(_) => {
            let _ = call
                .finish(&internal("the state machine is not serviceable"))
                .await;
            return;
        }
    };
    match witness(applied.as_ref(), barrier_id.as_ref()) {
        BarrierFit::Satisfied => {}
        BarrierFit::Behind => {
            let _ = call.finish(&behind()).await;
            return;
        }
        BarrierFit::Diverged => {
            runtime.state.mark_fatal();
            let _ = call.finish(&diverged()).await;
            return;
        }
    }

    let reply = |client_reply: ClientReplyV1| {
        mutation_reply(runtime, &decoded.prefix, &request_hash, client_reply)
    };

    let admission = match answer {
        PreflightAnswer::Admission(admission) => admission,
        // §10 stage 4 for a shapeless action whose sequence the classifier
        // left unresolved: `INVALID_ARGUMENT`, session state untouched.
        PreflightAnswer::ShapelessUnresolved => {
            let _ = call.finish(&decode_failure()).await;
            return;
        }
    };

    match admission {
        // Stage 2: every resolved row — replays and refusals, terminal or not.
        Admission::Resolved(result) => {
            if result.replayed {
                StoreMetrics::bump(&runtime.metrics.replayed);
            } else {
                StoreMetrics::bump(&runtime.metrics.denied_typed);
            }
            let provenance = if result.replayed {
                crate::coordination::model::project::Provenance::Replay
            } else {
                crate::coordination::model::project::Provenance::FirstAttempt
            };
            let projected = crate::coordination::model::project::client_reply(
                runtime.group.as_bytes(),
                &result.reply,
                result.disposition,
                provenance,
                None,
            );
            respond_reply(call, reply(projected)).await;
        }
        // Stage 3: a reason-coded admission denial. Typed, NotConsumed,
        // nothing written.
        Admission::Deny(refusal) => {
            StoreMetrics::bump(&runtime.metrics.denied_typed);
            let projected = ClientReplyV1 {
                disposition_code: 2,
                outcome_code: Outcome::Rejected.code(),
                reason_code: Reason::from(refusal).code(),
                snapshot: None,
                acl_revision: 0,
                admin_revision: 0,
                operation_id: None,
                membership_log_id: None,
            };
            respond_reply(call, reply(projected)).await;
        }
        // Stage 4: no §5.2 reason code exists, so an `RpcStatus`.
        Admission::Status(denial) => {
            StoreMetrics::bump(&runtime.metrics.denied_status);
            let status = match denial {
                StatusDenial::NotAuthorized => RpcStatus::error(
                    StatusCode::PermissionDenied,
                    "the actor lacks the required committed permission",
                ),
                StatusDenial::Shape(limit) => {
                    RpcStatus::error(StatusCode::InvalidArgument, limit.describe())
                }
            };
            deny(runtime, &actor, call, status).await;
        }
        // Stage 4 → 5: propose the leader-stamped command and project the
        // exact applied reply. The §11.5 growth pressure gate sits here —
        // after session resolution, with the other host-local stage-4
        // denials — so a saved result is never stranded behind disk pressure.
        Admission::Admit => {
            // §11.5 tier two: below the free-space floor, *every* new
            // mutation is refused — applying anything writes.
            if runtime.pressure.hard_hit() {
                StoreMetrics::bump(&runtime.metrics.pressure_refusals);
                StoreMetrics::bump(&runtime.metrics.denied_status);
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::ResourceExhausted,
                        "free disk space is below the §11.5 floor; this node is draining \
                         and refuses all new mutations",
                    ))
                    .await;
                return;
            }
            // §11.5 tier one: at the state-database high-water mark, refuse
            // what grows the state — growth-class actions and the creation
            // of a new session row — while safety-reducing operations from
            // known sessions continue.
            if runtime.pressure.growth_hit()
                && (is_growth_mutation(&decoded.wire_action) || creates_session)
            {
                StoreMetrics::bump(&runtime.metrics.pressure_refusals);
                StoreMetrics::bump(&runtime.metrics.denied_status);
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::ResourceExhausted,
                        "the state database is at its §11.5 high-water mark; growth-class \
                         mutations are refused while safety-reducing operations continue",
                    ))
                    .await;
                return;
            }
            // §12.2: the mutating-request bucket — after session resolution
            // so replays of saved results are never throttled, before any
            // propose.
            if !runtime.buckets.take_mutate(&actor) {
                StoreMetrics::bump(&runtime.metrics.denied_status);
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::ResourceExhausted,
                        "the actor's mutating-request budget is exhausted; retry later",
                    ))
                    .await;
                return;
            }
            StoreMetrics::bump(&runtime.metrics.proposed);
            let command = CommandV1::Client(ClientCommandV1 {
                actor_node_id: actor.to_vec(),
                client_id: decoded.prefix.client_id.to_vec(),
                sequence: decoded.prefix.sequence,
                request_hash: request_hash.to_vec(),
                action: decoded.wire_action,
            });
            propose_and_reply(runtime, command, call, |reply_v1| match reply_v1 {
                CommandReplyV1::Client(client_reply) => Ok(reply(client_reply)),
                other => Err(internal(&format!(
                    "a client command must answer with a Client reply, got {other:?}"
                ))),
            })
            .await;
        }
    }
}

/// Wrap a projected `ClientReplyV1` in the public `MutationReplyWireV1`.
fn mutation_reply(
    runtime: &AttachedStore,
    prefix: &MutationPrefix,
    request_hash: &[u8; 32],
    reply: ClientReplyV1,
) -> wire::MutationReplyWireV1 {
    wire::MutationReplyWireV1 {
        protocol_version: wire::STORE_PROTOCOL_VERSION,
        group_id: runtime.group.as_bytes().to_vec(),
        client_id: prefix.client_id.to_vec(),
        sequence: prefix.sequence,
        request_hash: request_hash.to_vec(),
        disposition_code: reply.disposition_code,
        outcome_code: reply.outcome_code,
        reason_code: reply.reason_code,
        snapshot: reply.snapshot,
        acl_revision: reply.acl_revision,
        admin_revision: reply.admin_revision,
        operation_id: reply.operation_id,
        membership_log_id: reply.membership_log_id,
    }
}

/// Propose one command through `Raft::client_write` and answer from the state
/// machine's own reply — never a synthesized acknowledgement.
async fn propose_and_reply<T, F>(
    runtime: &AttachedStore,
    command: CommandV1,
    call: Call,
    project: F,
) where
    T: crate::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
    F: FnOnce(CommandReplyV1) -> Result<T, RpcStatus>,
{
    use openraft::error::ClientWriteError;
    match runtime.raft.client_write(command).await {
        Ok(response) => match project(response.data) {
            Ok(reply) => respond_reply(call, reply).await,
            Err(status) => {
                let _ = call.finish(&status).await;
            }
        },
        Err(err) => {
            let status = match err.api_error() {
                Some(ClientWriteError::ForwardToLeader(_)) => not_leader(runtime),
                Some(ClientWriteError::ChangeMembershipError(_)) => RpcStatus::error(
                    StatusCode::FailedPrecondition,
                    "the proposal was refused by consensus membership rules",
                ),
                None => RpcStatus::error(
                    StatusCode::Unavailable,
                    "the proposal did not commit; the coordination runtime is unavailable",
                ),
            };
            let _ = call.finish(&status).await;
        }
    }
}

/// Encode and send one typed unary reply.
async fn respond_reply<T>(call: Call, reply: T)
where
    T: crate::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
{
    match payload_fory::<T>().serialize(&reply) {
        Ok(bytes) => {
            let _ = call.respond(bytes, &RpcStatus::ok()).await;
        }
        Err(_) => {
            let _ = call
                .finish(&internal(
                    "the reply does not encode; this is a server fault",
                ))
                .await;
        }
    }
}

// ── retire_client (sessionless, §5.2/§10) ───────────────────────────────────

async fn retire_client(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::RetireClientRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => {
            let _ = call.finish(&status).await;
            return;
        }
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        let _ = call.finish(&invalid(WireInvalid::ProtocolVersion)).await;
        return;
    }
    if request.group_id != runtime.group.as_bytes() {
        let _ = call.finish(&wrong_group()).await;
        return;
    }
    let target = match wire::node_id(&request.target_node_id, "target_node_id") {
        Ok(target) => target,
        Err(err) => {
            let _ = call.finish(&invalid(err)).await;
            return;
        }
    };
    let target_client = match wire::id16(&request.target_client_id, "target_client_id") {
        Ok(id) => id,
        Err(err) => {
            let _ = call.finish(&invalid(err)).await;
            return;
        }
    };

    let barrier_id = match barrier(runtime).await {
        Ok(id) => id,
        Err(status) => {
            let _ = call.finish(&status).await;
            return;
        }
    };
    let command = RetireCommand {
        actor,
        target_actor: target,
        target_client_id: target_client,
    };
    let (decision, applied) = match runtime.state.retire_preflight(command).await {
        Ok(result) => result,
        Err(_) => {
            let _ = call
                .finish(&internal("the state machine is not serviceable"))
                .await;
            return;
        }
    };
    match witness(applied.as_ref(), barrier_id.as_ref()) {
        BarrierFit::Satisfied => {}
        BarrierFit::Behind => {
            let _ = call.finish(&behind()).await;
            return;
        }
        BarrierFit::Diverged => {
            runtime.state.mark_fatal();
            let _ = call.finish(&diverged()).await;
            return;
        }
    }

    match decision {
        // Authorization has no §5.2 reason code pre-proposal: a status.
        retire::Decision::Refuse(RefusalReason::PermissionChanged) => {
            deny(
                runtime,
                &actor,
                call,
                RpcStatus::error(
                    StatusCode::PermissionDenied,
                    "only the session's own actor or a group administrator may retire it",
                ),
            )
            .await;
        }
        // The reason-coded refusal — the terminal 18 — is typed, like stage 3.
        retire::Decision::Refuse(refusal) => {
            let reply = wire::RetireClientReplyWireV1 {
                protocol_version: wire::STORE_PROTOCOL_VERSION,
                group_id: runtime.group.as_bytes().to_vec(),
                outcome_code: Outcome::Rejected.code(),
                reason_code: Reason::from(refusal).code(),
            };
            respond_reply(call, reply).await;
        }
        retire::Decision::Apply(_) => {
            // §11.5 tier two refuses even retirement — it writes. Tier one
            // does not: retirement shrinks state and §11.5 lists it among
            // the operations that continue at the high-water mark.
            if runtime.pressure.hard_hit() {
                StoreMetrics::bump(&runtime.metrics.pressure_refusals);
                StoreMetrics::bump(&runtime.metrics.denied_status);
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::ResourceExhausted,
                        "free disk space is below the §11.5 floor; this node is draining \
                         and refuses all new mutations",
                    ))
                    .await;
                return;
            }
            if !runtime.buckets.take_mutate(&actor) {
                StoreMetrics::bump(&runtime.metrics.denied_status);
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::ResourceExhausted,
                        "the actor's mutating-request budget is exhausted; retry later",
                    ))
                    .await;
                return;
            }
            let group = runtime.group.as_bytes().to_vec();
            let command = CommandV1::RetireClient(RetireClientCommandV1 {
                actor_node_id: actor.to_vec(),
                target_node_id: target.to_vec(),
                target_client_id: target_client.to_vec(),
            });
            propose_and_reply(runtime, command, call, move |reply_v1| match reply_v1 {
                CommandReplyV1::Retirement(retirement) => Ok(wire::RetireClientReplyWireV1 {
                    protocol_version: wire::STORE_PROTOCOL_VERSION,
                    group_id: group,
                    outcome_code: retirement.outcome_code,
                    reason_code: retirement.reason_code,
                }),
                other => Err(internal(&format!(
                    "a retirement must answer with a Retirement reply, got {other:?}"
                ))),
            })
            .await;
        }
    }
}

// ── The read-path spine (barrier → one guarded transaction → witness) ───────

/// Everything a read handler projects from: the one transaction and the meta
/// decoded from it. Constructing it *is* stages 1–2 of the read column.
struct ReadCut {
    guarded: GuardedRead,
    meta: crate::coordination::raft::CoordinationMetaV1,
    /// The transaction's own pinned applied log id — the `A >= B` witness.
    pinned: Option<LogId>,
}

async fn read_cut(runtime: &AttachedStore) -> Result<ReadCut, RpcStatus> {
    StoreMetrics::bump(&runtime.metrics.reads_served);
    let barrier_id = barrier(runtime).await?;
    let guarded = runtime
        .state
        .read_transaction()
        .await
        .map_err(|_| internal("the state machine is not serviceable"))?;
    let pinned = guarded
        .pinned()
        .map_err(|_| internal("the pinned version does not decode"))?;
    match pinned.against(barrier_id.as_ref()) {
        BarrierFit::Satisfied => {}
        BarrierFit::Behind => return Err(behind()),
        BarrierFit::Diverged => {
            runtime.state.mark_fatal();
            return Err(diverged());
        }
    }
    let meta = serve::meta(&guarded).map_err(|_| internal("the state meta does not decode"))?;
    let pinned = pinned.log_id().copied();
    Ok(ReadCut {
        guarded,
        meta,
        pinned,
    })
}

/// Authorize `actor` as a group administrator **from this transaction**.
fn require_administrator(cut: &ReadCut, actor: &[u8; 32]) -> Result<(), RpcStatus> {
    let admitted = serve::is_administrator(&cut.guarded, &cut.meta, actor)
        .map_err(|_| internal("the administrator set does not decode"))?;
    if admitted {
        Ok(())
    } else {
        Err(RpcStatus::error(
            StatusCode::PermissionDenied,
            "this method requires the group-administrator predicate",
        ))
    }
}

fn check_group(cut_group: &[u8], request_group: &[u8]) -> Result<(), RpcStatus> {
    if cut_group == request_group {
        Ok(())
    } else {
        Err(wrong_group())
    }
}

fn narrow_resource(
    resource: &crate::coordination::contract::ResourceKeyV1,
) -> Result<ResourceKey, RpcStatus> {
    wire::resource_bounds(resource).map_err(invalid)?;
    ResourceKey::new(&resource.namespace, resource.key.clone()).map_err(|_| {
        RpcStatus::error(
            StatusCode::InvalidArgument,
            "the resource key is not a valid §4.2 identity",
        )
    })
}

// ── read_linearizable ───────────────────────────────────────────────────────

async fn read_linearizable(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::ReadRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => return fail(call, status).await,
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        return fail(call, invalid(WireInvalid::ProtocolVersion)).await;
    }
    let key = match narrow_resource(&request.resource) {
        Ok(key) => key,
        Err(status) => return fail(call, status).await,
    };
    if let Err(status) = check_group(runtime.group.as_bytes(), &request.group_id) {
        return deny(runtime, &actor, call, status).await;
    }
    let cut = match read_cut(runtime).await {
        Ok(cut) => cut,
        Err(status) => return fail(call, status).await,
    };

    // Authorize at the pinned version: the exact §5.2 permission row —
    // resource `READ`, with no administrator bypass. Per-resource rights
    // have no published pre-barrier view, so this stays in-transaction.
    let rights = match serve::rights_of(&cut.guarded, &key, &actor) {
        Ok(rights) => rights.unwrap_or(0),
        Err(_) => return fail(call, internal("the ACL does not decode")).await,
    };
    if rights & RIGHT_READ == 0 {
        return deny(
            runtime,
            &actor,
            call,
            RpcStatus::error(
                StatusCode::PermissionDenied,
                "the actor holds no committed READ grant on this resource",
            ),
        )
        .await;
    }

    // Project from the same transaction. An absent row is the register's
    // never-written value, not an error: epoch 0, unheld, empty value.
    let row = match serve::resource(&cut.guarded, &key) {
        Ok(row) => row.unwrap_or_default(),
        Err(_) => return fail(call, internal("the resource row does not decode")).await,
    };
    let reply = wire::ReadReplyWireV1 {
        protocol_version: wire::STORE_PROTOCOL_VERSION,
        group_id: cut.meta.group_id.clone(),
        snapshot: crate::coordination::contract::ResourceSnapshotV1 {
            group_id: cut.meta.group_id.clone(),
            resource: request.resource,
            epoch: row.epoch,
            holder: row.holder.map(|h| h.to_vec()),
            held: row.held,
            ttl_ms: row.ttl_ms,
            lease_revision: row.lease_revision,
            value_revision: row.value_revision,
            acl_revision: row.acl_revision,
            value: Some(row.value),
            value_redacted: false,
        },
    };
    respond_reply(call, reply).await;
}

async fn fail(call: Call, status: RpcStatus) {
    let _ = call.finish(&status).await;
}

/// Finish with a denial the caller earned — permission or argument — and
/// charge the §12.2 denied-request bucket so a sustained stream of them is
/// eventually short-circuited at entry before any barrier work.
async fn deny(runtime: &AttachedStore, actor: &[u8; 32], call: Call, status: RpcStatus) {
    runtime.buckets.note_denied(actor);
    let _ = call.finish(&status).await;
}

/// The cheap pre-barrier administrator gate (§12.2 ordering): consult the
/// state actor's published [`AdminView`] before paying for a quorum barrier
/// or a storage transaction.
///
/// It only *denies* on the node that currently believes it is the leader,
/// where the view is republished by the same apply loop the authoritative
/// transaction reads. On any other node it passes, so a follower still
/// answers with stage 1's redirect rather than a possibly-stale denial. The
/// in-transaction [`require_administrator`] remains the decision of record.
fn cheap_administrator_gate(runtime: &AttachedStore, actor: &[u8; 32]) -> Result<(), RpcStatus> {
    if runtime.raft.metrics().borrow().current_leader != Some(runtime.local) {
        return Ok(());
    }
    if runtime.state.admin_view().is_administrator(actor) {
        Ok(())
    } else {
        Err(RpcStatus::error(
            StatusCode::PermissionDenied,
            "this method requires the group-administrator predicate",
        ))
    }
}

// ── inspect_acl / inspect_admins ────────────────────────────────────────────

async fn inspect_acl(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::AclStatusRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => return fail(call, status).await,
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        return fail(call, invalid(WireInvalid::ProtocolVersion)).await;
    }
    let key = match narrow_resource(&request.resource) {
        Ok(key) => key,
        Err(status) => return fail(call, status).await,
    };
    if let Err(status) = check_group(runtime.group.as_bytes(), &request.group_id) {
        return deny(runtime, &actor, call, status).await;
    }
    if let Err(status) = cheap_administrator_gate(runtime, &actor) {
        return deny(runtime, &actor, call, status).await;
    }
    let cut = match read_cut(runtime).await {
        Ok(cut) => cut,
        Err(status) => return fail(call, status).await,
    };
    if let Err(status) = require_administrator(&cut, &actor) {
        return deny(runtime, &actor, call, status).await;
    }

    let grants = match serve::grants_of(&cut.guarded, &key) {
        Ok(grants) => grants,
        Err(_) => return fail(call, internal("the ACL does not decode")).await,
    };
    let acl_revision = match serve::resource(&cut.guarded, &key) {
        Ok(row) => row.map_or(0, |r| r.acl_revision),
        Err(_) => return fail(call, internal("the resource row does not decode")).await,
    };
    let reply = wire::AclStatusReplyWireV1 {
        protocol_version: wire::STORE_PROTOCOL_VERSION,
        group_id: cut.meta.group_id.clone(),
        acl_snapshot: wire::AclSnapshotWireV1 {
            resource: request.resource,
            acl_revision,
            grants: grants
                .into_iter()
                .map(|(subject, rights)| wire::AclGrantWireV1 {
                    subject_node_id: subject.to_vec(),
                    rights,
                })
                .collect(),
        },
    };
    respond_reply(call, reply).await;
}

async fn inspect_admins(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::AdminStatusRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => return fail(call, status).await,
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        return fail(call, invalid(WireInvalid::ProtocolVersion)).await;
    }
    if let Err(status) = check_group(runtime.group.as_bytes(), &request.group_id) {
        return deny(runtime, &actor, call, status).await;
    }
    if let Err(status) = cheap_administrator_gate(runtime, &actor) {
        return deny(runtime, &actor, call, status).await;
    }
    let cut = match read_cut(runtime).await {
        Ok(cut) => cut,
        Err(status) => return fail(call, status).await,
    };
    if let Err(status) = require_administrator(&cut, &actor) {
        return deny(runtime, &actor, call, status).await;
    }
    let administrators = match serve::administrators(&cut.guarded) {
        Ok(admins) => admins,
        Err(_) => return fail(call, internal("the administrator set does not decode")).await,
    };
    let reply = wire::AdminStatusReplyWireV1 {
        protocol_version: wire::STORE_PROTOCOL_VERSION,
        group_id: cut.meta.group_id.clone(),
        admin_snapshot: wire::AdminSnapshotWireV1 {
            admin_revision: cut.meta.admin_revision,
            administrators: administrators
                .into_iter()
                .map(|node_id| NodeIdV1 {
                    node_id: node_id.to_vec(),
                })
                .collect(),
        },
    };
    respond_reply(call, reply).await;
}

// ── membership_status ───────────────────────────────────────────────────────

async fn membership_status(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::MembershipStatusRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => return fail(call, status).await,
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        return fail(call, invalid(WireInvalid::ProtocolVersion)).await;
    }
    let asked: Option<[u8; 16]> = match &request.operation_id {
        Some(bytes) => match wire::id16(bytes, "operation_id") {
            Ok(id) => Some(id),
            Err(err) => return fail(call, invalid(err)).await,
        },
        None => None,
    };
    if let Err(status) = check_group(runtime.group.as_bytes(), &request.group_id) {
        return deny(runtime, &actor, call, status).await;
    }
    if let Err(status) = cheap_administrator_gate(runtime, &actor) {
        return deny(runtime, &actor, call, status).await;
    }
    let cut = match read_cut(runtime).await {
        Ok(cut) => cut,
        Err(status) => return fail(call, status).await,
    };
    if let Err(status) = require_administrator(&cut, &actor) {
        return deny(runtime, &actor, call, status).await;
    }

    let found = match asked {
        Some(id) => match serve::intent(&cut.guarded, &id) {
            Ok(row) => row.map(|intent| (id, intent)),
            Err(_) => return fail(call, internal("the intent row does not decode")).await,
        },
        None => match serve::active_intent(&cut.guarded) {
            Ok(row) => row,
            Err(_) => return fail(call, internal("the intent rows do not decode")).await,
        },
    };

    let observed = cut.meta.membership.log_id.clone();
    let reply = match found {
        None => wire::MembershipStatusReplyWireV1 {
            protocol_version: wire::STORE_PROTOCOL_VERSION,
            group_id: cut.meta.group_id.clone(),
            operation_id: None,
            status_code: 0,
            expected_membership_log_id: None,
            observed_membership_log_id: observed,
            desired_voters: Vec::new(),
            retained_learners: Vec::new(),
            error_code: 0,
        },
        Some((id, intent)) => wire::MembershipStatusReplyWireV1 {
            protocol_version: wire::STORE_PROTOCOL_VERSION,
            group_id: cut.meta.group_id.clone(),
            operation_id: Some(id.to_vec()),
            status_code: intent.status_code,
            expected_membership_log_id: intent.expected_membership.as_ref().map(Into::into),
            observed_membership_log_id: observed,
            desired_voters: intent
                .plan
                .desired_voters
                .iter()
                .map(|n| NodeIdV1 {
                    node_id: n.to_vec(),
                })
                .collect(),
            retained_learners: intent
                .plan
                .retained_learners
                .iter()
                .map(|n| NodeIdV1 {
                    node_id: n.to_vec(),
                })
                .collect(),
            error_code: intent.result_code,
        },
    };
    respond_reply(call, reply).await;
}

// ── probe_client ────────────────────────────────────────────────────────────

async fn probe_client(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::ProbeClientRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => return fail(call, status).await,
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        return fail(call, invalid(WireInvalid::ProtocolVersion)).await;
    }
    let target = match wire::node_id(&request.target_node_id, "target_node_id") {
        Ok(target) => target,
        Err(err) => return fail(call, invalid(err)).await,
    };
    let target_client = match wire::id16(&request.target_client_id, "target_client_id") {
        Ok(id) => id,
        Err(err) => return fail(call, invalid(err)).await,
    };
    if let Err(status) = check_group(runtime.group.as_bytes(), &request.group_id) {
        return deny(runtime, &actor, call, status).await;
    }
    // §5.2: an actor may probe only its own sessions; an administrator any.
    if target != actor {
        if let Err(status) = cheap_administrator_gate(runtime, &actor) {
            return deny(runtime, &actor, call, status).await;
        }
    }
    let cut = match read_cut(runtime).await {
        Ok(cut) => cut,
        Err(status) => return fail(call, status).await,
    };
    if target != actor {
        if let Err(status) = require_administrator(&cut, &actor) {
            return deny(runtime, &actor, call, status).await;
        }
    }

    let session = match serve::session(&cut.guarded, &cut.meta, &target, &target_client) {
        Ok(session) => session,
        Err(_) => return fail(call, internal("the session row does not decode")).await,
    };
    let reply = match session {
        None => wire::ProbeClientReplyWireV1 {
            protocol_version: wire::STORE_PROTOCOL_VERSION,
            group_id: cut.meta.group_id.clone(),
            target_node_id: target.to_vec(),
            target_client_id: target_client.to_vec(),
            exists: false,
            last_sequence: 0,
            request_hash: Vec::new(),
            disposition_code: 0,
            retired: false,
        },
        Some(session) => wire::ProbeClientReplyWireV1 {
            protocol_version: wire::STORE_PROTOCOL_VERSION,
            group_id: cut.meta.group_id.clone(),
            target_node_id: target.to_vec(),
            target_client_id: target_client.to_vec(),
            exists: true,
            last_sequence: session.last_sequence,
            request_hash: session.request_hash.to_vec(),
            // §5.2: whether the last sequence's result was saved and with
            // which disposition — never the result body itself.
            // Only consumed results are ever saved, so a saved result *is*
            // disposition 1; `0` means none was saved.
            disposition_code: session.last_result.as_ref().map_or(0, |_| 1),
            retired: session.retired,
        },
    };
    respond_reply(call, reply).await;
}

// ── group_status (S4-STATUS-CUT) ────────────────────────────────────────────

async fn group_status(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::GroupStatusRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => return fail(call, status).await,
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        return fail(call, invalid(WireInvalid::ProtocolVersion)).await;
    }
    if let Err(status) = check_group(runtime.group.as_bytes(), &request.group_id) {
        return deny(runtime, &actor, call, status).await;
    }
    if let Err(status) = cheap_administrator_gate(runtime, &actor) {
        return deny(runtime, &actor, call, status).await;
    }
    let cut = match read_cut(runtime).await {
        Ok(cut) => cut,
        Err(status) => return fail(call, status).await,
    };
    if let Err(status) = require_administrator(&cut, &actor) {
        return deny(runtime, &actor, call, status).await;
    }

    // Replicated: everything below comes from the one guarded transaction.
    let membership = &cut.meta.membership.membership;
    let voter_set: std::collections::BTreeSet<Vec<u8>> = membership
        .configs
        .iter()
        .flat_map(|set| set.ids.iter())
        .map(|n| n.node_id.clone())
        .collect();
    let voters: Vec<NodeIdV1> = voter_set
        .iter()
        .map(|node_id| NodeIdV1 {
            node_id: node_id.clone(),
        })
        .collect();
    let learner_set: std::collections::BTreeSet<Vec<u8>> = membership
        .nodes
        .iter()
        .filter(|n| !voter_set.contains(&n.node_id))
        .map(|n| n.node_id.clone())
        .collect();
    let membership_joint = membership.configs.len() > 1;
    let pending = match serve::active_intent(&cut.guarded) {
        Ok(row) => row.map(|(id, _)| id.to_vec()),
        Err(_) => return fail(call, internal("the intent rows do not decode")).await,
    };
    let applied_log_id: Option<LogIdV1> = cut.pinned.as_ref().map(Into::into);
    let membership_log_id = cut.meta.membership.log_id.clone();
    let deployment_profile_code = cut.meta.deployment_profile_code;
    let group_id = cut.meta.group_id.clone();

    // Advisory: sampled only after the replicated cut, and declared advisory
    // on the wire type. The transaction stays open until the reply is built,
    // but nothing below reads it.
    let view = runtime.readiness.view();
    let metrics = runtime.raft.metrics().borrow().clone();
    let reply = wire::GroupStatusReplyWireV1 {
        protocol_version: wire::STORE_PROTOCOL_VERSION,
        group_id,
        lifecycle_code: view.code.code(),
        local_node_id: runtime.local.as_bytes().to_vec(),
        local_role_code: view.local_role.code(),
        leader_node_id: metrics.current_leader.map(|n| n.as_bytes().to_vec()),
        term: metrics.vote.leader_id().get_term(),
        // `RaftMetrics` carries indexes, not full log ids, and an advisory
        // field must not fabricate the halves it does not have. v1 reports the
        // advisory pair absent; the replicated `applied_log_id` below is the
        // one that carries proof.
        last_log_id: None,
        committed_log_id: None,
        applied_log_id,
        deployment_profile_code,
        membership_log_id,
        membership_joint,
        voters,
        learners: learner_set
            .into_iter()
            .map(|node_id| NodeIdV1 { node_id })
            .collect(),
        pending_operation_id: pending,
        quorum_available: view.quorum_available,
        fatal_code: view.fatal.as_ref().map_or(0, fatal_code_of),
    };
    respond_reply(call, reply).await;
}

/// The §5.2 `fatal_code` projection: `0` not fatal, then one code per
/// [`FatalCause`] variant. Advisory, like every local field it rides with.
fn fatal_code_of(cause: &crate::coordination::runtime::lifecycle::FatalCause) -> i32 {
    use crate::coordination::runtime::lifecycle::FatalCause;
    match cause {
        FatalCause::Storage(_) => 1,
        FatalCause::Raft(_) => 2,
        FatalCause::ProfileViolation { .. } => 3,
        FatalCause::IdentityMismatch(_) => 4,
    }
}

// ── audit_stream ────────────────────────────────────────────────────────────

async fn audit_stream(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::AuditStreamRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => return fail(call, status).await,
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        return fail(call, invalid(WireInvalid::ProtocolVersion)).await;
    }
    if let Err(err) = wire::audit_limit_bounds(request.limit) {
        return fail(call, invalid(err)).await;
    }
    let after: Option<LogId> = match &request.after_log_id {
        Some(id) => match LogId::try_from(id) {
            Ok(id) => Some(id),
            Err(_) => {
                return fail(
                    call,
                    invalid(WireInvalid::NodeIdWidth("after_log_id.leader_node_id")),
                )
                .await
            }
        },
        None => None,
    };
    if let Err(status) = check_group(runtime.group.as_bytes(), &request.group_id) {
        return deny(runtime, &actor, call, status).await;
    }
    if let Err(status) = cheap_administrator_gate(runtime, &actor) {
        return deny(runtime, &actor, call, status).await;
    }
    let cut = match read_cut(runtime).await {
        Ok(cut) => cut,
        Err(status) => return fail(call, status).await,
    };
    if let Err(status) = require_administrator(&cut, &actor) {
        return deny(runtime, &actor, call, status).await;
    }

    // §5.2: a request below the retained floor is OUT_OF_RANGE with the floor
    // in structured details, and **no partial stream**.
    if let (Some(after), Some(floor)) = (&after, &cut.meta.audit_floor) {
        if after.index < floor.index {
            let mut status = RpcStatus::error(
                StatusCode::OutOfRange,
                "the requested position precedes the retained audit floor",
            );
            status
                .detail_keys
                .push(wire::DETAIL_AUDIT_FLOOR_TERM.to_string());
            status.detail_values.push(floor.term.to_string());
            status
                .detail_keys
                .push(wire::DETAIL_AUDIT_FLOOR_INDEX.to_string());
            status.detail_values.push(floor.index.to_string());
            status
                .detail_keys
                .push(wire::DETAIL_AUDIT_FLOOR_LEADER.to_string());
            status
                .detail_values
                .push(hex::encode(&floor.leader_node_id));
            let _ = call.finish(&status).await;
            return;
        }
    }

    let rows = match serve::audit_after(&cut.guarded, after.as_ref(), request.limit as usize) {
        Ok(rows) => rows,
        Err(_) => return fail(call, internal("the audit rows do not decode")).await,
    };
    let fory = payload_fory::<wire::AuditRecordWireV1>();
    for ((term, index), row) in rows {
        if call.is_cancelled() {
            return;
        }
        let record = wire::AuditRecordWireV1 {
            protocol_version: wire::STORE_PROTOCOL_VERSION,
            group_id: cut.meta.group_id.clone(),
            log_id: LogIdV1 {
                term,
                leader_node_id: row.leader_node_id.to_vec(),
                index,
            },
            actor_node_id: row.actor.map(|a| a.to_vec()),
            operation_code: row.operation_code,
            resource: row.resource.as_ref().map(|key| {
                let (namespace, raw) = crate::coordination::model::state::key_parts(key);
                crate::coordination::contract::ResourceKeyV1 {
                    namespace: namespace.to_string(),
                    key: raw.to_vec(),
                }
            }),
            result_code: row.result.code(),
            epoch: row.epoch,
            value_revision: row.value_revision,
        };
        let Ok(bytes) = fory.serialize(&record) else {
            let _ = call
                .finish(&internal(
                    "an audit record does not encode; this is a server fault",
                ))
                .await;
            return;
        };
        if call.send(bytes).await.is_err() {
            return;
        }
    }
    let _ = call.finish(&RpcStatus::ok()).await;
}

// ── recovery_export (S4-EXPORT-OWNERSHIP: Slice 4 serves) ───────────────────

/// Export chunk size: comfortably under the 2 MiB store frame with envelope
/// headroom, matching the snapshot transfer's chunking discipline.
const EXPORT_CHUNK: usize = 1024 * 1024;

async fn recovery_export(runtime: &AttachedStore, actor: [u8; 32], frame: &[u8], call: Call) {
    let request: wire::RecoveryExportRequestV1 = match decode(frame) {
        Ok(request) => request,
        Err(status) => return fail(call, status).await,
    };
    if request.protocol_version != wire::STORE_PROTOCOL_VERSION {
        return fail(call, invalid(WireInvalid::ProtocolVersion)).await;
    }
    if let Err(status) = check_group(runtime.group.as_bytes(), &request.group_id) {
        return deny(runtime, &actor, call, status).await;
    }
    if let Err(status) = cheap_administrator_gate(runtime, &actor) {
        return deny(runtime, &actor, call, status).await;
    }
    let cut = match read_cut(runtime).await {
        Ok(cut) => cut,
        Err(status) => return fail(call, status).await,
    };
    if let Err(status) = require_administrator(&cut, &actor) {
        return deny(runtime, &actor, call, status).await;
    }

    let group_id = cut.meta.group_id.clone();
    let applied_log_id: Option<LogIdV1> = cut.pinned.as_ref().map(Into::into);
    let guarded = cut.guarded;

    // Pass 1: measure and hash the canonical §13 body without keeping a byte
    // of it. On a blocking worker for the same reason the snapshot builder
    // is: it is state-sized file I/O and hashing, and the transaction moves
    // with it, pinning the version both passes read.
    let (guarded, counted) = match tokio::task::spawn_blocking(move || {
        let counted = crate::coordination::storage::rows::stream_recovery_body(
            &guarded,
            std::path::Path::new("<recovery-export>"),
            CountingWriter::default(),
        );
        (guarded, counted)
    })
    .await
    {
        Ok(pair) => pair,
        Err(_) => return fail(call, internal("the export counter did not finish")).await,
    };
    let (total_len, checksum) = match counted {
        Ok((counter, _header)) => (counter.len, counter.hasher.finalize()),
        Err(_) => return fail(call, internal("the recovery body does not build")).await,
    };

    let fory = payload_fory::<wire::RecoveryExportFrameV1>();
    let manifest = wire::RecoveryExportFrameV1::Manifest(wire::RecoveryExportManifestWireV1 {
        protocol_version: wire::STORE_PROTOCOL_VERSION,
        group_id,
        total_len,
        blake3_checksum: checksum.as_bytes().to_vec(),
        applied_log_id,
    });
    let Ok(bytes) = fory.serialize(&manifest) else {
        return fail(call, internal("the manifest does not encode")).await;
    };
    if call.send(bytes).await.is_err() {
        return;
    }

    // Pass 2: stream the same pinned transaction again through a bounded
    // channel — the body is a pure function of the transaction, so these are
    // the bytes pass 1 measured. At most a few chunks exist at once: one in
    // the producer's buffer, four in the channel, one being sent. The full
    // body is never materialized (the reopen's blocker 5).
    let (chunks_tx, mut chunks_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let producer = tokio::task::spawn_blocking(move || {
        let outcome = crate::coordination::storage::rows::stream_recovery_body(
            &guarded,
            std::path::Path::new("<recovery-export>"),
            ChunkWriter::new(chunks_tx),
        );
        match outcome {
            Ok((writer, _header)) => writer.finish().is_ok(),
            Err(_) => false,
        }
    });

    let mut offset = 0u64;
    while let Some(data) = chunks_rx.recv().await {
        // Dropping the receiver on any early return unblocks the producer:
        // its next `blocking_send` fails and the worker winds down.
        if call.is_cancelled() {
            return;
        }
        let end = offset + data.len() as u64;
        let chunk = wire::RecoveryExportFrameV1::Chunk(wire::RecoveryExportChunkWireV1 {
            offset,
            data,
            final_chunk: end == total_len,
        });
        let Ok(bytes) = fory.serialize(&chunk) else {
            let _ = call
                .finish(&internal("an export chunk does not encode"))
                .await;
            return;
        };
        if call.send(bytes).await.is_err() {
            return;
        }
        offset = end;
    }

    // The channel closed: the producer is done. Its verdict and the byte
    // count both have to agree with pass 1 before the stream may end OK.
    let produced = matches!(producer.await, Ok(true));
    if !produced || offset != total_len {
        let _ = call
            .finish(&internal("the export body diverged between its two passes"))
            .await;
        return;
    }
    if total_len == 0 {
        // The §5.2 contract ends every export with an explicit final chunk,
        // the empty body included.
        let chunk = wire::RecoveryExportFrameV1::Chunk(wire::RecoveryExportChunkWireV1 {
            offset: 0,
            data: Vec::new(),
            final_chunk: true,
        });
        let Ok(bytes) = fory.serialize(&chunk) else {
            let _ = call
                .finish(&internal("an export chunk does not encode"))
                .await;
            return;
        };
        if call.send(bytes).await.is_err() {
            return;
        }
    }
    let _ = call.finish(&RpcStatus::ok()).await;
}

/// Pass 1's writer: the running length and checksum, no bytes kept.
#[derive(Default)]
struct CountingWriter {
    len: u64,
    hasher: blake3::Hasher,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        self.len += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Pass 2's writer: exact [`EXPORT_CHUNK`]-sized slices into a bounded
/// channel, blocking when the consumer is behind — that backpressure, not a
/// buffer, is what bounds the memory.
struct ChunkWriter {
    buffer: Vec<u8>,
    chunks: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl ChunkWriter {
    fn new(chunks: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            buffer: Vec::new(),
            chunks,
        }
    }

    fn send_buffer(&mut self) -> std::io::Result<()> {
        let chunk = std::mem::take(&mut self.buffer);
        self.chunks.blocking_send(chunk).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the export consumer is gone",
            )
        })
    }

    /// Flush the sub-chunk remainder. Consumes the writer: nothing may be
    /// written after the last partial chunk.
    fn finish(mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            self.send_buffer()?;
        }
        Ok(())
    }
}

impl std::io::Write for ChunkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut data = buf;
        while !data.is_empty() {
            let room = EXPORT_CHUNK - self.buffer.len();
            let take = room.min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buffer.len() == EXPORT_CHUNK {
                self.send_buffer()?;
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::{
        AcquireActionV1, CancelMembershipActionV1, ChangeMembershipActionV1, CompareAndSetActionV1,
        FenceV1, ReleaseActionV1, ReleaseWithValueActionV1, ResourceKeyV1, RevokeActionV1,
        SetAdminActionV1, SetGrantActionV1,
    };

    fn key() -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: "ns".into(),
            key: vec![1],
        }
    }

    fn fence() -> FenceV1 {
        FenceV1 {
            group_id: vec![0xAA; 32],
            resource: key(),
            epoch: 1,
            holder: vec![0xBB; 32],
        }
    }

    /// §11.5's split, per decoded action: growth is refused at the high-water
    /// mark, safety-reducing and size-preserving operations continue — and the
    /// two directions of `set_grant`/`set_admin` land on opposite sides.
    #[test]
    fn the_growth_classification_matches_the_section_11_5_lists() {
        let growth = [
            ActionV1::Acquire(AcquireActionV1 {
                resource: key(),
                ttl_ms: 30_000,
            }),
            ActionV1::CompareAndSet(CompareAndSetActionV1 {
                fence: fence(),
                expected_value_revision: 0,
                new_value: vec![1],
            }),
            ActionV1::ReleaseWithValue(ReleaseWithValueActionV1 {
                fence: fence(),
                expected_value_revision: 0,
                final_value: vec![1],
            }),
            // Admitting somebody new carries a seed and grows the group.
            ActionV1::ChangeMembership(ChangeMembershipActionV1 {
                operation_id: vec![0; 16],
                expected_membership_log_id: None,
                desired_voters: Vec::new(),
                retained_learners: Vec::new(),
                member_seeds: vec![crate::coordination::contract::MemberSeedV1 {
                    node_id: vec![0xDD; 32],
                    ticket: String::new(),
                    signed_join_permit: None,
                }],
            }),
            ActionV1::SetGrant(SetGrantActionV1 {
                resource: key(),
                target_node_id: vec![0xCC; 32],
                rights: 1,
                expected_acl_revision: 0,
            }),
            ActionV1::SetAdmin(SetAdminActionV1 {
                target_node_id: vec![0xCC; 32],
                enabled: true,
                expected_admin_revision: 1,
            }),
        ];
        for action in &growth {
            assert!(is_growth_mutation(action), "{action:?} adds bytes");
        }

        let continuing = [
            ActionV1::Renew(crate::coordination::contract::RenewActionV1 { fence: fence() }),
            ActionV1::Release(ReleaseActionV1 { fence: fence() }),
            ActionV1::Revoke(RevokeActionV1 {
                resource: key(),
                expected_epoch: None,
            }),
            ActionV1::CancelMembership(CancelMembershipActionV1 {
                operation_id: vec![0; 16],
                expected_phase_code: 1,
            }),
            // A cleanup plan carries no seeds: shrinking or reshuffling
            // already-admitted peers must stay possible under pressure.
            ActionV1::ChangeMembership(ChangeMembershipActionV1 {
                operation_id: vec![0; 16],
                expected_membership_log_id: None,
                desired_voters: Vec::new(),
                retained_learners: Vec::new(),
                member_seeds: Vec::new(),
            }),
            // ACL removal and admin removal free bytes; §11.5 keeps them.
            ActionV1::SetGrant(SetGrantActionV1 {
                resource: key(),
                target_node_id: vec![0xCC; 32],
                rights: 0,
                expected_acl_revision: 0,
            }),
            ActionV1::SetAdmin(SetAdminActionV1 {
                target_node_id: vec![0xCC; 32],
                enabled: false,
                expected_admin_revision: 1,
            }),
        ];
        for action in &continuing {
            assert!(!is_growth_mutation(action), "{action:?} must continue");
        }
    }

    /// The refill-on-read bucket: a burst empties, time refills at the rate,
    /// and `would_take` observes without consuming.
    #[test]
    fn the_token_bucket_refills_at_its_rate() {
        let mut bucket = TokenBucket::new(2.0);
        assert!(bucket.take(1.0, 2.0));
        assert!(bucket.take(1.0, 2.0));
        assert!(!bucket.take(1.0, 2.0));
        // Refill is driven by the clock; simulate one second of it.
        bucket.last -= std::time::Duration::from_secs(1);
        assert!(bucket.would_take(1.0, 2.0));
        assert!(bucket.take(1.0, 2.0));
        assert!(!bucket.take(1.0, 2.0));
        // The burst is a ceiling: a long idle never stores more than it.
        bucket.last -= std::time::Duration::from_secs(3600);
        assert!(bucket.take(1.0, 2.0));
        assert!(bucket.take(1.0, 2.0));
        assert!(!bucket.take(1.0, 2.0));
    }

    /// §12.2's per-actor in-flight ceiling: 32 slots, released on leave.
    #[test]
    fn the_actor_in_flight_ceiling_is_thirty_two() {
        let buckets = Buckets::default();
        let actor = [0xAA; 32];
        for _ in 0..MAX_IN_FLIGHT_PER_ACTOR {
            assert!(buckets.enter(&actor));
        }
        assert!(!buckets.enter(&actor));
        buckets.leave(&actor);
        assert!(buckets.enter(&actor));
        // Another actor has its own ceiling.
        assert!(buckets.enter(&[0xBB; 32]));
    }

    /// The denied bucket starts full — `denied_exhausted` is false until an
    /// actor has actually earned twenty denials — and drains one per denial.
    #[test]
    fn the_denied_bucket_short_circuits_only_after_the_burst() {
        let buckets = Buckets::default();
        let actor = [0xAC; 32];
        assert!(!buckets.denied_exhausted(&actor));
        for _ in 0..20 {
            buckets.note_denied(&actor);
        }
        assert!(buckets.denied_exhausted(&actor));
    }

    /// The admission table is bounded: a rotating flood of fresh identities
    /// evicts the least-recently-touched entry instead of growing the map.
    #[test]
    fn the_admission_table_is_bounded_by_eviction() {
        let buckets = Buckets::default();
        for i in 0..(MAX_TRACKED_ACTORS + 64) {
            let mut actor = [0u8; 32];
            actor[..8].copy_from_slice(&(i as u64).to_be_bytes());
            assert!(buckets.enter(&actor));
        }
        let actors = buckets.actors.lock().expect("the admission table");
        assert_eq!(actors.len(), MAX_TRACKED_ACTORS);
    }

    /// The tier-one predicate compares the database's real size against the
    /// threshold, and answers false for a database it cannot stat rather than
    /// refusing service on a metadata hiccup.
    #[test]
    fn the_pressure_predicate_reads_the_database_size() {
        let dir = tempfile::tempdir().expect("a directory");
        let db = dir.path().join("state.redb");
        std::fs::write(&db, vec![0u8; 1024]).expect("write");
        use crate::coordination::runtime::pressure::growth_pressure_at;
        assert!(!growth_pressure_at(&db, 2048));
        assert!(growth_pressure_at(&db, 1024));
        assert!(growth_pressure_at(&db, 512));
        assert!(!growth_pressure_at(&dir.path().join("absent"), 1));
    }
}
