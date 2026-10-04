//! UCv1 messages `D0`-`DE`: the coordination client's C ABI payloads
//! (ABI 4.4, `docs/_internal/bindings/coordination-abi-design.md` §2).
//!
//! Same discipline as the upcall payloads (`upcall-design.md` §3): tag byte,
//! version `0x01`, every field in a fixed order, minimal varints, no
//! trailing bytes, built from `core::canonical`. One addition:
//! `code := uint(u32)`, an `i32` code-space value as its `u32` bit pattern.
//!
//! Requests (`D0`, `D1`) are decoded here from the binding's bytes; results
//! are encoded from the validated Store replies. Decoders for the results
//! exist so the tests (and anyone checking a binding's codec) can read them
//! back; replies decode into the `aster` wire types with `protocol_version`
//! and `group_id` left at their defaults (UCv1 does not carry them).

use anyhow::{anyhow, bail, ensure, Result};
use aster::coordination::client::placement::{PlacementReport, SeparationSummary};
use aster::coordination::contract::{
    FenceV1, LogIdV1, MemberSeedV1, NodeIdV1, ResourceKeyV1, ResourceSnapshotV1,
};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::raft::RecoveredFromV1;
use aster::coordination::store::wire::{
    AclGrantWireV1, AclSnapshotWireV1, AclStatusReplyWireV1, AdminSnapshotWireV1,
    AdminStatusReplyWireV1, AuditRecordWireV1, GroupStatusReplyWireV1, MembershipStatusReplyWireV1,
    MutationReplyWireV1, ProbeClientReplyWireV1, ReadReplyWireV1, RecoveryExportManifestWireV1,
    RetireClientReplyWireV1, MAX_AUDIT_STREAM_LIMIT,
};
use aster_transport_core::canonical as c;

use crate::coordination::{
    aster_coordination_mutation_op_t as MutOp, aster_coordination_read_op_t as ReadOp,
};

/// The UCv1 version byte.
const VERSION: u8 = 0x01;

/// UCv1 tag `D0` MutationRequest (binding -> Rust, `aster_coordination_mutate`).
pub const ASTER_COORDINATION_TAG_MUTATION_REQUEST: u8 = 0xD0;
/// UCv1 tag `D1` ReadRequest (binding -> Rust, `aster_coordination_read`).
pub const ASTER_COORDINATION_TAG_READ_REQUEST: u8 = 0xD1;
/// UCv1 tag `D2` MutationReply.
pub const ASTER_COORDINATION_TAG_MUTATION_REPLY: u8 = 0xD2;
/// UCv1 tag `D3` ReadReply (a resource snapshot).
pub const ASTER_COORDINATION_TAG_READ_REPLY: u8 = 0xD3;
/// UCv1 tag `D4` AclStatus.
pub const ASTER_COORDINATION_TAG_ACL_STATUS: u8 = 0xD4;
/// UCv1 tag `D5` AdminStatus.
pub const ASTER_COORDINATION_TAG_ADMIN_STATUS: u8 = 0xD5;
/// UCv1 tag `D6` MembershipStatus.
pub const ASTER_COORDINATION_TAG_MEMBERSHIP_STATUS: u8 = 0xD6;
/// UCv1 tag `D7` GroupStatus.
pub const ASTER_COORDINATION_TAG_GROUP_STATUS: u8 = 0xD7;
/// UCv1 tag `D8` ClientProbe.
pub const ASTER_COORDINATION_TAG_CLIENT_PROBE: u8 = 0xD8;
/// UCv1 tag `D9` AuditRecords.
pub const ASTER_COORDINATION_TAG_AUDIT_RECORDS: u8 = 0xD9;
/// UCv1 tag `DA` RecoveryExport (manifest + body).
pub const ASTER_COORDINATION_TAG_RECOVERY_EXPORT: u8 = 0xDA;
/// UCv1 tag `DB` RecoveryExportManifest.
pub const ASTER_COORDINATION_TAG_RECOVERY_EXPORT_MANIFEST: u8 = 0xDB;
/// UCv1 tag `DC` RetireReply.
pub const ASTER_COORDINATION_TAG_RETIRE_REPLY: u8 = 0xDC;
/// UCv1 tag `DD` CoordinationFence.
pub const ASTER_COORDINATION_TAG_COORDINATION_FENCE: u8 = 0xDD;
/// UCv1 tag `DE` PlacementReport.
pub const ASTER_COORDINATION_TAG_PLACEMENT_REPORT: u8 = 0xDE;

// ── requests ────────────────────────────────────────────────────────────────

