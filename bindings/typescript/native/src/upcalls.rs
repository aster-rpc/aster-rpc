//! Upcalls — Rust asking JavaScript a question (plan D6 / R6; design
//! `docs/_internal/bindings/upcall-design.md` §4, NAPI).
//!
//! One [`Upcalls`] table per JavaScript owner. Rust's adapters
//! (`aster::binding_support::upcall::{lease, topology, placement,
//! admission}`) issue typed events into it; the sink here forwards them to a
//! JavaScript callback over a **weak** `ThreadsafeFunction` and never blocks:
//!
//! - events go into the shared bounded [`UpcallQueue`] (a full queue refuses
//!   the upcall, which then fails at once with `Overloaded`; `Cancel` events
//!   are always accepted, and withdraw an upcall JavaScript has not taken
//!   yet);
//! - at most one TSFN call is outstanding; its callback drains the whole
//!   queue into one `UpcallEvent[]` batch, decoding every UCv1 payload in
//!   Rust into a plain object (`upcallCodecDecode`);
//! - the Bun TSFN stall (plan §1.3) is covered by the dispatch server's 2 ms
//!   re-queue watchdog.
//!
//! JavaScript answers with the synchronous `respond(id, status, payload)`,
//! encoding OK payloads with `upcallCodecEncode` (the same codec the vectors
//! test exercises). `invocationId` is a `u64` in Rust; ids count from 1, so
//! they are passed as numbers and an id past 2^53 is refused (never reached
//! in practice).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

use aster::binding_support::upcall::{
    codec, HookFailureMode, UpcallEvent, UpcallKind, UpcallPolicy, UpcallQueue, UpcallSink,
    UpcallStatus, Upcalls as RsUpcalls,
};
use aster::coordination::client::manager::ProvisionRequest;
use aster::coordination::ids::{CoordinationGroupId, CoordinationNodeId};

use crate::coordination::{
    opt_log_in, policy_in, policy_out, CoordinationLogId, CoordinationMemberSeed,
    CoordinationPlacementPolicy,
};
use crate::lease::{Fence, LeaseOp, LeaseSnapshot, TransitionOutcome};
use crate::topology::ClusterView;

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// How long a scheduled drain call may sit unrun before the watchdog queues
/// another one (the dispatch server's Bun workaround, plan §1.3).
const TSFN_WATCHDOG: Duration = Duration::from_millis(2);

/// Default bound on upcalls queued for JavaScript and not yet drained.
const DEFAULT_CAPACITY: u32 = 1024;

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

fn codec_err(e: anyhow::Error) -> Error {
    invalid(format!("upcall codec: {e:#}"))
}

// ── kinds and policies ───────────────────────────────────────────────────────

pub(crate) fn kind_in(code: u32) -> Result<UpcallKind> {
    u16::try_from(code)
        .ok()
        .and_then(UpcallKind::from_code)
        .ok_or_else(|| invalid(format!("unknown upcall kind {code}")))
}

/// The UCv1 message type of a kind's request (`None`: no request payload
/// type — `cancel`).
fn request_type(kind: UpcallKind) -> Option<&'static str> {
    Some(match kind {
        UpcallKind::LeaseTransition => "LeaseTransitionRequest",
        UpcallKind::LeaseSnapshot => "LeaseSnapshotRequest",
        UpcallKind::LeaseAuthorize => "LeaseAuthorizeRequest",
        UpcallKind::FencedMutate => "FencedMutateRequest",
        UpcallKind::TopologyAdmitted => "TopologyAdmittedRequest",
        UpcallKind::TopologySnapshot => "TopologySnapshotRequest",
        UpcallKind::PlacementPolicy => "PlacementPolicyRequest",
        UpcallKind::PlacementProvision => "PlacementProvisionRequest",
        UpcallKind::ExposeAuthorize => "ExposeAuthorizeRequest",
        UpcallKind::ExposeHttp => "ExposeHttpRequest",
        UpcallKind::ExposeRoutePolicy => "ExposeRoutePolicyRequest",
        UpcallKind::AdmissionConnect => "AdmissionConnectRequest",
        UpcallKind::AdmissionHandshake => "AdmissionHandshakeRequest",
        _ => return None,
    })
}

/// The UCv1 message type of a kind's OK response.
fn response_type(kind: UpcallKind) -> Option<&'static str> {
    Some(match kind {
        UpcallKind::LeaseTransition => "TransitionOutcome",
        UpcallKind::LeaseSnapshot => "LeaseSnapshot",
        UpcallKind::LeaseAuthorize | UpcallKind::ExposeAuthorize | UpcallKind::AdmissionConnect => {
            "Decision"
        }
        UpcallKind::FencedMutate => "FencedMutateResponse",
        UpcallKind::TopologyAdmitted => "TopologyAdmittedResponse",
        UpcallKind::TopologySnapshot => "TopologySnapshot",
        UpcallKind::PlacementPolicy => "PlacementPolicy",
        UpcallKind::PlacementProvision => "MemberSeed",
        UpcallKind::ExposeHttp => "ExposeHttpResponse",
        UpcallKind::ExposeRoutePolicy => "ExposeRouteGrant",
        UpcallKind::AdmissionHandshake => "AdmissionHandshakeResponse",
        _ => return None,
    })
}

/// One row of the kind table.
#[napi(object)]
pub struct UpcallKindInfo {
    pub code: u32,
    pub name: String,
    /// A yes/no decision kind (the failure mode applies).
    pub decision: bool,
    /// The UCv1 request message type, if any.
    pub request_type: Option<String>,
    /// The UCv1 OK-response message type, if any.
    pub response_type: Option<String>,
}

/// Every upcall kind, straight from the Rust enum.
#[napi]
pub fn upcall_kinds() -> Vec<UpcallKindInfo> {
    UpcallKind::ALL
        .iter()
        .map(|k| UpcallKindInfo {
            code: k.code().into(),
            name: k.name().into(),
            decision: k.is_decision(),
            request_type: request_type(*k).map(Into::into),
            response_type: response_type(*k).map(Into::into),
        })
        .collect()
}

/// Timeout and failure mode for one kind (the failure mode applies to the
/// decision kinds only).
#[napi(object)]
pub struct UpcallPolicyJs {
    pub timeout_ms: f64,
    #[napi(ts_type = "'fail_open' | 'fail_closed'")]
    pub failure_mode: String,
}

