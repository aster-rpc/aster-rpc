//! Coordination client + placement (ABI 4.4, plan F7).
//!
//! Design: `docs/_internal/bindings/coordination-abi-design.md`. The client
//! is `aster::coordination::client` **wrapped, never ported** (D7): the
//! durable journal, redirect routing, the inbound validation boundary and
//! the delivery contract all stay in Rust; this module converts values and
//! carries handles, like the PyO3 (P9) and NAPI (T8) wrappers.
//!
//! ```text
//!   aster_coordination_client_open(rt, node, opts, ud, *op)   -> CLIENT_OPENED
//!   aster_coordination_mutate(rt, c, D0, ud, *op)             -> RESOLVED (D2 + handle)
//!   aster_coordination_resolved_acknowledge / _free
//!   aster_coordination_unacknowledged / _resume               -> RESOLVED (handle 0 = none)
//!   aster_coordination_read(rt, c, D1, ud, *op)               -> RESULT (D3..DA)
//!   aster_coordination_lease_acquire(..)                      -> LEASE_ACQUIRED (DD | B2)
//!   aster_coordination_lease_{fence,status,compare_and_set,release,release_with_value,free}
//!   aster_coordination_placement_manager_new(rt, c, upcalls, topology_node, *m)
//!   aster_coordination_placement_reconcile(rt, m, ud, *op)    -> RESULT (DE)
//!   aster_coordination_client_close / _free
//! ```
//!
//! Payloads are UCv1 messages `D0`-`DE` (`coordination_codec.rs`). A
//! coordination failure is `ASTER_EVENT_ERROR` with `error_code` =
//! `aster_coordination_error_t` and `flags` = the RPC status code for `RPC`.
//!
//! No new auth surface (D3): every request is authenticated by the Iroh
//! identity of the node the client was opened on and authorized by the
//! group's replicated ACL, deny by default.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use aster::binding_support::upcall::codec as ucv1;
use aster::binding_support::upcall::placement::{
    ForeignPlacementPolicySource, ForeignPlacementProvisioner,
};
use aster::binding_support::upcall::topology::ForeignTopologySource;
use aster::coordination::client::lease::{CoordinationLease, LeaseAcquireOutcome};
use aster::coordination::client::manager::{CoordinationPlacementManager, TopologySource};
use aster::coordination::client::{
    CoordinationClient, CoordinationErrorKind, CoordinationReadClient, ResolvedMutation,
};
use aster::coordination::contract::{NodeIdV1, ResourceKeyV1};
use aster::coordination::ids::CoordinationGroupId;
use aster::lease::{LeaseStatus, TransitionOutcome};

use crate::coordination_codec::{self as codec, MutationRequest, ReadRequest};
use crate::data::{
    bridge_for, in_slice, in_string, invalid, not_found, spawn_op_coded, Done, OpError,
};
use crate::pure::try_status;
use crate::upcall::{
    aster_lease_status_t, aster_upcalls_t, lease_options, lease_status_code, upcalls_for,
};
use crate::{
    aster_bytes_list_t, aster_bytes_t, aster_event_kind_t, aster_node_t, aster_operation_t,
    aster_runtime_t, aster_status_t, set_last_error, HandleRegistry,
};

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

// ============================================================================
// Types
// ============================================================================

/// A coordination client (`aster_coordination_client_open`): the durable
/// journaled client, or a read-only one.
pub type aster_coordination_client_t = u64;
/// A resolved mutation awaiting acknowledgement
/// (`ASTER_EVENT_COORDINATION_RESOLVED`).
pub type aster_coordination_resolved_t = u64;
/// A held coordination lease (`ASTER_EVENT_COORDINATION_LEASE_ACQUIRED`).
pub type aster_coordination_lease_t = u64;
/// A placement manager (`aster_coordination_placement_manager_new`).
pub type aster_coordination_placement_manager_t = u64;

/// `aster_coordination_client_options_t.flags`: open the read-only client
/// (linearizable reads and status; no journal, no mutation, no lease).
pub const ASTER_COORDINATION_CLIENT_READ_ONLY: u32 = 1;

/// `error_code` of a coordination `ASTER_EVENT_ERROR`
/// (= `aster::coordination::client::CoordinationErrorKind`). Switch on this,
/// never on the message.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_coordination_error_t {
    ASTER_COORDINATION_ERROR_NONE = 0,
    /// A resolved result awaits acknowledgement: take it with
    /// `aster_coordination_unacknowledged` and acknowledge it first.
    ASTER_COORDINATION_ERROR_AWAITING_ACK = 1,
    /// Another request is unresolved on this journal: `aster_coordination_resume` first.
    ASTER_COORDINATION_ERROR_UNRESOLVED = 2,
    /// Another process (or open client) holds this actor/group journal.
    ASTER_COORDINATION_ERROR_JOURNAL_LOCKED = 3,
    /// The journal is fatal: no further mutation is possible.
    ASTER_COORDINATION_ERROR_JOURNAL_FATAL = 4,
    /// The group answered with a non-OK RPC status; `flags` = the code
    /// (7 = permission denied by the replicated ACL).
    ASTER_COORDINATION_ERROR_RPC = 5,
    /// No member answered; a journaled sequence stays open and resumes with
    /// the same bytes.
    ASTER_COORDINATION_ERROR_UNAVAILABLE = 6,
    /// The client (or this handle's client) is closed.
    ASTER_COORDINATION_ERROR_CLOSED = 7,
    /// Reserved: a committed typed rejection is a result (`D2`), not an error.
    ASTER_COORDINATION_ERROR_REJECTED = 8,
    /// An argument or request outside the contract.
    ASTER_COORDINATION_ERROR_INVALID = 9,
    ASTER_COORDINATION_ERROR_OTHER = 10,
}

