//! UCv1: the canonical byte encodings of upcall payloads
//! (`docs/_internal/bindings/upcall-design.md` §3; golden vectors in
//! `conformance/vectors/upcalls.json`).
//!
//! Same discipline as the AEv1 RPC envelope (`core/src/envelope.rs`): a
//! leading tag byte names the message, a version byte (`0x01`) follows, then
//! every field in a fixed order with no padding and no trailing bytes, built
//! from the `core::canonical` primitives. Each value has exactly one
//! encoding, so bindings can compare bytes with the vectors.
//!
//! ```text
//! message := tag:u8  0x01  field*
//! uint    := LEB128 varint (minimal), range-checked to the field width
//! u8enum  := one raw byte
//! bool    := 0x00 | 0x01
//! string  := varint((utf8_len << 2) | 2)  utf8          (canonical.rs)
//! bytes   := varint(len)  raw
//! opt<T>  := 0xFD (absent) | 0x00 T
//! list<T> := varint(count)  0x0C  T{count}
//! ```
//!
//! Durations are whole milliseconds (`uint`). Sets (and the `eligible`
//! map) are lists in strictly ascending byte order; a decoder refuses any
//! other order or a duplicate.

use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Result};
use aster_transport_core::canonical as c;

use crate::lease::{DenyReason, Fence, FenceRejection, LeaseOp, LeaseSnapshot, TransitionOutcome};
use crate::topology::{ClusterView, TopologySnapshot};

/// The UCv1 version byte.
pub const VERSION: u8 = 0x01;

