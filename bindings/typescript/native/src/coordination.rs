//! Coordination client — wraps `aster::coordination::client` (plan T8, D7).
//!
//! The client is **wrapped, never ported**: journaling, redirect routing, the
//! inbound validation boundary and the delivery contract all stay in Rust.
//! This module only converts values and carries handles.
//!
//! Handles that keep the durable journal alive (resolved mutations and
//! leases) are tracked by their client, so `close()` releases the journal's
//! lifetime lock deterministically instead of waiting for JavaScript GC.
//!
//! Errors carry a machine-readable prefix, `[coordination:<kind>] message`
//! (`kind` is `awaiting_ack`, `unresolved`, `journal_fatal`, `journal_locked`,
//! `invalid`, `rpc:<code>`, `unavailable` or `closed`), which the TypeScript
//! layer turns into typed error classes.
//!
//! Not wrapped (needs the R6 upcall mechanism): the placement manager's
//! JavaScript-implemented `PlacementProvisioner` / `PlacementPolicySource` /
//! `TopologySource`. The pure planner is exposed as `coordinationPlacementPlan`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::coordination::client as cc;
use aster::coordination::client::lease::LeaseAcquireOutcome;
use aster::coordination::client::placement as pl;
use aster::coordination::contract::{
    FenceV1, LogIdV1, MemberSeedV1, NodeIdV1, ResourceKeyV1, ResourceSnapshotV1,
};
use aster::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use aster::coordination::reason::{Outcome, Reason};
use aster::coordination::store::wire as w;

use crate::lease::{deny_out, options_in, status_out, LeaseDenied, LeaseOptions};
use crate::node::Node;
use crate::topology::TopologySnapshot;

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

// ── Errors ───────────────────────────────────────────────────────────────────

fn tagged(kind: &str, status: Status, msg: impl std::fmt::Display) -> Error {
    Error::new(status, format!("[coordination:{kind}] {msg}"))
}

fn invalid(msg: impl std::fmt::Display) -> Error {
    tagged("invalid", Status::InvalidArg, msg)
}

fn closed() -> Error {
    tagged(
        "closed",
        Status::GenericFailure,
        "the coordination client is closed",
    )
}

/// Map an `aster` error onto the tagged form. The journal-state refusals are
/// `InvalidArgument` in Rust; their (stable, contract-worded) messages are
/// what distinguishes them.
fn coord_err(e: aster::Error) -> Error {
    match e {
        aster::Error::InvalidArgument(msg) => {
            // The lease adapter flattens inner errors into its own
            // `InvalidArgument`; recover an embedded RPC status.
            if let Some(code) = embedded_rpc_code(&msg) {
                return tagged(
                    &format!("rpc:{code}"),
                    Status::GenericFailure,
                    format!("invalid argument: {msg}"),
                );
            }
            let kind = if msg.contains("awaits acknowledgement") {
                "awaiting_ack"
            } else if msg.contains("unresolved on this journal") {
                "unresolved"
            } else if msg.contains("journal is fatal") {
                "journal_fatal"
            } else if msg.contains("lifetime lock") {
                "journal_locked"
            } else {
                "invalid"
            };
            let status = if kind == "invalid" {
                Status::InvalidArg
            } else {
                Status::GenericFailure
            };
            tagged(kind, status, format!("invalid argument: {msg}"))
        }
        aster::Error::Rpc { code, message, .. } => tagged(
            &format!("rpc:{code}"),
            Status::GenericFailure,
            format!("rpc error [{code}]: {message}"),
        ),
        aster::Error::Transport(err) => tagged("unavailable", Status::GenericFailure, err),
        other => tagged("other", Status::GenericFailure, other),
    }
}

/// `… rpc error [7]: …` → `7` (the `aster::Error::Rpc` display form).
fn embedded_rpc_code(msg: &str) -> Option<i32> {
    let start = msg.find("rpc error [")? + "rpc error [".len();
    let end = start + msg[start..].find(']')?;
    msg[start..end].parse().ok()
}

// ── Scalar conversions ───────────────────────────────────────────────────────

fn n(v: u64, what: &str) -> Result<f64> {
    if v > MAX_SAFE_INTEGER {
        return Err(Error::from_reason(format!(
            "{what} exceeds the JavaScript safe integer range"
        )));
    }
    Ok(v as f64)
}

fn u64_in(v: f64, what: &str) -> Result<u64> {
    if !v.is_finite() || v < 0.0 || v.fract() != 0.0 || v > MAX_SAFE_INTEGER as f64 {
        return Err(invalid(format!(
            "{what} must be a non-negative safe integer, got {v}"
        )));
    }
    Ok(v as u64)
}

fn u8_in(v: u32, what: &str) -> Result<u8> {
    u8::try_from(v).map_err(|_| invalid(format!("{what} must be at most 255, got {v}")))
}

fn id32(s: &str, what: &str) -> Result<[u8; 32]> {
    CoordinationNodeId::parse_hex(s.trim())
        .map(|id| *id.as_bytes())
        .map_err(|_| invalid(format!("{what} must be 64 hex characters, got {s:?}")))
}

fn node_id_in(s: &str, what: &str) -> Result<CoordinationNodeId> {
    id32(s, what).map(CoordinationNodeId::from_array)
}

fn id16(s: &str, what: &str) -> Result<[u8; 16]> {
    let bytes = hex::decode(s.trim())
        .map_err(|_| invalid(format!("{what} must be 32 hex characters, got {s:?}")))?;
    <[u8; 16]>::try_from(bytes.as_slice())
        .map_err(|_| invalid(format!("{what} must be 32 hex characters, got {s:?}")))
}

fn group_in(s: &str) -> Result<CoordinationGroupId> {
    CoordinationGroupId::parse_hex(s.trim())
        .map_err(|_| invalid(format!("group id must be 64 hex characters, got {s:?}")))
}

fn opt_hex(v: &Option<Vec<u8>>) -> Option<String> {
    v.as_ref().map(hex::encode)
}

// ── Value types ──────────────────────────────────────────────────────────────

/// An exact resource identity: `namespace` plus raw key bytes.
#[napi(object)]
pub struct CoordinationResourceKey {
    pub namespace: String,
    pub key: Buffer,
}