/// `D0`: one sequenced mutation.
#[derive(Clone, Debug, PartialEq)]
pub enum MutationRequest {
    Acquire {
        resource: ResourceKeyV1,
        ttl_ms: u64,
    },
    Renew {
        fence: FenceV1,
    },
    CompareAndSet {
        fence: FenceV1,
        expected_value_revision: u64,
        new_value: Vec<u8>,
    },
    Release {
        fence: FenceV1,
    },
    ReleaseWithValue {
        fence: FenceV1,
        expected_value_revision: u64,
        final_value: Vec<u8>,
    },
    Revoke {
        resource: ResourceKeyV1,
        expected_epoch: Option<u64>,
    },
    SetGrant {
        resource: ResourceKeyV1,
        target_node_id: [u8; 32],
        rights: u64,
        expected_acl_revision: u64,
    },
    SetAdmin {
        target_node_id: [u8; 32],
        enabled: bool,
        expected_admin_revision: u64,
    },
    ChangeMembership {
        operation_id: [u8; 16],
        expected_membership_log_id: Option<LogIdV1>,
        desired_voters: Vec<[u8; 32]>,
        retained_learners: Vec<[u8; 32]>,
        member_seeds: Vec<MemberSeedV1>,
    },
    CancelMembership {
        operation_id: [u8; 16],
        expected_phase_code: i32,
    },
}

/// `D1`: one read / status request.
#[derive(Clone, Debug, PartialEq)]
pub enum ReadRequest {
    ReadLinearizable(ResourceKeyV1),
    InspectAcl(ResourceKeyV1),
    InspectAdmins,
    MembershipStatus(Option<[u8; 16]>),
    GroupStatus,
    ProbeClient {
        target_node_id: [u8; 32],
        target_client_id: [u8; 16],
    },
    AuditStream {
        after: Option<LogIdV1>,
        limit: u32,
    },
    RecoveryExport,
}

// ── writer / reader ─────────────────────────────────────────────────────────

struct W(Vec<u8>);