/// Message tags (first byte of every UCv1 payload).
pub mod tag {
    pub const LEASE_TRANSITION_REQUEST: u8 = 0xB1;
    pub const TRANSITION_OUTCOME: u8 = 0xB2;
    pub const LEASE_SNAPSHOT_REQUEST: u8 = 0xB3;
    pub const LEASE_SNAPSHOT: u8 = 0xB4;
    pub const LEASE_AUTHORIZE_REQUEST: u8 = 0xB5;
    pub const FENCED_MUTATE_REQUEST: u8 = 0xB6;
    pub const FENCED_MUTATE_RESPONSE: u8 = 0xB7;
    pub const TOPOLOGY_ADMITTED_REQUEST: u8 = 0xB8;
    pub const TOPOLOGY_ADMITTED_RESPONSE: u8 = 0xB9;
    pub const TOPOLOGY_SNAPSHOT_REQUEST: u8 = 0xBA;
    pub const TOPOLOGY_SNAPSHOT: u8 = 0xBB;
    pub const PLACEMENT_POLICY_REQUEST: u8 = 0xBC;
    pub const PLACEMENT_POLICY: u8 = 0xBD;
    pub const PLACEMENT_PROVISION_REQUEST: u8 = 0xBE;
    pub const MEMBER_SEED: u8 = 0xBF;
    pub const EXPOSE_AUTHORIZE_REQUEST: u8 = 0xC0;
    pub const ADMISSION_CONNECT_REQUEST: u8 = 0xC1;
    pub const ADMISSION_HANDSHAKE_REQUEST: u8 = 0xC2;
    pub const ADMISSION_HANDSHAKE_RESPONSE: u8 = 0xC3;
    pub const EXPOSE_HTTP_REQUEST: u8 = 0xC4;
    pub const EXPOSE_HTTP_RESPONSE: u8 = 0xC5;
    pub const EXPOSE_HTTP_TRAILERS: u8 = 0xC6;
    pub const EXPOSE_ROUTE_POLICY_REQUEST: u8 = 0xC7;
    pub const EXPOSE_ROUTE_GRANT: u8 = 0xC8;
    pub const DECISION: u8 = 0xCF;
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
    fn ms(&mut self, d: Duration) -> &mut Self {
        self.uint(d.as_millis().min(u64::MAX as u128) as u64)
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
    fn strs(&mut self, v: &[String]) -> &mut Self {
        self.list(v.len());
        for s in v {
            self.str(s);
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
        // Minimal encoding: re-encode and compare length.
        let mut re = Vec::new();
        c::write_varint(&mut re, v);
        ensure!(
            re.len() == self.pos - start,
            "UCv1: non-minimal varint at offset {start}"
        );
        Ok(v)
    }
    fn uint_max(&mut self, max: u64, what: &str) -> Result<u64> {
        let v = self.uint()?;
        ensure!(v <= max, "UCv1: {what} {v} out of range (max {max})");
        Ok(v)
    }
    fn ms(&mut self) -> Result<Duration> {
        Ok(Duration::from_millis(self.uint()?))
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
    #[cfg_attr(not(feature = "coordination-client"), allow(dead_code))]
    fn fixed32(&mut self, what: &str) -> Result<[u8; 32]> {
        let b = self.bytes()?;
        b.as_slice()
            .try_into()
            .map_err(|_| anyhow!("UCv1: {what} must be 32 bytes, got {}", b.len()))
    }
    fn list(&mut self) -> Result<usize> {
        let start = self.pos;
        self.uint()?; // minimal-varint check
        self.pos = start;
        let n = c::read_list_header(self.buf, &mut self.pos)?;
        // Every element takes at least one byte.
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
    fn strs(&mut self) -> Result<Vec<String>> {
        let n = self.list()?;
        (0..n).map(|_| self.str()).collect()
    }
    fn end(self) -> Result<()> {
        c::expect_eof(self.buf, self.pos)
    }
}

#[cfg_attr(not(feature = "coordination-client"), allow(dead_code))]
fn ascending<T: Ord>(items: &[T], what: &str) -> Result<()> {
    ensure!(
        items.windows(2).all(|w| w[0] < w[1]),
        "UCv1: {what} not strictly ascending"
    );
    Ok(())
}

// ── leases ─────────────────────────────────────────────────────────────────

const OP_ACQUIRE: u8 = 1;
const OP_RENEW: u8 = 2;
const OP_RELEASE: u8 = 3;
const OP_REVOKE: u8 = 4;

fn put_op(w: &mut W, op: &LeaseOp) {
    match op {
        LeaseOp::Acquire { candidate, ttl } => {
            w.u8(OP_ACQUIRE).str(candidate).ms(*ttl);
        }
        LeaseOp::Renew { holder, epoch } => {
            w.u8(OP_RENEW).str(holder).uint(*epoch);
        }
        LeaseOp::Release { holder, epoch } => {
            w.u8(OP_RELEASE).str(holder).uint(*epoch);
        }
        LeaseOp::Revoke { authority } => {
            w.u8(OP_REVOKE).str(authority);
        }
    }
}

fn get_op(r: &mut R<'_>) -> Result<LeaseOp> {
    Ok(match r.u8()? {
        OP_ACQUIRE => LeaseOp::Acquire {
            candidate: r.str()?,
            ttl: r.ms()?,
        },
        OP_RENEW => LeaseOp::Renew {
            holder: r.str()?,
            epoch: r.uint()?,
        },
        OP_RELEASE => LeaseOp::Release {
            holder: r.str()?,
            epoch: r.uint()?,
        },
        OP_REVOKE => LeaseOp::Revoke {
            authority: r.str()?,
        },
        other => bail!("UCv1: unknown lease op {other}"),
    })
}

fn put_fence(w: &mut W, f: &Fence) {
    w.str(&f.resource).uint(f.epoch).str(&f.holder);
}

fn get_fence(r: &mut R<'_>) -> Result<Fence> {
    Ok(Fence {
        resource: r.str()?,
        epoch: r.uint()?,
        holder: r.str()?,
    })
}

fn put_snapshot(w: &mut W, s: &LeaseSnapshot) {
    w.str(&s.resource)
        .uint(s.epoch)
        .opt(s.holder.as_deref(), |w, h| {
            w.str(h);
        })
        .bool(s.held)
        .opt(s.remaining, |w, d| {
            w.ms(d);
        });
}

fn get_snapshot(r: &mut R<'_>) -> Result<LeaseSnapshot> {
    Ok(LeaseSnapshot {
        resource: r.str()?,
        epoch: r.uint()?,
        holder: r.opt(|r| r.str())?,
        held: r.bool()?,
        remaining: r.opt(|r| r.ms())?,
    })
}

/// `LeaseSerializer::transition` request: `resource:string op:LeaseOp`.
pub fn encode_lease_transition_request(resource: &str, op: &LeaseOp) -> Vec<u8> {
    let mut w = W::new(tag::LEASE_TRANSITION_REQUEST);
    w.str(resource);
    put_op(&mut w, op);
    w.done()
}

pub fn decode_lease_transition_request(buf: &[u8]) -> Result<(String, LeaseOp)> {
    let mut r = R::new(buf, tag::LEASE_TRANSITION_REQUEST)?;
    let resource = r.str()?;
    let op = get_op(&mut r)?;
    r.end()?;
    Ok((resource, op))
}

const OUT_GRANTED: u8 = 1;
const OUT_RENEWED: u8 = 2;
const OUT_RELEASED: u8 = 3;
const OUT_REVOKED: u8 = 4;
const OUT_DENIED: u8 = 5;

fn deny_code(r: DenyReason) -> u8 {
    match r {
        DenyReason::HeldByOther => 1,
        DenyReason::EpochMismatch => 2,
        DenyReason::NotHolder => 3,
        DenyReason::Expired => 4,
        DenyReason::NotHeld => 5,
        DenyReason::Unauthorized => 6,
    }
}

fn deny_from(code: u8) -> Result<DenyReason> {
    Ok(match code {
        1 => DenyReason::HeldByOther,
        2 => DenyReason::EpochMismatch,
        3 => DenyReason::NotHolder,
        4 => DenyReason::Expired,
        5 => DenyReason::NotHeld,
        6 => DenyReason::Unauthorized,
        other => bail!("UCv1: unknown deny reason {other}"),
    })
}

/// `TransitionOutcome`: `outcome:u8enum` then, for `Granted(1)` a fence,
/// for `Denied(5)` `reason:u8enum snapshot`.
pub fn encode_transition_outcome(o: &TransitionOutcome) -> Vec<u8> {
    let mut w = W::new(tag::TRANSITION_OUTCOME);
    match o {
        TransitionOutcome::Granted(f) => {
            w.u8(OUT_GRANTED);
            put_fence(&mut w, f);
        }
        TransitionOutcome::Renewed => {
            w.u8(OUT_RENEWED);
        }
        TransitionOutcome::Released => {
            w.u8(OUT_RELEASED);
        }
        TransitionOutcome::Revoked => {
            w.u8(OUT_REVOKED);
        }
        TransitionOutcome::Denied(reason, snap) => {
            w.u8(OUT_DENIED).u8(deny_code(*reason));
            put_snapshot(&mut w, snap);
        }
    }
    w.done()
}

pub fn decode_transition_outcome(buf: &[u8]) -> Result<TransitionOutcome> {
    let mut r = R::new(buf, tag::TRANSITION_OUTCOME)?;
    let out = match r.u8()? {
        OUT_GRANTED => TransitionOutcome::Granted(get_fence(&mut r)?),
        OUT_RENEWED => TransitionOutcome::Renewed,
        OUT_RELEASED => TransitionOutcome::Released,
        OUT_REVOKED => TransitionOutcome::Revoked,
        OUT_DENIED => {
            let reason = deny_from(r.u8()?)?;
            TransitionOutcome::Denied(reason, get_snapshot(&mut r)?)
        }
        other => bail!("UCv1: unknown transition outcome {other}"),
    };
    r.end()?;
    Ok(out)
}

/// `LeaseSerializer::snapshot` request: `resource:string`.
pub fn encode_lease_snapshot_request(resource: &str) -> Vec<u8> {
    let mut w = W::new(tag::LEASE_SNAPSHOT_REQUEST);
    w.str(resource);
    w.done()
}

pub fn decode_lease_snapshot_request(buf: &[u8]) -> Result<String> {
    let mut r = R::new(buf, tag::LEASE_SNAPSHOT_REQUEST)?;
    let resource = r.str()?;
    r.end()?;
    Ok(resource)
}

/// `LeaseSnapshot`: `resource:string epoch:uint holder:opt<string>
/// held:bool remaining_ms:opt<uint>`.
pub fn encode_lease_snapshot(s: &LeaseSnapshot) -> Vec<u8> {
    let mut w = W::new(tag::LEASE_SNAPSHOT);
    put_snapshot(&mut w, s);
    w.done()
}

pub fn decode_lease_snapshot(buf: &[u8]) -> Result<LeaseSnapshot> {
    let mut r = R::new(buf, tag::LEASE_SNAPSHOT)?;
    let s = get_snapshot(&mut r)?;
    r.end()?;
    Ok(s)
}

/// `LeaseAuthorizer` request: `op:LeaseOp`.
pub fn encode_lease_authorize_request(op: &LeaseOp) -> Vec<u8> {
    let mut w = W::new(tag::LEASE_AUTHORIZE_REQUEST);
    put_op(&mut w, op);
    w.done()
}

pub fn decode_lease_authorize_request(buf: &[u8]) -> Result<LeaseOp> {
    let mut r = R::new(buf, tag::LEASE_AUTHORIZE_REQUEST)?;
    let op = get_op(&mut r)?;
    r.end()?;
    Ok(op)
}

/// `FencedResource::mutate` request: `fence writer:string op:bytes`.
pub fn encode_fenced_mutate_request(fence: &Fence, writer: &str, op: &[u8]) -> Vec<u8> {
    let mut w = W::new(tag::FENCED_MUTATE_REQUEST);
    put_fence(&mut w, fence);
    w.str(writer).bytes(op);
    w.done()
}

pub fn decode_fenced_mutate_request(buf: &[u8]) -> Result<(Fence, String, Vec<u8>)> {
    let mut r = R::new(buf, tag::FENCED_MUTATE_REQUEST)?;
    let fence = get_fence(&mut r)?;
    let writer = r.str()?;
    let op = r.bytes()?;
    r.end()?;
    Ok((fence, writer, op))
}

const REJ_STALE: u8 = 1;
const REJ_NOT_HOLDER: u8 = 2;
const REJ_NOT_HELD: u8 = 3;
const REJ_FUTURE: u8 = 4;

/// `FencedResource::mutate` result: `0 output:bytes` (applied), or
/// `1 rejection:u8enum carried:uint current:uint` (fenced; the epochs are
/// 0 for `NotHolder(2)` / `NotHeld(3)`).
pub fn encode_fenced_mutate_response(res: &Result<Vec<u8>, FenceRejection>) -> Vec<u8> {
    let mut w = W::new(tag::FENCED_MUTATE_RESPONSE);
    match res {
        Ok(out) => {
            w.u8(0).bytes(out);
        }
        Err(rej) => {
            let (code, carried, current) = match rej {
                FenceRejection::StaleEpoch { carried, current } => (REJ_STALE, *carried, *current),
                FenceRejection::NotHolder => (REJ_NOT_HOLDER, 0, 0),
                FenceRejection::NotHeld => (REJ_NOT_HELD, 0, 0),
                FenceRejection::FutureEpoch { carried, current } => {
                    (REJ_FUTURE, *carried, *current)
                }
            };
            w.u8(1).u8(code).uint(carried).uint(current);
        }
    }
    w.done()
}

pub fn decode_fenced_mutate_response(buf: &[u8]) -> Result<Result<Vec<u8>, FenceRejection>> {
    let mut r = R::new(buf, tag::FENCED_MUTATE_RESPONSE)?;
    let res = match r.u8()? {
        0 => Ok(r.bytes()?),
        1 => {
            let code = r.u8()?;
            let carried = r.uint()?;
            let current = r.uint()?;
            Err(match code {
                REJ_STALE => FenceRejection::StaleEpoch { carried, current },
                REJ_NOT_HOLDER | REJ_NOT_HELD => {
                    ensure!(
                        carried == 0 && current == 0,
                        "UCv1: epochs must be 0 for rejection {code}"
                    );
                    if code == REJ_NOT_HOLDER {
                        FenceRejection::NotHolder
                    } else {
                        FenceRejection::NotHeld
                    }
                }
                REJ_FUTURE => FenceRejection::FutureEpoch { carried, current },
                other => bail!("UCv1: unknown fence rejection {other}"),
            })
        }
        other => bail!("UCv1: unknown fenced-mutate result {other}"),
    };
    r.end()?;
    Ok(res)
}

// ── decisions ──────────────────────────────────────────────────────────────

/// A yes/no answer: `allowed:bool`. The response of every decision kind
/// except `AdmissionHandshake`.
pub fn encode_decision(allowed: bool) -> Vec<u8> {
    let mut w = W::new(tag::DECISION);
    w.bool(allowed);
    w.done()
}

pub fn decode_decision(buf: &[u8]) -> Result<bool> {
    let mut r = R::new(buf, tag::DECISION)?;
    let v = r.bool()?;
    r.end()?;
    Ok(v)
}

// ── topology ───────────────────────────────────────────────────────────────

/// Topology `AdmittedFn`, batched: `node_ids:list<string>` (lowercase hex).
pub fn encode_topology_admitted_request(node_ids: &[String]) -> Vec<u8> {
    let mut w = W::new(tag::TOPOLOGY_ADMITTED_REQUEST);
    w.strs(node_ids);
    w.done()
}

pub fn decode_topology_admitted_request(buf: &[u8]) -> Result<Vec<String>> {
    let mut r = R::new(buf, tag::TOPOLOGY_ADMITTED_REQUEST)?;
    let ids = r.strs()?;
    r.end()?;
    Ok(ids)
}

/// One `bool` per requested id, same order: `admitted:list<bool>`.
pub fn encode_topology_admitted_response(admitted: &[bool]) -> Vec<u8> {
    let mut w = W::new(tag::TOPOLOGY_ADMITTED_RESPONSE);
    w.list(admitted.len());
    for a in admitted {
        w.bool(*a);
    }
    w.done()
}

pub fn decode_topology_admitted_response(buf: &[u8]) -> Result<Vec<bool>> {
    let mut r = R::new(buf, tag::TOPOLOGY_ADMITTED_RESPONSE)?;
    let n = r.list()?;
    let v = (0..n).map(|_| r.bool()).collect::<Result<Vec<_>>>()?;
    r.end()?;
    Ok(v)
}

/// Placement `TopologySource::snapshot` request: no fields.
pub fn encode_topology_snapshot_request() -> Vec<u8> {
    W::new(tag::TOPOLOGY_SNAPSHOT_REQUEST).done()
}

pub fn decode_topology_snapshot_request(buf: &[u8]) -> Result<()> {
    R::new(buf, tag::TOPOLOGY_SNAPSHOT_REQUEST)?.end()
}

/// The parts a serialized topology view carries -- exactly what
/// `SharedView::from_parts` takes, so a foreign snapshot answers
/// `cluster_of` / `separated` by the same rules as a live one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopologyViewParts {
    pub clusters: Vec<ClusterView>,
    /// Corroborated far pairs (hex ids, any order within a pair).
    pub far_pairs: Vec<(String, String)>,
    pub separation_coverage: u64,
}

impl TopologyViewParts {
    /// Build the immutable snapshot the placement planner reads.
    pub fn into_snapshot(self) -> TopologySnapshot {
        TopologySnapshot::from_view(std::sync::Arc::new(
            aster_transport_core::CoreTopoSharedView::from_parts(
                self.clusters,
                self.far_pairs,
                usize::try_from(self.separation_coverage).unwrap_or(usize::MAX),
            ),
        ))
    }
}

/// `clusters:list<{members:list<string> bridge:string
/// witnesses:list<string>}> far_pairs:list<{a:string b:string}>
/// separation_coverage:uint`.
pub fn encode_topology_snapshot(v: &TopologyViewParts) -> Vec<u8> {
    let mut w = W::new(tag::TOPOLOGY_SNAPSHOT);
    w.list(v.clusters.len());
    for cl in &v.clusters {
        w.strs(&cl.members).str(&cl.bridge).strs(&cl.witnesses);
    }
    w.list(v.far_pairs.len());
    for (a, b) in &v.far_pairs {
        w.str(a).str(b);
    }
    w.uint(v.separation_coverage);
    w.done()
}

pub fn decode_topology_snapshot(buf: &[u8]) -> Result<TopologyViewParts> {
    let mut r = R::new(buf, tag::TOPOLOGY_SNAPSHOT)?;
    let n = r.list()?;
    let mut clusters = Vec::new();
    for _ in 0..n {
        clusters.push(ClusterView {
            members: r.strs()?,
            bridge: r.str()?,
            witnesses: r.strs()?,
        });
    }
    let n = r.list()?;
    let mut far_pairs = Vec::new();
    for _ in 0..n {
        far_pairs.push((r.str()?, r.str()?));
    }
    let separation_coverage = r.uint()?;
    r.end()?;
    Ok(TopologyViewParts {
        clusters,
        far_pairs,
        separation_coverage,
    })
}

// ── expose ─────────────────────────────────────────────────────────────────

/// expose `AuthorizeFn` request: `service_id:string peer_id:string
/// metadata:bytes`. Answered with a [decision](encode_decision).
pub fn encode_expose_authorize_request(
    service_id: &str,
    peer_id: &str,
    metadata: &[u8],
) -> Vec<u8> {
    let mut w = W::new(tag::EXPOSE_AUTHORIZE_REQUEST);
    w.str(service_id).str(peer_id).bytes(metadata);
    w.done()
}

pub fn decode_expose_authorize_request(buf: &[u8]) -> Result<(String, String, Vec<u8>)> {
    let mut r = R::new(buf, tag::EXPOSE_AUTHORIZE_REQUEST)?;
    let v = (r.str()?, r.str()?, r.bytes()?);
    r.end()?;
    Ok(v)
}

// ── expose HTTP (R9) ───────────────────────────────────────────────────────

/// One HTTP header field: `(name, value)`. The name is lowercase (HTTP
/// names are case-insensitive, so UCv1 fixes one spelling); the value is raw
/// bytes (header values need not be UTF-8). A repeated name is one entry per
/// value, in order.
pub type HttpHeader = (String, Vec<u8>);

/// Highest HTTP version code: `0` HTTP/0.9, `1` HTTP/1.0, `2` HTTP/1.1,
/// `3` HTTP/2, `4` HTTP/3 (the `aster-expose` head codec's codes).
pub const HTTP_VERSION_MAX: u8 = 4;

/// Where a message body travels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpBodyMode {
    /// The whole body is in the message (possibly empty). Wire: `0 body:bytes`.
    Inline(Vec<u8>),
    /// The body streams through the exchange's body handle (`stream_id`):
    /// the binding reads the request body / writes the response body in
    /// chunks, then finishes it. Wire: `1`.
    Stream,
}

/// Kind 49 `expose_http` request: one HTTP request for a foreign handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExposeHttpRequest {
    /// The exposed service the request arrived for.
    pub service_id: String,
    /// The transport-authenticated peer (`AuthenticatedPeer`), when the
    /// request came over the relay; absent otherwise. Handlers that
    /// attribute requests must refuse an absent peer.
    pub peer_id: Option<String>,
    /// The exchange's body handle: read the request body and write the
    /// response body with it. Unique per body table, never reused.
    pub stream_id: u64,
    /// HTTP version code (`0..=`[`HTTP_VERSION_MAX`]).
    pub version: u8,
    pub method: String,
    /// The request target as received (path and query; absolute form when
    /// the client sent one).
    pub uri: String,
    pub headers: Vec<HttpHeader>,
    pub body: HttpBodyMode,
}

/// Kind 49 `expose_http` response: the response head, and the body inline
/// or streamed through `stream_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExposeHttpResponse {
    /// `100..=999`.
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: HttpBodyMode,
}

