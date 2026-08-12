//! `aster.lease.Grantor` — a designated-grantor lease serialization point
//! served over Aster RPC, plus the [`GrantorSerializer`] client that lets any
//! node use it as a [`LeaseSerializer`] backend for
//! [`LeaseHandle::acquire`](crate::lease::LeaseHandle::acquire).
//!
//! This is the "designated grantor" serialization point of the lease design
//! (`docs/_internal/aster-leases.md`): a single node — the
//! resource's home node, or the root in simple deployments — runs the
//! grantor and thereby linearizes ACQUIRE/RENEW/RELEASE/REVOKE for its
//! resources. It gets **no automatic failover** (failing it over without
//! consensus would split-brain the grantor itself): a dead grantor blocks
//! grants and renewals — holders run to their self-limit and stop —
//! availability loss, never corruption.
//!
//! ## Holder binding (why this service is hand-dispatched)
//!
//! The state machine's `caller` is the **authenticated QUIC peer**
//! ([`Call::peer`]) — never a request field. A node cannot acquire, renew,
//! or release as anyone but itself, no matter what it puts on the wire; the
//! epoch alone is never a bearer token. This is the one service property
//! that the `#[aster::service]` macro cannot express today (generated
//! methods don't see the caller), hence the hand-written dispatch.
//!
//! ## Authorization
//!
//! `revoke` requires the `operator` role by default (Gate 3; override with
//! [`LeaseGrantor::revoke_requires`]). `acquire`/`renew`/`release`/`inspect`
//! are ungated by default — connection admission is then the boundary — and
//! can be gated with [`LeaseGrantor::requires`] per the design doc's
//! lease-role recommendation.

use std::sync::Arc;
use std::time::Duration;

use fory_derive::ForyStruct;

use super::auth::{require_role, CapabilityRequirement};
use super::client::RpcConnection;
use super::codec::{RpcStatus, SerializationMode, StreamHeader};
use super::contract::{PayloadRegistry, WireField};
use super::server::{Call, ServiceDispatch};
use super::status::StatusCode;
use aster_transport_core::lease::{
    DenyReason, Fence, LeaseOp, LeaseSerializer, LeaseSnapshot, TransitionOutcome,
};

/// Service name on the wire.
pub const LEASE_GRANTOR_SERVICE: &str = "aster.lease.Grantor";
/// Contract version.
pub const LEASE_GRANTOR_VERSION: i32 = 1;

// ── Wire types ───────────────────────────────────────────────────────────────

/// Request for `acquire` (wire `aster/LeaseAcquire`). The candidate is the
/// authenticated caller — deliberately not a field.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/LeaseAcquire")]
pub struct LeaseAcquire {
    pub resource: String,
    /// Requested serializer-side TTL, milliseconds. Must be > 0.
    pub ttl_ms: i64,
}

/// Request for `renew` (wire `aster/LeaseRenew`). The holder is the
/// authenticated caller.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/LeaseRenew")]
pub struct LeaseRenew {
    pub resource: String,
    pub epoch: i64,
}

/// Request for `release` (wire `aster/LeaseRelease`). The holder is the
/// authenticated caller.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/LeaseRelease")]
pub struct LeaseRelease {
    pub resource: String,
    pub epoch: i64,
}

/// Request for `revoke` (wire `aster/LeaseRevoke`). The authority is the
/// authenticated caller; the method is Gate-3 gated.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/LeaseRevoke")]
pub struct LeaseRevoke {
    pub resource: String,
}

/// Request for `inspect` (wire `aster/LeaseQuery`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/LeaseQuery")]
pub struct LeaseQuery {
    pub resource: String,
}

/// Outcome codes for [`LeaseDecision::outcome`].
pub const LEASE_GRANTED: i32 = 1;
pub const LEASE_RENEWED: i32 = 2;
pub const LEASE_RELEASED: i32 = 3;
pub const LEASE_REVOKED: i32 = 4;
pub const LEASE_DENIED: i32 = 5;
pub const LEASE_SNAPSHOT: i32 = 6;

