//! Coordination client -- wraps `aster::coordination::client` (plan P9, D7).
//!
//! The client is **wrapped, never ported**: journaling, redirect routing, the
//! inbound validation boundary and the delivery contract all stay in Rust.
//! This module only converts values and carries handles.
//!
//! Value types are the frozen dataclasses of `aster.coordination`, built here
//! by keyword; failures raise the typed `aster.coordination` exceptions
//! (`CoordinationAwaitingAckError`, `CoordinationRpcError`, ...). Durations
//! are seconds (`float`), as everywhere else in the Python binding.
//!
//! Handles that keep the durable journal alive (resolved mutations and
//! leases) are tracked by their client, so `close()` releases the journal's
//! lifetime lock deterministically instead of waiting for Python GC.
//!
//! The placement manager ([`PlacementManager`]) runs in Rust with Python
//! providers answered by upcall (plan R6): a `PlacementProvisioner`, a
//! `PlacementPolicySource` (or a fixed `PlacementPolicy`) and a
//! `TopologySource` (or the node's own `Topology`). The pure planner is
//! `_coordination_placement_plan`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use pyo3::exceptions::PyStopAsyncIteration;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyTuple};
use pyo3_async_runtimes::tokio::future_into_py;

use aster::coordination::client as cc;
use aster::coordination::client::lease::LeaseAcquireOutcome;
use aster::coordination::client::placement as pl;
use aster::coordination::contract::{
    FenceV1, LogIdV1, MemberSeedV1, NodeIdV1, ResourceKeyV1, ResourceSnapshotV1,
};
use aster::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use aster::coordination::reason::{Outcome, Reason};
use aster::coordination::store::wire as w;

use crate::ids::{NodeId, PublicKey};
use crate::keys::{node_id_out, py_value};
use crate::lease::{deny_out, options_in, snapshot_out as lease_snapshot_out, status_out};
use crate::node::Node;
use crate::topology::TopologySnapshot;

/// The §5.2 hard cap on one `audit_stream` request.
const MAX_AUDIT_PAGE: u32 = 10_000;
const REJECTED: i32 = 100;

// ============================================================================
// Errors
// ============================================================================

/// Build the `aster.coordination` exception for `kind`.
fn coord_error(kind: &str, code: Option<i32>, message: String) -> PyErr {
    let class = match kind {
        "awaiting_ack" => "CoordinationAwaitingAckError",
        "unresolved" => "CoordinationUnresolvedError",
        "journal_fatal" => "CoordinationJournalFatalError",
        "journal_locked" => "CoordinationJournalLockedError",
        "invalid" => "CoordinationInvalidArgumentError",
        "rpc" => "CoordinationRpcError",
        "unavailable" => "CoordinationUnavailableError",
        "closed" => "CoordinationClosedError",
        _ => "CoordinationError",
    };
    Python::attach(|py| {
        let build = || -> PyResult<PyErr> {
            let cls = py.import("aster.coordination")?.getattr(class)?;
            let kw = PyDict::new(py);
            if let Some(code) = code {
                kw.set_item("code", code)?;
            }
            if class == "CoordinationError" {
                kw.set_item("kind", kind)?;
            }
            Ok(PyErr::from_value(cls.call((message,), Some(&kw))?))
        };
        build().unwrap_or_else(|e| e)
    })
}

fn invalid(msg: impl std::fmt::Display) -> PyErr {
    coord_error("invalid", None, msg.to_string())
}

fn closed() -> PyErr {
    coord_error(
        cc::CoordinationErrorKind::Closed.as_str(),
        None,
        "the coordination client is closed".to_string(),
    )
}

/// Map an `aster` error onto the typed exceptions by its structured
/// [`cc::CoordinationErrorKind`] -- never by its message, which passes
/// through unchanged.
pub(crate) fn coord_err(e: aster::Error) -> PyErr {
    match cc::CoordinationErrorKind::of(&e) {
        cc::CoordinationErrorKind::Rpc { code } => coord_error("rpc", Some(code), e.to_string()),
        kind => coord_error(kind.as_str(), None, e.to_string()),
    }
}

/// Test hook: the exception `coord_err` maps a synthetic `aster::Error` to
/// (returned, not raised). `variant` is `invalid_argument`, `rpc`,
/// `transport`, `connection`, or `coordination:<kind>` for a structured
/// client error (`awaiting_ack`, `unresolved`, `journal_fatal`,
/// `journal_locked`, `unavailable`, `closed`, or `rpc` with `code`).
#[pyfunction]
#[pyo3(name = "_coordination_error_for_testing", signature = (variant, message, code=0))]
fn error_for_testing(py: Python<'_>, variant: &str, message: String, code: i32) -> Py<PyAny> {
    use cc::CoordinationErrorKind as K;
    let e = match variant {
        "invalid_argument" => aster::Error::InvalidArgument(message),
        "rpc" => aster::Error::Rpc {
            code,
            message,
            details: Vec::new(),
        },
        "transport" => aster::Error::Transport(anyhow::anyhow!(message)),
        structured if structured.starts_with("coordination:") => {
            let kind = match &structured["coordination:".len()..] {
                "awaiting_ack" => K::AwaitingAck,
                "unresolved" => K::Unresolved,
                "journal_fatal" => K::JournalFatal,
                "journal_locked" => K::JournalLocked,
                "unavailable" => K::Unavailable,
                "closed" => K::Closed,
                "rpc" => K::Rpc { code },
                _ => K::Other,
            };
            cc::CoordinationError::new(kind, message).into()
        }
        _ => aster::Error::Connection(message),
    };
    coord_err(e).into_value(py).into_any()
}

// ============================================================================
// Scalar and id conversions
// ============================================================================

fn u64_in(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<u64> {
    obj.extract::<u64>()
        .map_err(|_| invalid(format!("{what} must be a non-negative int")))
}

fn u64_attr(obj: &Bound<'_, PyAny>, attr: &str, what: &str) -> PyResult<u64> {
    u64_in(&attr_of(obj, attr, what)?, what)
}

fn attr_of<'py>(obj: &Bound<'py, PyAny>, attr: &str, what: &str) -> PyResult<Bound<'py, PyAny>> {
    obj.getattr(attr)
        .map_err(|_| invalid(format!("{what}: expected an object with `{attr}`")))
}

fn opt_attr<'py>(obj: &Bound<'py, PyAny>, attr: &str) -> Option<Bound<'py, PyAny>> {
    obj.getattr(attr).ok().filter(|v| !v.is_none())
}

/// Whole milliseconds from non-negative, finite seconds.
fn millis_in(secs: f64, what: &str) -> PyResult<u64> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(invalid(format!(
            "{what} must be a non-negative number of seconds"
        )));
    }
    Ok(Duration::from_secs_f64(secs).as_millis() as u64)
}