impl W {
    fn new(tag: u8) -> Self {
        W(vec![tag, VERSION])
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn uint(&mut self, v: u64) -> &mut Self {
        c::write_varint(&mut self.0, v);
        self
    }
    fn code(&mut self, v: i32) -> &mut Self {
        self.uint(u64::from(v as u32))
    }
    fn bool(&mut self, v: bool) -> &mut Self {
        c::write_bool(&mut self.0, v);
        self
    }
    fn str(&mut self, s: &str) -> &mut Self {
        c::write_string(&mut self.0, s);
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        c::write_bytes_field(&mut self.0, b);
        self
    }
    fn list(&mut self, n: usize) -> &mut Self {
        c::write_list_header(&mut self.0, n);
        self
    }
    fn opt<T>(&mut self, v: Option<T>, f: impl FnOnce(&mut Self, T)) -> &mut Self {
        match v {
            None => c::write_optional_absent(&mut self.0),
            Some(v) => {
                c::write_optional_present_prefix(&mut self.0);
                f(self, v);
            }
        }
        self
    }
    fn byte_list<B: AsRef<[u8]>>(&mut self, items: &[B]) -> &mut Self {
        self.list(items.len());
        for b in items {
            self.bytes(b.as_ref());
        }
        self
    }
    fn resource(&mut self, r: &ResourceKeyV1) -> &mut Self {
        self.str(&r.namespace).bytes(&r.key)
    }
    fn log_id(&mut self, l: &LogIdV1) -> &mut Self {
        self.uint(l.term).bytes(&l.leader_node_id).uint(l.index)
    }
    fn opt_log_id(&mut self, l: &Option<LogIdV1>) -> &mut Self {
        self.opt(l.as_ref(), |w, l| {
            w.log_id(l);
        })
    }
    fn opt_bytes(&mut self, b: &Option<Vec<u8>>) -> &mut Self {
        self.opt(b.as_ref(), |w, b| {
            w.bytes(b);
        })
    }
    fn fence(&mut self, f: &FenceV1) -> &mut Self {
        self.bytes(&f.group_id)
            .resource(&f.resource)
            .uint(f.epoch)
            .bytes(&f.holder)
    }
    fn snapshot(&mut self, s: &ResourceSnapshotV1) -> &mut Self {
        self.bytes(&s.group_id)
            .resource(&s.resource)
            .uint(s.epoch)
            .opt_bytes(&s.holder)
            .bool(s.held)
            .uint(s.ttl_ms)
            .uint(s.lease_revision)
            .uint(s.value_revision)
            .uint(s.acl_revision)
            .opt_bytes(&s.value)
            .bool(s.value_redacted)
    }
    fn member_seed(&mut self, s: &MemberSeedV1) -> &mut Self {
        self.bytes(&s.node_id)
            .str(&s.ticket)
            .opt_bytes(&s.signed_join_permit)
    }
    fn node_ids(&mut self, ids: &[NodeIdV1]) -> &mut Self {
        self.list(ids.len());
        for n in ids {
            self.bytes(&n.node_id);
        }
        self
    }
    fn coord_ids(&mut self, ids: &[CoordinationNodeId]) -> &mut Self {
        self.list(ids.len());
        for n in ids {
            self.bytes(n.as_bytes());
        }
        self
    }
    fn done(self) -> Vec<u8> {
        self.0
    }
}

struct R<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> R<'a> {
    fn new(buf: &'a [u8], tag: u8) -> Result<Self> {
        ensure!(!buf.is_empty(), "UCv1: empty payload");
        ensure!(
            buf[0] == tag,
            "UCv1: tag {:#04x}, expected {:#04x}",
            buf[0],
            tag
        );
        ensure!(buf.len() >= 2, "UCv1: missing version");
        ensure!(
            buf[1] == VERSION,
            "UCv1: unsupported version {:#04x}",
            buf[1]
        );
        Ok(R { buf, pos: 2 })
    }
    fn u8(&mut self) -> Result<u8> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| anyhow!("UCv1: unexpected EOF"))?;
        self.pos += 1;
        Ok(b)
    }
    fn uint(&mut self) -> Result<u64> {
        let start = self.pos;
        let v = c::read_varint(self.buf, &mut self.pos)?;
        let mut re = Vec::new();
        c::write_varint(&mut re, v);
        ensure!(
            re.len() == self.pos - start,
            "UCv1: non-minimal varint at offset {start}"
        );
        Ok(v)
    }
    fn code(&mut self) -> Result<i32> {
        let v = self.uint()?;
        let v = u32::try_from(v).map_err(|_| anyhow!("UCv1: code {v} out of range"))?;
        Ok(v as i32)
    }
    fn bool(&mut self) -> Result<bool> {
        c::read_bool(self.buf, &mut self.pos)
    }
    fn str(&mut self) -> Result<String> {
        let start = self.pos;
        let header = c::read_varint(self.buf, &mut self.pos)?;
        let mut re = Vec::new();
        c::write_varint(&mut re, header);
        ensure!(
            re.len() == self.pos - start,
            "UCv1: non-minimal string header at offset {start}"
        );
        self.pos = start;
        c::read_string(self.buf, &mut self.pos)
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let start = self.pos;
        let len = self.uint()?;
        self.pos = start;
        ensure!(
            len as usize <= self.buf.len(),
            "UCv1: bytes length {len} exceeds payload"
        );
        c::read_bytes_field(self.buf, &mut self.pos)
    }
    fn fixed<const N: usize>(&mut self, what: &str) -> Result<[u8; N]> {
        let b = self.bytes()?;
        <[u8; N]>::try_from(b.as_slice())
            .map_err(|_| anyhow!("UCv1: {what} must be {N} bytes, got {}", b.len()))
    }
    fn list(&mut self) -> Result<usize> {
        let start = self.pos;
        self.uint()?;
        self.pos = start;
        let n = c::read_list_header(self.buf, &mut self.pos)?;
        ensure!(
            n <= self.buf.len() - self.pos,
            "UCv1: list count {n} exceeds payload"
        );
        Ok(n)
    }
    fn opt<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        if c::read_optional_present(self.buf, &mut self.pos)? {
            Ok(Some(f(self)?))
        } else {
            Ok(None)
        }
    }
    fn opt_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        self.opt(|r| r.bytes())
    }
    fn byte_list(&mut self) -> Result<Vec<Vec<u8>>> {
        let n = self.list()?;
        (0..n).map(|_| self.bytes()).collect()
    }
    fn fixed_list<const N: usize>(&mut self, what: &str) -> Result<Vec<[u8; N]>> {
        let n = self.list()?;
        (0..n).map(|_| self.fixed::<N>(what)).collect()
    }
    fn resource(&mut self) -> Result<ResourceKeyV1> {
        Ok(ResourceKeyV1 {
            namespace: self.str()?,
            key: self.bytes()?,
        })
    }
    fn log_id(&mut self) -> Result<LogIdV1> {
        Ok(LogIdV1 {
            term: self.uint()?,
            leader_node_id: self.bytes()?,
            index: self.uint()?,
        })
    }
    fn opt_log_id(&mut self) -> Result<Option<LogIdV1>> {
        self.opt(|r| r.log_id())
    }
    fn fence(&mut self) -> Result<FenceV1> {
        Ok(FenceV1 {
            group_id: self.fixed::<32>("fence.group_id")?.to_vec(),
            resource: self.resource()?,
            epoch: self.uint()?,
            holder: self.fixed::<32>("fence.holder")?.to_vec(),
        })
    }
    fn snapshot(&mut self) -> Result<ResourceSnapshotV1> {
        Ok(ResourceSnapshotV1 {
            group_id: self.bytes()?,
            resource: self.resource()?,
            epoch: self.uint()?,
            holder: self.opt_bytes()?,
            held: self.bool()?,
            ttl_ms: self.uint()?,
            lease_revision: self.uint()?,
            value_revision: self.uint()?,
            acl_revision: self.uint()?,
            value: self.opt_bytes()?,
            value_redacted: self.bool()?,
        })
    }
    fn member_seed(&mut self) -> Result<MemberSeedV1> {
        Ok(MemberSeedV1 {
            node_id: self.fixed::<32>("member_seed.node_id")?.to_vec(),
            ticket: self.str()?,
            signed_join_permit: self.opt_bytes()?,
        })
    }
    fn node_ids(&mut self) -> Result<Vec<NodeIdV1>> {
        Ok(self
            .byte_list()?
            .into_iter()
            .map(|node_id| NodeIdV1 { node_id })
            .collect())
    }
    fn end(self) -> Result<()> {
        c::expect_eof(self.buf, self.pos)
    }
}