/// A route spec as the edge's route policy sees it (`aster-expose`
/// `RouteSpec`; `protocol` `0` http, `1` tcp).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExposeRouteSpec {
    pub host: String,
    pub port: u16,
    pub protocol: u8,
    pub service_id: String,
}

const BODY_INLINE: u8 = 0;
const BODY_STREAM: u8 = 1;

fn put_headers(w: &mut W, headers: &[HttpHeader]) {
    w.list(headers.len());
    for (name, value) in headers {
        w.str(name).bytes(value);
    }
}

fn get_headers(r: &mut R<'_>) -> Result<Vec<HttpHeader>> {
    let n = r.list()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let name = r.str()?;
        ensure!(
            !name.is_empty() && !name.bytes().any(|b| b.is_ascii_uppercase()),
            "UCv1: header name {name:?} must be non-empty and lowercase"
        );
        out.push((name, r.bytes()?));
    }
    Ok(out)
}

fn put_body(w: &mut W, body: &HttpBodyMode) {
    match body {
        HttpBodyMode::Inline(b) => {
            w.u8(BODY_INLINE).bytes(b);
        }
        HttpBodyMode::Stream => {
            w.u8(BODY_STREAM);
        }
    }
}

fn get_body(r: &mut R<'_>) -> Result<HttpBodyMode> {
    match r.u8()? {
        BODY_INLINE => Ok(HttpBodyMode::Inline(r.bytes()?)),
        BODY_STREAM => Ok(HttpBodyMode::Stream),
        other => bail!("UCv1: unknown body mode {other}"),
    }
}