/// Deny codes for [`LeaseDecision::deny_reason`] (0 = none).
pub const DENY_HELD_BY_OTHER: i32 = 1;
pub const DENY_EPOCH_MISMATCH: i32 = 2;
pub const DENY_NOT_HOLDER: i32 = 3;
pub const DENY_EXPIRED: i32 = 4;
pub const DENY_NOT_HELD: i32 = 5;
pub const DENY_UNAUTHORIZED: i32 = 6;

/// Response of every `aster.lease.Grantor` method (wire `aster/LeaseDecision`).
///
/// A predicate refusal is a **successful RPC** carrying `outcome =`
/// [`LEASE_DENIED`] — a refusal is information for backoff, not a transport
/// error. RPC-level errors are reserved for malformed requests, Gate-3
/// rejections, and serializer faults.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/LeaseDecision")]
pub struct LeaseDecision {
    /// One of the `LEASE_*` outcome codes.
    pub outcome: i32,
    /// One of the `DENY_*` codes when `outcome == LEASE_DENIED`; else 0.
    pub deny_reason: i32,
    pub resource: String,
    /// On grant: the issued epoch. On deny/snapshot: the row's epoch.
    pub epoch: i64,
    /// On grant: the holder the epoch was issued to (the authenticated
    /// caller). On deny/snapshot: the row's holder, "" when free.
    pub holder: String,
    pub held: bool,
    /// Serializer-local expiry hint, milliseconds; -1 = unknown/none.
    pub remaining_ms: i64,
}

fn deny_code(reason: DenyReason) -> i32 {
    match reason {
        DenyReason::HeldByOther => DENY_HELD_BY_OTHER,
        DenyReason::EpochMismatch => DENY_EPOCH_MISMATCH,
        DenyReason::NotHolder => DENY_NOT_HOLDER,
        DenyReason::Expired => DENY_EXPIRED,
        DenyReason::NotHeld => DENY_NOT_HELD,
        DenyReason::Unauthorized => DENY_UNAUTHORIZED,
    }
}

fn deny_from_code(code: i32) -> Option<DenyReason> {
    match code {
        DENY_HELD_BY_OTHER => Some(DenyReason::HeldByOther),
        DENY_EPOCH_MISMATCH => Some(DenyReason::EpochMismatch),
        DENY_NOT_HOLDER => Some(DenyReason::NotHolder),
        DENY_EXPIRED => Some(DenyReason::Expired),
        DENY_NOT_HELD => Some(DenyReason::NotHeld),
        DENY_UNAUTHORIZED => Some(DenyReason::Unauthorized),
        _ => None,
    }
}

fn snapshot_fields(decision: &mut LeaseDecision, snap: &LeaseSnapshot) {
    decision.resource = snap.resource.clone();
    decision.epoch = snap.epoch as i64;
    decision.holder = snap.holder.clone().unwrap_or_default();
    decision.held = snap.held;
    decision.remaining_ms = snap.remaining.map(|d| d.as_millis() as i64).unwrap_or(-1);
}

fn snapshot_from_decision(d: &LeaseDecision) -> LeaseSnapshot {
    LeaseSnapshot {
        resource: d.resource.clone(),
        epoch: d.epoch as u64,
        holder: (!d.holder.is_empty()).then(|| d.holder.clone()),
        held: d.held,
        remaining: (d.remaining_ms >= 0).then(|| Duration::from_millis(d.remaining_ms as u64)),
    }
}

// ── Shared payload runtimes ──────────────────────────────────────────────────

/// One Fory runtime per payload root, per the framework's collision rules.
struct LeaseForys {
    acquire: fory_core::Fory,
    renew: fory_core::Fory,
    release: fory_core::Fory,
    revoke: fory_core::Fory,
    query: fory_core::Fory,
    decision: fory_core::Fory,
}

fn payload_fory<T: WireField>() -> fory_core::Fory {
    let mut reg = PayloadRegistry::new();
    T::register_payload(&mut reg);
    reg.into_fory()
}