fn secs_out(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

/// Raw bytes of an id: `NodeId`, `PublicKey`, `bytes`, or a hex string.
fn id_bytes(obj: &Bound<'_, PyAny>) -> Option<Vec<u8>> {
    if let Ok(id) = obj.cast::<NodeId>() {
        return hex::decode(&id.get().hex).ok();
    }
    if let Ok(k) = obj.cast::<PublicKey>() {
        return hex::decode(k.get().inner.to_hex()).ok();
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Some(b.as_bytes().to_vec());
    }
    obj.extract::<String>()
        .ok()
        .and_then(|s| hex::decode(s.trim()).ok())
}

pub(crate) fn id32(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<[u8; 32]> {
    id_bytes(obj)
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or_else(|| invalid(format!("{what} must be 64 hex characters or 32 bytes")))
}

fn id16(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<[u8; 16]> {
    id_bytes(obj)
        .and_then(|v| <[u8; 16]>::try_from(v).ok())
        .ok_or_else(|| invalid(format!("{what} must be 32 hex characters or 16 bytes")))
}

pub(crate) fn node_id_in(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<CoordinationNodeId> {
    id32(obj, what).map(CoordinationNodeId::from_array)
}

pub(crate) fn group_in(obj: &Bound<'_, PyAny>) -> PyResult<CoordinationGroupId> {
    id32(obj, "group id").map(CoordinationGroupId::from_array)
}

fn bytes_in(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Vec<u8>> {
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(b.as_bytes().to_vec());
    }
    if let Ok(s) = obj.extract::<String>() {
        return Ok(s.into_bytes());
    }
    obj.extract::<Vec<u8>>()
        .map_err(|_| invalid(format!("{what} must be bytes or str")))
}

fn node_ids_in(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Vec<[u8; 32]>> {
    let mut out = Vec::new();
    for item in obj
        .try_iter()
        .map_err(|_| invalid(format!("{what} must be an iterable of node ids")))?
    {
        out.push(id32(&item?, what)?);
    }
    Ok(out)
}

fn opt_node_set(
    obj: Option<Bound<'_, PyAny>>,
    what: &str,
) -> PyResult<BTreeSet<CoordinationNodeId>> {
    match obj {
        None => Ok(BTreeSet::new()),
        Some(o) => Ok(node_ids_in(&o, what)?
            .into_iter()
            .map(CoordinationNodeId::from_array)
            .collect()),
    }
}

// ============================================================================
// Value conversion: Python -> wire
// ============================================================================

/// A `ResourceKey` (or anything with `namespace` / `key`), or a
/// `(namespace, key)` tuple; a `str` key is UTF-8.
fn resource_in(obj: &Bound<'_, PyAny>) -> PyResult<ResourceKeyV1> {
    let (ns, key) = if let Ok(t) = obj.cast::<PyTuple>() {
        if t.len() != 2 {
            return Err(invalid("a resource tuple is (namespace, key)"));
        }
        (t.get_item(0)?, t.get_item(1)?)
    } else {
        (
            attr_of(obj, "namespace", "resource")?,
            attr_of(obj, "key", "resource")?,
        )
    };
    Ok(ResourceKeyV1 {
        namespace: ns
            .extract()
            .map_err(|_| invalid("resource namespace must be a str"))?,
        key: bytes_in(&key, "resource key")?,
    })
}

fn fence_in(obj: &Bound<'_, PyAny>) -> PyResult<FenceV1> {
    Ok(FenceV1 {
        group_id: group_in(&attr_of(obj, "group_id", "fence")?)?.to_vec(),
        resource: resource_in(&attr_of(obj, "resource", "fence")?)?,
        epoch: u64_attr(obj, "epoch", "fence.epoch")?,
        holder: id32(&attr_of(obj, "holder", "fence")?, "fence.holder")?.to_vec(),
    })
}

pub(crate) fn log_in(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Option<LogIdV1>> {
    let Some(obj) = obj.filter(|o| !o.is_none()) else {
        return Ok(None);
    };
    Ok(Some(LogIdV1 {
        term: u64_attr(obj, "term", "log_id.term")?,
        leader_node_id: id32(
            &attr_of(obj, "leader_node_id", "log_id")?,
            "log_id.leader_node_id",
        )?
        .to_vec(),
        index: u64_attr(obj, "index", "log_id.index")?,
    }))
}

pub(crate) fn seed_in(obj: &Bound<'_, PyAny>) -> PyResult<MemberSeedV1> {
    Ok(MemberSeedV1 {
        node_id: id32(
            &attr_of(obj, "node_id", "member seed")?,
            "member_seed.node_id",
        )?
        .to_vec(),
        ticket: attr_of(obj, "ticket", "member seed")?
            .extract()
            .map_err(|_| invalid("member_seed.ticket must be a str"))?,
        signed_join_permit: opt_attr(obj, "signed_join_permit")
            .map(|p| bytes_in(&p, "member_seed.signed_join_permit"))
            .transpose()?,
    })
}

fn seeds_in(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<MemberSeedV1>> {
    let mut out = Vec::new();
    if let Some(obj) = obj.filter(|o| !o.is_none()) {
        for item in obj
            .try_iter()
            .map_err(|_| invalid("member_seeds must be an iterable of MemberSeed"))?
        {
            out.push(seed_in(&item?)?);
        }
    }
    Ok(out)
}

fn wire_node_ids(ids: Vec<[u8; 32]>) -> Vec<NodeIdV1> {
    ids.into_iter()
        .map(|id| NodeIdV1 {
            node_id: id.to_vec(),
        })
        .collect()
}

// ============================================================================
// Value conversion: wire -> Python
// ============================================================================

pub(crate) fn nid(v: &[u8]) -> NodeId {
    node_id_out(hex::encode(v))
}

fn opt_nid(v: &Option<Vec<u8>>) -> Option<NodeId> {
    v.as_deref().map(nid)
}

fn opt_hex(v: &Option<Vec<u8>>) -> Option<String> {
    v.as_ref().map(hex::encode)
}

fn nids<'py>(py: Python<'py>, v: &[NodeIdV1]) -> PyResult<Bound<'py, PyTuple>> {
    PyTuple::new(py, v.iter().map(|n| nid(&n.node_id)))
}

pub(crate) fn hex_ids<'py, 'a>(
    py: Python<'py>,
    v: impl IntoIterator<Item = &'a CoordinationNodeId>,
) -> PyResult<Bound<'py, PyTuple>> {
    let ids: Vec<NodeId> = v.into_iter().map(|id| node_id_out(id.to_hex())).collect();
    PyTuple::new(py, ids)
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

fn outcome_name(code: i32) -> String {
    Outcome::from_code(code)
        .map(|o| snake(&format!("{o:?}")))
        .unwrap_or_else(|| "unknown".into())
}

fn reason_name(code: i32) -> String {
    Reason::from_code(code)
        .map(|r| snake(&format!("{r:?}")))
        .unwrap_or_else(|| "unknown".into())
}

fn resource_out<'py>(py: Python<'py>, r: &ResourceKeyV1) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "ResourceKey" {
        namespace: r.namespace.clone(),
        key: PyBytes::new(py, &r.key),
    })
}

pub(crate) fn log_out<'py>(py: Python<'py>, l: &LogIdV1) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "LogId" {
        term: l.term,
        leader_node_id: nid(&l.leader_node_id),
        index: l.index,
    })
}

pub(crate) fn opt_log_out<'py>(
    py: Python<'py>,
    l: &Option<LogIdV1>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    l.as_ref().map(|l| log_out(py, l)).transpose()
}

fn fence_out<'py>(py: Python<'py>, f: &FenceV1) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "CoordinationFence" {
        group_id: hex::encode(&f.group_id),
        resource: resource_out(py, &f.resource)?,
        epoch: f.epoch,
        holder: nid(&f.holder),
    })
}

fn snapshot_out<'py>(py: Python<'py>, s: &ResourceSnapshotV1) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "ResourceSnapshot" {
        group_id: hex::encode(&s.group_id),
        resource: resource_out(py, &s.resource)?,
        epoch: s.epoch,
        holder: opt_nid(&s.holder),
        held: s.held,
        ttl: secs_out(s.ttl_ms),
        lease_revision: s.lease_revision,
        value_revision: s.value_revision,
        acl_revision: s.acl_revision,
        value: s.value.as_deref().map(|v| PyBytes::new(py, v)),
        value_redacted: s.value_redacted,
    })
}

fn reply_out<'py>(py: Python<'py>, r: &w::MutationReplyWireV1) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "MutationReply" {
        group_id: hex::encode(&r.group_id),
        client_id: hex::encode(&r.client_id),
        sequence: r.sequence,
        request_hash: hex::encode(&r.request_hash),
        consumed: r.disposition_code == 1,
        disposition_code: r.disposition_code,
        outcome: outcome_name(r.outcome_code),
        outcome_code: r.outcome_code,
        reason: reason_name(r.reason_code),
        reason_code: r.reason_code,
        snapshot: r.snapshot.as_ref().map(|s| snapshot_out(py, s)).transpose()?,
        acl_revision: r.acl_revision,
        admin_revision: r.admin_revision,
        operation_id: opt_hex(&r.operation_id),
        membership_log_id: opt_log_out(py, &r.membership_log_id)?,
    })
}

fn rejected_error(py: Python<'_>, reply: &Bound<'_, PyAny>) -> PyResult<PyErr> {
    let cls = py
        .import("aster.coordination")?
        .getattr("CoordinationRejectedError")?;
    Ok(PyErr::from_value(cls.call1((reply,))?))
}