/// `service_id:string peer_id:opt<string> stream_id:uint version:u8enum
/// method:string uri:string headers:list<Header> body:Body`, where
/// `Header := name:string value:bytes` and `Body := 0 bytes | 1`.
pub fn encode_expose_http_request(req: &ExposeHttpRequest) -> Vec<u8> {
    let mut w = W::new(tag::EXPOSE_HTTP_REQUEST);
    w.str(&req.service_id)
        .opt(req.peer_id.as_deref(), |w, p| {
            w.str(p);
        })
        .uint(req.stream_id)
        .u8(req.version)
        .str(&req.method)
        .str(&req.uri);
    put_headers(&mut w, &req.headers);
    put_body(&mut w, &req.body);
    w.done()
}

pub fn decode_expose_http_request(buf: &[u8]) -> Result<ExposeHttpRequest> {
    let mut r = R::new(buf, tag::EXPOSE_HTTP_REQUEST)?;
    let service_id = r.str()?;
    let peer_id = r.opt(|r| r.str())?;
    let stream_id = r.uint()?;
    let version = r.u8()?;
    ensure!(
        version <= HTTP_VERSION_MAX,
        "UCv1: unknown HTTP version {version}"
    );
    let method = r.str()?;
    ensure!(!method.is_empty(), "UCv1: empty HTTP method");
    let uri = r.str()?;
    let headers = get_headers(&mut r)?;
    let body = get_body(&mut r)?;
    r.end()?;
    Ok(ExposeHttpRequest {
        service_id,
        peer_id,
        stream_id,
        version,
        method,
        uri,
        headers,
        body,
    })
}