impl LeaseForys {
    fn new() -> Self {
        Self {
            acquire: payload_fory::<LeaseAcquire>(),
            renew: payload_fory::<LeaseRenew>(),
            release: payload_fory::<LeaseRelease>(),
            revoke: payload_fory::<LeaseRevoke>(),
            query: payload_fory::<LeaseQuery>(),
            decision: payload_fory::<LeaseDecision>(),
        }
    }
}

// ── Server: the grantor service ──────────────────────────────────────────────

/// The `aster.lease.Grantor` service: serializes lease transitions for the
/// resources it is authoritative for, binding every op to the authenticated
/// caller. Register it on the node chosen as the grantor:
///
/// ```no_run
/// # use std::sync::Arc;
/// # async fn run(config: aster::AsterConfig) -> aster::Result<()> {
/// use aster::rpc::lease::LeaseGrantor;
/// use aster::lease::MemorySerializer;
///
/// let store = MemorySerializer::new();
/// let server = aster::rpc::AsterServer::builder()
///     .config(config)
///     .service(LeaseGrantor::new(Arc::new(store.clone())))
///     .start()
///     .await?;
/// # drop(server); Ok(())
/// # }
/// ```
///
/// Keep a clone of the [`MemorySerializer`](crate::lease::MemorySerializer)
/// when the grantor node also hosts the protected resource — its
/// `mutate_fenced` is the co-located enforcement point.
pub struct LeaseGrantor {
    serializer: Arc<dyn LeaseSerializer>,
    requires: Option<CapabilityRequirement>,
    revoke_requires: Option<CapabilityRequirement>,
    fory: LeaseForys,
}

impl LeaseGrantor {
    pub fn new(serializer: Arc<dyn LeaseSerializer>) -> Self {
        Self {
            serializer,
            requires: None,
            revoke_requires: Some(require_role("operator")),
            fory: LeaseForys::new(),
        }
    }

    /// Gate every method (Gate 3) — the design doc's lease-role
    /// recommendation: without it, any admitted caller can burn epochs (an
    /// availability attack, never corruption).
    pub fn requires(mut self, requires: CapabilityRequirement) -> Self {
        self.requires = Some(requires);
        self
    }

    /// Override the `revoke` gate (default: `require_role("operator")`).
    /// `None` leaves revoke gated only by [`requires`](Self::requires).
    pub fn revoke_requires(mut self, requires: Option<CapabilityRequirement>) -> Self {
        self.revoke_requires = requires;
        self
    }

    async fn run(
        &self,
        method: &str,
        caller: String,
        raw: Vec<u8>,
    ) -> Result<LeaseDecision, RpcStatus> {
        let bad_req =
            |e: fory_core::Error| RpcStatus::error(StatusCode::InvalidArgument, e.to_string());
        let backend =
            |e: anyhow::Error| RpcStatus::error(StatusCode::Internal, format!("serializer: {e}"));

        let (resource, op) = match method {
            "acquire" => {
                let req: LeaseAcquire = self.fory.acquire.deserialize(&raw).map_err(bad_req)?;
                if req.ttl_ms <= 0 {
                    return Err(RpcStatus::error(
                        StatusCode::InvalidArgument,
                        "ttl_ms must be > 0",
                    ));
                }
                let op = LeaseOp::Acquire {
                    candidate: caller,
                    ttl: Duration::from_millis(req.ttl_ms as u64),
                };
                (req.resource, op)
            }
            "renew" => {
                let req: LeaseRenew = self.fory.renew.deserialize(&raw).map_err(bad_req)?;
                let op = LeaseOp::Renew {
                    holder: caller,
                    epoch: req.epoch as u64,
                };
                (req.resource, op)
            }
            "release" => {
                let req: LeaseRelease = self.fory.release.deserialize(&raw).map_err(bad_req)?;
                let op = LeaseOp::Release {
                    holder: caller,
                    epoch: req.epoch as u64,
                };
                (req.resource, op)
            }
            "revoke" => {
                let req: LeaseRevoke = self.fory.revoke.deserialize(&raw).map_err(bad_req)?;
                (req.resource, LeaseOp::Revoke { authority: caller })
            }
            "inspect" => {
                let req: LeaseQuery = self.fory.query.deserialize(&raw).map_err(bad_req)?;
                let snap = self
                    .serializer
                    .snapshot(&req.resource)
                    .await
                    .map_err(backend)?;
                let mut decision = LeaseDecision {
                    outcome: LEASE_SNAPSHOT,
                    ..Default::default()
                };
                snapshot_fields(&mut decision, &snap);
                return Ok(decision);
            }
            other => {
                return Err(RpcStatus::error(
                    StatusCode::InvalidArgument,
                    format!("unknown method {other}"),
                ))
            }
        };

        let outcome = self
            .serializer
            .transition(&resource, op)
            .await
            .map_err(backend)?;
        let mut decision = LeaseDecision {
            resource,
            remaining_ms: -1,
            ..Default::default()
        };
        match outcome {
            TransitionOutcome::Granted(fence) => {
                decision.outcome = LEASE_GRANTED;
                decision.epoch = fence.epoch as i64;
                decision.holder = fence.holder;
                decision.held = true;
            }
            TransitionOutcome::Renewed => decision.outcome = LEASE_RENEWED,
            TransitionOutcome::Released => decision.outcome = LEASE_RELEASED,
            TransitionOutcome::Revoked => decision.outcome = LEASE_REVOKED,
            TransitionOutcome::Denied(reason, snap) => {
                decision.outcome = LEASE_DENIED;
                decision.deny_reason = deny_code(reason);
                snapshot_fields(&mut decision, &snap);
            }
        }
        Ok(decision)
    }
}