fn policy_from_js(p: &UpcallPolicyJs) -> Result<UpcallPolicy> {
    if !p.timeout_ms.is_finite() || p.timeout_ms <= 0.0 {
        return Err(invalid("timeoutMs must be a positive number of ms"));
    }
    let failure_mode = match p.failure_mode.as_str() {
        "fail_open" => HookFailureMode::FailOpen,
        "fail_closed" => HookFailureMode::FailClosed,
        other => {
            return Err(invalid(format!(
                "unknown failure mode {other:?} (fail_open | fail_closed)"
            )))
        }
    };
    Ok(UpcallPolicy::new(
        Duration::from_secs_f64(p.timeout_ms / 1000.0),
        failure_mode,
    ))
}

fn policy_to_js(p: UpcallPolicy) -> UpcallPolicyJs {
    UpcallPolicyJs {
        timeout_ms: p.timeout.as_secs_f64() * 1000.0,
        failure_mode: match p.failure_mode {
            HookFailureMode::FailOpen => "fail_open",
            _ => "fail_closed",
        }
        .into(),
    }
}

// ── UCv1 payload objects ─────────────────────────────────────────────────────

/// `LeaseSerializer::transition` request (kind 1).
#[napi(object)]
pub struct UpcallLeaseTransitionRequest {
    pub resource: String,
    pub op: LeaseOp,
}

/// `LeaseSerializer::snapshot` request (kind 2).
#[napi(object)]
pub struct UpcallLeaseSnapshotRequest {
    pub resource: String,
}

/// Lease authorizer request (kind 3).
#[napi(object)]
pub struct UpcallLeaseAuthorizeRequest {
    pub op: LeaseOp,
}

/// `FencedResource::mutate` request (kind 4).
#[napi(object)]
pub struct UpcallFencedMutateRequest {
    pub fence: Fence,
    /// The authenticated writer (hex node id).
    pub writer: String,
    pub op: Buffer,
}

/// `FencedResource::mutate` answer (kind 4): `applied` with `output`, or a
/// fence rejection (`carried` / `current` epochs for `stale_epoch` /
/// `future_epoch`).
#[napi(object)]
pub struct UpcallFencedMutateResponse {
    #[napi(ts_type = "'applied' | 'stale_epoch' | 'not_holder' | 'not_held' | 'future_epoch'")]
    pub kind: String,
    pub output: Option<Buffer>,
    pub carried: Option<i64>,
    pub current: Option<i64>,
}

/// Topology `admitted` batch request (kind 16).
#[napi(object)]
pub struct UpcallTopologyAdmittedRequest {
    /// Lowercase hex node ids.
    pub node_ids: Vec<String>,
}

/// Topology `admitted` batch answer: one flag per requested id, in order.
#[napi(object)]
pub struct UpcallTopologyAdmittedResponse {
    pub admitted: Vec<bool>,
}

/// A request message without fields (kinds 17, 32).
#[napi(object)]
pub struct UpcallEmpty {}

/// A serialized shared topology view (kind 17's answer).
#[napi(object)]
pub struct UpcallTopologyView {
    pub clusters: Vec<ClusterView>,
    /// Corroborated far pairs (hex ids).
    #[napi(ts_type = "Array<[string, string]>")]
    pub far_pairs: Vec<Vec<String>>,
    pub separation_coverage: u32,
}

/// A yes/no answer (kinds 3, 48, 64).
#[napi(object)]
pub struct UpcallDecision {
    pub allowed: bool,
}

/// `PlacementProvisioner::provision` request (kind 33): the idempotency
/// tuple — the same tuple must yield the same candidate.
#[napi(object)]
pub struct UpcallPlacementProvisionRequest {
    pub group_id: String,
    pub policy_digest: String,
    pub membership_log_id: Option<CoordinationLogId>,
    pub node_id: String,
}

/// expose `AuthorizeFn` request (kind 48).
#[napi(object)]
pub struct UpcallExposeAuthorizeRequest {
    pub service_id: String,
    pub peer_id: String,
    pub metadata: Buffer,
}

/// One HTTP header (or trailer): `name` is lowercase and non-empty; `value`
/// is raw bytes.
#[napi(object)]
pub struct ExposeHeader {
    pub name: String,
    pub value: Buffer,
}

/// A route an edge binds for a peer (`protocol`: `"http"` or `"tcp"`).
#[napi(object)]
pub struct ExposeRouteSpec {
    pub host: String,
    pub port: u32,
    pub protocol: String,
    pub service_id: String,
}

/// expose HTTP handler request (kind 49). `body` is the whole inline body,
/// or absent when it streams through the exchange's body handle
/// (`streamId`; `Upcalls.httpBody`). `version`: `"HTTP/0.9"` ... `"HTTP/3"`.
#[napi(object)]
pub struct UpcallExposeHttpRequest {
    pub service_id: String,
    pub peer_id: Option<String>,
    pub stream_id: f64,
    pub version: String,
    pub method: String,
    pub uri: String,
    pub headers: Vec<ExposeHeader>,
    pub body: Option<Buffer>,
}

/// expose HTTP handler answer (kind 49). `body`: the inline body, or absent
/// for a body the handler then writes through the body handle.
#[napi(object)]
pub struct UpcallExposeHttpResponse {
    pub status: u32,
    pub headers: Vec<ExposeHeader>,
    pub body: Option<Buffer>,
}

/// Edge route policy request (kind 50).
#[napi(object)]
pub struct UpcallExposeRoutePolicyRequest {
    pub peer_id: String,
    pub metadata: Buffer,
    pub specs: Vec<ExposeRouteSpec>,
}

/// Edge route policy answer (kind 50): indexes into the request's `specs`.
#[napi(object)]
pub struct UpcallExposeRouteGrant {
    pub granted: Vec<u32>,
}

/// Gate 0 outbound connect request (kind 64).
#[napi(object)]
pub struct UpcallAdmissionConnectRequest {
    pub peer: String,
    pub alpn: Buffer,
}

/// Gate 0 inbound after-handshake request (kind 65).
#[napi(object)]
pub struct UpcallAdmissionHandshakeRequest {
    pub peer: String,
    pub alpn: Buffer,
    pub is_alive: bool,
}

/// Gate 0 after-handshake answer (kind 65). `code` / `reason` are ignored
/// when accepting (decoded as `0` / empty).
#[napi(object)]
pub struct UpcallAdmissionHandshakeResponse {
    pub accept: bool,
    pub code: Option<u32>,
    pub reason: Option<Buffer>,
}