/// `status:uint(100..=999) headers:list<Header> body:Body`.
pub fn encode_expose_http_response(resp: &ExposeHttpResponse) -> Vec<u8> {
    let mut w = W::new(tag::EXPOSE_HTTP_RESPONSE);
    w.uint(resp.status as u64);
    put_headers(&mut w, &resp.headers);
    put_body(&mut w, &resp.body);
    w.done()
}

pub fn decode_expose_http_response(buf: &[u8]) -> Result<ExposeHttpResponse> {
    let mut r = R::new(buf, tag::EXPOSE_HTTP_RESPONSE)?;
    let status = r.uint_max(999, "HTTP status")? as u16;
    ensure!(status >= 100, "UCv1: HTTP status {status} below 100");
    let headers = get_headers(&mut r)?;
    let body = get_body(&mut r)?;
    r.end()?;
    Ok(ExposeHttpResponse {
        status,
        headers,
        body,
    })
}

/// Trailers that end a streamed body: `headers:list<Header>`.
pub fn encode_expose_http_trailers(trailers: &[HttpHeader]) -> Vec<u8> {
    let mut w = W::new(tag::EXPOSE_HTTP_TRAILERS);
    put_headers(&mut w, trailers);
    w.done()
}

pub fn decode_expose_http_trailers(buf: &[u8]) -> Result<Vec<HttpHeader>> {
    let mut r = R::new(buf, tag::EXPOSE_HTTP_TRAILERS)?;
    let v = get_headers(&mut r)?;
    r.end()?;
    Ok(v)
}