#[async_trait::async_trait]
impl ServiceDispatch for LeaseGrantor {
    fn name(&self) -> &str {
        LEASE_GRANTOR_SERVICE
    }

    fn version(&self) -> i32 {
        LEASE_GRANTOR_VERSION
    }

    fn methods(&self) -> &[&str] {
        &["acquire", "renew", "release", "revoke", "inspect"]
    }

    fn service_requires(&self) -> Option<CapabilityRequirement> {
        self.requires.clone()
    }

    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        match method {
            "revoke" => self.revoke_requires.clone(),
            _ => None,
        }
    }

    async fn dispatch(&self, method: &str, mut call: Call) {
        let raw = call.recv_request().await.unwrap_or_default();
        let caller = call.peer().to_string();
        match self.run(method, caller, raw).await {
            Ok(decision) => match self.fory.decision.serialize(&decision) {
                Ok(bytes) => {
                    let _ = call.respond(bytes, &RpcStatus::ok());
                }
                Err(e) => {
                    let _ = call.finish(&RpcStatus::error(StatusCode::Internal, e.to_string()));
                }
            },
            Err(status) => {
                let _ = call.finish(&status);
            }
        }
    }
}

// ── Client: the grantor as a LeaseSerializer backend ────────────────────────

/// A [`LeaseSerializer`] backed by a remote `aster.lease.Grantor`, so
/// [`LeaseHandle::acquire`](crate::lease::LeaseHandle::acquire) works
/// unchanged against a designated grantor:
///
/// ```no_run
/// # use std::sync::Arc;
/// # async fn run(node: aster::Node, grantor_id: aster::NodeId) -> anyhow::Result<()> {
/// use aster::lease::{AcquireOutcome, LeaseHandle, LeaseOptions};
/// use aster::rpc::lease::GrantorSerializer;
///
/// let conn = node.rpc_connect(&grantor_id).await?;
/// let serializer = Arc::new(GrantorSerializer::new(conn, node.id().to_string()));
/// let outcome = LeaseHandle::acquire(serializer, "db/primary", &node.id().to_string(),
///     LeaseOptions::default()).await?;
/// # let _ = outcome; Ok(())
/// # }
/// ```
///
/// The grantor binds every op to the **authenticated** connection identity;
/// `self_id` here only lets the client fail fast on ops constructed for a
/// different actor (they could never succeed remotely).
pub struct GrantorSerializer {
    conn: RpcConnection,
    self_id: String,
    fory: LeaseForys,
}