/// Any decoded UCv1 message (the JavaScript object of its type).
pub enum UpcallValue {
    LeaseTransitionRequest(UpcallLeaseTransitionRequest),
    TransitionOutcome(TransitionOutcome),
    LeaseSnapshotRequest(UpcallLeaseSnapshotRequest),
    LeaseSnapshot(LeaseSnapshot),
    LeaseAuthorizeRequest(UpcallLeaseAuthorizeRequest),
    FencedMutateRequest(UpcallFencedMutateRequest),
    FencedMutateResponse(UpcallFencedMutateResponse),
    Decision(UpcallDecision),
    TopologyAdmittedRequest(UpcallTopologyAdmittedRequest),
    TopologyAdmittedResponse(UpcallTopologyAdmittedResponse),
    Empty(UpcallEmpty),
    TopologyView(UpcallTopologyView),
    PlacementPolicy(CoordinationPlacementPolicy),
    PlacementProvisionRequest(UpcallPlacementProvisionRequest),
    MemberSeed(CoordinationMemberSeed),
    ExposeAuthorizeRequest(UpcallExposeAuthorizeRequest),
    ExposeHttpRequest(UpcallExposeHttpRequest),
    ExposeHttpResponse(UpcallExposeHttpResponse),
    ExposeHttpTrailers(Vec<ExposeHeader>),
    ExposeRoutePolicyRequest(UpcallExposeRoutePolicyRequest),
    ExposeRouteGrant(UpcallExposeRouteGrant),
    AdmissionConnectRequest(UpcallAdmissionConnectRequest),
    AdmissionHandshakeRequest(UpcallAdmissionHandshakeRequest),
    AdmissionHandshakeResponse(UpcallAdmissionHandshakeResponse),
}

impl TypeName for UpcallValue {
    fn type_name() -> &'static str {
        "UpcallValue"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for UpcallValue {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        use UpcallValue as V;
        unsafe {
            match val {
                V::LeaseTransitionRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::TransitionOutcome(v) => ToNapiValue::to_napi_value(env, v),
                V::LeaseSnapshotRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::LeaseSnapshot(v) => ToNapiValue::to_napi_value(env, v),
                V::LeaseAuthorizeRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::FencedMutateRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::FencedMutateResponse(v) => ToNapiValue::to_napi_value(env, v),
                V::Decision(v) => ToNapiValue::to_napi_value(env, v),
                V::TopologyAdmittedRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::TopologyAdmittedResponse(v) => ToNapiValue::to_napi_value(env, v),
                V::Empty(v) => ToNapiValue::to_napi_value(env, v),
                V::TopologyView(v) => ToNapiValue::to_napi_value(env, v),
                V::PlacementPolicy(v) => ToNapiValue::to_napi_value(env, v),
                V::PlacementProvisionRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::MemberSeed(v) => ToNapiValue::to_napi_value(env, v),
                V::ExposeAuthorizeRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::ExposeHttpRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::ExposeHttpResponse(v) => ToNapiValue::to_napi_value(env, v),
                V::ExposeHttpTrailers(v) => ToNapiValue::to_napi_value(env, v),
                V::ExposeRoutePolicyRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::ExposeRouteGrant(v) => ToNapiValue::to_napi_value(env, v),
                V::AdmissionConnectRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::AdmissionHandshakeRequest(v) => ToNapiValue::to_napi_value(env, v),
                V::AdmissionHandshakeResponse(v) => ToNapiValue::to_napi_value(env, v),
            }
        }
    }
}

fn hex32(s: &str, what: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(s.trim())
        .map_err(|_| invalid(format!("{what} must be 64 hex characters, got {s:?}")))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| invalid(format!("{what} must be 64 hex characters, got {s:?}")))
}

fn epoch_out(v: u64) -> Result<i64> {
    if v > MAX_SAFE_INTEGER {
        return Err(invalid("epoch exceeds the JavaScript safe integer range"));
    }
    Ok(v as i64)
}

fn epoch_in(v: Option<i64>, what: &str) -> Result<u64> {
    let v = v.ok_or_else(|| invalid(format!("{what} is required")))?;
    u64::try_from(v).map_err(|_| invalid(format!("{what} must be non-negative")))
}

fn rejection_out(r: aster::lease::FenceRejection) -> Result<UpcallFencedMutateResponse> {
    use aster::lease::FenceRejection as R;
    let (kind, carried, current) = match r {
        R::StaleEpoch { carried, current } => ("stale_epoch", carried, current),
        R::NotHolder => ("not_holder", 0, 0),
        R::NotHeld => ("not_held", 0, 0),
        R::FutureEpoch { carried, current } => ("future_epoch", carried, current),
    };
    Ok(UpcallFencedMutateResponse {
        kind: kind.into(),
        output: None,
        carried: Some(epoch_out(carried)?),
        current: Some(epoch_out(current)?),
    })
}

fn fenced_response_in(
    v: &UpcallFencedMutateResponse,
) -> Result<std::result::Result<Vec<u8>, aster::lease::FenceRejection>> {
    use aster::lease::FenceRejection as R;
    Ok(match v.kind.as_str() {
        "applied" => Ok(v.output.as_ref().map(|b| b.to_vec()).unwrap_or_default()),
        "stale_epoch" => Err(R::StaleEpoch {
            carried: epoch_in(v.carried, "carried")?,
            current: epoch_in(v.current, "current")?,
        }),
        "future_epoch" => Err(R::FutureEpoch {
            carried: epoch_in(v.carried, "carried")?,
            current: epoch_in(v.current, "current")?,
        }),
        "not_holder" => Err(R::NotHolder),
        "not_held" => Err(R::NotHeld),
        other => return Err(invalid(format!("unknown fenced mutate result {other:?}"))),
    })
}

fn provision_out(p: &ProvisionRequest) -> Result<UpcallPlacementProvisionRequest> {
    Ok(UpcallPlacementProvisionRequest {
        group_id: p.group_id.to_hex(),
        policy_digest: hex::encode(p.policy_digest),
        membership_log_id: CoordinationLogId::opt(&p.membership_log_id)?,
        node_id: p.node_id.to_hex(),
    })
}

fn provision_in(p: &UpcallPlacementProvisionRequest) -> Result<ProvisionRequest> {
    Ok(ProvisionRequest {
        group_id: CoordinationGroupId::from_array(hex32(&p.group_id, "groupId")?),
        policy_digest: hex32(&p.policy_digest, "policyDigest")?,
        membership_log_id: opt_log_in(&p.membership_log_id)?,
        node_id: CoordinationNodeId::from_array(hex32(&p.node_id, "nodeId")?),
    })
}

const HTTP_VERSIONS: [&str; 5] = ["HTTP/0.9", "HTTP/1.0", "HTTP/1.1", "HTTP/2", "HTTP/3"];