/// Kind 50 `expose_route_policy` request: `peer_id:string metadata:bytes
/// specs:list<{host:string port:uint(u16) protocol:u8enum
/// service_id:string}>`. Answered with an [`encode_expose_route_grant`].
pub fn encode_expose_route_policy_request(
    peer_id: &str,
    metadata: &[u8],
    specs: &[ExposeRouteSpec],
) -> Vec<u8> {
    let mut w = W::new(tag::EXPOSE_ROUTE_POLICY_REQUEST);
    w.str(peer_id).bytes(metadata).list(specs.len());
    for s in specs {
        w.str(&s.host)
            .uint(s.port as u64)
            .u8(s.protocol)
            .str(&s.service_id);
    }
    w.done()
}

pub fn decode_expose_route_policy_request(
    buf: &[u8],
) -> Result<(String, Vec<u8>, Vec<ExposeRouteSpec>)> {
    let mut r = R::new(buf, tag::EXPOSE_ROUTE_POLICY_REQUEST)?;
    let peer_id = r.str()?;
    let metadata = r.bytes()?;
    let n = r.list()?;
    let mut specs = Vec::with_capacity(n);
    for _ in 0..n {
        let host = r.str()?;
        let port = r.uint_max(u16::MAX as u64, "port")? as u16;
        let protocol = r.u8()?;
        ensure!(protocol <= 1, "UCv1: unknown route protocol {protocol}");
        specs.push(ExposeRouteSpec {
            host,
            port,
            protocol,
            service_id: r.str()?,
        });
    }
    r.end()?;
    Ok((peer_id, metadata, specs))
}

/// The granted subset of a route request: `granted:list<uint(u32)>`,
/// indexes into the request's `specs`, strictly ascending.
pub fn encode_expose_route_grant(granted: &[u32]) -> Vec<u8> {
    let mut w = W::new(tag::EXPOSE_ROUTE_GRANT);
    w.list(granted.len());
    for i in granted {
        w.uint(*i as u64);
    }
    w.done()
}

pub fn decode_expose_route_grant(buf: &[u8]) -> Result<Vec<u32>> {
    let mut r = R::new(buf, tag::EXPOSE_ROUTE_GRANT)?;
    let n = r.list()?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(r.uint_max(u32::MAX as u64, "route index")? as u32);
    }
    r.end()?;
    ensure!(
        v.windows(2).all(|w| w[0] < w[1]),
        "UCv1: granted route indexes not strictly ascending"
    );
    Ok(v)
}