/// A lease-path reply: returned when the mutation took effect, raised as
/// `CoordinationRejectedError` on a typed rejection.
fn lease_reply(r: w::MutationReplyWireV1) -> PyResult<Py<PyAny>> {
    Python::attach(|py| {
        let reply = reply_out(py, &r)?;
        if r.outcome_code == REJECTED {
            return Err(rejected_error(py, &reply)?);
        }
        Ok(reply.unbind())
    })
}

fn group_status_out<'py>(
    py: Python<'py>,
    s: &w::GroupStatusReplyWireV1,
) -> PyResult<Bound<'py, PyAny>> {
    let recovered = s
        .recovered_from
        .as_ref()
        .map(|r| {
            py_value!(py, "aster.coordination", "RecoveredFrom" {
                source_group_id: hex::encode(&r.source_group_id),
                source_genesis_hash: hex::encode(&r.source_genesis_hash),
                source_authority_public_key: hex::encode(&r.source_authority_public_key),
                source_authority_domain: r.source_authority_domain.clone(),
                export_hash: hex::encode(&r.export_hash),
                source_last_applied: opt_log_out(py, &r.source_last_applied)?,
            })
        })
        .transpose()?;
    py_value!(py, "aster.coordination", "GroupStatus" {
        group_id: hex::encode(&s.group_id),
        lifecycle_code: s.lifecycle_code,
        local_node_id: nid(&s.local_node_id),
        local_role_code: s.local_role_code,
        leader_node_id: opt_nid(&s.leader_node_id),
        term: s.term,
        last_log_id: opt_log_out(py, &s.last_log_id)?,
        committed_log_id: opt_log_out(py, &s.committed_log_id)?,
        applied_log_id: opt_log_out(py, &s.applied_log_id)?,
        deployment_profile_code: s.deployment_profile_code,
        membership_log_id: opt_log_out(py, &s.membership_log_id)?,
        membership_joint: s.membership_joint,
        voters: nids(py, &s.voters)?,
        learners: nids(py, &s.learners)?,
        pending_operation_id: opt_hex(&s.pending_operation_id),
        quorum_available: s.quorum_available,
        fatal_code: s.fatal_code,
        audit_floor: opt_log_out(py, &s.audit_floor)?,
        genesis_hash: hex::encode(&s.genesis_hash),
        authority_public_key: hex::encode(&s.authority_public_key),
        authority_domain: s.authority_domain.clone(),
        recovered_from: recovered,
    })
}

fn audit_out<'py>(py: Python<'py>, r: &w::AuditRecordWireV1) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "AuditRecord" {
        log_id: log_out(py, &r.log_id)?,
        actor_node_id: opt_nid(&r.actor_node_id),
        operation_code: r.operation_code,
        resource: r.resource.as_ref().map(|k| resource_out(py, k)).transpose()?,
        result_code: r.result_code,
        epoch: r.epoch,
        value_revision: r.value_revision,
    })
}

fn manifest_out<'py>(
    py: Python<'py>,
    m: &w::RecoveryExportManifestWireV1,
) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "ExportManifest" {
        total_len: m.total_len,
        blake3_checksum: hex::encode(&m.blake3_checksum),
        applied_log_id: opt_log_out(py, &m.applied_log_id)?,
    })
}

pub(crate) fn seed_out<'py>(py: Python<'py>, s: &MemberSeedV1) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "MemberSeed" {
        node_id: nid(&s.node_id),
        ticket: s.ticket.clone(),
        signed_join_permit: s.signed_join_permit.as_deref().map(|p| PyBytes::new(py, p)),
    })
}

/// Run `f` with the GIL and hand back an owned Python value.
fn to_py<F>(f: F) -> PyResult<Py<PyAny>>
where
    F: for<'py> FnOnce(Python<'py>) -> PyResult<Bound<'py, PyAny>>,
{
    Python::attach(|py| f(py).map(Bound::unbind))
}

// ============================================================================
// Code tables
// ============================================================================

/// The §5.2 reason-code table, straight from the Rust enum.
#[pyfunction]
#[pyo3(name = "_coordination_reason_codes")]
fn reason_codes() -> Vec<(i32, String)> {
    Reason::ALL
        .iter()
        .map(|r| (r.code(), snake(&format!("{r:?}"))))
        .collect()
}

/// The §5.2 outcome-code table, straight from the Rust enum.
#[pyfunction]
#[pyo3(name = "_coordination_outcome_codes")]
fn outcome_codes() -> Vec<(i32, String)> {
    Outcome::ALL
        .iter()
        .map(|o| (o.code(), snake(&format!("{o:?}"))))
        .collect()
}

// ============================================================================
// Client state
// ============================================================================

type ResolvedSlot = Mutex<Option<cc::ResolvedMutation>>;
type LeaseSlot = Mutex<Option<Arc<cc::lease::CoordinationLease>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Everything that keeps the journal's lifetime lock alive, so `close()` can
/// drop it all. No lock here is ever held while taking the GIL.
struct ClientState {
    client: Mutex<Option<cc::CoordinationClient>>,
    resolved: Mutex<Vec<Weak<ResolvedSlot>>>,
    leases: Mutex<Vec<Weak<LeaseSlot>>>,
}

impl ClientState {
    fn new(client: cc::CoordinationClient) -> Self {
        Self {
            client: Mutex::new(Some(client)),
            resolved: Mutex::new(Vec::new()),
            leases: Mutex::new(Vec::new()),
        }
    }

    fn client(&self) -> PyResult<cc::CoordinationClient> {
        let client = lock(&self.client).clone();
        client.ok_or_else(closed)
    }

    /// Register a resolved mutation -- unless the client closed meanwhile, in
    /// which case the handle is dropped (its result stays journaled and is
    /// redelivered by `unacknowledged()` after reopening).
    fn track(&self, resolved: cc::ResolvedMutation) -> PyResult<ResolvedMutation> {
        let wire = resolved.reply().clone();
        let slot = {
            let client = lock(&self.client);
            if client.is_none() {
                None
            } else {
                let slot = Arc::new(Mutex::new(Some(resolved)));
                let mut list = lock(&self.resolved);
                list.retain(|w| w.strong_count() > 0);
                list.push(Arc::downgrade(&slot));
                Some(slot)
            }
        };
        let slot = slot.ok_or_else(closed)?;
        let reply = to_py(|py| reply_out(py, &wire))?;
        Ok(ResolvedMutation { slot, reply })
    }

    fn track_lease(
        &self,
        lease: cc::lease::CoordinationLease,
        resource: ResourceKeyV1,
    ) -> PyResult<CoordinationLease> {
        let slot = {
            let client = lock(&self.client);
            if client.is_none() {
                None
            } else {
                let slot = Arc::new(Mutex::new(Some(Arc::new(lease))));
                let mut list = lock(&self.leases);
                list.retain(|w| w.strong_count() > 0);
                list.push(Arc::downgrade(&slot));
                Some(slot)
            }
        };
        Ok(CoordinationLease {
            slot: slot.ok_or_else(closed)?,
            resource,
        })
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

fn opt_resolved(
    state: &ClientState,
    r: aster::Result<Option<cc::ResolvedMutation>>,
) -> PyResult<Option<ResolvedMutation>> {
    r.map_err(coord_err)?.map(|r| state.track(r)).transpose()
}

// ============================================================================
// CoordinationClient
// ============================================================================

/// The durable, journaled coordination client -- the only way to mutate.
/// Binds the node's identity to one group and owns the journal's lifetime
/// lock until `close()` (also `async with`).
#[pyclass(frozen, module = "aster._aster")]
pub struct CoordinationClient {
    state: Arc<ClientState>,
}

impl CoordinationClient {
    /// The wrapped client (shares the journal), or `CoordinationClosedError`.
    pub(crate) fn inner_client(&self) -> PyResult<cc::CoordinationClient> {
        self.state.client()
    }

    /// One journaled mutation resolving to a tracked `ResolvedMutation`.
    fn mutate<'py, F, Fut>(&self, py: Python<'py>, f: F) -> PyResult<Bound<'py, PyAny>>
    where
        F: FnOnce(cc::CoordinationClient) -> Fut + Send + 'static,
        Fut: Future<Output = aster::Result<cc::ResolvedMutation>> + Send + 'static,
    {
        let c = self.state.client()?;
        let state = self.state.clone();
        future_into_py(
            py,
            async move { state.track(f(c).await.map_err(coord_err)?) },
        )
    }
}

#[pymethods]
impl CoordinationClient {
    /// Open the durable client for `group` (hex) on `node`. `journal_dir` is
    /// the durable journal directory -- persist it with the node identity.
    /// `seed_tickets` are Open member tickets, routing hints only. Awaitable;
    /// raises `CoordinationJournalLockedError` if the journal is held
    /// elsewhere.
    #[staticmethod]
    #[pyo3(signature = (node, group, *, journal_dir, seed_tickets=None))]
    fn connect<'py>(
        py: Python<'py>,
        node: &Bound<'py, Node>,
        group: &Bound<'py, PyAny>,
        journal_dir: PathBuf,
        seed_tickets: Option<Vec<String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let group = group_in(group)?;
        let node = node.borrow().inner.clone();
        future_into_py(py, async move {
            let client = cc::CoordinationClient::builder(node, group)
                .seed_tickets(seed_tickets.unwrap_or_default())
                .journal_dir(journal_dir)
                .build()
                .map_err(coord_err)?;
            Ok(CoordinationClient {
                state: Arc::new(ClientState::new(client)),
            })
        })
    }