// ── D0 / D1 ─────────────────────────────────────────────────────────────────

const M_ACQUIRE: u8 = MutOp::ASTER_COORDINATION_MUTATE_ACQUIRE as u8;
const M_RENEW: u8 = MutOp::ASTER_COORDINATION_MUTATE_RENEW as u8;
const M_CAS: u8 = MutOp::ASTER_COORDINATION_MUTATE_COMPARE_AND_SET as u8;
const M_RELEASE: u8 = MutOp::ASTER_COORDINATION_MUTATE_RELEASE as u8;
const M_RELEASE_WITH_VALUE: u8 = MutOp::ASTER_COORDINATION_MUTATE_RELEASE_WITH_VALUE as u8;
const M_REVOKE: u8 = MutOp::ASTER_COORDINATION_MUTATE_REVOKE as u8;
const M_SET_GRANT: u8 = MutOp::ASTER_COORDINATION_MUTATE_SET_GRANT as u8;
const M_SET_ADMIN: u8 = MutOp::ASTER_COORDINATION_MUTATE_SET_ADMIN as u8;
const M_CHANGE_MEMBERSHIP: u8 = MutOp::ASTER_COORDINATION_MUTATE_CHANGE_MEMBERSHIP as u8;
const M_CANCEL_MEMBERSHIP: u8 = MutOp::ASTER_COORDINATION_MUTATE_CANCEL_MEMBERSHIP as u8;

pub fn encode_mutation_request(m: &MutationRequest) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_MUTATION_REQUEST);
    match m {
        MutationRequest::Acquire { resource, ttl_ms } => {
            w.u8(M_ACQUIRE).resource(resource).uint(*ttl_ms);
        }
        MutationRequest::Renew { fence } => {
            w.u8(M_RENEW).fence(fence);
        }
        MutationRequest::CompareAndSet {
            fence,
            expected_value_revision,
            new_value,
        } => {
            w.u8(M_CAS)
                .fence(fence)
                .uint(*expected_value_revision)
                .bytes(new_value);
        }
        MutationRequest::Release { fence } => {
            w.u8(M_RELEASE).fence(fence);
        }
        MutationRequest::ReleaseWithValue {
            fence,
            expected_value_revision,
            final_value,
        } => {
            w.u8(M_RELEASE_WITH_VALUE)
                .fence(fence)
                .uint(*expected_value_revision)
                .bytes(final_value);
        }
        MutationRequest::Revoke {
            resource,
            expected_epoch,
        } => {
            w.u8(M_REVOKE)
                .resource(resource)
                .opt(*expected_epoch, |w, e| {
                    w.uint(e);
                });
        }
        MutationRequest::SetGrant {
            resource,
            target_node_id,
            rights,
            expected_acl_revision,
        } => {
            w.u8(M_SET_GRANT)
                .resource(resource)
                .bytes(target_node_id)
                .uint(*rights)
                .uint(*expected_acl_revision);
        }
        MutationRequest::SetAdmin {
            target_node_id,
            enabled,
            expected_admin_revision,
        } => {
            w.u8(M_SET_ADMIN)
                .bytes(target_node_id)
                .bool(*enabled)
                .uint(*expected_admin_revision);
        }
        MutationRequest::ChangeMembership {
            operation_id,
            expected_membership_log_id,
            desired_voters,
            retained_learners,
            member_seeds,
        } => {
            w.u8(M_CHANGE_MEMBERSHIP)
                .bytes(operation_id)
                .opt_log_id(expected_membership_log_id)
                .byte_list(desired_voters)
                .byte_list(retained_learners)
                .list(member_seeds.len());
            for s in member_seeds {
                w.member_seed(s);
            }
        }
        MutationRequest::CancelMembership {
            operation_id,
            expected_phase_code,
        } => {
            w.u8(M_CANCEL_MEMBERSHIP)
                .bytes(operation_id)
                .code(*expected_phase_code);
        }
    }
    w.done()
}