// ── Gate 0 admission ───────────────────────────────────────────────────────

/// Outbound connect hook: `peer:string alpn:bytes`. Answered with a
/// [decision](encode_decision).
pub fn encode_admission_connect_request(peer: &str, alpn: &[u8]) -> Vec<u8> {
    let mut w = W::new(tag::ADMISSION_CONNECT_REQUEST);
    w.str(peer).bytes(alpn);
    w.done()
}

pub fn decode_admission_connect_request(buf: &[u8]) -> Result<(String, Vec<u8>)> {
    let mut r = R::new(buf, tag::ADMISSION_CONNECT_REQUEST)?;
    let v = (r.str()?, r.bytes()?);
    r.end()?;
    Ok(v)
}

/// Inbound after-handshake hook: `peer:string alpn:bytes is_alive:bool`.
pub fn encode_admission_handshake_request(peer: &str, alpn: &[u8], is_alive: bool) -> Vec<u8> {
    let mut w = W::new(tag::ADMISSION_HANDSHAKE_REQUEST);
    w.str(peer).bytes(alpn).bool(is_alive);
    w.done()
}

pub fn decode_admission_handshake_request(buf: &[u8]) -> Result<(String, Vec<u8>, bool)> {
    let mut r = R::new(buf, tag::ADMISSION_HANDSHAKE_REQUEST)?;
    let v = (r.str()?, r.bytes()?, r.bool()?);
    r.end()?;
    Ok(v)
}

/// The after-handshake answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandshakeDecision {
    Accept,
    Reject { code: u32, reason: Vec<u8> },
}

/// `accept:bool code:uint reason:bytes` (code 0 and empty reason when
/// accepting).
pub fn encode_admission_handshake_response(d: &HandshakeDecision) -> Vec<u8> {
    let mut w = W::new(tag::ADMISSION_HANDSHAKE_RESPONSE);
    match d {
        HandshakeDecision::Accept => w.bool(true).uint(0).bytes(&[]),
        HandshakeDecision::Reject { code, reason } => {
            w.bool(false).uint(*code as u64).bytes(reason)
        }
    };
    w.done()
}

pub fn decode_admission_handshake_response(buf: &[u8]) -> Result<HandshakeDecision> {
    let mut r = R::new(buf, tag::ADMISSION_HANDSHAKE_RESPONSE)?;
    let accept = r.bool()?;
    let code = r.uint_max(u32::MAX as u64, "reject code")? as u32;
    let reason = r.bytes()?;
    r.end()?;
    if accept {
        ensure!(
            code == 0 && reason.is_empty(),
            "UCv1: accept carries no code or reason"
        );
        Ok(HandshakeDecision::Accept)
    } else {
        Ok(HandshakeDecision::Reject { code, reason })
    }
}

// ── placement (coordination-client) ────────────────────────────────────────

#[cfg(feature = "coordination-client")]
pub use placement_codec::*;