pub(crate) fn headers_out(h: Vec<codec::HttpHeader>) -> Vec<ExposeHeader> {
    h.into_iter()
        .map(|(name, value)| ExposeHeader {
            name,
            value: value.into(),
        })
        .collect()
}

/// Header names are lowercased; an empty name is refused.
pub(crate) fn headers_in(h: &[ExposeHeader], what: &str) -> Result<Vec<codec::HttpHeader>> {
    h.iter()
        .map(|h| {
            if h.name.is_empty() {
                return Err(invalid(format!("{what}: empty header name")));
            }
            Ok((h.name.to_ascii_lowercase(), h.value.to_vec()))
        })
        .collect()
}

fn body_out(b: codec::HttpBodyMode) -> Option<Buffer> {
    match b {
        codec::HttpBodyMode::Inline(v) => Some(v.into()),
        codec::HttpBodyMode::Stream => None,
    }
}

fn body_in(b: &Option<Buffer>) -> codec::HttpBodyMode {
    match b {
        Some(b) => codec::HttpBodyMode::Inline(b.to_vec()),
        None => codec::HttpBodyMode::Stream,
    }
}

fn protocol_in(p: &str) -> Result<u8> {
    match p {
        "http" => Ok(0),
        "tcp" => Ok(1),
        other => Err(invalid(format!(
            "route protocol must be http or tcp, got {other:?}"
        ))),
    }
}

fn port_in(p: u32) -> Result<u16> {
    u16::try_from(p).map_err(|_| invalid("route port must be 0..=65535"))
}

pub(crate) fn route_spec_to_core(s: &ExposeRouteSpec) -> Result<aster::expose::RouteSpec> {
    use aster::expose::RouteProtocol;
    Ok(aster::expose::RouteSpec {
        host: s.host.clone(),
        port: port_in(s.port)?,
        protocol: if protocol_in(&s.protocol)? == 1 {
            RouteProtocol::Tcp
        } else {
            RouteProtocol::Http
        },
        service_id: s.service_id.clone(),
    })
}

pub(crate) fn route_spec_from_core(s: &aster::expose::RouteSpec) -> ExposeRouteSpec {
    ExposeRouteSpec {
        host: s.host.clone(),
        port: s.port.into(),
        protocol: match s.protocol {
            aster::expose::RouteProtocol::Http => "http",
            aster::expose::RouteProtocol::Tcp => "tcp",
        }
        .into(),
        service_id: s.service_id.clone(),
    }
}

fn route_spec_out(s: codec::ExposeRouteSpec) -> ExposeRouteSpec {
    ExposeRouteSpec {
        host: s.host,
        port: s.port.into(),
        protocol: if s.protocol == 1 { "tcp" } else { "http" }.into(),
        service_id: s.service_id,
    }
}

fn route_spec_in(s: &ExposeRouteSpec) -> Result<codec::ExposeRouteSpec> {
    Ok(codec::ExposeRouteSpec {
        host: s.host.clone(),
        port: port_in(s.port)?,
        protocol: protocol_in(&s.protocol)?,
        service_id: s.service_id.clone(),
    })
}