impl CoordinationResourceKey {
    fn to_wire(&self) -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: self.namespace.clone(),
            key: self.key.to_vec(),
        }
    }

    fn from_wire(r: &ResourceKeyV1) -> Self {
        Self {
            namespace: r.namespace.clone(),
            key: Buffer::from(r.key.clone()),
        }
    }
}

/// The `(group, resource, epoch, holder)` fact a fenced mutation carries.
#[napi(object)]
pub struct CoordinationFence {
    pub group_id: String,
    pub resource: CoordinationResourceKey,
    pub epoch: f64,
    pub holder: String,
}

impl CoordinationFence {
    fn to_wire(&self) -> Result<FenceV1> {
        Ok(FenceV1 {
            group_id: group_in(&self.group_id)?.as_bytes().to_vec(),
            resource: self.resource.to_wire(),
            epoch: u64_in(self.epoch, "fence.epoch")?,
            holder: id32(&self.holder, "fence.holder")?.to_vec(),
        })
    }

    fn from_wire(f: &FenceV1) -> Result<Self> {
        Ok(Self {
            group_id: hex::encode(&f.group_id),
            resource: CoordinationResourceKey::from_wire(&f.resource),
            epoch: n(f.epoch, "epoch")?,
            holder: hex::encode(&f.holder),
        })
    }
}

/// A Raft log id (`term`, leader, `index`).
#[napi(object)]
pub struct CoordinationLogId {
    pub term: f64,
    pub leader_node_id: String,
    pub index: f64,
}

impl CoordinationLogId {
    fn to_wire(&self) -> Result<LogIdV1> {
        Ok(LogIdV1 {
            term: u64_in(self.term, "logId.term")?,
            leader_node_id: id32(&self.leader_node_id, "logId.leaderNodeId")?.to_vec(),
            index: u64_in(self.index, "logId.index")?,
        })
    }

    fn from_wire(l: &LogIdV1) -> Result<Self> {
        Ok(Self {
            term: n(l.term, "log term")?,
            leader_node_id: hex::encode(&l.leader_node_id),
            index: n(l.index, "log index")?,
        })
    }

    fn opt(l: &Option<LogIdV1>) -> Result<Option<Self>> {
        l.as_ref().map(Self::from_wire).transpose()
    }
}

fn opt_log_in(l: &Option<CoordinationLogId>) -> Result<Option<LogIdV1>> {
    l.as_ref().map(CoordinationLogId::to_wire).transpose()
}

/// One resource's committed row at the reply's cut.
#[napi(object)]
pub struct CoordinationResourceSnapshot {
    pub group_id: String,
    pub resource: CoordinationResourceKey,
    pub epoch: f64,
    pub holder: Option<String>,
    pub held: bool,
    pub ttl_ms: f64,
    pub lease_revision: f64,
    pub value_revision: f64,
    pub acl_revision: f64,
    pub value: Option<Buffer>,
    pub value_redacted: bool,
}

impl CoordinationResourceSnapshot {
    fn from_wire(s: &ResourceSnapshotV1) -> Result<Self> {
        Ok(Self {
            group_id: hex::encode(&s.group_id),
            resource: CoordinationResourceKey::from_wire(&s.resource),
            epoch: n(s.epoch, "epoch")?,
            holder: opt_hex(&s.holder),
            held: s.held,
            ttl_ms: n(s.ttl_ms, "ttl")?,
            lease_revision: n(s.lease_revision, "lease revision")?,
            value_revision: n(s.value_revision, "value revision")?,
            acl_revision: n(s.acl_revision, "acl revision")?,
            value: s.value.clone().map(Buffer::from),
            value_redacted: s.value_redacted,
        })
    }
}

/// The exact reply a journaled mutation resolved to (§5.2).
#[napi(object)]
pub struct CoordinationMutationReply {
    pub group_id: String,
    pub client_id: String,
    pub sequence: f64,
    pub request_hash: String,
    /// `1` consumed, `2` not consumed.
    pub disposition_code: i32,
    pub outcome_code: i32,
    /// `0` means none.
    pub reason_code: i32,
    pub snapshot: Option<CoordinationResourceSnapshot>,
    pub acl_revision: f64,
    pub admin_revision: f64,
    pub operation_id: Option<String>,
    pub membership_log_id: Option<CoordinationLogId>,
}

impl CoordinationMutationReply {
    fn from_wire(r: &w::MutationReplyWireV1) -> Result<Self> {
        Ok(Self {
            group_id: hex::encode(&r.group_id),
            client_id: hex::encode(&r.client_id),
            sequence: n(r.sequence, "sequence")?,
            request_hash: hex::encode(&r.request_hash),
            disposition_code: r.disposition_code,
            outcome_code: r.outcome_code,
            reason_code: r.reason_code,
            snapshot: r
                .snapshot
                .as_ref()
                .map(CoordinationResourceSnapshot::from_wire)
                .transpose()?,
            acl_revision: n(r.acl_revision, "acl revision")?,
            admin_revision: n(r.admin_revision, "admin revision")?,
            operation_id: opt_hex(&r.operation_id),
            membership_log_id: CoordinationLogId::opt(&r.membership_log_id)?,
        })
    }
}

/// Where a group founded by a §13 recovery import came from.
#[napi(object)]
pub struct CoordinationRecoveredFrom {
    pub source_group_id: String,
    pub source_genesis_hash: String,
    pub source_authority_public_key: String,
    pub source_authority_domain: String,
    pub export_hash: String,
    pub source_last_applied: Option<CoordinationLogId>,
}

/// The `group_status` cut (S4-STATUS-CUT). Advisory fields describe the
/// answering node; replicated fields are the group's.
#[napi(object)]
pub struct CoordinationGroupStatus {
    pub group_id: String,
    pub lifecycle_code: i32,
    pub local_node_id: String,
    /// `0` none, `1` learner, `2` voter.
    pub local_role_code: i32,
    pub leader_node_id: Option<String>,
    pub term: f64,
    pub last_log_id: Option<CoordinationLogId>,
    pub committed_log_id: Option<CoordinationLogId>,
    pub applied_log_id: Option<CoordinationLogId>,
    pub deployment_profile_code: i32,
    pub membership_log_id: Option<CoordinationLogId>,
    pub membership_joint: bool,
    pub voters: Vec<String>,
    pub learners: Vec<String>,
    pub pending_operation_id: Option<String>,
    pub quorum_available: Option<bool>,
    pub fatal_code: i32,
    pub audit_floor: Option<CoordinationLogId>,
    pub genesis_hash: String,
    pub authority_public_key: String,
    pub authority_domain: String,
    pub recovered_from: Option<CoordinationRecoveredFrom>,
}