pub fn decode_mutation_request(buf: &[u8]) -> Result<MutationRequest> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_MUTATION_REQUEST)?;
    let m = match r.u8()? {
        M_ACQUIRE => MutationRequest::Acquire {
            resource: r.resource()?,
            ttl_ms: r.uint()?,
        },
        M_RENEW => MutationRequest::Renew { fence: r.fence()? },
        M_CAS => MutationRequest::CompareAndSet {
            fence: r.fence()?,
            expected_value_revision: r.uint()?,
            new_value: r.bytes()?,
        },
        M_RELEASE => MutationRequest::Release { fence: r.fence()? },
        M_RELEASE_WITH_VALUE => MutationRequest::ReleaseWithValue {
            fence: r.fence()?,
            expected_value_revision: r.uint()?,
            final_value: r.bytes()?,
        },
        M_REVOKE => MutationRequest::Revoke {
            resource: r.resource()?,
            expected_epoch: r.opt(|r| r.uint())?,
        },
        M_SET_GRANT => MutationRequest::SetGrant {
            resource: r.resource()?,
            target_node_id: r.fixed::<32>("target_node_id")?,
            rights: r.uint()?,
            expected_acl_revision: r.uint()?,
        },
        M_SET_ADMIN => MutationRequest::SetAdmin {
            target_node_id: r.fixed::<32>("target_node_id")?,
            enabled: r.bool()?,
            expected_admin_revision: r.uint()?,
        },
        M_CHANGE_MEMBERSHIP => {
            let operation_id = r.fixed::<16>("operation_id")?;
            let expected_membership_log_id = r.opt_log_id()?;
            let desired_voters = r.fixed_list::<32>("desired_voters[]")?;
            let retained_learners = r.fixed_list::<32>("retained_learners[]")?;
            let n = r.list()?;
            let member_seeds = (0..n)
                .map(|_| r.member_seed())
                .collect::<Result<Vec<_>>>()?;
            MutationRequest::ChangeMembership {
                operation_id,
                expected_membership_log_id,
                desired_voters,
                retained_learners,
                member_seeds,
            }
        }
        M_CANCEL_MEMBERSHIP => MutationRequest::CancelMembership {
            operation_id: r.fixed::<16>("operation_id")?,
            expected_phase_code: r.code()?,
        },
        other => bail!("UCv1: unknown mutation op {other}"),
    };
    r.end()?;
    Ok(m)
}

const R_READ: u8 = ReadOp::ASTER_COORDINATION_READ_READ_LINEARIZABLE as u8;
const R_ACL: u8 = ReadOp::ASTER_COORDINATION_READ_INSPECT_ACL as u8;
const R_ADMINS: u8 = ReadOp::ASTER_COORDINATION_READ_INSPECT_ADMINS as u8;
const R_MEMBERSHIP: u8 = ReadOp::ASTER_COORDINATION_READ_MEMBERSHIP_STATUS as u8;
const R_GROUP: u8 = ReadOp::ASTER_COORDINATION_READ_GROUP_STATUS as u8;
const R_PROBE: u8 = ReadOp::ASTER_COORDINATION_READ_PROBE_CLIENT as u8;
const R_AUDIT: u8 = ReadOp::ASTER_COORDINATION_READ_AUDIT_STREAM as u8;
const R_EXPORT: u8 = ReadOp::ASTER_COORDINATION_READ_RECOVERY_EXPORT as u8;

pub fn encode_read_request(q: &ReadRequest) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_READ_REQUEST);
    match q {
        ReadRequest::ReadLinearizable(res) => {
            w.u8(R_READ).resource(res);
        }
        ReadRequest::InspectAcl(res) => {
            w.u8(R_ACL).resource(res);
        }
        ReadRequest::InspectAdmins => {
            w.u8(R_ADMINS);
        }
        ReadRequest::MembershipStatus(op) => {
            w.u8(R_MEMBERSHIP).opt(op.as_ref(), |w, op| {
                w.bytes(op);
            });
        }
        ReadRequest::GroupStatus => {
            w.u8(R_GROUP);
        }
        ReadRequest::ProbeClient {
            target_node_id,
            target_client_id,
        } => {
            w.u8(R_PROBE).bytes(target_node_id).bytes(target_client_id);
        }
        ReadRequest::AuditStream { after, limit } => {
            w.u8(R_AUDIT).opt_log_id(after).uint(u64::from(*limit));
        }
        ReadRequest::RecoveryExport => {
            w.u8(R_EXPORT);
        }
    }
    w.done()
}

pub fn decode_read_request(buf: &[u8]) -> Result<ReadRequest> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_READ_REQUEST)?;
    let q = match r.u8()? {
        R_READ => ReadRequest::ReadLinearizable(r.resource()?),
        R_ACL => ReadRequest::InspectAcl(r.resource()?),
        R_ADMINS => ReadRequest::InspectAdmins,
        R_MEMBERSHIP => ReadRequest::MembershipStatus(r.opt(|r| r.fixed::<16>("operation_id"))?),
        R_GROUP => ReadRequest::GroupStatus,
        R_PROBE => ReadRequest::ProbeClient {
            target_node_id: r.fixed::<32>("target_node_id")?,
            target_client_id: r.fixed::<16>("target_client_id")?,
        },
        R_AUDIT => {
            let after = r.opt_log_id()?;
            let limit = r.uint()?;
            ensure!(
                (1..=u64::from(MAX_AUDIT_STREAM_LIMIT)).contains(&limit),
                "UCv1: audit_stream limit {limit} outside 1..={MAX_AUDIT_STREAM_LIMIT}"
            );
            ReadRequest::AuditStream {
                after,
                limit: limit as u32,
            }
        }
        R_EXPORT => ReadRequest::RecoveryExport,
        other => bail!("UCv1: unknown read op {other}"),
    };
    r.end()?;
    Ok(q)
}