/// Decode one UCv1 message of type `ty` into its JavaScript object.
pub(crate) fn decode_value(ty: &str, b: &[u8]) -> Result<UpcallValue> {
    use UpcallValue as V;
    Ok(match ty {
        "LeaseTransitionRequest" => {
            let (resource, op) = codec::decode_lease_transition_request(b).map_err(codec_err)?;
            V::LeaseTransitionRequest(UpcallLeaseTransitionRequest {
                resource,
                op: LeaseOp::from_rust(&op),
            })
        }
        "TransitionOutcome" => V::TransitionOutcome(
            codec::decode_transition_outcome(b)
                .map_err(codec_err)?
                .into(),
        ),
        "LeaseSnapshotRequest" => V::LeaseSnapshotRequest(UpcallLeaseSnapshotRequest {
            resource: codec::decode_lease_snapshot_request(b).map_err(codec_err)?,
        }),
        "LeaseSnapshot" => {
            V::LeaseSnapshot(codec::decode_lease_snapshot(b).map_err(codec_err)?.into())
        }
        "LeaseAuthorizeRequest" => V::LeaseAuthorizeRequest(UpcallLeaseAuthorizeRequest {
            op: LeaseOp::from_rust(&codec::decode_lease_authorize_request(b).map_err(codec_err)?),
        }),
        "FencedMutateRequest" => {
            let (fence, writer, op) = codec::decode_fenced_mutate_request(b).map_err(codec_err)?;
            V::FencedMutateRequest(UpcallFencedMutateRequest {
                fence: fence.into(),
                writer,
                op: op.into(),
            })
        }
        "FencedMutateResponse" => V::FencedMutateResponse(
            match codec::decode_fenced_mutate_response(b).map_err(codec_err)? {
                Ok(output) => UpcallFencedMutateResponse {
                    kind: "applied".into(),
                    output: Some(output.into()),
                    carried: None,
                    current: None,
                },
                Err(r) => rejection_out(r)?,
            },
        ),
        "Decision" => V::Decision(UpcallDecision {
            allowed: codec::decode_decision(b).map_err(codec_err)?,
        }),
        "TopologyAdmittedRequest" => V::TopologyAdmittedRequest(UpcallTopologyAdmittedRequest {
            node_ids: codec::decode_topology_admitted_request(b).map_err(codec_err)?,
        }),
        "TopologyAdmittedResponse" => V::TopologyAdmittedResponse(UpcallTopologyAdmittedResponse {
            admitted: codec::decode_topology_admitted_response(b).map_err(codec_err)?,
        }),
        "TopologySnapshotRequest" => {
            codec::decode_topology_snapshot_request(b).map_err(codec_err)?;
            V::Empty(UpcallEmpty {})
        }
        "TopologySnapshot" => {
            let parts = codec::decode_topology_snapshot(b).map_err(codec_err)?;
            V::TopologyView(UpcallTopologyView {
                clusters: parts.clusters.into_iter().map(ClusterView::from).collect(),
                far_pairs: parts
                    .far_pairs
                    .into_iter()
                    .map(|(a, b)| vec![a, b])
                    .collect(),
                separation_coverage: u32::try_from(parts.separation_coverage)
                    .map_err(|_| invalid("separationCoverage out of range"))?,
            })
        }
        "PlacementPolicyRequest" => {
            codec::decode_placement_policy_request(b).map_err(codec_err)?;
            V::Empty(UpcallEmpty {})
        }
        "PlacementPolicy" => V::PlacementPolicy(policy_out(
            &codec::decode_placement_policy(b).map_err(codec_err)?,
        )?),
        "PlacementProvisionRequest" => V::PlacementProvisionRequest(provision_out(
            &codec::decode_provision_request(b).map_err(codec_err)?,
        )?),
        "MemberSeed" => V::MemberSeed(CoordinationMemberSeed::from_wire(
            &codec::decode_member_seed(b).map_err(codec_err)?,
        )),
        "ExposeAuthorizeRequest" => {
            let (service_id, peer_id, metadata) =
                codec::decode_expose_authorize_request(b).map_err(codec_err)?;
            V::ExposeAuthorizeRequest(UpcallExposeAuthorizeRequest {
                service_id,
                peer_id,
                metadata: metadata.into(),
            })
        }
        "ExposeHttpRequest" => {
            let r = codec::decode_expose_http_request(b).map_err(codec_err)?;
            if r.stream_id > MAX_SAFE_INTEGER {
                return Err(invalid("stream_id past 2^53"));
            }
            V::ExposeHttpRequest(UpcallExposeHttpRequest {
                service_id: r.service_id,
                peer_id: r.peer_id,
                stream_id: r.stream_id as f64,
                version: HTTP_VERSIONS[r.version as usize].into(),
                method: r.method,
                uri: r.uri,
                headers: headers_out(r.headers),
                body: body_out(r.body),
            })
        }
        "ExposeHttpResponse" => {
            let r = codec::decode_expose_http_response(b).map_err(codec_err)?;
            V::ExposeHttpResponse(UpcallExposeHttpResponse {
                status: r.status.into(),
                headers: headers_out(r.headers),
                body: body_out(r.body),
            })
        }
        "ExposeHttpTrailers" => V::ExposeHttpTrailers(headers_out(
            codec::decode_expose_http_trailers(b).map_err(codec_err)?,
        )),
        "ExposeRoutePolicyRequest" => {
            let (peer_id, metadata, specs) =
                codec::decode_expose_route_policy_request(b).map_err(codec_err)?;
            V::ExposeRoutePolicyRequest(UpcallExposeRoutePolicyRequest {
                peer_id,
                metadata: metadata.into(),
                specs: specs.into_iter().map(route_spec_out).collect(),
            })
        }
        "ExposeRouteGrant" => V::ExposeRouteGrant(UpcallExposeRouteGrant {
            granted: codec::decode_expose_route_grant(b).map_err(codec_err)?,
        }),
        "AdmissionConnectRequest" => {
            let (peer, alpn) = codec::decode_admission_connect_request(b).map_err(codec_err)?;
            V::AdmissionConnectRequest(UpcallAdmissionConnectRequest {
                peer,
                alpn: alpn.into(),
            })
        }
        "AdmissionHandshakeRequest" => {
            let (peer, alpn, is_alive) =
                codec::decode_admission_handshake_request(b).map_err(codec_err)?;
            V::AdmissionHandshakeRequest(UpcallAdmissionHandshakeRequest {
                peer,
                alpn: alpn.into(),
                is_alive,
            })
        }
        "AdmissionHandshakeResponse" => V::AdmissionHandshakeResponse(
            match codec::decode_admission_handshake_response(b).map_err(codec_err)? {
                codec::HandshakeDecision::Accept => UpcallAdmissionHandshakeResponse {
                    accept: true,
                    code: Some(0),
                    reason: Some(Vec::new().into()),
                },
                codec::HandshakeDecision::Reject { code, reason } => {
                    UpcallAdmissionHandshakeResponse {
                        accept: false,
                        code: Some(code),
                        reason: Some(reason.into()),
                    }
                }
            },
        ),
        other => return Err(invalid(format!("unknown upcall message type {other:?}"))),
    })
}

fn from_js<T: FromNapiValue>(env: &Env, value: &Unknown) -> Result<T> {
    unsafe { T::from_napi_value(env.raw(), value.raw()) }
}