fn node_ids_out(v: &[NodeIdV1]) -> Vec<String> {
    v.iter().map(|n| hex::encode(&n.node_id)).collect()
}

impl CoordinationGroupStatus {
    fn from_wire(s: &w::GroupStatusReplyWireV1) -> Result<Self> {
        Ok(Self {
            group_id: hex::encode(&s.group_id),
            lifecycle_code: s.lifecycle_code,
            local_node_id: hex::encode(&s.local_node_id),
            local_role_code: s.local_role_code,
            leader_node_id: opt_hex(&s.leader_node_id),
            term: n(s.term, "term")?,
            last_log_id: CoordinationLogId::opt(&s.last_log_id)?,
            committed_log_id: CoordinationLogId::opt(&s.committed_log_id)?,
            applied_log_id: CoordinationLogId::opt(&s.applied_log_id)?,
            deployment_profile_code: s.deployment_profile_code,
            membership_log_id: CoordinationLogId::opt(&s.membership_log_id)?,
            membership_joint: s.membership_joint,
            voters: node_ids_out(&s.voters),
            learners: node_ids_out(&s.learners),
            pending_operation_id: opt_hex(&s.pending_operation_id),
            quorum_available: s.quorum_available,
            fatal_code: s.fatal_code,
            audit_floor: CoordinationLogId::opt(&s.audit_floor)?,
            genesis_hash: hex::encode(&s.genesis_hash),
            authority_public_key: hex::encode(&s.authority_public_key),
            authority_domain: s.authority_domain.clone(),
            recovered_from: s
                .recovered_from
                .as_ref()
                .map(|r| -> Result<CoordinationRecoveredFrom> {
                    Ok(CoordinationRecoveredFrom {
                        source_group_id: hex::encode(&r.source_group_id),
                        source_genesis_hash: hex::encode(&r.source_genesis_hash),
                        source_authority_public_key: hex::encode(&r.source_authority_public_key),
                        source_authority_domain: r.source_authority_domain.clone(),
                        export_hash: hex::encode(&r.export_hash),
                        source_last_applied: CoordinationLogId::opt(&r.source_last_applied)?,
                    })
                })
                .transpose()?,
        })
    }
}

/// One retained audit row (§14).
#[napi(object)]
pub struct CoordinationAuditRecord {
    pub log_id: CoordinationLogId,
    pub actor_node_id: Option<String>,
    /// 1–11 mutation method codes; 100 expire, 101 membership progress, 102
    /// genesis installation.
    pub operation_code: i32,
    pub resource: Option<CoordinationResourceKey>,
    pub result_code: i32,
    pub epoch: f64,
    pub value_revision: f64,
}

/// One ACL row: a subject and its exact rights bits.
#[napi(object)]
pub struct CoordinationAclGrant {
    pub subject_node_id: String,
    pub rights: f64,
}

/// The committed ACL of one resource.
#[napi(object)]
pub struct CoordinationAclStatus {
    pub resource: CoordinationResourceKey,
    pub acl_revision: f64,
    pub grants: Vec<CoordinationAclGrant>,
}

/// The delegated administrator set (ROOT is static and not listed).
#[napi(object)]
pub struct CoordinationAdminStatus {
    pub admin_revision: f64,
    pub administrators: Vec<String>,
}

/// One membership intent's status (§5.2).
#[napi(object)]
pub struct CoordinationMembershipStatus {
    pub operation_id: Option<String>,
    pub status_code: i32,
    pub expected_membership_log_id: Option<CoordinationLogId>,
    pub observed_membership_log_id: Option<CoordinationLogId>,
    pub desired_voters: Vec<String>,
    pub retained_learners: Vec<String>,
    pub error_code: i32,
}

/// `probe_client` diagnostics — never authority to reassign a sequence.
#[napi(object)]
pub struct CoordinationClientProbe {
    pub target_node_id: String,
    pub target_client_id: String,
    pub exists: bool,
    pub last_sequence: f64,
    pub request_hash: String,
    pub disposition_code: i32,
    pub retired: bool,
}

/// `retire_client` reply: `(9, 0)` retired, or `(100, reason)`.
#[napi(object)]
pub struct CoordinationRetireReply {
    pub outcome_code: i32,
    pub reason_code: i32,
}

/// A verified recovery export's manifest.
#[napi(object)]
pub struct CoordinationExportManifest {
    pub total_len: f64,
    pub blake3_checksum: String,
    pub applied_log_id: Option<CoordinationLogId>,
}

impl CoordinationExportManifest {
    fn from_wire(m: &w::RecoveryExportManifestWireV1) -> Result<Self> {
        Ok(Self {
            total_len: n(m.total_len, "export length")?,
            blake3_checksum: hex::encode(&m.blake3_checksum),
            applied_log_id: CoordinationLogId::opt(&m.applied_log_id)?,
        })
    }
}

/// A recovery export held in memory.
#[napi(object)]
pub struct CoordinationRecoveryExport {
    pub manifest: CoordinationExportManifest,
    pub body: Buffer,
}

/// Canonical seed material for a new member.
#[napi(object)]
pub struct CoordinationMemberSeed {
    pub node_id: String,
    pub ticket: String,
    pub signed_join_permit: Option<Buffer>,
}

impl CoordinationMemberSeed {
    fn to_wire(&self) -> Result<MemberSeedV1> {
        Ok(MemberSeedV1 {
            node_id: id32(&self.node_id, "memberSeed.nodeId")?.to_vec(),
            ticket: self.ticket.clone(),
            signed_join_permit: self.signed_join_permit.as_ref().map(|b| b.to_vec()),
        })
    }

    fn from_wire(s: &MemberSeedV1) -> Self {
        Self {
            node_id: hex::encode(&s.node_id),
            ticket: s.ticket.clone(),
            signed_join_permit: s.signed_join_permit.clone().map(Buffer::from),
        }
    }
}

/// A `(code, name)` row of the §5.2 code tables.
#[napi(object)]
pub struct CoordinationCode {
    pub code: i32,
    pub name: String,
}