    /// The group this client is bound to (hex).
    #[getter]
    fn group_id(&self) -> PyResult<String> {
        Ok(self.state.client()?.group().to_hex())
    }

    /// This client's actor identity, as the group authenticates it.
    #[getter]
    fn local_id(&self) -> PyResult<NodeId> {
        Ok(nid(&self.state.client()?.local_id().map_err(coord_err)?))
    }

    /// `True` once `close()` ran.
    #[getter]
    fn closed(&self) -> bool {
        lock(&self.state.client).is_none()
    }

    /// The journal's current client id (hex). Diagnostic only. Awaitable.
    fn client_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let c = self.state.client()?;
        future_into_py(py, async move { Ok(hex::encode(c.client_id().await)) })
    }

    /// The result a previous run resolved and never acknowledged, or `None`.
    /// Call on startup, before any mutation. Awaitable.
    fn unacknowledged<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let c = self.state.client()?;
        let state = self.state.clone();
        future_into_py(
            py,
            async move { opt_resolved(&state, c.unacknowledged().await) },
        )
    }

    /// Drive any half-finished request from a previous run to resolution;
    /// `None` means the journal is ready for new mutations. Awaitable.
    fn resume<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let c = self.state.client()?;
        let state = self.state.clone();
        future_into_py(py, async move { opt_resolved(&state, c.resume().await) })
    }

    /// `acquire` (§5.2 code 1) for `ttl` seconds. Awaitable `ResolvedMutation`.
    fn acquire<'py>(
        &self,
        py: Python<'py>,
        resource: &Bound<'py, PyAny>,
        ttl: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let resource = resource_in(resource)?;
        let ttl = millis_in(ttl, "ttl")?;
        self.mutate(py, move |c| async move { c.acquire(resource, ttl).await })
    }

    /// `renew` (§5.2 code 2).
    fn renew<'py>(
        &self,
        py: Python<'py>,
        fence: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fence = fence_in(fence)?;
        self.mutate(py, move |c| async move { c.renew(fence).await })
    }

    /// `compare_and_set` (§5.2 code 3): fence + expected value revision.
    fn compare_and_set<'py>(
        &self,
        py: Python<'py>,
        fence: &Bound<'py, PyAny>,
        expected_value_revision: &Bound<'py, PyAny>,
        new_value: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fence = fence_in(fence)?;
        let expected = u64_in(expected_value_revision, "expected_value_revision")?;
        let value = bytes_in(new_value, "new_value")?;
        self.mutate(py, move |c| async move {
            c.compare_and_set(fence, expected, value).await
        })
    }

    /// `release` (§5.2 code 4).
    fn release<'py>(
        &self,
        py: Python<'py>,
        fence: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fence = fence_in(fence)?;
        self.mutate(py, move |c| async move { c.release(fence).await })
    }

    /// `release_with_value` (§5.2 code 5): the atomic final-value handoff.
    fn release_with_value<'py>(
        &self,
        py: Python<'py>,
        fence: &Bound<'py, PyAny>,
        expected_value_revision: &Bound<'py, PyAny>,
        final_value: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fence = fence_in(fence)?;
        let expected = u64_in(expected_value_revision, "expected_value_revision")?;
        let value = bytes_in(final_value, "final_value")?;
        self.mutate(py, move |c| async move {
            c.release_with_value(fence, expected, value).await
        })
    }

    /// `revoke` (§5.2 code 6); unconditional unless `expected_epoch` is given.
    #[pyo3(signature = (resource, *, expected_epoch=None))]
    fn revoke<'py>(
        &self,
        py: Python<'py>,
        resource: &Bound<'py, PyAny>,
        expected_epoch: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let resource = resource_in(resource)?;
        let expected = expected_epoch
            .filter(|e| !e.is_none())
            .map(|e| u64_in(e, "expected_epoch"))
            .transpose()?;
        self.mutate(
            py,
            move |c| async move { c.revoke(resource, expected).await },
        )
    }

    /// `set_grant` (§5.2 code 7): exact `CoordinationRights` bits for one node.
    fn set_grant<'py>(
        &self,
        py: Python<'py>,
        resource: &Bound<'py, PyAny>,
        target_node_id: &Bound<'py, PyAny>,
        rights: &Bound<'py, PyAny>,
        expected_acl_revision: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let resource = resource_in(resource)?;
        let target = id32(target_node_id, "target_node_id")?;
        let rights = u64_in(rights, "rights")?;
        let expected = u64_in(expected_acl_revision, "expected_acl_revision")?;
        self.mutate(py, move |c| async move {
            c.set_grant(resource, target, rights, expected).await
        })
    }

    /// `set_admin` (§5.2 code 8).
    fn set_admin<'py>(
        &self,
        py: Python<'py>,
        target_node_id: &Bound<'py, PyAny>,
        enabled: bool,
        expected_admin_revision: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let target = id32(target_node_id, "target_node_id")?;
        let expected = u64_in(expected_admin_revision, "expected_admin_revision")?;
        self.mutate(py, move |c| async move {
            c.set_admin(target, enabled, expected).await
        })
    }

    /// `change_membership` (§5.2 code 9), validated against a fresh status
    /// cut first. `desired_voters` is an iterable of node ids -- or a
    /// `PlacementPlan`, whose voters, learners, seeds and expected
    /// membership log id are then used.
    #[pyo3(signature = (
        operation_id, desired_voters, *, expected_membership_log_id=None,
        retained_learners=None, member_seeds=None
    ))]
    fn change_membership<'py>(
        &self,
        py: Python<'py>,
        operation_id: &Bound<'py, PyAny>,
        desired_voters: &Bound<'py, PyAny>,
        expected_membership_log_id: Option<&Bound<'py, PyAny>>,
        retained_learners: Option<&Bound<'py, PyAny>>,
        member_seeds: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let op = id16(operation_id, "operation_id")?;
        let (voters, learners, seeds, expected) = if desired_voters.hasattr("desired_voters")? {
            let plan = desired_voters;
            (
                node_ids_in(&plan.getattr("desired_voters")?, "desired_voters")?,
                opt_attr(plan, "retained_learners")
                    .map(|l| node_ids_in(&l, "retained_learners"))
                    .transpose()?
                    .unwrap_or_default(),
                seeds_in(opt_attr(plan, "member_seeds").as_ref())?,
                log_in(opt_attr(plan, "expected_membership_log_id").as_ref())?,
            )
        } else {
            (
                node_ids_in(desired_voters, "desired_voters")?,
                retained_learners
                    .filter(|l| !l.is_none())
                    .map(|l| node_ids_in(l, "retained_learners"))
                    .transpose()?
                    .unwrap_or_default(),
                seeds_in(member_seeds)?,
                log_in(expected_membership_log_id)?,
            )
        };
        let voters = wire_node_ids(voters);
        let learners = wire_node_ids(learners);
        self.mutate(py, move |c| async move {
            c.change_membership(op, expected, voters, learners, seeds)
                .await
        })
    }

    /// `cancel_membership` (§5.2 code 11).
    fn cancel_membership<'py>(
        &self,
        py: Python<'py>,
        operation_id: &Bound<'py, PyAny>,
        expected_phase_code: i32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let op = id16(operation_id, "operation_id")?;
        self.mutate(py, move |c| async move {
            c.cancel_membership(op, expected_phase_code).await
        })
    }

    /// Retire one of this actor's *other* client ids (operational cleanup).
    /// Awaitable `RetireReply`.
    fn retire_other_client<'py>(
        &self,
        py: Python<'py>,
        target_client_id: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let target = id16(target_client_id, "target_client_id")?;
        let c = self.state.client()?;
        future_into_py(py, async move {
            let r = c.retire_other_client(target).await.map_err(coord_err)?;
            to_py(|py| {
                py_value!(py, "aster.coordination", "RetireReply" {
                    outcome_code: r.outcome_code,
                    reason_code: r.reason_code,
                })
            })
        })
    }

    /// One committed acquire; on grant, Rust renews in the background and
    /// the fence stops at the self-limit. `options` is an
    /// `aster.LeaseOptions` (seconds). Lease transitions are journaled and
    /// acknowledged automatically. Awaitable `CoordinationLeaseOutcome`.
    #[pyo3(signature = (resource, options=None))]
    fn acquire_lease<'py>(
        &self,
        py: Python<'py>,
        resource: &Bound<'py, PyAny>,
        options: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wire = resource_in(resource)?;
        let opts = options_in(options)?;
        let c = self.state.client()?;
        let state = self.state.clone();
        future_into_py(py, async move {
            let outcome = cc::lease::CoordinationLease::acquire(c, wire.clone(), opts)
                .await
                .map_err(coord_err)?;
            match outcome {
                LeaseAcquireOutcome::Granted(lease) => {
                    let lease = state.track_lease(lease, wire)?;
                    to_py(|py| {
                        let lease = Py::new(py, lease)?;
                        py_value!(py, "aster.coordination", "CoordinationLeaseOutcome" {
                            granted: true,
                            lease: lease,
                        })
                    })
                }
                LeaseAcquireOutcome::Denied(reason, snapshot) => to_py(|py| {
                    py_value!(py, "aster.coordination", "CoordinationLeaseOutcome" {
                        granted: false,
                        reason: deny_out(reason),
                        snapshot: lease_snapshot_out(py, snapshot)?,
                    })
                }),
            }
        })
    }

    /// The read/status surface, sharing this client's routing.
    fn reads(&self) -> PyResult<CoordinationReadClient> {
        Ok(CoordinationReadClient {
            inner: self.state.client()?.reads(),
        })
    }

    /// Release the journal lock. Outstanding `ResolvedMutation` and
    /// `CoordinationLease` handles become inert (unacknowledged results stay
    /// journaled for the next open). Idempotent.
    fn close(&self) {
        self.state.close();
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let obj: Py<PyAny> = slf.into_any().unbind();
        future_into_py(py, async move { Ok(obj) })
    }

    #[pyo3(signature = (_exc_type=None, _exc=None, _tb=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Bound<'py, PyAny>>,
        _exc: Option<Bound<'py, PyAny>>,
        _tb: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.state.close();
        future_into_py(py, async move { Ok(false) })
    }

    fn __repr__(&self) -> String {
        match lock(&self.state.client).as_ref() {
            Some(c) => format!("CoordinationClient(group_id='{}')", c.group().to_hex()),
            None => "CoordinationClient(closed)".into(),
        }
    }
}