/// The `op` byte of a UCv1 `D0` MutationRequest (design §2).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_coordination_mutation_op_t {
    /// `resource ttl_ms:uint`
    ASTER_COORDINATION_MUTATE_ACQUIRE = 1,
    /// `fence`
    ASTER_COORDINATION_MUTATE_RENEW = 2,
    /// `fence expected_value_revision:uint new_value:bytes`
    ASTER_COORDINATION_MUTATE_COMPARE_AND_SET = 3,
    /// `fence`
    ASTER_COORDINATION_MUTATE_RELEASE = 4,
    /// `fence expected_value_revision:uint final_value:bytes`
    ASTER_COORDINATION_MUTATE_RELEASE_WITH_VALUE = 5,
    /// `resource expected_epoch:opt<uint>`
    ASTER_COORDINATION_MUTATE_REVOKE = 6,
    /// `resource target_node_id:bytes[32] rights:uint expected_acl_revision:uint`
    ASTER_COORDINATION_MUTATE_SET_GRANT = 7,
    /// `target_node_id:bytes[32] enabled:bool expected_admin_revision:uint`
    ASTER_COORDINATION_MUTATE_SET_ADMIN = 8,
    /// `operation_id:bytes[16] expected_membership_log_id:opt<LogId>
    /// desired_voters:list<bytes[32]> retained_learners:list<bytes[32]>
    /// member_seeds:list<MemberSeed>`
    ASTER_COORDINATION_MUTATE_CHANGE_MEMBERSHIP = 9,
    /// `operation_id:bytes[16] expected_phase_code:code`
    ASTER_COORDINATION_MUTATE_CANCEL_MEMBERSHIP = 10,
}

/// The `op` byte of a UCv1 `D1` ReadRequest, with the result tag.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_coordination_read_op_t {
    /// `resource` -> `D3`
    ASTER_COORDINATION_READ_READ_LINEARIZABLE = 1,
    /// `resource` -> `D4`
    ASTER_COORDINATION_READ_INSPECT_ACL = 2,
    /// -> `D5`
    ASTER_COORDINATION_READ_INSPECT_ADMINS = 3,
    /// `operation_id:opt<bytes[16]>` -> `D6`
    ASTER_COORDINATION_READ_MEMBERSHIP_STATUS = 4,
    /// -> `D7`
    ASTER_COORDINATION_READ_GROUP_STATUS = 5,
    /// `target_node_id:bytes[32] target_client_id:bytes[16]` -> `D8`
    ASTER_COORDINATION_READ_PROBE_CLIENT = 6,
    /// `after:opt<LogId> limit:uint(1..=10000)` -> `D9`
    ASTER_COORDINATION_READ_AUDIT_STREAM = 7,
    /// -> `DA`
    ASTER_COORDINATION_READ_RECOVERY_EXPORT = 8,
}

/// Options of `aster_coordination_client_open`. 48 bytes (LP64). Fill with
/// `aster_coordination_client_options_init`; `struct_size` must equal
/// `sizeof(aster_coordination_client_options_t)`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct aster_coordination_client_options_t {
    pub struct_size: u32,
    /// `ASTER_COORDINATION_CLIENT_READ_ONLY` or 0.
    pub flags: u32,
    /// The group id: exactly 32 raw bytes.
    pub group_id: *const u8,
    /// Seed tickets (UTF-8 base58, Open credential only): routing hints;
    /// they grant nothing.
    pub seed_tickets: aster_bytes_list_t,
    /// Journal directory (UTF-8 path). Required for the durable client,
    /// ignored by the read-only one.
    pub journal_dir: aster_bytes_t,
}

// ============================================================================
// State
// ============================================================================

enum Inner {
    Full(CoordinationClient),
    Read(CoordinationReadClient),
}

type ResolvedSlot = Mutex<Option<ResolvedMutation>>;

/// A lease behind a read/write lock: `compare_and_set` reads, `release`
/// writes (so it waits for an in-flight CAS), `fence` / `status` try-read.
struct LeaseSlot {
    /// Set synchronously by close / free / release: no new operation may
    /// start, even while the deferred take still waits for an in-flight CAS.
    closed: AtomicBool,
    lease: Arc<tokio::sync::RwLock<Option<CoordinationLease>>>,
}

/// The manager's topology: a node's own shared view, or the binding (kind 17).
enum ManagerTopology {
    Node(aster::topology::Topology),
    Foreign(ForeignTopologySource),
}

#[async_trait::async_trait]
impl TopologySource for ManagerTopology {
    async fn snapshot(&self) -> aster::Result<aster::topology::TopologySnapshot> {
        match self {
            ManagerTopology::Node(t) => TopologySource::snapshot(t).await,
            ManagerTopology::Foreign(f) => TopologySource::snapshot(f).await,
        }
    }
}

type Manager = CoordinationPlacementManager<
    ManagerTopology,
    ForeignPlacementPolicySource,
    ForeignPlacementProvisioner,
>;

struct ManagerSlot {
    /// Set synchronously by close / free: no new pass may start.
    closed: AtomicBool,
    manager: tokio::sync::Mutex<Option<Manager>>,
}

/// One client handle and everything that keeps its journal alive, so
/// `close` can drop it all (lock order: `inner`, then a list).
pub(crate) struct ClientState {
    group: [u8; 32],
    local: [u8; 32],
    inner: Mutex<Option<Inner>>,
    resolved: Mutex<Vec<Weak<ResolvedSlot>>>,
    leases: Mutex<Vec<Weak<LeaseSlot>>>,
    managers: Mutex<Vec<Weak<ManagerSlot>>>,
    /// Where a lease / manager busy with an operation is retired on close.
    runtime: tokio::runtime::Handle,
}

static CLIENTS: once_cell::sync::Lazy<HandleRegistry<ClientState>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static RESOLVED: once_cell::sync::Lazy<HandleRegistry<ResolvedSlot>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static LEASES: once_cell::sync::Lazy<HandleRegistry<LeaseSlot>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static MANAGERS: once_cell::sync::Lazy<HandleRegistry<ManagerSlot>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn status_err(status: aster_status_t, msg: impl ToString) -> i32 {
    set_last_error(msg);
    status as i32
}

fn closed_status() -> i32 {
    status_err(
        aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
        "the coordination client is closed",
    )
}

fn closed_op() -> OpError {
    OpError {
        status: aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
        code: aster_coordination_error_t::ASTER_COORDINATION_ERROR_CLOSED as i32,
        flags: 0,
        message: "the coordination client is closed".into(),
    }
}

fn kind_code(kind: CoordinationErrorKind) -> aster_coordination_error_t {
    use aster_coordination_error_t::*;
    match kind {
        CoordinationErrorKind::AwaitingAck => ASTER_COORDINATION_ERROR_AWAITING_ACK,
        CoordinationErrorKind::Unresolved => ASTER_COORDINATION_ERROR_UNRESOLVED,
        CoordinationErrorKind::JournalLocked => ASTER_COORDINATION_ERROR_JOURNAL_LOCKED,
        CoordinationErrorKind::JournalFatal => ASTER_COORDINATION_ERROR_JOURNAL_FATAL,
        CoordinationErrorKind::Rpc { .. } => ASTER_COORDINATION_ERROR_RPC,
        CoordinationErrorKind::Unavailable => ASTER_COORDINATION_ERROR_UNAVAILABLE,
        CoordinationErrorKind::Closed => ASTER_COORDINATION_ERROR_CLOSED,
        CoordinationErrorKind::Rejected { .. } => ASTER_COORDINATION_ERROR_REJECTED,
        CoordinationErrorKind::Invalid => ASTER_COORDINATION_ERROR_INVALID,
        _ => ASTER_COORDINATION_ERROR_OTHER,
    }
}