impl GrantorSerializer {
    /// `self_id` is this node's hex id (`node.id().to_string()`) — the only
    /// identity the grantor will ever bind our calls to.
    pub fn new(conn: RpcConnection, self_id: impl Into<String>) -> Self {
        Self {
            conn,
            self_id: self_id.into(),
            fory: LeaseForys::new(),
        }
    }

    fn header(&self, method: &str) -> StreamHeader {
        StreamHeader {
            service: LEASE_GRANTOR_SERVICE.into(),
            method: method.into(),
            version: LEASE_GRANTOR_VERSION,
            call_id: 0,
            deadline: 0,
            serialization_mode: SerializationMode::Xlang.as_i8(),
            metadata_keys: vec![],
            metadata_values: vec![],
            session_id: 0,
        }
    }

    async fn call(&self, method: &str, request: Vec<u8>) -> anyhow::Result<LeaseDecision> {
        let raw = self.conn.unary(&self.header(method), request).await?;
        let decision: LeaseDecision = self
            .fory
            .decision
            .deserialize(&raw)
            .map_err(|e| anyhow::anyhow!("bad LeaseDecision: {e}"))?;
        Ok(decision)
    }

    fn ensure_self(&self, actor: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            actor == self.self_id,
            "grantor ops are bound to this node's authenticated identity \
             ({}); op was built for {actor}",
            self.self_id
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl LeaseSerializer for GrantorSerializer {
    async fn transition(&self, resource: &str, op: LeaseOp) -> anyhow::Result<TransitionOutcome> {
        self.ensure_self(op.actor())?;
        let decision = match &op {
            LeaseOp::Acquire { ttl, .. } => {
                let req = LeaseAcquire {
                    resource: resource.into(),
                    ttl_ms: ttl.as_millis() as i64,
                };
                let bytes = self
                    .fory
                    .acquire
                    .serialize(&req)
                    .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
                self.call("acquire", bytes).await?
            }
            LeaseOp::Renew { epoch, .. } => {
                let req = LeaseRenew {
                    resource: resource.into(),
                    epoch: *epoch as i64,
                };
                let bytes = self
                    .fory
                    .renew
                    .serialize(&req)
                    .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
                self.call("renew", bytes).await?
            }
            LeaseOp::Release { epoch, .. } => {
                let req = LeaseRelease {
                    resource: resource.into(),
                    epoch: *epoch as i64,
                };
                let bytes = self
                    .fory
                    .release
                    .serialize(&req)
                    .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
                self.call("release", bytes).await?
            }
            LeaseOp::Revoke { .. } => {
                let req = LeaseRevoke {
                    resource: resource.into(),
                };
                let bytes = self
                    .fory
                    .revoke
                    .serialize(&req)
                    .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
                self.call("revoke", bytes).await?
            }
        };

        match decision.outcome {
            LEASE_GRANTED => Ok(TransitionOutcome::Granted(Fence {
                resource: decision.resource,
                epoch: decision.epoch as u64,
                holder: decision.holder,
            })),
            LEASE_RENEWED => Ok(TransitionOutcome::Renewed),
            LEASE_RELEASED => Ok(TransitionOutcome::Released),
            LEASE_REVOKED => Ok(TransitionOutcome::Revoked),
            LEASE_DENIED => {
                let reason = deny_from_code(decision.deny_reason).ok_or_else(|| {
                    anyhow::anyhow!("unknown deny_reason {}", decision.deny_reason)
                })?;
                Ok(TransitionOutcome::Denied(
                    reason,
                    snapshot_from_decision(&decision),
                ))
            }
            other => anyhow::bail!("unexpected lease outcome code {other}"),
        }
    }

    async fn snapshot(&self, resource: &str) -> anyhow::Result<LeaseSnapshot> {
        let req = LeaseQuery {
            resource: resource.into(),
        };
        let bytes = self
            .fory
            .query
            .serialize(&req)
            .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
        let decision = self.call("inspect", bytes).await?;
        anyhow::ensure!(
            decision.outcome == LEASE_SNAPSHOT,
            "unexpected inspect outcome {}",
            decision.outcome
        );
        Ok(snapshot_from_decision(&decision))
    }
}