fn snake(debug: &str) -> String {
    let mut out = String::new();
    for (i, ch) in debug.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The §5.2 reason-code table, straight from the Rust enum.
#[napi]
pub fn coordination_reason_codes() -> Vec<CoordinationCode> {
    Reason::ALL
        .iter()
        .map(|r| CoordinationCode {
            code: r.code(),
            name: snake(&format!("{r:?}")),
        })
        .collect()
}

/// The §5.2 outcome-code table, straight from the Rust enum.
#[napi]
pub fn coordination_outcome_codes() -> Vec<CoordinationCode> {
    Outcome::ALL
        .iter()
        .map(|o| CoordinationCode {
            code: o.code(),
            name: snake(&format!("{o:?}")),
        })
        .collect()
}

// ── Client state ─────────────────────────────────────────────────────────────

type ResolvedSlot = Mutex<Option<cc::ResolvedMutation>>;
type LeaseSlot = Mutex<Option<Arc<cc::lease::CoordinationLease>>>;

/// Everything that keeps the journal's lifetime lock alive, so `close()` can
/// drop it all.
struct ClientState {
    client: Mutex<Option<cc::CoordinationClient>>,
    resolved: Mutex<Vec<Weak<ResolvedSlot>>>,
    leases: Mutex<Vec<Weak<LeaseSlot>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ClientState {
    fn client(&self) -> Result<cc::CoordinationClient> {
        lock(&self.client).clone().ok_or_else(closed)
    }

    /// Register a resolved mutation — unless the client closed meanwhile, in
    /// which case the handle is dropped (its result stays journaled and is
    /// redelivered by `unacknowledged()` after reopening).
    fn track(&self, resolved: cc::ResolvedMutation) -> Result<CoordinationResolvedMutation> {
        let reply = resolved.reply().clone();
        let client = lock(&self.client);
        if client.is_none() {
            return Err(closed());
        }
        let slot = Arc::new(Mutex::new(Some(resolved)));
        let mut list = lock(&self.resolved);
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(&slot));
        Ok(CoordinationResolvedMutation { slot, reply })
    }

    fn track_lease(
        &self,
        lease: cc::lease::CoordinationLease,
        resource: ResourceKeyV1,
    ) -> Result<CoordinationLease> {
        let client = lock(&self.client);
        if client.is_none() {
            return Err(closed());
        }
        let slot = Arc::new(Mutex::new(Some(Arc::new(lease))));
        let mut list = lock(&self.leases);
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(&slot));
        Ok(CoordinationLease { slot, resource })
    }

    fn close(&self) {
        let mut client = lock(&self.client);
        client.take();
        for slot in lock(&self.resolved).drain(..) {
            if let Some(slot) = slot.upgrade() {
                lock(&slot).take();
            }
        }
        for slot in lock(&self.leases).drain(..) {
            if let Some(slot) = slot.upgrade() {
                lock(&slot).take();
            }
        }
    }
}

fn seeds_in(seed_tickets: Option<Vec<String>>) -> Vec<String> {
    seed_tickets.unwrap_or_default()
}

/// Open the durable, journaled client bound to `node`'s identity and `group`.
/// Takes the per-actor/group journal lifetime lock until `close()`.
#[napi]
pub fn coordination_client_open(
    node: &Node,
    group_id: String,
    seed_tickets: Option<Vec<String>>,
    journal_dir: String,
) -> Result<CoordinationClient> {
    let group = group_in(&group_id)?;
    let node = node.aster().clone();
    let client = napi::bindgen_prelude::within_runtime_if_available(|| {
        cc::CoordinationClient::builder(node, group)
            .seed_tickets(seeds_in(seed_tickets))
            .journal_dir(journal_dir)
            .build()
    })
    .map_err(coord_err)?;
    Ok(CoordinationClient {
        state: Arc::new(ClientState {
            client: Mutex::new(Some(client)),
            resolved: Mutex::new(Vec::new()),
            leases: Mutex::new(Vec::new()),
        }),
    })
}

/// Open the read-only client: linearizable reads and status, no journal.
#[napi]
pub fn coordination_read_client_open(
    node: &Node,
    group_id: String,
    seed_tickets: Option<Vec<String>>,
) -> Result<CoordinationReadClient> {
    let group = group_in(&group_id)?;
    let node = node.aster().clone();
    let inner = napi::bindgen_prelude::within_runtime_if_available(|| {
        cc::CoordinationClient::builder(node, group)
            .seed_tickets(seeds_in(seed_tickets))
            .build_read_only()
    })
    .map_err(coord_err)?;
    Ok(CoordinationReadClient { inner })
}

/// The full durable, redirecting coordination client.
#[napi]
pub struct CoordinationClient {
    state: Arc<ClientState>,
}

#[napi]
impl CoordinationClient {
    fn c(&self) -> Result<cc::CoordinationClient> {
        self.state.client()
    }

    fn done(&self, r: aster::Result<cc::ResolvedMutation>) -> Result<CoordinationResolvedMutation> {
        self.state.track(r.map_err(coord_err)?)
    }

    /// The group this client is bound to (hex).
    #[napi(getter)]
    pub fn group_id(&self) -> Result<String> {
        Ok(hex::encode(self.c()?.group().as_bytes()))
    }

    /// The local actor identity as the group authenticates it (hex).
    #[napi(getter)]
    pub fn local_id(&self) -> Result<String> {
        Ok(hex::encode(self.c()?.local_id().map_err(coord_err)?))
    }

    /// The journal's current client id (hex, 16 bytes).
    #[napi]
    pub async fn client_id(&self) -> Result<String> {
        Ok(hex::encode(self.c()?.client_id().await))
    }

    /// The saved result a previous run resolved and nobody acknowledged.
    #[napi]
    pub async fn unacknowledged(&self) -> Result<Option<CoordinationResolvedMutation>> {
        let c = self.c()?;
        match c.unacknowledged().await.map_err(coord_err)? {
            Some(r) => Ok(Some(self.state.track(r)?)),
            None => Ok(None),
        }
    }

    /// Drive any half-finished request from a previous run to resolution;
    /// `null` when the journal is ready.
    #[napi]
    pub async fn resume(&self) -> Result<Option<CoordinationResolvedMutation>> {
        let c = self.c()?;
        match c.resume().await.map_err(coord_err)? {
            Some(r) => Ok(Some(self.state.track(r)?)),
            None => Ok(None),
        }
    }