/// The coarse `aster_status_t` of a coordination failure (design §4).
fn kind_status(kind: CoordinationErrorKind) -> aster_status_t {
    use aster_status_t::*;
    match kind {
        CoordinationErrorKind::Rpc { code } => match code {
            1 => ASTER_STATUS_CANCELLED,
            3 => ASTER_STATUS_INVALID_ARGUMENT,
            4 => ASTER_STATUS_TIMEOUT,
            5 => ASTER_STATUS_NOT_FOUND,
            7 => ASTER_STATUS_PERMISSION_DENIED,
            12 => ASTER_STATUS_UNSUPPORTED,
            // Coordination admission's "unavailable" (no leader / quorum,
            // admission not ready) reads like lower-layer unavailability.
            14 => ASTER_STATUS_CONNECTION_REFUSED,
            // Coordination admission is a finished auth system: its
            // refusals use the reserved auth codes (D3).
            16 => ASTER_STATUS_UNAUTHENTICATED,
            _ => ASTER_STATUS_INTERNAL,
        },
        CoordinationErrorKind::Unavailable => ASTER_STATUS_CONNECTION_REFUSED,
        CoordinationErrorKind::Invalid
        | CoordinationErrorKind::AwaitingAck
        | CoordinationErrorKind::Unresolved => ASTER_STATUS_INVALID_ARGUMENT,
        CoordinationErrorKind::Closed => ASTER_STATUS_ALREADY_CLOSED,
        _ => ASTER_STATUS_INTERNAL,
    }
}

/// An `aster::Error` from the coordination client as an error event:
/// classified by its structured kind, never by its message.
fn coord_err(e: aster::Error) -> OpError {
    let kind = CoordinationErrorKind::of(&e);
    let flags = match kind {
        CoordinationErrorKind::Rpc { code } => code as u32,
        _ => 0,
    };
    OpError {
        status: kind_status(kind),
        code: kind_code(kind) as i32,
        flags,
        message: format!("{e:#}"),
    }
}

fn other_op(msg: impl Into<String>) -> OpError {
    OpError {
        status: aster_status_t::ASTER_STATUS_INTERNAL,
        code: aster_coordination_error_t::ASTER_COORDINATION_ERROR_OTHER as i32,
        flags: 0,
        message: msg.into(),
    }
}

impl ClientState {
    fn full(&self) -> Result<CoordinationClient, i32> {
        match lock(&self.inner).as_ref() {
            Some(Inner::Full(c)) => Ok(c.clone()),
            Some(Inner::Read(_)) => Err(status_err(
                aster_status_t::ASTER_STATUS_UNSUPPORTED,
                "a read-only coordination client has no journal: \
                 mutations, leases and placement need the durable client",
            )),
            None => Err(closed_status()),
        }
    }

    fn reads(&self) -> Result<CoordinationReadClient, i32> {
        match lock(&self.inner).as_ref() {
            Some(Inner::Full(c)) => Ok(c.reads()),
            Some(Inner::Read(r)) => Ok(r.clone()),
            None => Err(closed_status()),
        }
    }

    /// Hand out a resolved mutation -- unless the client closed meanwhile;
    /// then the result stays journaled and is redelivered after reopening.
    fn track_resolved(&self, r: ResolvedMutation) -> Result<u64, OpError> {
        let inner = lock(&self.inner);
        if inner.is_none() {
            return Err(closed_op());
        }
        let id = RESOLVED.insert(Mutex::new(Some(r)));
        let slot = RESOLVED.get(id).expect("just inserted");
        let mut list = lock(&self.resolved);
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(&slot));
        Ok(id)
    }

    fn track_lease(&self, lease: CoordinationLease) -> Result<u64, OpError> {
        let inner = lock(&self.inner);
        if inner.is_none() {
            return Err(closed_op());
        }
        let id = LEASES.insert(LeaseSlot {
            closed: AtomicBool::new(false),
            lease: Arc::new(tokio::sync::RwLock::new(Some(lease))),
        });
        let slot = LEASES.get(id).expect("just inserted");
        let mut list = lock(&self.leases);
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(&slot));
        Ok(id)
    }

    fn track_manager(&self, manager: Manager) -> Result<u64, i32> {
        let inner = lock(&self.inner);
        if inner.is_none() {
            return Err(closed_status());
        }
        let id = MANAGERS.insert(ManagerSlot {
            closed: AtomicBool::new(false),
            manager: tokio::sync::Mutex::new(Some(manager)),
        });
        let slot = MANAGERS.get(id).expect("just inserted");
        let mut list = lock(&self.managers);
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(&slot));
        Ok(id)
    }

    /// Drop the client and everything that keeps its journal alive. A lease
    /// or manager busy with an operation is retired once that operation
    /// finishes. Idempotent.
    fn close(&self) {
        let mut inner = lock(&self.inner);
        inner.take();
        for slot in lock(&self.resolved).drain(..) {
            if let Some(slot) = slot.upgrade() {
                lock(&slot).take();
            }
        }
        for slot in lock(&self.leases).drain(..) {
            let Some(slot) = slot.upgrade() else { continue };
            slot.closed.store(true, Ordering::SeqCst);
            let busy = match slot.lease.try_write() {
                Ok(mut g) => {
                    drop(g.take());
                    false
                }
                Err(_) => true,
            };
            if busy {
                self.runtime.spawn(async move {
                    drop(slot.lease.write().await.take());
                });
            }
        }
        for slot in lock(&self.managers).drain(..) {
            let Some(slot) = slot.upgrade() else { continue };
            slot.closed.store(true, Ordering::SeqCst);
            let busy = match slot.manager.try_lock() {
                Ok(mut g) => {
                    drop(g.take());
                    false
                }
                Err(_) => true,
            };
            if busy {
                self.runtime.spawn(async move {
                    drop(slot.manager.lock().await.take());
                });
            }
        }
    }
}