// ── results ─────────────────────────────────────────────────────────────────

pub fn encode_mutation_reply(m: &MutationReplyWireV1) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_MUTATION_REPLY);
    w.bytes(&m.client_id)
        .uint(m.sequence)
        .bytes(&m.request_hash)
        .code(m.disposition_code)
        .code(m.outcome_code)
        .code(m.reason_code)
        .opt(m.snapshot.as_ref(), |w, s| {
            w.snapshot(s);
        })
        .uint(m.acl_revision)
        .uint(m.admin_revision)
        .opt_bytes(&m.operation_id)
        .opt_log_id(&m.membership_log_id);
    w.done()
}

pub fn decode_mutation_reply(buf: &[u8]) -> Result<MutationReplyWireV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_MUTATION_REPLY)?;
    let m = MutationReplyWireV1 {
        client_id: r.bytes()?,
        sequence: r.uint()?,
        request_hash: r.bytes()?,
        disposition_code: r.code()?,
        outcome_code: r.code()?,
        reason_code: r.code()?,
        snapshot: r.opt(|r| r.snapshot())?,
        acl_revision: r.uint()?,
        admin_revision: r.uint()?,
        operation_id: r.opt_bytes()?,
        membership_log_id: r.opt_log_id()?,
        ..Default::default()
    };
    r.end()?;
    Ok(m)
}

pub fn encode_read_reply(m: &ReadReplyWireV1) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_READ_REPLY);
    w.snapshot(&m.snapshot);
    w.done()
}

pub fn decode_read_reply(buf: &[u8]) -> Result<ResourceSnapshotV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_READ_REPLY)?;
    let s = r.snapshot()?;
    r.end()?;
    Ok(s)
}

pub fn encode_acl_status(m: &AclStatusReplyWireV1) -> Vec<u8> {
    let s = &m.acl_snapshot;
    let mut w = W::new(ASTER_COORDINATION_TAG_ACL_STATUS);
    w.resource(&s.resource)
        .uint(s.acl_revision)
        .list(s.grants.len());
    for g in &s.grants {
        w.bytes(&g.subject_node_id).uint(g.rights);
    }
    w.done()
}

pub fn decode_acl_status(buf: &[u8]) -> Result<AclSnapshotWireV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_ACL_STATUS)?;
    let resource = r.resource()?;
    let acl_revision = r.uint()?;
    let n = r.list()?;
    let grants = (0..n)
        .map(|_| {
            Ok(AclGrantWireV1 {
                subject_node_id: r.bytes()?,
                rights: r.uint()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    r.end()?;
    Ok(AclSnapshotWireV1 {
        resource,
        acl_revision,
        grants,
    })
}

pub fn encode_admin_status(m: &AdminStatusReplyWireV1) -> Vec<u8> {
    let s = &m.admin_snapshot;
    let mut w = W::new(ASTER_COORDINATION_TAG_ADMIN_STATUS);
    w.uint(s.admin_revision).node_ids(&s.administrators);
    w.done()
}

pub fn decode_admin_status(buf: &[u8]) -> Result<AdminSnapshotWireV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_ADMIN_STATUS)?;
    let s = AdminSnapshotWireV1 {
        admin_revision: r.uint()?,
        administrators: r.node_ids()?,
    };
    r.end()?;
    Ok(s)
}

pub fn encode_membership_status(m: &MembershipStatusReplyWireV1) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_MEMBERSHIP_STATUS);
    w.opt_bytes(&m.operation_id)
        .code(m.status_code)
        .opt_log_id(&m.expected_membership_log_id)
        .opt_log_id(&m.observed_membership_log_id)
        .node_ids(&m.desired_voters)
        .node_ids(&m.retained_learners)
        .code(m.error_code);
    w.done()
}

pub fn decode_membership_status(buf: &[u8]) -> Result<MembershipStatusReplyWireV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_MEMBERSHIP_STATUS)?;
    let m = MembershipStatusReplyWireV1 {
        operation_id: r.opt_bytes()?,
        status_code: r.code()?,
        expected_membership_log_id: r.opt_log_id()?,
        observed_membership_log_id: r.opt_log_id()?,
        desired_voters: r.node_ids()?,
        retained_learners: r.node_ids()?,
        error_code: r.code()?,
        ..Default::default()
    };
    r.end()?;
    Ok(m)
}