    #[napi]
    pub async fn acquire(
        &self,
        resource: CoordinationResourceKey,
        ttl_ms: f64,
    ) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let ttl = u64_in(ttl_ms, "ttlMs")?;
        self.done(c.acquire(resource.to_wire(), ttl).await)
    }

    #[napi]
    pub async fn renew(&self, fence: CoordinationFence) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let fence = fence.to_wire()?;
        self.done(c.renew(fence).await)
    }

    #[napi]
    pub async fn compare_and_set(
        &self,
        fence: CoordinationFence,
        expected_value_revision: f64,
        new_value: Buffer,
    ) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let fence = fence.to_wire()?;
        let expected = u64_in(expected_value_revision, "expectedValueRevision")?;
        self.done(c.compare_and_set(fence, expected, new_value.to_vec()).await)
    }

    #[napi]
    pub async fn release(&self, fence: CoordinationFence) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let fence = fence.to_wire()?;
        self.done(c.release(fence).await)
    }

    #[napi]
    pub async fn release_with_value(
        &self,
        fence: CoordinationFence,
        expected_value_revision: f64,
        final_value: Buffer,
    ) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let fence = fence.to_wire()?;
        let expected = u64_in(expected_value_revision, "expectedValueRevision")?;
        self.done(
            c.release_with_value(fence, expected, final_value.to_vec())
                .await,
        )
    }

    #[napi]
    pub async fn revoke(
        &self,
        resource: CoordinationResourceKey,
        expected_epoch: Option<f64>,
    ) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let expected = expected_epoch
            .map(|e| u64_in(e, "expectedEpoch"))
            .transpose()?;
        self.done(c.revoke(resource.to_wire(), expected).await)
    }

    #[napi]
    pub async fn set_grant(
        &self,
        resource: CoordinationResourceKey,
        target_node_id: String,
        rights: f64,
        expected_acl_revision: f64,
    ) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let target = id32(&target_node_id, "targetNodeId")?;
        let rights = u64_in(rights, "rights")?;
        let expected = u64_in(expected_acl_revision, "expectedAclRevision")?;
        self.done(
            c.set_grant(resource.to_wire(), target, rights, expected)
                .await,
        )
    }

    #[napi]
    pub async fn set_admin(
        &self,
        target_node_id: String,
        enabled: bool,
        expected_admin_revision: f64,
    ) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let target = id32(&target_node_id, "targetNodeId")?;
        let expected = u64_in(expected_admin_revision, "expectedAdminRevision")?;
        self.done(c.set_admin(target, enabled, expected).await)
    }

    #[napi]
    pub async fn change_membership(
        &self,
        operation_id: String,
        expected_membership_log_id: Option<CoordinationLogId>,
        desired_voters: Vec<String>,
        retained_learners: Vec<String>,
        member_seeds: Vec<CoordinationMemberSeed>,
    ) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let op = id16(&operation_id, "operationId")?;
        let expected = opt_log_in(&expected_membership_log_id)?;
        let voters = desired_voters
            .iter()
            .map(|v| {
                id32(v, "desiredVoters[]").map(|id| NodeIdV1 {
                    node_id: id.to_vec(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let learners = retained_learners
            .iter()
            .map(|v| {
                id32(v, "retainedLearners[]").map(|id| NodeIdV1 {
                    node_id: id.to_vec(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let seeds = member_seeds
            .iter()
            .map(CoordinationMemberSeed::to_wire)
            .collect::<Result<Vec<_>>>()?;
        self.done(
            c.change_membership(op, expected, voters, learners, seeds)
                .await,
        )
    }

    #[napi]
    pub async fn cancel_membership(
        &self,
        operation_id: String,
        expected_phase_code: i32,
    ) -> Result<CoordinationResolvedMutation> {
        let c = self.c()?;
        let op = id16(&operation_id, "operationId")?;
        self.done(c.cancel_membership(op, expected_phase_code).await)
    }

    /// Retire one of this actor's **other** client ids.
    #[napi]
    pub async fn retire_other_client(
        &self,
        target_client_id: String,
    ) -> Result<CoordinationRetireReply> {
        let c = self.c()?;
        let target = id16(&target_client_id, "targetClientId")?;
        let r = c.retire_other_client(target).await.map_err(coord_err)?;
        Ok(CoordinationRetireReply {
            outcome_code: r.outcome_code,
            reason_code: r.reason_code,
        })
    }

    /// One committed acquire; on grant the Rust renew loop and self-limit
    /// take over. Lease transitions are journaled and auto-acknowledged.
    #[napi(ts_return_type = "Promise<CoordinationLease | LeaseDenied>")]
    pub async fn acquire_lease(
        &self,
        resource: CoordinationResourceKey,
        options: Option<LeaseOptions>,
    ) -> Result<Either<CoordinationLease, LeaseDenied>> {
        let c = self.c()?;
        let opts = options_in(options)?;
        let wire = resource.to_wire();
        match cc::lease::CoordinationLease::acquire(c, wire.clone(), opts)
            .await
            .map_err(coord_err)?
        {
            LeaseAcquireOutcome::Granted(lease) => {
                Ok(Either::A(self.state.track_lease(lease, wire)?))
            }
            LeaseAcquireOutcome::Denied(reason, snapshot) => Ok(Either::B(LeaseDenied {
                reason: deny_out(reason),
                snapshot: snapshot.into(),
            })),
        }
    }

    /// The read/status surface (shares this client's routing).
    #[napi]
    pub fn reads(&self) -> Result<CoordinationReadClient> {
        Ok(CoordinationReadClient {
            inner: self.c()?.reads(),
        })
    }

    /// Release the journal: drops every outstanding resolved-mutation and
    /// lease handle this client produced (unacknowledged results stay
    /// journaled for the next open). Idempotent.
    #[napi]
    pub fn close(&self) {
        self.state.close();
    }

    #[napi(getter)]
    pub fn closed(&self) -> bool {
        lock(&self.state.client).is_none()
    }
}

/// A resolved mutation, fsynced `ResolvedAwaitingAck` before it was returned.
#[napi]
pub struct CoordinationResolvedMutation {
    slot: Arc<ResolvedSlot>,
    reply: w::MutationReplyWireV1,
}

#[napi]
impl CoordinationResolvedMutation {
    /// The exact reply the journal holds.
    #[napi(getter)]
    pub fn reply(&self) -> Result<CoordinationMutationReply> {
        CoordinationMutationReply::from_wire(&self.reply)
    }

    /// `false` once acknowledged (or its client closed).
    #[napi(getter)]
    pub fn pending(&self) -> bool {
        lock(&self.slot).is_some()
    }

    /// The application has durably handled this result: free the journal
    /// for the next sequence.
    #[napi]
    pub async fn acknowledge(&self) -> Result<()> {
        let resolved = lock(&self.slot).take().ok_or_else(|| {
            tagged(
                "closed",
                Status::GenericFailure,
                "this result was already acknowledged, or its client is closed",
            )
        })?;
        resolved.acknowledge().await.map_err(coord_err)
    }
}

/// A held coordination lease (fenced register, §9.4).
#[napi]
pub struct CoordinationLease {
    slot: Arc<LeaseSlot>,
    resource: ResourceKeyV1,
}

#[napi]
impl CoordinationLease {
    fn lease(&self) -> Option<Arc<cc::lease::CoordinationLease>> {
        lock(&self.slot).clone()
    }

    fn take(&self) -> Result<cc::lease::CoordinationLease> {
        let arc = lock(&self.slot)
            .take()
            .ok_or_else(|| invalid("the lease was already released, or its client is closed"))?;
        match Arc::try_unwrap(arc) {
            Ok(lease) => Ok(lease),
            Err(arc) => {
                *lock(&self.slot) = Some(arc);
                Err(invalid(
                    "another operation on this lease is still in flight",
                ))
            }
        }
    }

    #[napi(getter)]
    pub fn resource(&self) -> CoordinationResourceKey {
        CoordinationResourceKey::from_wire(&self.resource)
    }

    /// The full fence for a protected action, or `null` once this holder
    /// must stop. Ask immediately before each action; never cache it.
    #[napi]
    pub fn fence(&self) -> Result<Option<CoordinationFence>> {
        match self.lease().and_then(|l| l.fence()) {
            Some(f) => Ok(Some(CoordinationFence::from_wire(&f)?)),
            None => Ok(None),
        }
    }

    /// The holder's status (`released` once handed back or closed).
    #[napi(getter, ts_return_type = "'held' | 'released' | 'fenced' | 'lost'")]
    pub fn status(&self) -> String {
        match self.lease() {
            Some(l) => status_out(l.status()),
            None => "released".into(),
        }
    }

    /// `compare_and_set` under the current fence; journaled and acknowledged.
    #[napi]
    pub async fn compare_and_set(
        &self,
        expected_value_revision: f64,
        new_value: Buffer,
    ) -> Result<CoordinationMutationReply> {
        let lease = self
            .lease()
            .ok_or_else(|| invalid("the lease was released, or its client is closed"))?;
        let expected = u64_in(expected_value_revision, "expectedValueRevision")?;
        let reply = lease
            .compare_and_set(expected, new_value.to_vec())
            .await
            .map_err(coord_err)?;
        CoordinationMutationReply::from_wire(&reply)
    }

    /// Graceful release through the serializer. Idempotent.
    #[napi]
    pub async fn release(&self) -> Result<()> {
        if lock(&self.slot).is_none() {
            return Ok(());
        }
        self.take()?.release().await.map_err(coord_err)
    }

    /// The atomic final handoff: write `finalValue` and release together, or
    /// neither.
    #[napi]
    pub async fn release_with_value(
        &self,
        expected_value_revision: f64,
        final_value: Buffer,
    ) -> Result<CoordinationMutationReply> {
        let expected = u64_in(expected_value_revision, "expectedValueRevision")?;
        let lease = self.take()?;
        let reply = lease
            .release_with_value(expected, final_value.to_vec())
            .await
            .map_err(coord_err)?;
        CoordinationMutationReply::from_wire(&reply)
    }
}

// ── Read client ──────────────────────────────────────────────────────────────

/// Linearizable reads and status; no journal, no mutation.
#[napi]
pub struct CoordinationReadClient {
    inner: cc::CoordinationReadClient,
}

#[napi]
impl CoordinationReadClient {
    #[napi]
    pub async fn read_linearizable(
        &self,
        resource: CoordinationResourceKey,
    ) -> Result<CoordinationResourceSnapshot> {
        let r = self
            .inner
            .read_linearizable(resource.to_wire())
            .await
            .map_err(coord_err)?;
        CoordinationResourceSnapshot::from_wire(&r.snapshot)
    }

    #[napi]
    pub async fn inspect_acl(
        &self,
        resource: CoordinationResourceKey,
    ) -> Result<CoordinationAclStatus> {
        let r = self
            .inner
            .inspect_acl(resource.to_wire())
            .await
            .map_err(coord_err)?;
        let s = r.acl_snapshot;
        Ok(CoordinationAclStatus {
            resource: CoordinationResourceKey::from_wire(&s.resource),
            acl_revision: n(s.acl_revision, "acl revision")?,
            grants: s
                .grants
                .iter()
                .map(|g| {
                    Ok(CoordinationAclGrant {
                        subject_node_id: hex::encode(&g.subject_node_id),
                        rights: n(g.rights, "rights")?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        })
    }

    #[napi]
    pub async fn inspect_admins(&self) -> Result<CoordinationAdminStatus> {
        let r = self.inner.inspect_admins().await.map_err(coord_err)?;
        Ok(CoordinationAdminStatus {
            admin_revision: n(r.admin_snapshot.admin_revision, "admin revision")?,
            administrators: node_ids_out(&r.admin_snapshot.administrators),
        })
    }

    #[napi]
    pub async fn membership_status(
        &self,
        operation_id: Option<String>,
    ) -> Result<CoordinationMembershipStatus> {
        let op = operation_id
            .as_deref()
            .map(|s| id16(s, "operationId"))
            .transpose()?;
        let r = self.inner.membership_status(op).await.map_err(coord_err)?;
        Ok(CoordinationMembershipStatus {
            operation_id: opt_hex(&r.operation_id),
            status_code: r.status_code,
            expected_membership_log_id: CoordinationLogId::opt(&r.expected_membership_log_id)?,
            observed_membership_log_id: CoordinationLogId::opt(&r.observed_membership_log_id)?,
            desired_voters: node_ids_out(&r.desired_voters),
            retained_learners: node_ids_out(&r.retained_learners),
            error_code: r.error_code,
        })
    }

    #[napi]
    pub async fn group_status(&self) -> Result<CoordinationGroupStatus> {
        let r = self.inner.group_status().await.map_err(coord_err)?;
        CoordinationGroupStatus::from_wire(&r)
    }

    #[napi]
    pub async fn probe_client(
        &self,
        target_node_id: String,
        target_client_id: String,
    ) -> Result<CoordinationClientProbe> {
        let node = id32(&target_node_id, "targetNodeId")?;
        let client = id16(&target_client_id, "targetClientId")?;
        let r = self
            .inner
            .probe_client(node, client)
            .await
            .map_err(coord_err)?;
        Ok(CoordinationClientProbe {
            target_node_id: hex::encode(&r.target_node_id),
            target_client_id: hex::encode(&r.target_client_id),
            exists: r.exists,
            last_sequence: n(r.last_sequence, "sequence")?,
            request_hash: hex::encode(&r.request_hash),
            disposition_code: r.disposition_code,
            retired: r.retired,
        })
    }

    /// At most `limit` (1..=10 000) retained audit rows strictly after
    /// `after`, ascending.
    #[napi]
    pub async fn audit_stream(
        &self,
        after: Option<CoordinationLogId>,
        limit: u32,
    ) -> Result<Vec<CoordinationAuditRecord>> {
        let after = opt_log_in(&after)?;
        let rows = self
            .inner
            .audit_stream(after, limit)
            .await
            .map_err(coord_err)?;
        rows.iter()
            .map(|r| {
                Ok(CoordinationAuditRecord {
                    log_id: CoordinationLogId::from_wire(&r.log_id)?,
                    actor_node_id: opt_hex(&r.actor_node_id),
                    operation_code: r.operation_code,
                    resource: r.resource.as_ref().map(CoordinationResourceKey::from_wire),
                    result_code: r.result_code,
                    epoch: n(r.epoch, "epoch")?,
                    value_revision: n(r.value_revision, "value revision")?,
                })
            })
            .collect()
    }

    /// The canonical recovery export, verified against its manifest.
    #[napi]
    pub async fn recovery_export(&self) -> Result<CoordinationRecoveryExport> {
        let (manifest, body) = self.inner.recovery_export().await.map_err(coord_err)?;
        Ok(CoordinationRecoveryExport {
            manifest: CoordinationExportManifest::from_wire(&manifest)?,
            body: Buffer::from(body),
        })
    }

    /// Stream the export into `path` (via `path.partial`, renamed into place
    /// only once the checksum held).
    #[napi]
    pub async fn recovery_export_to_file(
        &self,
        path: String,
    ) -> Result<CoordinationExportManifest> {
        let partial = format!("{path}.partial");
        let file = std::fs::File::create(&partial)
            .map_err(|e| Error::from_reason(format!("creating {partial}: {e}")))?;
        let result = self.inner.recovery_export_to(file).await;
        match result {
            Ok((manifest, file)) => {
                file.sync_all()
                    .map_err(|e| Error::from_reason(format!("syncing {partial}: {e}")))?;
                drop(file);
                std::fs::rename(&partial, &path)
                    .map_err(|e| Error::from_reason(format!("renaming into {path}: {e}")))?;
                CoordinationExportManifest::from_wire(&manifest)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                Err(coord_err(e))
            }
        }
    }
}

// ── Pure placement planner ───────────────────────────────────────────────────

/// One eligible candidate, optionally with its seed material.
#[napi(object)]
pub struct CoordinationPlacementCandidate {
    pub node_id: String,
    pub member_seed: Option<CoordinationMemberSeed>,
}

/// The application-owned placement policy (§7.7.1).
#[napi(object)]
pub struct CoordinationPlacementPolicy {
    pub revision: f64,
    /// BLAKE3 of the application's canonical policy record (hex).
    pub digest: String,
    pub authority_public_key: String,
    pub authority_domain: String,
    pub target_voters: u32,
    pub target_retained_learners: u32,
    pub eligible: Vec<CoordinationPlacementCandidate>,
    pub pinned_voters: Option<Vec<String>>,
    pub excluded: Option<Vec<String>>,
    pub minimum_separated_voter_pairs: Option<u32>,
    pub replacement_grace_ms: Option<f64>,
    pub allow_healthy_rebalance: Option<bool>,
}

/// One planning input (the topology snapshot is passed separately).
#[napi(object)]
pub struct CoordinationPlacementInput {
    pub group_id: String,
    pub genesis_hash: String,
    pub deployment_profile_code: i32,
    pub membership_log_id: Option<CoordinationLogId>,
    pub current_voters: Vec<String>,
    pub current_learners: Option<Vec<String>>,
    pub active_membership_intent: Option<bool>,
    pub grace_crossed_members: Option<Vec<String>>,
    pub policy: CoordinationPlacementPolicy,
}

/// Pairwise separation evidence for a selected voter set.
#[napi(object)]
pub struct CoordinationSeparationSummary {
    pub separated_pairs: u32,
    pub connected_pairs: u32,
    pub unknown_pairs: u32,
    pub total_pairs: u32,
    pub represented_clusters: u32,
    pub diversity_proved: bool,
}

impl From<&pl::SeparationSummary> for CoordinationSeparationSummary {
    fn from(s: &pl::SeparationSummary) -> Self {
        Self {
            separated_pairs: s.separated_pairs.into(),
            connected_pairs: s.connected_pairs.into(),
            unknown_pairs: s.unknown_pairs.into(),
            total_pairs: s.total_pairs.into(),
            represented_clusters: s.represented_clusters.into(),
            diversity_proved: s.diversity_proved,
        }
    }
}

/// A complete desired membership, ready for `changeMembership`.
#[napi(object)]
pub struct CoordinationPlacementPlan {
    pub desired_voters: Vec<String>,
    pub retained_learners: Vec<String>,
    pub member_seeds: Vec<CoordinationMemberSeed>,
    pub expected_membership_log_id: Option<CoordinationLogId>,
    pub separation: CoordinationSeparationSummary,
}

/// The planner's answer (§7.7.2).
#[napi(object)]
pub struct CoordinationPlacementRecommendation {
    #[napi(
        ts_type = "'stable' | 'degraded' | 'converging' | 'needs_provisioning' | 'insufficient_evidence' | 'invalid_policy' | 'plan'"
    )]
    pub kind: String,
    /// `degraded` / `invalid_policy`.
    pub why: Option<String>,
    /// `insufficient_evidence`.
    pub missing: Option<String>,
    /// `stable` / `degraded`.
    pub separation: Option<CoordinationSeparationSummary>,
    /// `needs_provisioning`: the selected NodeIds lacking seed material.
    pub candidates: Option<Vec<String>>,
    /// `needs_provisioning`: the selection they belong to.
    pub desired_voters: Option<Vec<String>>,
    pub retained_learners: Option<Vec<String>>,
    /// `plan`.
    pub plan: Option<CoordinationPlacementPlan>,
}

fn node_set(v: Option<&Vec<String>>, what: &str) -> Result<BTreeSet<CoordinationNodeId>> {
    v.map(|v| v.iter().map(|s| node_id_in(s, what)).collect())
        .unwrap_or_else(|| Ok(BTreeSet::new()))
}

fn ids_out(v: &[CoordinationNodeId]) -> Vec<String> {
    v.iter().map(|id| id.to_hex()).collect()
}

/// §7.7.2's pure, deterministic voter placement planner — local advice only.
/// The membership facts usually come from `groupStatus()`, the topology
/// snapshot from `node.topology().snapshot()`.
#[napi]
pub fn coordination_placement_plan(
    input: CoordinationPlacementInput,
    topology: &TopologySnapshot,
) -> Result<CoordinationPlacementRecommendation> {
    let p = &input.policy;
    let mut eligible = BTreeMap::new();
    for c in &p.eligible {
        let node_id = node_id_in(&c.node_id, "policy.eligible[].nodeId")?;
        eligible.insert(
            node_id,
            pl::PlacementCandidate {
                node_id,
                member_seed: c
                    .member_seed
                    .as_ref()
                    .map(CoordinationMemberSeed::to_wire)
                    .transpose()?,
            },
        );
    }
    let policy = pl::PlacementPolicy {
        revision: u64_in(p.revision, "policy.revision")?,
        digest: id32(&p.digest, "policy.digest")?,
        authority_public_key: id32(&p.authority_public_key, "policy.authorityPublicKey")?,
        authority_domain: p.authority_domain.clone(),
        target_voters: u8_in(p.target_voters, "policy.targetVoters")?,
        target_retained_learners: u8_in(
            p.target_retained_learners,
            "policy.targetRetainedLearners",
        )?,
        eligible,
        pinned_voters: node_set(p.pinned_voters.as_ref(), "policy.pinnedVoters[]")?,
        excluded: node_set(p.excluded.as_ref(), "policy.excluded[]")?,
        minimum_separated_voter_pairs: u8_in(
            p.minimum_separated_voter_pairs.unwrap_or(0),
            "policy.minimumSeparatedVoterPairs",
        )?,
        replacement_grace: Duration::from_millis(u64_in(
            p.replacement_grace_ms.unwrap_or(0.0),
            "policy.replacementGraceMs",
        )?),
        allow_healthy_rebalance: p.allow_healthy_rebalance.unwrap_or(false),
    };
    let plan_input = pl::PlacementInput {
        group_id: group_in(&input.group_id)?,
        genesis_hash: id32(&input.genesis_hash, "genesisHash")?,
        deployment_profile_code: input.deployment_profile_code,
        membership_log_id: opt_log_in(&input.membership_log_id)?,
        current_voters: node_set(Some(&input.current_voters), "currentVoters[]")?,
        current_learners: node_set(input.current_learners.as_ref(), "currentLearners[]")?,
        active_membership_intent: input.active_membership_intent.unwrap_or(false),
        grace_crossed_members: node_set(
            input.grace_crossed_members.as_ref(),
            "graceCrossedMembers[]",
        )?,
        policy,
        topology: (*topology.inner).clone(),
    };
    // `validate_policy` computes `target choose 2` in u32; a zero target is
    // refused by the profile check first, but guard it here regardless.
    if plan_input.policy.target_voters == 0 {
        return Ok(CoordinationPlacementRecommendation {
            kind: "invalid_policy".into(),
            why: Some("target_voters must be at least 1".into()),
            ..empty_recommendation()
        });
    }
    Ok(match pl::plan(&plan_input) {
        pl::PlacementRecommendation::Stable { separation } => CoordinationPlacementRecommendation {
            kind: "stable".into(),
            separation: Some((&separation).into()),
            ..empty_recommendation()
        },
        pl::PlacementRecommendation::Degraded { why, separation } => {
            CoordinationPlacementRecommendation {
                kind: "degraded".into(),
                why: Some(why),
                separation: Some((&separation).into()),
                ..empty_recommendation()
            }
        }
        pl::PlacementRecommendation::Converging => CoordinationPlacementRecommendation {
            kind: "converging".into(),
            ..empty_recommendation()
        },
        pl::PlacementRecommendation::NeedsProvisioning {
            candidates,
            desired_voters,
            retained_learners,
        } => CoordinationPlacementRecommendation {
            kind: "needs_provisioning".into(),
            candidates: Some(candidates.iter().map(|c| c.to_hex()).collect()),
            desired_voters: Some(ids_out(&desired_voters)),
            retained_learners: Some(ids_out(&retained_learners)),
            ..empty_recommendation()
        },
        pl::PlacementRecommendation::InsufficientEvidence { missing } => {
            CoordinationPlacementRecommendation {
                kind: "insufficient_evidence".into(),
                missing: Some(missing),
                ..empty_recommendation()
            }
        }
        pl::PlacementRecommendation::InvalidPolicy { why } => CoordinationPlacementRecommendation {
            kind: "invalid_policy".into(),
            why: Some(why),
            ..empty_recommendation()
        },
        pl::PlacementRecommendation::Plan(plan) => CoordinationPlacementRecommendation {
            kind: "plan".into(),
            plan: Some(CoordinationPlacementPlan {
                desired_voters: ids_out(&plan.desired_voters),
                retained_learners: ids_out(&plan.retained_learners),
                member_seeds: plan
                    .member_seeds
                    .iter()
                    .map(CoordinationMemberSeed::from_wire)
                    .collect(),
                expected_membership_log_id: CoordinationLogId::opt(
                    &plan.expected_membership_log_id,
                )?,
                separation: (&plan.separation).into(),
            }),
            ..empty_recommendation()
        },
    })
}

fn empty_recommendation() -> CoordinationPlacementRecommendation {
    CoordinationPlacementRecommendation {
        kind: String::new(),
        why: None,
        missing: None,
        separation: None,
        candidates: None,
        desired_voters: None,
        retained_learners: None,
        plan: None,
    }
}