fn client_for(client: aster_coordination_client_t) -> Result<Arc<ClientState>, i32> {
    CLIENTS
        .get(client)
        .ok_or_else(|| not_found(format!("unknown coordination client {client}")))
}

/// Copy `bytes` into `(buf, cap)`; `BUFFER_TOO_SMALL` (with `*len` set)
/// when it does not fit -- ask with `NULL, 0`.
unsafe fn copy_out(bytes: &[u8], buf: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    *out_len = bytes.len();
    if bytes.len() > cap {
        return status_err(
            aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL,
            format!("need {} bytes, have {cap}", bytes.len()),
        );
    }
    if !bytes.is_empty() {
        if buf.is_null() {
            return invalid("buf is NULL with a non-zero capacity");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
    }
    OK
}

// ============================================================================
// Client lifecycle
// ============================================================================

/// Fill `*out` with the defaults (`struct_size` set, durable client, every
/// pointer NULL / empty). Set `group_id` (and `journal_dir`) before use.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_client_options_init(
    out: *mut aster_coordination_client_options_t,
) -> i32 {
    let Some(out) = out.as_mut() else {
        return invalid("aster_coordination_client_options_init: out is NULL");
    };
    *out = aster_coordination_client_options_t {
        struct_size: std::mem::size_of::<aster_coordination_client_options_t>() as u32,
        flags: 0,
        group_id: std::ptr::null(),
        seed_tickets: aster_bytes_list_t {
            items: std::ptr::null(),
            len: 0,
        },
        journal_dir: aster_bytes_t {
            ptr: std::ptr::null(),
            len: 0,
        },
    };
    OK
}

/// Open a coordination client bound to `node`'s identity and the options'
/// group. The durable client (flags 0) opens the journal in `journal_dir`
/// and holds its per-actor/group lifetime lock until closed; a second open
/// over the same journal fails with `JOURNAL_LOCKED`. The read-only client
/// (`ASTER_COORDINATION_CLIENT_READ_ONLY`) has no journal.
///
/// Seed tickets are routing hints (Open credential only; a credential
/// ticket is refused); coordination access is decided by the group's
/// replicated ACL against the node's transport identity.
///
/// Completes with `ASTER_EVENT_COORDINATION_CLIENT_OPENED` (`handle` = the
/// client, `related` = node) or `ASTER_EVENT_ERROR` (`error_code` =
/// `aster_coordination_error_t`). Bad options are `INVALID_ARGUMENT` here.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_client_open(
    runtime: aster_runtime_t,
    node: aster_node_t,
    options: *const aster_coordination_client_options_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_client_open: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(o) = options.as_ref() else {
        return invalid("aster_coordination_client_open: options is NULL");
    };
    let expected = std::mem::size_of::<aster_coordination_client_options_t>() as u32;
    if o.struct_size != expected {
        return invalid(format!(
            "aster_coordination_client_options_t.struct_size is {}, expected {expected}",
            o.struct_size
        ));
    }
    if o.flags & !ASTER_COORDINATION_CLIENT_READ_ONLY != 0 {
        return invalid(format!("unknown coordination client flags {:#x}", o.flags));
    }
    let read_only = o.flags & ASTER_COORDINATION_CLIENT_READ_ONLY != 0;
    let group = try_status!(crate::pure::in_key(o.group_id, "group_id"));
    let group = CoordinationGroupId::from_array(group);
    let mut seeds = Vec::new();
    if o.seed_tickets.len != 0 {
        if o.seed_tickets.items.is_null() {
            return invalid("seed_tickets: NULL items with a non-zero length");
        }
        for (i, b) in std::slice::from_raw_parts(o.seed_tickets.items, o.seed_tickets.len)
            .iter()
            .enumerate()
        {
            seeds.push(try_status!(in_string(b, &format!("seed_tickets[{i}]"))));
        }
    }
    let journal_dir = if read_only {
        None
    } else {
        let dir = try_status!(in_string(&o.journal_dir, "journal_dir"));
        if dir.is_empty() {
            return invalid("journal_dir is required for the durable client");
        }
        Some(dir)
    };
    let Some(core) = bridge.nodes.get(node) else {
        return not_found(format!("unknown node handle {node}"));
    };
    let aster_node = aster::Node::from_core_node(core.as_ref().clone());
    let handle = bridge.runtime.handle().clone();
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        // The journal open does file I/O and an fsync: off the async workers.
        let inner = tokio::task::spawn_blocking(move || {
            let builder = CoordinationClient::builder(aster_node, group).seed_tickets(seeds);
            Ok::<_, aster::Error>(match journal_dir {
                Some(dir) => Inner::Full(builder.journal_dir(dir).build()?),
                None => Inner::Read(builder.build_read_only()?),
            })
        })
        .await
        .map_err(|e| other_op(format!("journal open task: {e}")))?
        .map_err(coord_err)?;
        let local = match &inner {
            Inner::Full(c) => c.local_id().map_err(coord_err)?,
            Inner::Read(_) => {
                let id = core.node_id();
                let mut out = [0u8; 32];
                hex::decode_to_slice(&id, &mut out)
                    .map_err(|e| other_op(format!("node id: {e}")))?;
                out
            }
        };
        let id = CLIENTS.insert(ClientState {
            group: *group.as_bytes(),
            local,
            inner: Mutex::new(Some(inner)),
            resolved: Mutex::new(Vec::new()),
            leases: Mutex::new(Vec::new()),
            managers: Mutex::new(Vec::new()),
            runtime: handle,
        });
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_COORDINATION_CLIENT_OPENED,
            id,
        )
        .related(node))
    })
}

/// Close the client: new calls return `ALREADY_CLOSED`; its resolved-
/// mutation, lease and placement-manager handles become inert (calls return
/// `ALREADY_CLOSED`; leases stop renewing and lapse by TTL; unacknowledged
/// results stay journaled and are redelivered after reopening). Operations
/// already in flight complete; the journal lock is released when the last
/// of them finishes. Handles stay registered until freed. Idempotent.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_client_close(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    try_status!(client_for(client)).close();
    OK
}

/// Close (as `aster_coordination_client_close`) and free the handle.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_client_free(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match CLIENTS.remove(client) {
        Some(state) => {
            state.close();
            OK
        }
        None => not_found(format!("unknown coordination client {client}")),
    }
}