/// Encode a JavaScript object as the UCv1 message of type `ty`.
pub(crate) fn encode_value(env: &Env, ty: &str, v: &Unknown) -> Result<Vec<u8>> {
    Ok(match ty {
        "LeaseTransitionRequest" => {
            let r: UpcallLeaseTransitionRequest = from_js(env, v)?;
            codec::encode_lease_transition_request(&r.resource, &r.op.to_rust()?)
        }
        "TransitionOutcome" => {
            let o: TransitionOutcome = from_js(env, v)?;
            codec::encode_transition_outcome(&o.to_rust()?)
        }
        "LeaseSnapshotRequest" => {
            let r: UpcallLeaseSnapshotRequest = from_js(env, v)?;
            codec::encode_lease_snapshot_request(&r.resource)
        }
        "LeaseSnapshot" => {
            let s: LeaseSnapshot = from_js(env, v)?;
            codec::encode_lease_snapshot(&s.to_rust()?)
        }
        "LeaseAuthorizeRequest" => {
            let r: UpcallLeaseAuthorizeRequest = from_js(env, v)?;
            codec::encode_lease_authorize_request(&r.op.to_rust()?)
        }
        "FencedMutateRequest" => {
            let r: UpcallFencedMutateRequest = from_js(env, v)?;
            codec::encode_fenced_mutate_request(&r.fence.to_rust()?, &r.writer, &r.op)
        }
        "FencedMutateResponse" => {
            let r: UpcallFencedMutateResponse = from_js(env, v)?;
            codec::encode_fenced_mutate_response(&fenced_response_in(&r)?)
        }
        "Decision" => {
            let d: UpcallDecision = from_js(env, v)?;
            codec::encode_decision(d.allowed)
        }
        "TopologyAdmittedRequest" => {
            let r: UpcallTopologyAdmittedRequest = from_js(env, v)?;
            codec::encode_topology_admitted_request(&r.node_ids)
        }
        "TopologyAdmittedResponse" => {
            let r: UpcallTopologyAdmittedResponse = from_js(env, v)?;
            codec::encode_topology_admitted_response(&r.admitted)
        }
        "TopologySnapshotRequest" => codec::encode_topology_snapshot_request(),
        "TopologySnapshot" => {
            let t: UpcallTopologyView = from_js(env, v)?;
            let mut far_pairs = Vec::with_capacity(t.far_pairs.len());
            for p in t.far_pairs {
                let [a, b]: [String; 2] = p
                    .try_into()
                    .map_err(|_| invalid("every far pair must be [a, b]"))?;
                far_pairs.push((a, b));
            }
            codec::encode_topology_snapshot(&codec::TopologyViewParts {
                clusters: t
                    .clusters
                    .into_iter()
                    .map(|c| aster::topology::ClusterView {
                        members: c.members,
                        bridge: c.bridge,
                        witnesses: c.witnesses,
                    })
                    .collect(),
                far_pairs,
                separation_coverage: t.separation_coverage.into(),
            })
        }
        "PlacementPolicyRequest" => codec::encode_placement_policy_request(),
        "PlacementPolicy" => {
            let p: CoordinationPlacementPolicy = from_js(env, v)?;
            codec::encode_placement_policy(&policy_in(&p)?)
        }
        "PlacementProvisionRequest" => {
            let r: UpcallPlacementProvisionRequest = from_js(env, v)?;
            codec::encode_provision_request(&provision_in(&r)?)
        }
        "MemberSeed" => {
            let s: CoordinationMemberSeed = from_js(env, v)?;
            codec::encode_member_seed(&s.to_wire()?)
        }
        "ExposeAuthorizeRequest" => {
            let r: UpcallExposeAuthorizeRequest = from_js(env, v)?;
            codec::encode_expose_authorize_request(&r.service_id, &r.peer_id, &r.metadata)
        }
        "ExposeHttpRequest" => {
            let r: UpcallExposeHttpRequest = from_js(env, v)?;
            if r.method.is_empty() {
                return Err(invalid("HTTP method must be non-empty"));
            }
            let version = HTTP_VERSIONS
                .iter()
                .position(|x| *x == r.version)
                .ok_or_else(|| invalid(format!("HTTP version must be one of {HTTP_VERSIONS:?}")))?;
            if !r.stream_id.is_finite()
                || r.stream_id < 0.0
                || r.stream_id.fract() != 0.0
                || r.stream_id > MAX_SAFE_INTEGER as f64
            {
                return Err(invalid("streamId must be a non-negative safe integer"));
            }
            codec::encode_expose_http_request(&codec::ExposeHttpRequest {
                service_id: r.service_id,
                peer_id: r.peer_id,
                stream_id: r.stream_id as u64,
                version: version as u8,
                method: r.method,
                uri: r.uri,
                headers: headers_in(&r.headers, "headers")?,
                body: body_in(&r.body),
            })
        }
        "ExposeHttpResponse" => {
            let r: UpcallExposeHttpResponse = from_js(env, v)?;
            if !(100..=999).contains(&r.status) {
                return Err(invalid(format!(
                    "HTTP status must be 100..=999, got {}",
                    r.status
                )));
            }
            codec::encode_expose_http_response(&codec::ExposeHttpResponse {
                status: r.status as u16,
                headers: headers_in(&r.headers, "headers")?,
                body: body_in(&r.body),
            })
        }
        "ExposeHttpTrailers" => {
            let t: Vec<ExposeHeader> = from_js(env, v)?;
            codec::encode_expose_http_trailers(&headers_in(&t, "trailers")?)
        }
        "ExposeRoutePolicyRequest" => {
            let r: UpcallExposeRoutePolicyRequest = from_js(env, v)?;
            let specs = r
                .specs
                .iter()
                .map(route_spec_in)
                .collect::<Result<Vec<_>>>()?;
            codec::encode_expose_route_policy_request(&r.peer_id, &r.metadata, &specs)
        }
        "ExposeRouteGrant" => {
            let mut g: UpcallExposeRouteGrant = from_js(env, v)?;
            g.granted.sort_unstable();
            g.granted.dedup();
            codec::encode_expose_route_grant(&g.granted)
        }
        "AdmissionConnectRequest" => {
            let r: UpcallAdmissionConnectRequest = from_js(env, v)?;
            codec::encode_admission_connect_request(&r.peer, &r.alpn)
        }
        "AdmissionHandshakeRequest" => {
            let r: UpcallAdmissionHandshakeRequest = from_js(env, v)?;
            codec::encode_admission_handshake_request(&r.peer, &r.alpn, r.is_alive)
        }
        "AdmissionHandshakeResponse" => {
            let r: UpcallAdmissionHandshakeResponse = from_js(env, v)?;
            codec::encode_admission_handshake_response(&if r.accept {
                codec::HandshakeDecision::Accept
            } else {
                codec::HandshakeDecision::Reject {
                    code: r
                        .code
                        .unwrap_or(aster::binding_support::upcall::admission::UNAVAILABLE_CODE),
                    reason: r.reason.map(|b| b.to_vec()).unwrap_or_default(),
                }
            })
        }
        other => return Err(invalid(format!("unknown upcall message type {other:?}"))),
    })
}

/// Decode the UCv1 message `bytes` of type `typeName` (a type name of
/// `conformance/vectors/upcalls.json`, e.g. `"LeaseTransitionRequest"`).
/// Throws on any non-canonical input.
#[napi(
    ts_return_type = "UpcallLeaseTransitionRequest | TransitionOutcome | UpcallLeaseSnapshotRequest | LeaseSnapshot | UpcallLeaseAuthorizeRequest | UpcallFencedMutateRequest | UpcallFencedMutateResponse | UpcallDecision | UpcallTopologyAdmittedRequest | UpcallTopologyAdmittedResponse | UpcallEmpty | UpcallTopologyView | CoordinationPlacementPolicy | UpcallPlacementProvisionRequest | CoordinationMemberSeed | UpcallExposeAuthorizeRequest | UpcallExposeHttpRequest | UpcallExposeHttpResponse | Array<ExposeHeader> | UpcallExposeRoutePolicyRequest | UpcallExposeRouteGrant | UpcallAdmissionConnectRequest | UpcallAdmissionHandshakeRequest | UpcallAdmissionHandshakeResponse"
)]
pub fn upcall_codec_decode(type_name: String, bytes: Buffer) -> Result<UpcallValue> {
    decode_value(&type_name, &bytes)
}

/// Encode `value` as the UCv1 message of type `typeName` (the inverse of
/// `upcallCodecDecode`).
#[napi]
pub fn upcall_codec_encode(
    env: Env,
    type_name: String,
    #[napi(
        ts_arg_type = "UpcallLeaseTransitionRequest | TransitionOutcome | UpcallLeaseSnapshotRequest | LeaseSnapshot | UpcallLeaseAuthorizeRequest | UpcallFencedMutateRequest | UpcallFencedMutateResponse | UpcallDecision | UpcallTopologyAdmittedRequest | UpcallTopologyAdmittedResponse | UpcallEmpty | UpcallTopologyView | CoordinationPlacementPolicy | UpcallPlacementProvisionRequest | CoordinationMemberSeed | UpcallExposeAuthorizeRequest | UpcallExposeHttpRequest | UpcallExposeHttpResponse | Array<ExposeHeader> | UpcallExposeRoutePolicyRequest | UpcallExposeRouteGrant | UpcallAdmissionConnectRequest | UpcallAdmissionHandshakeRequest | UpcallAdmissionHandshakeResponse"
    )]
    value: Unknown,
) -> Result<Buffer> {
    Ok(encode_value(&env, &type_name, &value)?.into())
}