// ============================================================================
// ResolvedMutation
// ============================================================================

/// A resolved mutation: fsynced `ResolvedAwaitingAck` before it was
/// returned. `await acknowledge()` once the application has durably handled
/// it. Dropping it unacknowledged is safe -- the same reply is redelivered by
/// `CoordinationClient.unacknowledged()`, including after a restart. A typed
/// rejection (`rejected`) is still a resolved result and needs acknowledging.
#[pyclass(frozen, module = "aster._aster")]
pub struct ResolvedMutation {
    slot: Arc<ResolvedSlot>,
    reply: Py<PyAny>,
}

impl ResolvedMutation {
    fn field<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
        self.reply.bind(py).getattr(name)
    }
}

#[pymethods]
impl ResolvedMutation {
    /// The exact reply the journal holds (`MutationReply`).
    #[getter]
    fn reply(&self, py: Python<'_>) -> Py<PyAny> {
        self.reply.clone_ref(py)
    }

    #[getter]
    fn outcome<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.field(py, "outcome")
    }

    #[getter]
    fn reason<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.field(py, "reason")
    }

    #[getter]
    fn sequence<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.field(py, "sequence")
    }

    #[getter]
    fn snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.field(py, "snapshot")
    }

    /// A committed typed rejection (§5.2 outcome 100; still needs acknowledging).
    #[getter]
    fn rejected(&self, py: Python<'_>) -> PyResult<bool> {
        Ok(self.field(py, "outcome_code")?.extract::<i32>()? == REJECTED)
    }

    /// `False` once acknowledged (or the client closed).
    #[getter]
    fn pending(&self) -> bool {
        lock(&self.slot).is_some()
    }

    /// The rejection as a `CoordinationRejectedError` (or `None` when the
    /// mutation took effect). Returned, not raised.
    fn rejection(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if !self.rejected(py)? {
            return Ok(None);
        }
        Ok(Some(
            rejected_error(py, self.reply.bind(py))?
                .into_value(py)
                .into_any(),
        ))
    }

    /// Free the journal for the next sequence. Awaitable; raises
    /// `CoordinationClosedError` if already acknowledged or the client closed.
    fn acknowledge<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let resolved = lock(&self.slot).take();
        future_into_py(py, async move {
            let resolved = resolved.ok_or_else(|| {
                coord_error(
                    "closed",
                    None,
                    "this result was already acknowledged, or its client is closed".into(),
                )
            })?;
            resolved.acknowledge().await.map_err(coord_err)
        })
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "ResolvedMutation(outcome={}, reason={}, sequence={}, pending={})",
            self.field(py, "outcome")?.repr()?,
            self.field(py, "reason")?.repr()?,
            self.field(py, "sequence")?,
            if self.pending() { "True" } else { "False" },
        ))
    }
}

// ============================================================================
// CoordinationLease
// ============================================================================

/// A held fenced register lease (§9.4): renewed in Rust, fence gated by the
/// holder's self-limit. Lease transitions are journaled and acknowledged
/// automatically. `async with lease:` releases on exit.
#[pyclass(frozen, module = "aster._aster")]
pub struct CoordinationLease {
    slot: Arc<LeaseSlot>,
    resource: ResourceKeyV1,
}

impl CoordinationLease {
    fn lease(&self) -> Option<Arc<cc::lease::CoordinationLease>> {
        lock(&self.slot).clone()
    }

    fn take(&self) -> PyResult<cc::lease::CoordinationLease> {
        let arc = lock(&self.slot).take();
        let arc =
            arc.ok_or_else(|| invalid("the lease was already released, or its client is closed"))?;
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

    fn release_fut<'py>(&self, py: Python<'py>, ret: bool) -> PyResult<Bound<'py, PyAny>> {
        let lease = if lock(&self.slot).is_none() {
            None
        } else {
            Some(self.take()?)
        };
        future_into_py(py, async move {
            if let Some(lease) = lease {
                lease.release().await.map_err(coord_err)?;
            }
            Ok(if ret { Some(false) } else { None })
        })
    }
}