/// The group id (32 bytes) and the local actor id (32 bytes: the node's
/// identity as the group authenticates it). Either output may be NULL.
/// Works on a closed client.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_client_ids(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    out_group_id: *mut u8,
    out_local_id: *mut u8,
) -> i32 {
    try_status!(bridge_for(runtime));
    let state = try_status!(client_for(client));
    if !out_group_id.is_null() {
        std::ptr::copy_nonoverlapping(state.group.as_ptr(), out_group_id, 32);
    }
    if !out_local_id.is_null() {
        std::ptr::copy_nonoverlapping(state.local.as_ptr(), out_local_id, 32);
    }
    OK
}

/// The journal's current client id. Completes with
/// `ASTER_EVENT_BYTES_RESULT` (payload = 16 raw bytes). Durable client only.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_client_id(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_client_id: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let c = try_status!(try_status!(client_for(client)).full());
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT, client)
                .data(c.client_id().await.to_vec()),
        )
    })
}

// ============================================================================
// Mutations and the delivery contract
// ============================================================================

/// Run one sequenced mutation (`request` = UCv1 `D0`; malformed is
/// `INVALID_ARGUMENT` here). The reply is fsynced to the journal before the
/// event: `ASTER_EVENT_COORDINATION_RESOLVED`, `handle` = a new
/// `aster_coordination_resolved_t`, payload = `D2`. A committed typed
/// rejection is such a result (`outcome_code` / `reason_code`), not an
/// error. Acknowledge it after durably handling it; until then no new
/// mutation is permitted (`AWAITING_ACK`). Durable client only.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_mutate(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    request: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_mutate: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(client_for(client));
    let bytes = try_status!(in_slice(&request, "request"));
    let req = match codec::decode_mutation_request(bytes) {
        Ok(r) => r,
        Err(e) => return invalid(format!("coordination mutation request: {e:#}")),
    };
    let c = try_status!(state.full());
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let resolved = run_mutation(&c, req).await.map_err(coord_err)?;
        let payload = codec::encode_mutation_reply(resolved.reply());
        #[cfg(feature = "coordination-fixture")]
        test_hooks::before_track(client).await;
        let id = state.track_resolved(resolved)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_COORDINATION_RESOLVED, id).data(payload))
    })
}

fn ids(v: Vec<[u8; 32]>) -> Vec<NodeIdV1> {
    v.into_iter()
        .map(|id| NodeIdV1 {
            node_id: id.to_vec(),
        })
        .collect()
}

async fn run_mutation(
    c: &CoordinationClient,
    req: MutationRequest,
) -> aster::Result<ResolvedMutation> {
    match req {
        MutationRequest::Acquire { resource, ttl_ms } => c.acquire(resource, ttl_ms).await,
        MutationRequest::Renew { fence } => c.renew(fence).await,
        MutationRequest::CompareAndSet {
            fence,
            expected_value_revision,
            new_value,
        } => {
            c.compare_and_set(fence, expected_value_revision, new_value)
                .await
        }
        MutationRequest::Release { fence } => c.release(fence).await,
        MutationRequest::ReleaseWithValue {
            fence,
            expected_value_revision,
            final_value,
        } => {
            c.release_with_value(fence, expected_value_revision, final_value)
                .await
        }
        MutationRequest::Revoke {
            resource,
            expected_epoch,
        } => c.revoke(resource, expected_epoch).await,
        MutationRequest::SetGrant {
            resource,
            target_node_id,
            rights,
            expected_acl_revision,
        } => {
            c.set_grant(resource, target_node_id, rights, expected_acl_revision)
                .await
        }
        MutationRequest::SetAdmin {
            target_node_id,
            enabled,
            expected_admin_revision,
        } => {
            c.set_admin(target_node_id, enabled, expected_admin_revision)
                .await
        }
        MutationRequest::ChangeMembership {
            operation_id,
            expected_membership_log_id,
            desired_voters,
            retained_learners,
            member_seeds,
        } => {
            c.change_membership(
                operation_id,
                expected_membership_log_id,
                ids(desired_voters),
                ids(retained_learners),
                member_seeds,
            )
            .await
        }
        MutationRequest::CancelMembership {
            operation_id,
            expected_phase_code,
        } => c.cancel_membership(operation_id, expected_phase_code).await,
    }
}

unsafe fn redeliver(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
    resume: bool,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(client_for(client));
    let c = try_status!(state.full());
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let kind = aster_event_kind_t::ASTER_EVENT_COORDINATION_RESOLVED;
        let found = if resume {
            c.resume().await
        } else {
            c.unacknowledged().await
        }
        .map_err(coord_err)?;
        Ok(match found {
            None => Done::ok(kind, 0),
            Some(resolved) => {
                let payload = codec::encode_mutation_reply(resolved.reply());
                Done::ok(kind, state.track_resolved(resolved)?).data(payload)
            }
        })
    })
}

/// The saved result a previous run resolved and nobody acknowledged. Call
/// on startup before any mutation. Completes with
/// `ASTER_EVENT_COORDINATION_RESOLVED`: `handle` 0 (no payload) when there
/// is none. Durable client only.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_unacknowledged(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    redeliver(runtime, client, user_data, out_operation, false)
}

/// Drive any half-finished request from a previous run to resolution.
/// Completes like `aster_coordination_unacknowledged`: `handle` 0 = the
/// journal is ready. Durable client only.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_resume(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    redeliver(runtime, client, user_data, out_operation, true)
}

/// Acknowledge: the application has durably handled this result; frees the
/// journal for the next sequence. Consumes the handle (later calls
/// `NOT_FOUND`). Completes with `ASTER_EVENT_UNIT_RESULT`; on failure the
/// result stays journaled and is redelivered. `ALREADY_CLOSED` here when
/// the client was closed -- the handle is consumed then too (a later
/// `aster_coordination_resolved_free` is `NOT_FOUND`); the result stays
/// journaled for the next open.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_resolved_acknowledge(
    runtime: aster_runtime_t,
    resolved: aster_coordination_resolved_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_resolved_acknowledge: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(slot) = RESOLVED.remove(resolved) else {
        return not_found(format!("unknown resolved mutation {resolved}"));
    };
    let Some(r) = lock(&slot).take() else {
        return closed_status();
    };
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        r.acknowledge().await.map_err(coord_err)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            resolved,
        ))
    })
}

/// Free a resolved-mutation handle **without** acknowledging: the result
/// stays journaled (`aster_coordination_unacknowledged` redelivers it) and
/// no new mutation is permitted until it is acknowledged.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_resolved_free(
    runtime: aster_runtime_t,
    resolved: aster_coordination_resolved_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match RESOLVED.remove(resolved) {
        Some(_) => OK,
        None => not_found(format!("unknown resolved mutation {resolved}")),
    }
}