pub fn encode_group_status(m: &GroupStatusReplyWireV1) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_GROUP_STATUS);
    w.code(m.lifecycle_code)
        .bytes(&m.local_node_id)
        .code(m.local_role_code)
        .opt_bytes(&m.leader_node_id)
        .uint(m.term)
        .opt_log_id(&m.last_log_id)
        .opt_log_id(&m.committed_log_id)
        .opt_log_id(&m.applied_log_id)
        .code(m.deployment_profile_code)
        .opt_log_id(&m.membership_log_id)
        .bool(m.membership_joint)
        .node_ids(&m.voters)
        .node_ids(&m.learners)
        .opt_bytes(&m.pending_operation_id)
        .opt(m.quorum_available, |w, q| {
            w.bool(q);
        })
        .code(m.fatal_code)
        .opt_log_id(&m.audit_floor)
        .bytes(&m.genesis_hash)
        .bytes(&m.authority_public_key)
        .str(&m.authority_domain)
        .opt(m.recovered_from.as_ref(), |w, f| {
            w.bytes(&f.source_group_id)
                .bytes(&f.source_genesis_hash)
                .bytes(&f.source_authority_public_key)
                .str(&f.source_authority_domain)
                .bytes(&f.export_hash)
                .opt_log_id(&f.source_last_applied);
        });
    w.done()
}

pub fn decode_group_status(buf: &[u8]) -> Result<GroupStatusReplyWireV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_GROUP_STATUS)?;
    let m = GroupStatusReplyWireV1 {
        lifecycle_code: r.code()?,
        local_node_id: r.bytes()?,
        local_role_code: r.code()?,
        leader_node_id: r.opt_bytes()?,
        term: r.uint()?,
        last_log_id: r.opt_log_id()?,
        committed_log_id: r.opt_log_id()?,
        applied_log_id: r.opt_log_id()?,
        deployment_profile_code: r.code()?,
        membership_log_id: r.opt_log_id()?,
        membership_joint: r.bool()?,
        voters: r.node_ids()?,
        learners: r.node_ids()?,
        pending_operation_id: r.opt_bytes()?,
        quorum_available: r.opt(|r| r.bool())?,
        fatal_code: r.code()?,
        audit_floor: r.opt_log_id()?,
        genesis_hash: r.bytes()?,
        authority_public_key: r.bytes()?,
        authority_domain: r.str()?,
        recovered_from: r.opt(|r| {
            Ok(RecoveredFromV1 {
                source_group_id: r.bytes()?,
                source_genesis_hash: r.bytes()?,
                source_authority_public_key: r.bytes()?,
                source_authority_domain: r.str()?,
                export_hash: r.bytes()?,
                source_last_applied: r.opt_log_id()?,
            })
        })?,
        ..Default::default()
    };
    r.end()?;
    Ok(m)
}

pub fn encode_client_probe(m: &ProbeClientReplyWireV1) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_CLIENT_PROBE);
    w.bytes(&m.target_node_id)
        .bytes(&m.target_client_id)
        .bool(m.exists)
        .uint(m.last_sequence)
        .bytes(&m.request_hash)
        .code(m.disposition_code)
        .bool(m.retired);
    w.done()
}

pub fn decode_client_probe(buf: &[u8]) -> Result<ProbeClientReplyWireV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_CLIENT_PROBE)?;
    let m = ProbeClientReplyWireV1 {
        target_node_id: r.bytes()?,
        target_client_id: r.bytes()?,
        exists: r.bool()?,
        last_sequence: r.uint()?,
        request_hash: r.bytes()?,
        disposition_code: r.code()?,
        retired: r.bool()?,
        ..Default::default()
    };
    r.end()?;
    Ok(m)
}

pub fn encode_audit_records(rows: &[AuditRecordWireV1]) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_AUDIT_RECORDS);
    w.list(rows.len());
    for a in rows {
        w.log_id(&a.log_id)
            .opt_bytes(&a.actor_node_id)
            .code(a.operation_code)
            .opt(a.resource.as_ref(), |w, res| {
                w.resource(res);
            })
            .code(a.result_code)
            .uint(a.epoch)
            .uint(a.value_revision);
    }
    w.done()
}

pub fn decode_audit_records(buf: &[u8]) -> Result<Vec<AuditRecordWireV1>> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_AUDIT_RECORDS)?;
    let n = r.list()?;
    let rows = (0..n)
        .map(|_| {
            Ok(AuditRecordWireV1 {
                log_id: r.log_id()?,
                actor_node_id: r.opt_bytes()?,
                operation_code: r.code()?,
                resource: r.opt(|r| r.resource())?,
                result_code: r.code()?,
                epoch: r.uint()?,
                value_revision: r.uint()?,
                ..Default::default()
            })
        })
        .collect::<Result<Vec<_>>>()?;
    r.end()?;
    Ok(rows)
}

fn put_manifest(w: &mut W, m: &RecoveryExportManifestWireV1) {
    w.uint(m.total_len)
        .bytes(&m.blake3_checksum)
        .opt_log_id(&m.applied_log_id);
}

fn get_manifest(r: &mut R<'_>) -> Result<RecoveryExportManifestWireV1> {
    Ok(RecoveryExportManifestWireV1 {
        total_len: r.uint()?,
        blake3_checksum: r.bytes()?,
        applied_log_id: r.opt_log_id()?,
        ..Default::default()
    })
}