#[pymethods]
impl CoordinationLease {
    /// The leased resource (`ResourceKey`).
    #[getter]
    fn resource<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        resource_out(py, &self.resource)
    }

    /// `"held"`, `"released"`, `"fenced"` or `"lost"` (`"released"` once
    /// handed back or its client closed).
    #[getter]
    fn status(&self) -> &'static str {
        match self.lease() {
            Some(l) => status_out(l.status()),
            None => "released",
        }
    }

    /// The `CoordinationFence` for a protected action, or `None` once the
    /// holder must stop. Ask immediately before each action; never cache it
    /// across awaits.
    fn fence<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        let fence = self.lease().and_then(|l| l.fence());
        fence.map(|f| fence_out(py, &f)).transpose()
    }

    /// `compare_and_set` under the current fence. Awaitable `MutationReply`;
    /// raises `CoordinationRejectedError` on a typed rejection.
    fn compare_and_set<'py>(
        &self,
        py: Python<'py>,
        expected_value_revision: &Bound<'py, PyAny>,
        new_value: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lease = self
            .lease()
            .ok_or_else(|| invalid("the lease was released, or its client is closed"))?;
        let expected = u64_in(expected_value_revision, "expected_value_revision")?;
        let value = bytes_in(new_value, "new_value")?;
        future_into_py(py, async move {
            let reply = lease
                .compare_and_set(expected, value)
                .await
                .map_err(coord_err)?;
            lease_reply(reply)
        })
    }

    /// Graceful release. Awaitable; idempotent.
    fn release<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.release_fut(py, false)
    }

    /// The atomic final handoff: write `final_value` and release together,
    /// or neither. Awaitable `MutationReply`; raises
    /// `CoordinationRejectedError` on a typed rejection.
    fn release_with_value<'py>(
        &self,
        py: Python<'py>,
        expected_value_revision: &Bound<'py, PyAny>,
        final_value: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let expected = u64_in(expected_value_revision, "expected_value_revision")?;
        let value = bytes_in(final_value, "final_value")?;
        let lease = self.take()?;
        future_into_py(py, async move {
            let reply = lease
                .release_with_value(expected, value)
                .await
                .map_err(coord_err)?;
            lease_reply(reply)
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let obj: Py<PyAny> = slf.into_any().unbind();
        future_into_py(py, async move { Ok(obj) })
    }

    #[pyo3(signature = (_exc_type=None, _exc=None, _tb=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Bound<'py, PyAny>>,
        _exc: Option<Bound<'py, PyAny>>,
        _tb: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.release_fut(py, true)
    }

    fn __repr__(&self) -> String {
        format!(
            "CoordinationLease(namespace={:?}, status={:?})",
            self.resource.namespace,
            self.status()
        )
    }
}

// ============================================================================
// CoordinationReadClient
// ============================================================================

/// Linearizable reads and status -- no journal, no mutation.
#[pyclass(frozen, module = "aster._aster")]
pub struct CoordinationReadClient {
    inner: cc::CoordinationReadClient,
}

#[pymethods]
impl CoordinationReadClient {
    /// Open a read-only client for `group` (hex) on `node`. Awaitable.
    #[staticmethod]
    #[pyo3(signature = (node, group, *, seed_tickets=None))]
    fn connect<'py>(
        py: Python<'py>,
        node: &Bound<'py, Node>,
        group: &Bound<'py, PyAny>,
        seed_tickets: Option<Vec<String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let group = group_in(group)?;
        let node = node.borrow().inner.clone();
        future_into_py(py, async move {
            let inner = cc::CoordinationClient::builder(node, group)
                .seed_tickets(seed_tickets.unwrap_or_default())
                .build_read_only()
                .map_err(coord_err)?;
            Ok(CoordinationReadClient { inner })
        })
    }

    /// The committed row at a leader-barriered cut (`ResourceSnapshot`).
    fn read_linearizable<'py>(
        &self,
        py: Python<'py>,
        resource: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let resource = resource_in(resource)?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let r = inner.read_linearizable(resource).await.map_err(coord_err)?;
            to_py(|py| snapshot_out(py, &r.snapshot))
        })
    }

    /// A resource's committed ACL (administrator). Awaitable `AclStatus`.
    fn inspect_acl<'py>(
        &self,
        py: Python<'py>,
        resource: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let resource = resource_in(resource)?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let s = inner
                .inspect_acl(resource)
                .await
                .map_err(coord_err)?
                .acl_snapshot;
            to_py(|py| {
                let grants = s
                    .grants
                    .iter()
                    .map(|g| {
                        py_value!(py, "aster.coordination", "AclGrant" {
                            subject_node_id: nid(&g.subject_node_id),
                            rights: g.rights,
                        })
                    })
                    .collect::<PyResult<Vec<_>>>()?;
                py_value!(py, "aster.coordination", "AclStatus" {
                    resource: resource_out(py, &s.resource)?,
                    acl_revision: s.acl_revision,
                    grants: PyTuple::new(py, grants)?,
                })
            })
        })
    }

    /// The delegated administrator set (administrator; ROOT is static and
    /// not listed). Awaitable `AdminStatus`.
    fn inspect_admins<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let s = inner
                .inspect_admins()
                .await
                .map_err(coord_err)?
                .admin_snapshot;
            to_py(|py| {
                py_value!(py, "aster.coordination", "AdminStatus" {
                    admin_revision: s.admin_revision,
                    administrators: nids(py, &s.administrators)?,
                })
            })
        })
    }

    /// One membership intent's status, or the active one (administrator).
    /// Awaitable `MembershipStatus`.
    #[pyo3(signature = (operation_id=None))]
    fn membership_status<'py>(
        &self,
        py: Python<'py>,
        operation_id: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let op = operation_id
            .filter(|o| !o.is_none())
            .map(|o| id16(o, "operation_id"))
            .transpose()?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let r = inner.membership_status(op).await.map_err(coord_err)?;
            to_py(|py| {
                py_value!(py, "aster.coordination", "MembershipStatus" {
                    operation_id: opt_hex(&r.operation_id),
                    status_code: r.status_code,
                    expected_membership_log_id: opt_log_out(py, &r.expected_membership_log_id)?,
                    observed_membership_log_id: opt_log_out(py, &r.observed_membership_log_id)?,
                    desired_voters: nids(py, &r.desired_voters)?,
                    retained_learners: nids(py, &r.retained_learners)?,
                    error_code: r.error_code,
                })
            })
        })
    }

    /// The S4-STATUS-CUT group status (administrator). Awaitable `GroupStatus`.
    fn group_status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let s = inner.group_status().await.map_err(coord_err)?;
            to_py(|py| group_status_out(py, &s))
        })
    }

    /// Client-session diagnostics -- never authority to reassign a sequence.
    /// Awaitable `ClientProbe`.
    fn probe_client<'py>(
        &self,
        py: Python<'py>,
        target_node_id: &Bound<'py, PyAny>,
        target_client_id: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let node = id32(target_node_id, "target_node_id")?;
        let client = id16(target_client_id, "target_client_id")?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let r = inner.probe_client(node, client).await.map_err(coord_err)?;
            to_py(|py| {
                py_value!(py, "aster.coordination", "ClientProbe" {
                    target_node_id: nid(&r.target_node_id),
                    target_client_id: hex::encode(&r.target_client_id),
                    exists: r.exists,
                    last_sequence: r.last_sequence,
                    request_hash: hex::encode(&r.request_hash),
                    disposition_code: r.disposition_code,
                    retired: r.retired,
                })
            })
        })
    }

    /// One validated `audit_stream` request: up to `limit` (1..=10 000)
    /// retained rows strictly after `after` (a `LogId`), ascending.
    /// Awaitable tuple of `AuditRecord`.
    fn audit_page<'py>(
        &self,
        py: Python<'py>,
        after: Option<&Bound<'py, PyAny>>,
        limit: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let after = log_in(after)?;
        let limit = u32::try_from(u64_in(limit, "limit")?)
            .map_err(|_| invalid(format!("limit must be at most {MAX_AUDIT_PAGE}")))?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let rows = inner.audit_stream(after, limit).await.map_err(coord_err)?;
            to_py(|py| {
                let rows = rows
                    .iter()
                    .map(|r| audit_out(py, r))
                    .collect::<PyResult<Vec<_>>>()?;
                Ok(PyTuple::new(py, rows)?.into_any())
            })
        })
    }

    /// Retained audit rows in ascending log order (administrator), as an
    /// async iterator paged `page_size` (1..=10 000) rows at a time. Starts
    /// strictly after `after` (default: the oldest retained row) and stops
    /// after `limit` rows (default: the end of retained history). Each page
    /// is validated in Rust (envelope, bound, strict ascent).
    #[pyo3(signature = (*, after=None, limit=None, page_size=1000))]
    fn audit_stream(
        &self,
        after: Option<&Bound<'_, PyAny>>,
        limit: Option<u64>,
        page_size: i64,
    ) -> PyResult<AuditStream> {
        if !(1..=i64::from(MAX_AUDIT_PAGE)).contains(&page_size) {
            return Err(invalid(format!(
                "audit_stream: page_size must be 1..={MAX_AUDIT_PAGE}"
            )));
        }
        Ok(AuditStream {
            inner: self.inner.clone(),
            page_size: page_size as u32,
            cursor: Arc::new(tokio::sync::Mutex::new(AuditCursor {
                after: log_in(after)?,
                remaining: limit,
                buffer: VecDeque::new(),
                done: false,
            })),
        })
    }

    /// The canonical recovery export, verified against its manifest, in
    /// memory. Awaitable `RecoveryExport`.
    fn recovery_export<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let (manifest, body) = inner.recovery_export().await.map_err(coord_err)?;
            to_py(|py| {
                py_value!(py, "aster.coordination", "RecoveryExport" {
                    manifest: manifest_out(py, &manifest)?,
                    body: PyBytes::new(py, &body),
                })
            })
        })
    }

    /// Stream the export into `path` (via `path.partial`, renamed into place
    /// only once the checksum held). Awaitable `ExportManifest`.
    fn recovery_export_to_file<'py>(
        &self,
        py: Python<'py>,
        path: PathBuf,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let mut partial = path.clone().into_os_string();
            partial.push(".partial");
            let partial = PathBuf::from(partial);
            let file = std::fs::File::create(&partial)?;
            match inner.recovery_export_to(file).await {
                Ok((manifest, file)) => {
                    file.sync_all()?;
                    drop(file);
                    std::fs::rename(&partial, &path)?;
                    to_py(|py| manifest_out(py, &manifest))
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&partial);
                    Err(coord_err(e))
                }
            }
        })
    }

    fn __repr__(&self) -> String {
        "CoordinationReadClient()".into()
    }
}