/// Retire one of this actor's **other** client ids (16 raw bytes). Completes
/// with `ASTER_EVENT_COORDINATION_RESULT`, payload = `DC`. Durable client
/// only.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_retire_other_client(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    target_client_id: *const u8,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || target_client_id.is_null() {
        return invalid("aster_coordination_retire_other_client: NULL argument");
    }
    let bridge = try_status!(bridge_for(runtime));
    let c = try_status!(try_status!(client_for(client)).full());
    let mut target = [0u8; 16];
    std::ptr::copy_nonoverlapping(target_client_id, target.as_mut_ptr(), 16);
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let r = c.retire_other_client(target).await.map_err(coord_err)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_COORDINATION_RESULT, client)
                .data(codec::encode_retire_reply(&r)),
        )
    })
}

// ============================================================================
// Reads
// ============================================================================

/// One linearizable read or status request (`request` = UCv1 `D1`;
/// malformed, or an `audit_stream` limit outside 1..=10000, is
/// `INVALID_ARGUMENT` here). Completes with
/// `ASTER_EVENT_COORDINATION_RESULT`, payload by op: read `D3`, acl `D4`,
/// admins `D5`, membership `D6`, group `D7`, probe `D8`, audit `D9`,
/// recovery export `DA`. Either client.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_read(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    request: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_read: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(client_for(client));
    let bytes = try_status!(in_slice(&request, "request"));
    let req = match codec::decode_read_request(bytes) {
        Ok(r) => r,
        Err(e) => return invalid(format!("coordination read request: {e:#}")),
    };
    let r = try_status!(state.reads());
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let payload = run_read(&r, req).await.map_err(coord_err)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_COORDINATION_RESULT, client).data(payload))
    })
}

async fn run_read(r: &CoordinationReadClient, req: ReadRequest) -> aster::Result<Vec<u8>> {
    Ok(match req {
        ReadRequest::ReadLinearizable(res) => {
            codec::encode_read_reply(&r.read_linearizable(res).await?)
        }
        ReadRequest::InspectAcl(res) => codec::encode_acl_status(&r.inspect_acl(res).await?),
        ReadRequest::InspectAdmins => codec::encode_admin_status(&r.inspect_admins().await?),
        ReadRequest::MembershipStatus(op) => {
            codec::encode_membership_status(&r.membership_status(op).await?)
        }
        ReadRequest::GroupStatus => codec::encode_group_status(&r.group_status().await?),
        ReadRequest::ProbeClient {
            target_node_id,
            target_client_id,
        } => codec::encode_client_probe(&r.probe_client(target_node_id, target_client_id).await?),
        ReadRequest::AuditStream { after, limit } => {
            codec::encode_audit_records(&r.audit_stream(after, limit).await?)
        }
        ReadRequest::RecoveryExport => {
            let (manifest, body) = r.recovery_export().await?;
            codec::encode_recovery_export(&manifest, &body)
        }
    })
}

/// Stream the recovery export into `path` (via `path.partial`, renamed into
/// place only once the checksum held). Completes with
/// `ASTER_EVENT_COORDINATION_RESULT`, payload = `DB` (the manifest).
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_recovery_export_to_file(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    path: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_recovery_export_to_file: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let r = try_status!(try_status!(client_for(client)).reads());
    let path = try_status!(in_string(&path, "path"));
    if path.is_empty() {
        return invalid("path is empty");
    }
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let partial = format!("{path}.partial");
        let file = std::fs::File::create(&partial)
            .map_err(|e| other_op(format!("creating {partial}: {e}")))?;
        match r.recovery_export_to(file).await {
            Ok((manifest, file)) => {
                file.sync_all()
                    .map_err(|e| other_op(format!("syncing {partial}: {e}")))?;
                drop(file);
                std::fs::rename(&partial, &path)
                    .map_err(|e| other_op(format!("renaming into {path}: {e}")))?;
                Ok(
                    Done::ok(aster_event_kind_t::ASTER_EVENT_COORDINATION_RESULT, client)
                        .data(codec::encode_recovery_export_manifest(&manifest)),
                )
            }
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                Err(coord_err(e))
            }
        }
    })
}

// ============================================================================
// Leases
// ============================================================================

/// One committed acquire of `(namespace, key)`; on grant the Rust renew
/// loop and self-limit take over, and every transition is journaled and
/// acknowledged by the lease itself. Timing as `aster_lease_acquire` (0 =
/// defaults). Completes with `ASTER_EVENT_COORDINATION_LEASE_ACQUIRED`:
/// granted -- `handle` = a new `aster_coordination_lease_t`, payload = `DD`;
/// denied -- `handle` 0, payload = UCv1 `B2` (denied: reason + snapshot).
/// Durable client only.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_coordination_lease_acquire(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    namespace: aster_bytes_t,
    key: aster_bytes_t,
    ttl_ms: u64,
    renew_interval_ms: u64,
    holder_margin_ms: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_lease_acquire: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(client_for(client));
    let resource = ResourceKeyV1 {
        namespace: try_status!(in_string(&namespace, "namespace")),
        key: try_status!(in_slice(&key, "key")).to_vec(),
    };
    let opts = try_status!(lease_options(ttl_ms, renew_interval_ms, holder_margin_ms));
    let c = try_status!(state.full());
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let kind = aster_event_kind_t::ASTER_EVENT_COORDINATION_LEASE_ACQUIRED;
        match CoordinationLease::acquire(c, resource, opts)
            .await
            .map_err(coord_err)?
        {
            LeaseAcquireOutcome::Granted(lease) => {
                let fence = lease
                    .fence()
                    .ok_or_else(|| other_op("the lease lapsed before it could be handed out"))?;
                let payload = codec::encode_coordination_fence(&fence);
                Ok(Done::ok(kind, state.track_lease(lease)?).data(payload))
            }
            LeaseAcquireOutcome::Denied(reason, snapshot) => Ok(Done::ok(kind, 0).data(
                ucv1::encode_transition_outcome(&TransitionOutcome::Denied(reason, snapshot)),
            )),
        }
    })
}

fn lease_for(lease: aster_coordination_lease_t) -> Result<Arc<LeaseSlot>, i32> {
    LEASES
        .get(lease)
        .ok_or_else(|| not_found(format!("unknown coordination lease {lease}")))
}