// ── events to JavaScript ─────────────────────────────────────────────────────

/// One upcall for JavaScript. `kind` 0 is a cancellation of
/// `invocationId` (`cancelReason`: 1 timeout, 2 abandoned, 3 shutdown).
#[napi(object, object_from_js = false)]
pub struct UpcallEventJs {
    pub invocation_id: f64,
    pub kind: u32,
    /// When Rust stops waiting (Unix ms; 0 on a cancellation).
    pub deadline_unix_ms: f64,
    pub cancel_reason: Option<u32>,
    #[napi(
        ts_type = "UpcallLeaseTransitionRequest | TransitionOutcome | UpcallLeaseSnapshotRequest | LeaseSnapshot | UpcallLeaseAuthorizeRequest | UpcallFencedMutateRequest | UpcallFencedMutateResponse | UpcallDecision | UpcallTopologyAdmittedRequest | UpcallTopologyAdmittedResponse | UpcallEmpty | UpcallTopologyView | CoordinationPlacementPolicy | UpcallPlacementProvisionRequest | CoordinationMemberSeed | UpcallExposeAuthorizeRequest | UpcallExposeHttpRequest | UpcallExposeHttpResponse | Array<ExposeHeader> | UpcallExposeRoutePolicyRequest | UpcallExposeRouteGrant | UpcallAdmissionConnectRequest | UpcallAdmissionHandshakeRequest | UpcallAdmissionHandshakeResponse"
    )]
    pub request: Option<UpcallValue>,
    /// Set when the request could not be decoded (answer it with an error).
    pub error: Option<String>,
}

fn event_out(ev: UpcallEvent) -> UpcallEventJs {
    let mut out = UpcallEventJs {
        invocation_id: ev.invocation_id as f64,
        kind: ev.kind.code().into(),
        deadline_unix_ms: ev.deadline_unix_ms as f64,
        cancel_reason: None,
        request: None,
        error: None,
    };
    if ev.kind == UpcallKind::Cancel {
        out.cancel_reason = Some(ev.payload.first().copied().unwrap_or(0).into());
        return out;
    }
    match request_type(ev.kind) {
        Some(ty) => match decode_value(ty, &ev.payload) {
            Ok(v) => out.request = Some(v),
            Err(e) => out.error = Some(e.reason.clone()),
        },
        None => out.error = Some(format!("no request type for upcall kind {}", ev.kind)),
    }
    out
}

/// A drained batch, converted to `UpcallEvent[]` on the JavaScript thread.
pub struct UpcallBatch(Vec<UpcallEvent>);

impl TypeName for UpcallBatch {
    fn type_name() -> &'static str {
        "UpcallEventJs[]"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for UpcallBatch {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        let js: Vec<UpcallEventJs> = val.0.into_iter().map(event_out).collect();
        unsafe { Vec::<UpcallEventJs>::to_napi_value(env, js) }
    }
}

// ── the TSFN sink ────────────────────────────────────────────────────────────

/// Weak (does not keep the event loop alive), unhandled-errors-to-JS off.
type UpcallTsfn = ThreadsafeFunction<(), Unknown<'static>, UpcallBatch, Status, false, true>;

#[derive(Default)]
struct SinkStats {
    /// Upcall events accepted (cancellations included).
    delivered: AtomicU64,
    /// Upcalls refused: queue full, closed, or an id past 2^53.
    refused: AtomicU64,
    /// Drain calls queued on the TSFN (rescues included).
    calls: AtomicU64,
    /// Drain callbacks run on the JavaScript thread.
    callbacks: AtomicU64,
    /// Failed TSFN calls.
    failures: AtomicU64,
    /// Drain calls the watchdog re-queued.
    rescues: AtomicU64,
}

struct TsfnUpcallSink {
    queue: Arc<UpcallQueue>,
    tsfn: Mutex<Option<Arc<UpcallTsfn>>>,
    /// A drain call is queued and has not started draining yet.
    scheduled: Arc<AtomicBool>,
    stats: Arc<SinkStats>,
    /// Signalled once per scheduled drain call; wakes the watchdog.
    wake: Arc<tokio::sync::Notify>,
}