struct AuditCursor {
    after: Option<LogIdV1>,
    remaining: Option<u64>,
    buffer: VecDeque<w::AuditRecordWireV1>,
    done: bool,
}

/// Async iterator of `AuditRecord` (see `CoordinationReadClient.audit_stream`).
#[pyclass(frozen, module = "aster._aster")]
pub struct AuditStream {
    inner: cc::CoordinationReadClient,
    page_size: u32,
    cursor: Arc<tokio::sync::Mutex<AuditCursor>>,
}

#[pymethods]
impl AuditStream {
    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let page_size = self.page_size;
        let cursor = self.cursor.clone();
        future_into_py(py, async move {
            let mut cur = cursor.lock().await;
            loop {
                if let Some(row) = cur.buffer.pop_front() {
                    return to_py(|py| audit_out(py, &row));
                }
                if cur.done || cur.remaining == Some(0) {
                    return Err(PyStopAsyncIteration::new_err(()));
                }
                let want = match cur.remaining {
                    Some(r) => r.min(u64::from(page_size)) as u32,
                    None => page_size,
                };
                let page = inner
                    .audit_stream(cur.after.clone(), want)
                    .await
                    .map_err(coord_err)?;
                if page.len() < want as usize {
                    cur.done = true;
                }
                if let Some(last) = page.last() {
                    cur.after = Some(last.log_id.clone());
                }
                if let Some(r) = cur.remaining.as_mut() {
                    *r = r.saturating_sub(page.len() as u64);
                }
                cur.buffer.extend(page);
            }
        })
    }
}

// ============================================================================
// Pure placement planner
// ============================================================================

pub(crate) fn policy_in(p: &Bound<'_, PyAny>) -> PyResult<pl::PlacementPolicy> {
    let mut eligible = BTreeMap::new();
    for c in attr_of(p, "eligible", "policy")?
        .try_iter()
        .map_err(|_| invalid("policy.eligible must be an iterable of PlacementCandidate"))?
    {
        let c = c?;
        let node_id = node_id_in(
            &attr_of(&c, "node_id", "policy.eligible[]")?,
            "policy.eligible[].node_id",
        )?;
        eligible.insert(
            node_id,
            pl::PlacementCandidate {
                node_id,
                member_seed: opt_attr(&c, "member_seed")
                    .map(|s| seed_in(&s))
                    .transpose()?,
            },
        );
    }
    let u8_attr = |attr: &str, default: Option<u64>| -> PyResult<u8> {
        let what = format!("policy.{attr}");
        let v = match (opt_attr(p, attr), default) {
            (Some(v), _) => u64_in(&v, &what)?,
            (None, Some(d)) => d,
            (None, None) => return Err(invalid(format!("{what} is required"))),
        };
        u8::try_from(v).map_err(|_| invalid(format!("{what} must be at most 255, got {v}")))
    };
    let grace: f64 = match opt_attr(p, "replacement_grace") {
        Some(v) => v
            .extract()
            .map_err(|_| invalid("policy.replacement_grace must be seconds"))?,
        None => 0.0,
    };
    Ok(pl::PlacementPolicy {
        revision: u64_attr(p, "revision", "policy.revision")?,
        digest: id32(&attr_of(p, "digest", "policy")?, "policy.digest")?,
        authority_public_key: id32(
            &attr_of(p, "authority_public_key", "policy")?,
            "policy.authority_public_key",
        )?,
        authority_domain: attr_of(p, "authority_domain", "policy")?
            .extract()
            .map_err(|_| invalid("policy.authority_domain must be a str"))?,
        target_voters: u8_attr("target_voters", None)?,
        target_retained_learners: u8_attr("target_retained_learners", Some(0))?,
        eligible,
        pinned_voters: opt_node_set(opt_attr(p, "pinned_voters"), "policy.pinned_voters")?,
        excluded: opt_node_set(opt_attr(p, "excluded"), "policy.excluded")?,
        minimum_separated_voter_pairs: u8_attr("minimum_separated_voter_pairs", Some(0))?,
        replacement_grace: Duration::from_millis(millis_in(grace, "policy.replacement_grace")?),
        allow_healthy_rebalance: match opt_attr(p, "allow_healthy_rebalance") {
            Some(v) => v.is_truthy()?,
            None => false,
        },
    })
}

/// A `PlacementPolicy` dataclass (the inverse of [`policy_in`]).
pub(crate) fn policy_out<'py>(
    py: Python<'py>,
    p: &pl::PlacementPolicy,
) -> PyResult<Bound<'py, PyAny>> {
    let eligible = p
        .eligible
        .values()
        .map(|c| {
            py_value!(py, "aster.coordination", "PlacementCandidate" {
                node_id: node_id_out(c.node_id.to_hex()),
                member_seed: c.member_seed.as_ref().map(|s| seed_out(py, s)).transpose()?,
            })
        })
        .collect::<PyResult<Vec<_>>>()?;
    py_value!(py, "aster.coordination", "PlacementPolicy" {
        revision: p.revision,
        digest: hex::encode(p.digest),
        authority_public_key: hex::encode(p.authority_public_key),
        authority_domain: p.authority_domain.clone(),
        target_voters: p.target_voters,
        eligible: PyTuple::new(py, eligible)?,
        target_retained_learners: p.target_retained_learners,
        pinned_voters: hex_ids(py, &p.pinned_voters)?,
        excluded: hex_ids(py, &p.excluded)?,
        minimum_separated_voter_pairs: p.minimum_separated_voter_pairs,
        replacement_grace: p.replacement_grace.as_secs_f64(),
        allow_healthy_rebalance: p.allow_healthy_rebalance,
    })
}

pub(crate) fn separation_out<'py>(
    py: Python<'py>,
    s: &pl::SeparationSummary,
) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "SeparationSummary" {
        separated_pairs: s.separated_pairs,
        connected_pairs: s.connected_pairs,
        unknown_pairs: s.unknown_pairs,
        total_pairs: s.total_pairs,
        represented_clusters: s.represented_clusters,
        diversity_proved: s.diversity_proved,
    })
}