/// The full fence (UCv1 `DD`) for a protected action, copied into
/// `(buf, cap)`; `*out_len` = its size (`BUFFER_TOO_SMALL` when it does not
/// fit: ask with `NULL, 0`). Ask immediately before each action; never
/// cache it. `ALREADY_CLOSED` once the holder must stop (not held, the
/// self-limit passed, released, being released, or the client closed).
/// Never blocks.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_lease_fence(
    runtime: aster_runtime_t,
    lease: aster_coordination_lease_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return invalid("aster_coordination_lease_fence: out_len is NULL");
    }
    *out_len = 0;
    try_status!(bridge_for(runtime));
    let slot = try_status!(lease_for(lease));
    let fence = match slot.lease.try_read() {
        _ if slot.closed.load(Ordering::SeqCst) => None,
        Ok(g) => g.as_ref().and_then(CoordinationLease::fence),
        Err(_) => None,
    };
    match fence {
        Some(f) => copy_out(&codec::encode_coordination_fence(&f), buf, cap, out_len),
        None => status_err(
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "the fence is no longer presentable (not held, self-limit passed, \
             released or closed)",
        ),
    }
}

/// The holder-side status (`aster_lease_status_t`); `RELEASED` once
/// released, being released, or its client closed. Never blocks.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_lease_status(
    runtime: aster_runtime_t,
    lease: aster_coordination_lease_t,
    out_status: *mut u32,
) -> i32 {
    if out_status.is_null() {
        return invalid("aster_coordination_lease_status: out_status is NULL");
    }
    try_status!(bridge_for(runtime));
    let slot = try_status!(lease_for(lease));
    let status = match slot.lease.try_read() {
        _ if slot.closed.load(Ordering::SeqCst) => None,
        Ok(g) => g.as_ref().map(CoordinationLease::status),
        Err(_) => None,
    };
    *out_status = status.map_or(
        aster_lease_status_t::ASTER_LEASE_STATUS_RELEASED as u32,
        |s: LeaseStatus| lease_status_code(s),
    );
    OK
}

/// `compare_and_set` on the register under the lease's current fence;
/// journaled and acknowledged by the lease. Completes with
/// `ASTER_EVENT_COORDINATION_RESULT`, payload = `D2`. A release, free or
/// client close called after this returns waits for it to finish;
/// `ALREADY_CLOSED` here when the lease is closed or being released.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_lease_compare_and_set(
    runtime: aster_runtime_t,
    lease: aster_coordination_lease_t,
    expected_value_revision: u64,
    new_value: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_lease_compare_and_set: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let slot = try_status!(lease_for(lease));
    let value = try_status!(in_slice(&new_value, "new_value")).to_vec();
    // Take the read guard now, so a release (or close) called after this
    // returns waits for the CAS instead of racing it.
    if slot.closed.load(Ordering::SeqCst) {
        return closed_status();
    }
    let guard = match slot.lease.clone().try_read_owned() {
        Ok(g) if g.is_some() => g,
        _ => return closed_status(),
    };
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let l = guard.as_ref().ok_or_else(closed_op)?;
        let reply = l
            .compare_and_set(expected_value_revision, value)
            .await
            .map_err(coord_err)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_COORDINATION_RESULT, lease)
                .data(codec::encode_mutation_reply(&reply)),
        )
    })
}

/// Take the lease out of a consumed handle, after any in-flight CAS.
async fn take_lease(slot: Arc<LeaseSlot>) -> Result<CoordinationLease, OpError> {
    // `closed` was set by a client close before this release: the lease is
    // the close's to drop, not ours to release.
    if slot.closed.swap(true, Ordering::SeqCst) {
        return Err(closed_op());
    }
    slot.lease.write().await.take().ok_or_else(closed_op)
}

/// Graceful release through the lease's own transition. Consumes the handle
/// (later calls `NOT_FOUND`); waits for an in-flight `compare_and_set` on it.
/// Completes with `ASTER_EVENT_UNIT_RESULT`.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_lease_release(
    runtime: aster_runtime_t,
    lease: aster_coordination_lease_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_lease_release: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(slot) = LEASES.remove(lease) else {
        return not_found(format!("unknown coordination lease {lease}"));
    };
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        take_lease(slot).await?.release().await.map_err(coord_err)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, lease))
    })
}

/// The atomic final handoff: write `final_value` and release together, or
/// neither. Consumes the handle. Completes with
/// `ASTER_EVENT_COORDINATION_RESULT`, payload = `D2`.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_lease_release_with_value(
    runtime: aster_runtime_t,
    lease: aster_coordination_lease_t,
    expected_value_revision: u64,
    final_value: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_lease_release_with_value: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let value = try_status!(in_slice(&final_value, "final_value")).to_vec();
    let Some(slot) = LEASES.remove(lease) else {
        return not_found(format!("unknown coordination lease {lease}"));
    };
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let reply = take_lease(slot)
            .await?
            .release_with_value(expected_value_revision, value)
            .await
            .map_err(coord_err)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_COORDINATION_RESULT, lease)
                .data(codec::encode_mutation_reply(&reply)),
        )
    })
}

/// Free a lease handle **without** releasing: renewals stop and the lease
/// lapses by TTL. Prefer `aster_coordination_lease_release`.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_lease_free(
    runtime: aster_runtime_t,
    lease: aster_coordination_lease_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match LEASES.remove(lease) {
        Some(slot) => {
            slot.closed.store(true, Ordering::SeqCst);
            if let Ok(mut g) = slot.lease.try_write() {
                drop(g.take());
            }
            OK
        }
        None => not_found(format!("unknown coordination lease {lease}")),
    }
}

// ============================================================================
// Placement
// ============================================================================