impl TsfnUpcallSink {
    fn tsfn(&self) -> Option<Arc<UpcallTsfn>> {
        self.tsfn.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Queue one drain call unless one is outstanding.
    fn schedule(&self) {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(tsfn) = self.tsfn() else {
            self.scheduled.store(false, Ordering::Release);
            return;
        };
        self.stats.calls.fetch_add(1, Ordering::Relaxed);
        let status = tsfn.call((), ThreadsafeFunctionCallMode::NonBlocking);
        if status == Status::Ok {
            self.wake.notify_one();
        } else {
            // The events stay queued (Rust times them out); the next
            // delivery schedules again.
            self.stats.failures.fetch_add(1, Ordering::Relaxed);
            self.scheduled.store(false, Ordering::Release);
        }
    }
}

impl UpcallSink for TsfnUpcallSink {
    fn deliver(&self, event: UpcallEvent) -> bool {
        if event.kind != UpcallKind::Cancel && event.invocation_id > MAX_SAFE_INTEGER {
            self.stats.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if !self.queue.deliver(event) {
            self.stats.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.stats.delivered.fetch_add(1, Ordering::Relaxed);
        if !self.queue.is_empty() {
            self.schedule();
        }
        true
    }
}

/// The Bun TSFN stall watchdog (see `rpc.rs::tsfn_watchdog`): re-queue a
/// drain call that has been outstanding for [`TSFN_WATCHDOG`] with no
/// callback run. Parks on a `Notify` while idle.
async fn tsfn_watchdog(sink: Weak<TsfnUpcallSink>, wake: Arc<tokio::sync::Notify>) {
    loop {
        wake.notified().await;
        loop {
            let Some(s) = sink.upgrade() else { return };
            let before = s.stats.callbacks.load(Ordering::Acquire);
            drop(s);
            tokio::time::sleep(TSFN_WATCHDOG).await;
            let Some(s) = sink.upgrade() else { return };
            let Some(tsfn) = s.tsfn() else { return };
            if !s.scheduled.load(Ordering::Acquire) {
                break;
            }
            if s.stats.callbacks.load(Ordering::Acquire) == before {
                s.stats.calls.fetch_add(1, Ordering::Relaxed);
                s.stats.rescues.fetch_add(1, Ordering::Relaxed);
                let _ = tsfn.call((), ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

// ── the JavaScript-facing table ──────────────────────────────────────────────

/// Options for `new Upcalls(onEvents, options)`.
#[napi(object)]
pub struct UpcallsOptions {
    /// Most upcalls queued for JavaScript and not yet drained; further
    /// upcalls are refused (`Overloaded`: decisions fall back to their
    /// failure mode, other kinds fail). Default 1024.
    pub capacity: Option<u32>,
}

/// Delivery counters.
#[napi(object)]
pub struct UpcallStats {
    pub delivered: f64,
    pub refused: f64,
    pub tsfn_calls: f64,
    pub tsfn_callbacks: f64,
    pub tsfn_failures: f64,
    pub watchdog_rescues: f64,
    /// Events queued and not yet drained.
    pub queued: u32,
    /// Upcalls waiting for an answer.
    pub pending: u32,
}

/// One upcall table: Rust's adapters issue events into it, JavaScript
/// answers with `respond`. Close it when done — the callback is held until
/// then (it does not keep the process alive).
#[napi]
pub struct Upcalls {
    inner: RsUpcalls,
    sink: Arc<TsfnUpcallSink>,
    /// The body handles of expose HTTP exchanges (R9) served on this table.
    pub(crate) bodies: aster::binding_support::upcall::expose::HttpBodies,
}

impl Upcalls {
    /// The Rust table (clone-cheap), for the adapters.
    pub(crate) fn table(&self) -> RsUpcalls {
        self.inner.clone()
    }
}

#[napi]
impl Upcalls {
    /// A table delivering to `onEvents` (called on the JavaScript thread
    /// with each drained batch).
    #[napi(constructor)]
    pub fn new(
        #[napi(ts_arg_type = "(events: UpcallEventJs[]) => void")] on_events: Function<
            '_,
            UpcallBatch,
            Unknown<'static>,
        >,
        options: Option<UpcallsOptions>,
    ) -> Result<Self> {
        let capacity = options
            .and_then(|o| o.capacity)
            .unwrap_or(DEFAULT_CAPACITY)
            .max(1) as usize;
        let queue = UpcallQueue::new(capacity);
        let scheduled = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(SinkStats::default());
        let (q, s, st) = (queue.clone(), scheduled.clone(), stats.clone());
        let tsfn: UpcallTsfn = on_events
            .build_threadsafe_function::<()>()
            .callee_handled::<false>()
            .weak::<true>()
            .build_callback(move |_ctx| {
                st.callbacks.fetch_add(1, Ordering::Relaxed);
                // Clear the flag before draining: an event pushed after this
                // point schedules a new call instead of being stranded.
                s.store(false, Ordering::Release);
                let mut batch = Vec::new();
                q.drain(usize::MAX, |ev| batch.push(ev));
                Ok(UpcallBatch(batch))
            })?;
        let sink = Arc::new(TsfnUpcallSink {
            queue,
            tsfn: Mutex::new(Some(Arc::new(tsfn))),
            scheduled,
            stats,
            wake: Arc::new(tokio::sync::Notify::new()),
        });
        let (weak, wake) = (Arc::downgrade(&sink), sink.wake.clone());
        // napi v3 does not enter the Tokio runtime around synchronous exports.
        within_runtime_if_available(move || {
            tokio::spawn(tsfn_watchdog(weak, wake));
        });
        Ok(Self {
            inner: RsUpcalls::new(sink.clone()),
            sink,
            bodies: aster::binding_support::upcall::expose::HttpBodies::new(),
        })
    }

    /// Set the policy of kind `kind` (an `upcallKinds()` code). Applies to
    /// upcalls issued afterwards.
    #[napi]
    pub fn set_policy(&self, kind: u32, policy: UpcallPolicyJs) -> Result<()> {
        self.inner
            .set_policy(kind_in(kind)?, policy_from_js(&policy)?);
        Ok(())
    }

    /// Set the policy for kinds without their own (default 5 000 ms,
    /// fail-closed).
    #[napi]
    pub fn set_default_policy(&self, policy: UpcallPolicyJs) -> Result<()> {
        self.inner.set_default_policy(policy_from_js(&policy)?);
        Ok(())
    }

    /// The policy kind `kind` runs under.
    #[napi]
    pub fn policy(&self, kind: u32) -> Result<UpcallPolicyJs> {
        Ok(policy_to_js(self.inner.policy(kind_in(kind)?)))
    }

    /// Answer `invocationId`: `status` 0 OK (`payload` = the kind's UCv1
    /// response), 1 error (`payload` = UTF-8 message), 2 unimplemented.
    /// Returns `false` when nothing waits for the id any more (answered,
    /// cancelled, timed out) — ignore it. Never blocks.
    #[napi]
    pub fn respond(&self, invocation_id: f64, status: u32, payload: Option<Buffer>) -> bool {
        if !invocation_id.is_finite()
            || invocation_id < 1.0
            || invocation_id.fract() != 0.0
            || invocation_id > MAX_SAFE_INTEGER as f64
        {
            return false;
        }
        self.inner
            .respond(
                invocation_id as u64,
                UpcallStatus::from_code(status),
                payload.map(|b| b.to_vec()).unwrap_or_default(),
            )
            .is_ok()
    }

    /// Fail every pending upcall (`Closed`; decisions fall back) and refuse
    /// later ones, then release the callback. Idempotent.
    #[napi]
    pub fn close(&self) {
        self.inner.close();
        self.sink.queue.close();
        self.sink
            .tsfn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    #[napi(getter)]
    pub fn closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Upcalls waiting for an answer.
    #[napi(getter)]
    pub fn pending_count(&self) -> u32 {
        self.inner.pending_count() as u32
    }

    /// Delivery counters.
    #[napi]
    pub fn stats(&self) -> UpcallStats {
        let s = &self.sink.stats;
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64;
        UpcallStats {
            delivered: n(&s.delivered),
            refused: n(&s.refused),
            tsfn_calls: n(&s.calls),
            tsfn_callbacks: n(&s.callbacks),
            tsfn_failures: n(&s.failures),
            watchdog_rescues: n(&s.rescues),
            queued: self.sink.queue.len() as u32,
            pending: self.inner.pending_count() as u32,
        }
    }
}