#[cfg(feature = "coordination-client")]
mod placement_codec {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::coordination::client::manager::ProvisionRequest;
    use crate::coordination::client::placement::{PlacementCandidate, PlacementPolicy};
    use crate::coordination::contract::{LogIdV1, MemberSeedV1};
    use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId};

    fn put_seed(w: &mut W, s: &MemberSeedV1) {
        w.bytes(&s.node_id)
            .str(&s.ticket)
            .opt(s.signed_join_permit.as_deref(), |w, p| {
                w.bytes(p);
            });
    }

    fn get_seed(r: &mut R<'_>) -> Result<MemberSeedV1> {
        Ok(MemberSeedV1 {
            node_id: r.bytes()?,
            ticket: r.str()?,
            signed_join_permit: r.opt(|r| r.bytes())?,
        })
    }

    fn put_ids(w: &mut W, ids: &BTreeSet<CoordinationNodeId>) {
        w.list(ids.len());
        for id in ids {
            w.bytes(id.as_bytes());
        }
    }

    fn get_ids(r: &mut R<'_>, what: &str) -> Result<BTreeSet<CoordinationNodeId>> {
        let n = r.list()?;
        let mut v = Vec::new();
        for _ in 0..n {
            v.push(CoordinationNodeId::from_array(r.fixed32(what)?));
        }
        ascending(&v, what)?;
        Ok(v.into_iter().collect())
    }

    fn put_log_id(w: &mut W, l: &LogIdV1) {
        w.uint(l.term).bytes(&l.leader_node_id).uint(l.index);
    }

    fn get_log_id(r: &mut R<'_>) -> Result<LogIdV1> {
        Ok(LogIdV1 {
            term: r.uint()?,
            leader_node_id: r.bytes()?,
            index: r.uint()?,
        })
    }

    /// `PlacementPolicySource::policy` request: no fields.
    pub fn encode_placement_policy_request() -> Vec<u8> {
        W::new(tag::PLACEMENT_POLICY_REQUEST).done()
    }

    pub fn decode_placement_policy_request(buf: &[u8]) -> Result<()> {
        R::new(buf, tag::PLACEMENT_POLICY_REQUEST)?.end()
    }

    /// `PlacementPolicy`: `revision:uint digest:bytes[32]
    /// authority_public_key:bytes[32] authority_domain:string
    /// target_voters:uint(u8) target_retained_learners:uint(u8)
    /// eligible:list<{node_id:bytes[32] member_seed:opt<MemberSeed>}>
    /// pinned_voters:list<bytes[32]> excluded:list<bytes[32]>
    /// minimum_separated_voter_pairs:uint(u8) replacement_grace_ms:uint
    /// allow_healthy_rebalance:bool`. `MemberSeed` := `node_id:bytes
    /// ticket:string signed_join_permit:opt<bytes>`. The `eligible` map is
    /// keyed by each candidate's own node id.
    pub fn encode_placement_policy(p: &PlacementPolicy) -> Vec<u8> {
        let mut w = W::new(tag::PLACEMENT_POLICY);
        w.uint(p.revision)
            .bytes(&p.digest)
            .bytes(&p.authority_public_key)
            .str(&p.authority_domain)
            .uint(p.target_voters as u64)
            .uint(p.target_retained_learners as u64);
        w.list(p.eligible.len());
        for (id, cand) in &p.eligible {
            w.bytes(id.as_bytes())
                .opt(cand.member_seed.as_ref(), |w, s| {
                    put_seed(w, s);
                });
        }
        put_ids(&mut w, &p.pinned_voters);
        put_ids(&mut w, &p.excluded);
        w.uint(p.minimum_separated_voter_pairs as u64)
            .ms(p.replacement_grace)
            .bool(p.allow_healthy_rebalance);
        w.done()
    }

    pub fn decode_placement_policy(buf: &[u8]) -> Result<PlacementPolicy> {
        let mut r = R::new(buf, tag::PLACEMENT_POLICY)?;
        let revision = r.uint()?;
        let digest = r.fixed32("digest")?;
        let authority_public_key = r.fixed32("authority_public_key")?;
        let authority_domain = r.str()?;
        let target_voters = r.uint_max(u8::MAX as u64, "target_voters")? as u8;
        let target_retained_learners =
            r.uint_max(u8::MAX as u64, "target_retained_learners")? as u8;
        let n = r.list()?;
        let mut keys = Vec::new();
        let mut eligible = BTreeMap::new();
        for _ in 0..n {
            let id = CoordinationNodeId::from_array(r.fixed32("eligible node id")?);
            let member_seed = r.opt(get_seed)?;
            keys.push(id);
            eligible.insert(
                id,
                PlacementCandidate {
                    node_id: id,
                    member_seed,
                },
            );
        }
        ascending(&keys, "eligible")?;
        let pinned_voters = get_ids(&mut r, "pinned_voters")?;
        let excluded = get_ids(&mut r, "excluded")?;
        let minimum_separated_voter_pairs =
            r.uint_max(u8::MAX as u64, "minimum_separated_voter_pairs")? as u8;
        let replacement_grace = r.ms()?;
        let allow_healthy_rebalance = r.bool()?;
        r.end()?;
        Ok(PlacementPolicy {
            revision,
            digest,
            authority_public_key,
            authority_domain,
            target_voters,
            target_retained_learners,
            eligible,
            pinned_voters,
            excluded,
            minimum_separated_voter_pairs,
            replacement_grace,
            allow_healthy_rebalance,
        })
    }

    /// `PlacementProvisioner::provision` request: `group_id:bytes[32]
    /// policy_digest:bytes[32] membership_log_id:opt<{term:uint
    /// leader_node_id:bytes index:uint}> node_id:bytes[32]`.
    pub fn encode_provision_request(p: &ProvisionRequest) -> Vec<u8> {
        let mut w = W::new(tag::PLACEMENT_PROVISION_REQUEST);
        w.bytes(p.group_id.as_bytes())
            .bytes(&p.policy_digest)
            .opt(p.membership_log_id.as_ref(), put_log_id)
            .bytes(p.node_id.as_bytes());
        w.done()
    }

    pub fn decode_provision_request(buf: &[u8]) -> Result<ProvisionRequest> {
        let mut r = R::new(buf, tag::PLACEMENT_PROVISION_REQUEST)?;
        let group_id = CoordinationGroupId::from_array(r.fixed32("group_id")?);
        let policy_digest = r.fixed32("policy_digest")?;
        let membership_log_id = r.opt(get_log_id)?;
        let node_id = CoordinationNodeId::from_array(r.fixed32("node_id")?);
        r.end()?;
        Ok(ProvisionRequest {
            group_id,
            policy_digest,
            membership_log_id,
            node_id,
        })
    }

    /// `MemberSeedV1`: `node_id:bytes ticket:string
    /// signed_join_permit:opt<bytes>`.
    pub fn encode_member_seed(s: &MemberSeedV1) -> Vec<u8> {
        let mut w = W::new(tag::MEMBER_SEED);
        put_seed(&mut w, s);
        w.done()
    }

    pub fn decode_member_seed(buf: &[u8]) -> Result<MemberSeedV1> {
        let mut r = R::new(buf, tag::MEMBER_SEED)?;
        let s = get_seed(&mut r)?;
        r.end()?;
        Ok(s)
    }
}