/// The §7.7.3 placement manager over a durable client. Its policy source
/// (kind 32 `BC` -> `BD`) and provisioner (kind 33 `BE` -> `BF`) are the
/// binding, answered on `upcalls`; its topology is `topology_node`'s own
/// shared view (the node must have joined a swarm with
/// `aster_topology_enable_shared`) or, with 0, the binding (kind 17 `BA` ->
/// `BB`). Provider failures (error, unimplemented, timeout, malformed
/// answer) fail the pass: these kinds have no fail-open. It changes
/// membership only through the client's durable journal.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_placement_manager_new(
    runtime: aster_runtime_t,
    client: aster_coordination_client_t,
    upcalls: aster_upcalls_t,
    topology_node: aster_node_t,
    out_manager: *mut aster_coordination_placement_manager_t,
) -> i32 {
    if out_manager.is_null() {
        return invalid("aster_coordination_placement_manager_new: out_manager is NULL");
    }
    *out_manager = 0;
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(client_for(client));
    let u = try_status!(upcalls_for(upcalls)).upcalls.clone();
    let topology = if topology_node == 0 {
        ManagerTopology::Foreign(ForeignTopologySource::new(u.clone()))
    } else {
        let Some(core) = bridge.nodes.get(topology_node) else {
            return not_found(format!("unknown node handle {topology_node}"));
        };
        ManagerTopology::Node(aster::Node::from_core_node(core.as_ref().clone()).topology())
    };
    let c = try_status!(state.full());
    let manager = CoordinationPlacementManager::new(
        c,
        topology,
        ForeignPlacementPolicySource::new(u.clone()),
        ForeignPlacementProvisioner::new(u),
    );
    *out_manager = try_status!(state.track_manager(manager));
    OK
}

/// Run one reconciliation pass (passes on one manager are serialized).
/// Completes with `ASTER_EVENT_COORDINATION_RESULT`, payload = `DE` (the
/// report), or `ASTER_EVENT_ERROR`.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_placement_reconcile(
    runtime: aster_runtime_t,
    manager: aster_coordination_placement_manager_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_coordination_placement_reconcile: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(slot) = MANAGERS.get(manager) else {
        return not_found(format!("unknown placement manager {manager}"));
    };
    if slot.closed.load(Ordering::SeqCst) {
        return closed_status();
    }
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let mut g = slot.manager.lock().await;
        if slot.closed.load(Ordering::SeqCst) {
            drop(g.take());
            return Err(closed_op());
        }
        let m = g.as_mut().ok_or_else(closed_op)?;
        let report = m.reconcile().await.map_err(coord_err)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_COORDINATION_RESULT, manager)
                .data(codec::encode_placement_report(&report)),
        )
    })
}

/// Free a placement manager (after any running pass).
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_placement_manager_free(
    runtime: aster_runtime_t,
    manager: aster_coordination_placement_manager_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match MANAGERS.remove(manager) {
        Some(slot) => {
            slot.closed.store(true, Ordering::SeqCst);
            if let Ok(mut g) = slot.manager.try_lock() {
                drop(g.take());
            }
            OK
        }
        None => not_found(format!("unknown placement manager {manager}")),
    }
}

/// Test-only seams (feature `coordination-fixture`, never in a shipping
/// build): pause a mutation of one client between "resolved and fsynced" and
/// "handle handed out", so a test can close the client in exactly that
/// window.
#[cfg(feature = "coordination-fixture")]
#[doc(hidden)]
pub mod test_hooks {
    use std::collections::HashMap;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::aster_coordination_client_t;

    /// An armed gate: the next mutation of its client stops before tracking.
    pub struct Gate {
        reached: Mutex<bool>,
        cv: Condvar,
        release: tokio::sync::Semaphore,
    }

    impl Gate {
        /// Wait until the mutation reached the gate.
        pub fn wait_reached(&self, timeout: Duration) -> bool {
            let g = self.reached.lock().unwrap();
            let (g, _) = self.cv.wait_timeout_while(g, timeout, |r| !*r).unwrap();
            *g
        }
        /// Let it continue.
        pub fn release(&self) {
            self.release.add_permits(1);
        }
    }

    fn gates() -> &'static Mutex<HashMap<u64, Arc<Gate>>> {
        static GATES: std::sync::OnceLock<Mutex<HashMap<u64, Arc<Gate>>>> =
            std::sync::OnceLock::new();
        GATES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Arm a one-shot gate for `client`'s next mutation.
    pub fn arm(client: aster_coordination_client_t) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            reached: Mutex::new(false),
            cv: Condvar::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        gates().lock().unwrap().insert(client, gate.clone());
        gate
    }

    pub(crate) async fn before_track(client: aster_coordination_client_t) {
        let Some(gate) = gates().lock().unwrap().remove(&client) else {
            return;
        };
        *gate.reached.lock().unwrap() = true;
        gate.cv.notify_all();
        let permit = gate.release.acquire().await;
        if let Ok(p) = permit {
            p.forget();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_status_t::*;

    #[test]
    fn rpc_codes_map_to_the_documented_statuses() {
        let rpc = |code| kind_status(CoordinationErrorKind::Rpc { code });
        assert_eq!(rpc(1), ASTER_STATUS_CANCELLED);
        assert_eq!(rpc(3), ASTER_STATUS_INVALID_ARGUMENT);
        assert_eq!(rpc(4), ASTER_STATUS_TIMEOUT);
        assert_eq!(rpc(5), ASTER_STATUS_NOT_FOUND);
        assert_eq!(rpc(7), ASTER_STATUS_PERMISSION_DENIED);
        assert_eq!(rpc(12), ASTER_STATUS_UNSUPPORTED);
        assert_eq!(rpc(14), ASTER_STATUS_CONNECTION_REFUSED);
        assert_eq!(rpc(16), ASTER_STATUS_UNAUTHENTICATED);
        for other in [0, 2, 6, 8, 9, 10, 11, 13, 15, 99, -1] {
            assert_eq!(rpc(other), ASTER_STATUS_INTERNAL, "rpc {other}");
        }
        use CoordinationErrorKind as K;
        assert_eq!(kind_status(K::Unavailable), ASTER_STATUS_CONNECTION_REFUSED);
        assert_eq!(kind_status(K::Closed), ASTER_STATUS_ALREADY_CLOSED);
        for k in [K::Invalid, K::AwaitingAck, K::Unresolved] {
            assert_eq!(kind_status(k), ASTER_STATUS_INVALID_ARGUMENT);
        }
        for k in [
            K::JournalLocked,
            K::JournalFatal,
            K::Other,
            K::Rejected { reason_code: 3 },
        ] {
            assert_eq!(kind_status(k), ASTER_STATUS_INTERNAL);
        }
        // The kind and the RPC code travel in error_code / flags.
        let e = coord_err(aster::Error::Rpc {
            code: 16,
            message: "who are you".into(),
            details: Vec::new(),
        });
        assert_eq!(e.status, ASTER_STATUS_UNAUTHENTICATED);
        assert_eq!(
            e.code,
            aster_coordination_error_t::ASTER_COORDINATION_ERROR_RPC as i32
        );
        assert_eq!(e.flags, 16);
    }
}