/// §7.7.2's pure, deterministic voter placement planner (backs
/// `aster.coordination.placement_plan`).
#[pyfunction]
#[pyo3(name = "_coordination_placement_plan")]
fn placement_plan<'py>(
    py: Python<'py>,
    input: &Bound<'py, PyAny>,
    topology: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let topology = topology.cast::<TopologySnapshot>().map_err(|_| {
        invalid("placement_plan: pass a snapshot from await node.topology().snapshot()")
    })?;
    let policy = policy_in(&attr_of(input, "policy", "placement input")?)?;
    let plan_input = pl::PlacementInput {
        group_id: group_in(&attr_of(input, "group_id", "placement input")?)?,
        genesis_hash: id32(
            &attr_of(input, "genesis_hash", "placement input")?,
            "genesis_hash",
        )?,
        deployment_profile_code: attr_of(input, "deployment_profile_code", "placement input")?
            .extract()
            .map_err(|_| invalid("deployment_profile_code must be an int"))?,
        membership_log_id: log_in(opt_attr(input, "membership_log_id").as_ref())?,
        current_voters: opt_node_set(
            Some(attr_of(input, "current_voters", "placement input")?),
            "current_voters",
        )?,
        current_learners: opt_node_set(opt_attr(input, "current_learners"), "current_learners")?,
        active_membership_intent: match opt_attr(input, "active_membership_intent") {
            Some(v) => v.is_truthy()?,
            None => false,
        },
        grace_crossed_members: opt_node_set(
            opt_attr(input, "grace_crossed_members"),
            "grace_crossed_members",
        )?,
        policy,
        topology: (*topology.get().inner).clone(),
    };

    macro_rules! rec {
        ($($k:ident : $v:expr),* $(,)?) => {
            py_value!(py, "aster.coordination", "PlacementRecommendation" { $($k: $v),* })
        };
    }
    // `validate_policy` computes `target choose 2` in u32; a zero target is
    // refused by the profile check first, but guard it here regardless.
    if plan_input.policy.target_voters == 0 {
        return rec!(kind: "invalid_policy", why: "target_voters must be at least 1");
    }
    match pl::plan(&plan_input) {
        pl::PlacementRecommendation::Stable { separation } => {
            rec!(kind: "stable", separation: separation_out(py, &separation)?)
        }
        pl::PlacementRecommendation::Degraded { why, separation } => rec!(
            kind: "degraded",
            why: why,
            separation: separation_out(py, &separation)?,
        ),
        pl::PlacementRecommendation::Converging => rec!(kind: "converging"),
        pl::PlacementRecommendation::NeedsProvisioning {
            candidates,
            desired_voters,
            retained_learners,
        } => rec!(
            kind: "needs_provisioning",
            candidates: hex_ids(py, &candidates)?,
            desired_voters: hex_ids(py, &desired_voters)?,
            retained_learners: hex_ids(py, &retained_learners)?,
        ),
        pl::PlacementRecommendation::InsufficientEvidence { missing } => {
            rec!(kind: "insufficient_evidence", missing: missing)
        }
        pl::PlacementRecommendation::InvalidPolicy { why } => {
            rec!(kind: "invalid_policy", why: why)
        }
        pl::PlacementRecommendation::Plan(plan) => {
            let seeds = plan
                .member_seeds
                .iter()
                .map(|s| seed_out(py, s))
                .collect::<PyResult<Vec<_>>>()?;
            let p = py_value!(py, "aster.coordination", "PlacementPlan" {
                desired_voters: hex_ids(py, &plan.desired_voters)?,
                retained_learners: hex_ids(py, &plan.retained_learners)?,
                member_seeds: PyTuple::new(py, seeds)?,
                expected_membership_log_id: opt_log_out(py, &plan.expected_membership_log_id)?,
                separation: separation_out(py, &plan.separation)?,
            })?;
            rec!(kind: "plan", plan: p)
        }
    }
}

// ============================================================================
// Placement manager (Python providers by upcall)
// ============================================================================

use aster::binding_support::upcall::placement::{
    ForeignPlacementPolicySource, ForeignPlacementProvisioner,
};
use aster::binding_support::upcall::topology::ForeignTopologySource;
use aster::coordination::client::manager as mgr;

/// The manager's topology: the node's own view, or a Python source.
enum TopoSource {
    Native(aster::topology::Topology),
    Foreign(ForeignTopologySource),
}

#[async_trait::async_trait]
impl mgr::TopologySource for TopoSource {
    async fn snapshot(&self) -> aster::Result<aster::topology::TopologySnapshot> {
        match self {
            TopoSource::Native(t) => mgr::TopologySource::snapshot(t).await,
            TopoSource::Foreign(f) => mgr::TopologySource::snapshot(f).await,
        }
    }
}

/// The manager's policy: fixed, or read from Python on every observation.
enum PolicySource {
    Fixed(pl::PlacementPolicy),
    Foreign(ForeignPlacementPolicySource),
}

impl mgr::PlacementPolicySource for PolicySource {
    fn policy(&self) -> aster::Result<pl::PlacementPolicy> {
        match self {
            PolicySource::Fixed(p) => Ok(p.clone()),
            PolicySource::Foreign(f) => f.policy(),
        }
    }
}

type Manager =
    mgr::CoordinationPlacementManager<TopoSource, PolicySource, ForeignPlacementProvisioner>;

fn placement_err(e: aster::Error) -> PyErr {
    crate::upcall::aster_to_py(e, coord_err)
}

fn report_out<'py>(py: Python<'py>, r: &pl::PlacementReport) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "PlacementReport" {
        policy_revision: r.policy_revision,
        policy_digest: hex::encode(r.policy_digest),
        membership_log_id: opt_log_out(py, &r.membership_log_id)?,
        current_voters: hex_ids(py, &r.current_voters)?,
        current_learners: hex_ids(py, &r.current_learners)?,
        selected_voters: hex_ids(py, &r.selected_voters)?,
        selected_learners: hex_ids(py, &r.selected_learners)?,
        grace_crossed_members: hex_ids(py, &r.grace_crossed_members)?,
        separation: r.separation.as_ref().map(|s| separation_out(py, s)).transpose()?,
        pending_operation_id: r.pending_operation_id.map(hex::encode),
        reason: r.reason.clone(),
    })
}

/// The native half of `aster.coordination.PlacementManager` (section
/// 7.7.3): reconciles membership through the client's durable journal,
/// with the providers in Python.
#[pyclass(frozen, module = "aster._aster", name = "_PlacementManager")]
pub struct PlacementManager {
    inner: Arc<tokio::sync::Mutex<Manager>>,
}

#[pymethods]
impl PlacementManager {
    /// `handler`: the `aster.coordination.PlacementManager` (bound as the
    /// placement upcall handler). `topology`: a native `Topology` to use
    /// directly, or `None` to ask `handler` (kind `topology_snapshot`).
    /// `policy`: a fixed `PlacementPolicy`, or `None` to ask `handler`
    /// on every observation (kind `placement_policy`).
    #[new]
    #[pyo3(signature = (client, handler, *, topology=None, policy=None))]
    fn new(
        client: &Bound<'_, CoordinationClient>,
        handler: &Bound<'_, PyAny>,
        topology: Option<&Bound<'_, crate::topology::Topology>>,
        policy: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let client = client.get().inner_client()?;
        let upcalls = crate::upcall::bind(handler, "placement")?;
        let topo = match topology {
            Some(t) => TopoSource::Native(t.get().inner().clone()),
            None => TopoSource::Foreign(ForeignTopologySource::new(upcalls.clone())),
        };
        let policy = match policy.filter(|p| !p.is_none()) {
            Some(p) => PolicySource::Fixed(policy_in(p)?),
            None => PolicySource::Foreign(ForeignPlacementPolicySource::new(upcalls.clone())),
        };
        Ok(Self {
            inner: Arc::new(tokio::sync::Mutex::new(
                mgr::CoordinationPlacementManager::new(
                    client,
                    topo,
                    policy,
                    ForeignPlacementProvisioner::new(upcalls),
                ),
            )),
        })
    }

    /// Run one reconciliation (awaitable `PlacementReport`). Passes are
    /// serialized. A provider failure (an exception, a timeout) stops the
    /// pass: `aster.upcalls.UpcallError`.
    fn reconcile<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let report = inner
                .lock()
                .await
                .reconcile()
                .await
                .map_err(placement_err)?;
            to_py(|py| report_out(py, &report))
        })
    }
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CoordinationClient>()?;
    m.add_class::<CoordinationReadClient>()?;
    m.add_class::<ResolvedMutation>()?;
    m.add_class::<CoordinationLease>()?;
    m.add_class::<AuditStream>()?;
    m.add_function(wrap_pyfunction!(reason_codes, m)?)?;
    m.add_function(wrap_pyfunction!(outcome_codes, m)?)?;
    m.add_function(wrap_pyfunction!(placement_plan, m)?)?;
    m.add_class::<PlacementManager>()?;
    m.add_function(wrap_pyfunction!(error_for_testing, m)?)?;
    Ok(())
}