pub fn encode_recovery_export(m: &RecoveryExportManifestWireV1, body: &[u8]) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_RECOVERY_EXPORT);
    put_manifest(&mut w, m);
    w.bytes(body);
    w.done()
}

pub fn decode_recovery_export(buf: &[u8]) -> Result<(RecoveryExportManifestWireV1, Vec<u8>)> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_RECOVERY_EXPORT)?;
    let m = get_manifest(&mut r)?;
    let body = r.bytes()?;
    r.end()?;
    Ok((m, body))
}

pub fn encode_recovery_export_manifest(m: &RecoveryExportManifestWireV1) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_RECOVERY_EXPORT_MANIFEST);
    put_manifest(&mut w, m);
    w.done()
}

pub fn decode_recovery_export_manifest(buf: &[u8]) -> Result<RecoveryExportManifestWireV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_RECOVERY_EXPORT_MANIFEST)?;
    let m = get_manifest(&mut r)?;
    r.end()?;
    Ok(m)
}

pub fn encode_retire_reply(m: &RetireClientReplyWireV1) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_RETIRE_REPLY);
    w.code(m.outcome_code).code(m.reason_code);
    w.done()
}

pub fn decode_retire_reply(buf: &[u8]) -> Result<RetireClientReplyWireV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_RETIRE_REPLY)?;
    let m = RetireClientReplyWireV1 {
        outcome_code: r.code()?,
        reason_code: r.code()?,
        ..Default::default()
    };
    r.end()?;
    Ok(m)
}

pub fn encode_coordination_fence(f: &FenceV1) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_COORDINATION_FENCE);
    w.fence(f);
    w.done()
}

pub fn decode_coordination_fence(buf: &[u8]) -> Result<FenceV1> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_COORDINATION_FENCE)?;
    let f = r.fence()?;
    r.end()?;
    Ok(f)
}

pub fn encode_placement_report(p: &PlacementReport) -> Vec<u8> {
    let mut w = W::new(ASTER_COORDINATION_TAG_PLACEMENT_REPORT);
    w.uint(p.policy_revision)
        .bytes(&p.policy_digest)
        .opt_log_id(&p.membership_log_id)
        .coord_ids(&p.current_voters)
        .coord_ids(&p.current_learners)
        .coord_ids(&p.selected_voters)
        .coord_ids(&p.selected_learners)
        .coord_ids(&p.grace_crossed_members)
        .opt(p.separation.as_ref(), |w, s| {
            w.uint(u64::from(s.separated_pairs))
                .uint(u64::from(s.connected_pairs))
                .uint(u64::from(s.unknown_pairs))
                .uint(u64::from(s.total_pairs))
                .uint(u64::from(s.represented_clusters))
                .bool(s.diversity_proved);
        })
        .opt(p.pending_operation_id.as_ref(), |w, op| {
            w.bytes(op);
        })
        .str(&p.reason);
    w.done()
}

/// A decoded `DE` (ids as raw byte vectors).
#[derive(Clone, Debug, PartialEq)]
pub struct PlacementReportParts {
    pub policy_revision: u64,
    pub policy_digest: [u8; 32],
    pub membership_log_id: Option<LogIdV1>,
    pub current_voters: Vec<Vec<u8>>,
    pub current_learners: Vec<Vec<u8>>,
    pub selected_voters: Vec<Vec<u8>>,
    pub selected_learners: Vec<Vec<u8>>,
    pub grace_crossed_members: Vec<Vec<u8>>,
    pub separation: Option<SeparationSummary>,
    pub pending_operation_id: Option<Vec<u8>>,
    pub reason: String,
}

pub fn decode_placement_report(buf: &[u8]) -> Result<PlacementReportParts> {
    let mut r = R::new(buf, ASTER_COORDINATION_TAG_PLACEMENT_REPORT)?;
    let u8f = |r: &mut R<'_>, what: &str| -> Result<u8> {
        let v = r.uint()?;
        u8::try_from(v).map_err(|_| anyhow!("UCv1: {what} {v} out of range"))
    };
    let p = PlacementReportParts {
        policy_revision: r.uint()?,
        policy_digest: r.fixed::<32>("policy_digest")?,
        membership_log_id: r.opt_log_id()?,
        current_voters: r.byte_list()?,
        current_learners: r.byte_list()?,
        selected_voters: r.byte_list()?,
        selected_learners: r.byte_list()?,
        grace_crossed_members: r.byte_list()?,
        separation: r.opt(|r| {
            Ok(SeparationSummary {
                separated_pairs: u8f(r, "separated_pairs")?,
                connected_pairs: u8f(r, "connected_pairs")?,
                unknown_pairs: u8f(r, "unknown_pairs")?,
                total_pairs: u8f(r, "total_pairs")?,
                represented_clusters: u8f(r, "represented_clusters")?,
                diversity_proved: r.bool()?,
            })
        })?,
        pending_operation_id: r.opt_bytes()?,
        reason: r.str()?,
    };
    r.end()?;
    Ok(p)
}
