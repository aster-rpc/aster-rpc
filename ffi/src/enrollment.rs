//! ABI 5.1 (additive): Enrollment for enrolled nodes and delegated issuers.
//!
//! Thin wrappers over `aster::enrollment` (design
//! `docs/_internal/working_ideas/aster-enrollment.md`). Bindings get the two
//! roles a non-Rust process plays:
//!
//! - **Enrolled node**: `aster_enrollment_install` puts Enrollment in charge
//!   of a node's admission from a JSON configuration (trusted root, audience,
//!   fleet namespace, durable state path, the node's credential chain, a
//!   declarative admission policy, root rotation pin). Then connect to
//!   replicas, synchronize policy, check peers, read a peer's verified claims,
//!   replace the credential, follow revocations, and renew from an issuer.
//! - **Delegated issuer**: `aster_enrollment_issuer_new` holds a delegation
//!   chain from the root and signs credentials for subjects the root has not
//!   denied (`aster_enrollment_issue`); `aster_server_set_enrollment` serves
//!   `aster.enrollment.IssuerV1` renewals next to the node's enrollment ALPN.
//!
//! Root operations (namespace create/bind, promote, revoke, reauthorize,
//! rotate, delegate) are deliberately absent: they stay in the Rust CLI.
//!
//! # Conventions
//!
//! Configuration and results are UTF-8 JSON (`aster_bytes_t`, borrowed for the
//! call). Synchronous outputs are owned byte spans freed with
//! `aster_string_release`. Asynchronous operations complete with
//! `ASTER_EVENT_ENROLLMENT_RESULT` (JSON payload, `handle` = the enrollment or
//! renewal) or `ASTER_EVENT_ERROR`. Admission errors carry their kind in the
//! event's `flags` (see `aster_enrollment_error_t`). Keys and node ids are
//! 64 lowercase-or-uppercase hex characters; outputs are lowercase.
//!
//! # Node prerequisites
//!
//! The node must come from `aster_node_start` with `enable_hooks = 1`,
//! `hook_failure_mode = ASTER_HOOK_FAILURE_FAIL_CLOSED`, `aster.enrollment/1`
//! among its ALPNs, and no gate. After install the node's admission drainers
//! forward both hook directions to Enrollment; `aster_node_set_gate` returns
//! `UNSUPPORTED` from then on. Inbound `aster.enrollment/1` connections are
//! served by an RPC server built with `aster_server_set_enrollment`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use aster::delegation::Chain;
use aster::enrollment::{
    AdmissionClaims, AdmissionError, AdmissionEvidenceSource, DenialReason, Enrollment,
    EnrollmentOptions, EnrollmentPolicyStore, EnrollmentReadiness, EnrollmentRevoked, Issuer,
    IssuerErrorV1, JwtEnrollmentVerifier, PolicyExpressionV1, PolicyPredicate, Registration,
    RenewalConfig, RenewalOutcome, RotationPin, SourceCidr, SynchronizationSource, TypedAttribute,
    VerifiedAdmissionEvidence, DEFAULT_ISSUED_LIFETIME,
};
use aster_transport_core::CoreHookReceiver;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

use crate::data::{bridge_for, in_slice, in_string, invalid, spawn_op_cancellable, Done, OpError};
use crate::*;

/// An installed Enrollment (`aster_enrollment_install`), scoped to a runtime.
pub type aster_enrollment_t = u64;
/// A delegated issuer (`aster_enrollment_issuer_new`), scoped to a runtime.
pub type aster_enrollment_issuer_t = u64;
/// A running credential renewal (`aster_enrollment_start_renewal`).
pub type aster_enrollment_renewal_t = u64;

/// `flags` of an `ASTER_EVENT_ERROR` from an Enrollment admission check
/// (`aster_enrollment_check_peer`): which `AdmissionError` it was.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_enrollment_error_t {
    /// Not an admission error (configuration, transport, timeout).
    ASTER_ENROLLMENT_ERROR_OTHER = 0,
    /// No admission session: the peer must enroll (again).
    ASTER_ENROLLMENT_ERROR_NOT_ADMITTED = 1,
    /// A definite denial (root policy, signed validity or the admission
    /// policy). Only this kind revokes a session.
    ASTER_ENROLLMENT_ERROR_DENIED = 2,
    /// The peer's (or its issuer's) clock is outside the activation leeway.
    ASTER_ENROLLMENT_ERROR_CLOCK_SKEW = 3,
    /// Policy could not be evaluated now; the session is kept. Retry.
    ASTER_ENROLLMENT_ERROR_UNAVAILABLE = 4,
}

/// Configuration and policy JSON is bounded before parsing.
const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_REPLICAS: usize = 16;

/// The registry entry behind an `aster_enrollment_t`.
pub(crate) struct EnrollmentState {
    pub(crate) enrollment: Enrollment,
    node: aster::Node,
    /// Subscribed at install, so no revocation after install is missed.
    revocations: tokio::sync::Mutex<broadcast::Receiver<EnrollmentRevoked>>,
    /// Set by `aster_enrollment_free`; ends pending revocation waits.
    released: AtomicBool,
    released_notify: tokio::sync::Notify,
}

impl EnrollmentState {
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.released_notify.notify_waiters();
    }

    async fn wait_released(&self) {
        let notified = self.released_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.released.load(Ordering::Acquire) {
            notified.await;
        }
    }
}

/// The registry entry behind an `aster_enrollment_issuer_t`.
pub(crate) struct IssuerState {
    pub(crate) issuer: Issuer,
    enrollment: Enrollment,
}

/// The registry entry behind an `aster_enrollment_renewal_t`. Dropping it
/// aborts the watcher, which drops the renewal handle and stops renewal.
pub(crate) struct RenewalState {
    outcome: watch::Receiver<Option<RenewalOutcome>>,
    task: tokio::task::AbortHandle,
}

impl Drop for RenewalState {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ── JSON inputs ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallConfig {
    /// The trusted root public key.
    root: String,
    /// The fleet audience the verifier and stores pin.
    audience: String,
    /// The fleet policy namespace pinned to `root`.
    fleet_namespace: String,
    /// Durable policy state file (denials and revision watermarks).
    state_path: String,
    /// This node's credential chain; absent for a pure verifier.
    #[serde(default)]
    credential: Option<String>,
    /// The admission policy (required, explicit).
    policy: PolicyJson,
    #[serde(default)]
    rotation: Option<RotationJson>,
    #[serde(default)]
    applied_rotations: Vec<String>,
    #[serde(default)]
    require_synchronized_clock: bool,
    #[serde(default)]
    activation_leeway_secs: Option<u64>,
    #[serde(default)]
    limits: LimitsJson,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RotationJson {
    next_commitment: String,
    #[serde(default)]
    seq: u64,
}

/// Overrides of `EnrollmentOptions`; omitted values keep the defaults.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LimitsJson {
    handshake_timeout_ms: Option<u64>,
    step_timeout_ms: Option<u64>,
    policy_timeout_ms: Option<u64>,
    reconciliation_interval_ms: Option<u64>,
    max_admitted_peers: Option<usize>,
}

/// `PolicyExpressionV1` as JSON: an externally tagged tree, e.g.
/// `{"all_of":[{"scope":"session"},"tolerate_stale_policy"]}`.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PolicyJson {
    AllOf(Vec<PolicyJson>),
    AnyOf(Vec<PolicyJson>),
    NoneOf(Vec<PolicyJson>),
    SourceCidr(String),
    Audience(String),
    Scope(String),
    Tag(String),
    AttributeEquals {
        key: String,
        value: TypedAttribute,
    },
    TolerateStalePolicy,
    PolicySynchronizedWithin {
        max_age_secs: u64,
        #[serde(default)]
        require_root: bool,
    },
    RequireRegistration,
    IssuedBy(String),
    MaxCredentialAgeSecs(u64),
}

impl PolicyJson {
    fn lower(self) -> Result<PolicyExpressionV1, String> {
        let all = |children: Vec<PolicyJson>| -> Result<Vec<PolicyExpressionV1>, String> {
            children.into_iter().map(PolicyJson::lower).collect()
        };
        let predicate = PolicyExpressionV1::Predicate;
        Ok(match self {
            Self::AllOf(children) => PolicyExpressionV1::AllOf(all(children)?),
            Self::AnyOf(children) => PolicyExpressionV1::AnyOf(all(children)?),
            Self::NoneOf(children) => PolicyExpressionV1::NoneOf(all(children)?),
            Self::SourceCidr(cidr) => predicate(PolicyPredicate::SourceCidr(
                cidr.parse::<SourceCidr>()
                    .map_err(|_| format!("policy: invalid source_cidr {cidr:?}"))?,
            )),
            Self::Audience(value) => predicate(PolicyPredicate::Audience(value)),
            Self::Scope(value) => predicate(PolicyPredicate::Scope(value)),
            Self::Tag(value) => predicate(PolicyPredicate::Tag(value)),
            Self::AttributeEquals { key, value } => {
                predicate(PolicyPredicate::AttributeEquals { key, value })
            }
            Self::TolerateStalePolicy => predicate(PolicyPredicate::TolerateStalePolicy),
            Self::PolicySynchronizedWithin {
                max_age_secs,
                require_root,
            } => predicate(PolicyPredicate::PolicySynchronizedWithin {
                max_age: Duration::from_secs(max_age_secs),
                require_root,
            }),
            Self::RequireRegistration => predicate(PolicyPredicate::RequireRegistration),
            Self::IssuedBy(key) => predicate(PolicyPredicate::IssuedBy(hex32(&key, "issued_by")?)),
            Self::MaxCredentialAgeSecs(seconds) => predicate(PolicyPredicate::MaxCredentialAge(
                Duration::from_secs(seconds),
            )),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddrJson {
    node_id: String,
    #[serde(default)]
    relay_url: Option<String>,
    #[serde(default)]
    direct_addresses: Vec<String>,
}

impl AddrJson {
    fn lower(self) -> Result<aster::NodeAddr, String> {
        let node_id = hex::encode(hex32(&self.node_id, "node_id")?);
        Ok(aster::NodeAddr {
            node_id: aster::NodeId::from_hex(node_id),
            relay_url: self.relay_url.filter(|url| !url.is_empty()),
            direct_addresses: self.direct_addresses,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenewalJson {
    issuer: AddrJson,
    #[serde(default)]
    retry_ms: Option<u64>,
    #[serde(default)]
    max_retry_ms: Option<u64>,
    #[serde(default)]
    call_timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueJson {
    subject: String,
    #[serde(default)]
    claims: AdmissionClaims,
    #[serde(default)]
    registration: Registration,
}

fn hex32(value: &str, what: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err(format!("{what}: expected 64 hex characters"));
    }
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| format!("{what}: expected 64 hex characters"))
}

fn parse_json<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T, i32> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(invalid(format!(
            "{what}: larger than {MAX_CONFIG_BYTES} bytes"
        )));
    }
    serde_json::from_slice(bytes).map_err(|error| invalid(format!("{what}: {error}")))
}

unsafe fn in_key(b: &aster_bytes_t, what: &str) -> Result<[u8; 32], i32> {
    let text = in_string(b, what)?;
    hex32(&text, what).map_err(invalid)
}

// ── JSON outputs ────────────────────────────────────────────────────────────

fn denial_json(reason: &DenialReason) -> Value {
    let code = match reason {
        DenialReason::NodeRevoked => "node_revoked",
        DenialReason::RegistrationSuperseded => "registration_superseded",
        DenialReason::RegistrationRejected => "registration_rejected",
        DenialReason::PolicyStateUnavailable => "policy_state_unavailable",
        DenialReason::CredentialRevoked => "credential_revoked",
        DenialReason::IssuerRevoked => "issuer_revoked",
        DenialReason::InvalidValidity => "invalid_validity",
        DenialReason::RootRotated => "root_rotated",
        DenialReason::ClockSkew { .. } => "clock_skew",
        DenialReason::PolicyDidNotMatch => "policy_did_not_match",
        DenialReason::MissingEvidence => "missing_evidence",
        DenialReason::PolicyError(_) => "policy_error",
        DenialReason::PolicyTimeout => "policy_timeout",
        DenialReason::Consumer(_) => "consumer",
    };
    let detail = match reason {
        DenialReason::ClockSkew { offset_seconds } => json!(offset_seconds),
        DenialReason::PolicyError(error) => json!(format!("{error:?}")),
        DenialReason::Consumer(detail) => json!(detail),
        _ => Value::Null,
    };
    json!({"code": code, "definite": reason.is_definite(), "detail": detail})
}

fn evidence_json(evidence: &VerifiedAdmissionEvidence) -> Value {
    let root = hex::encode(evidence.issuer());
    let (source, issuers, digest, document_id, revision) = match evidence.source() {
        AdmissionEvidenceSource::PresentedChain {
            issuers, digest, ..
        } => (
            "chain",
            issuers.iter().map(hex::encode).collect::<Vec<_>>(),
            Some(hex::encode(digest)),
            None,
            None,
        ),
        AdmissionEvidenceSource::RootAuthoredRecord {
            document_id,
            revision,
        } => (
            "registration",
            Vec::new(),
            None,
            Some(hex::encode(document_id)),
            Some(revision),
        ),
    };
    // The key that signed the leaf (a delegated issuer, or the root itself);
    // for a registration record, the root.
    let issuer = issuers.first().cloned().unwrap_or_else(|| root.clone());
    let claims = evidence.claims();
    json!({
        "subject": hex::encode(evidence.subject()),
        "root": root,
        "issuer": issuer,
        "issuers": issuers,
        "source": source,
        "chain_digest": digest,
        "document_id": document_id,
        "revision": revision,
        "credential_id": evidence.credential_id(),
        "audience": evidence.audience().values().collect::<Vec<_>>(),
        "scopes": claims.scopes,
        "tags": claims.tags,
        "attributes": claims.attributes,
        "issued_at": evidence.issued_at(),
        "not_before": evidence.not_before(),
        "expires_at": evidence.expires_at(),
        "registration": evidence.registration(),
    })
}

fn readiness_json(readiness: &EnrollmentReadiness) -> Value {
    let source = match &readiness.source {
        AdmissionEvidenceSource::PresentedChain { .. } => "chain",
        AdmissionEvidenceSource::RootAuthoredRecord { .. } => "registration",
    };
    let policy_sync = readiness.policy_sync.as_ref().map(|sync| {
        json!({
            "completed_at": sync.completed_at,
            "source": match &sync.source {
                SynchronizationSource::Root => "root".to_string(),
                SynchronizationSource::Replica(peer) => hex::encode(peer),
            },
            "observed_revision": sync.observed_revision,
        })
    });
    let clock = match readiness.local_clock {
        aster::enrollment::ClockSynchronization::Synchronized => "synchronized",
        aster::enrollment::ClockSynchronization::Unsynchronized => "unsynchronized",
        aster::enrollment::ClockSynchronization::NotChecked => "not_checked",
    };
    json!({
        "peer": hex::encode(readiness.peer),
        "source": source,
        "observed_source": readiness.observed_source.map(|source| source.address.to_string()),
        "policy_sync": policy_sync,
        "local_clock": clock,
    })
}

fn admission_error(error: AdmissionError) -> OpError {
    let (status, kind) = match &error {
        AdmissionError::NotAdmitted => (
            aster_status_t::ASTER_STATUS_UNAUTHENTICATED,
            aster_enrollment_error_t::ASTER_ENROLLMENT_ERROR_NOT_ADMITTED,
        ),
        AdmissionError::Denied(_) => (
            aster_status_t::ASTER_STATUS_PERMISSION_DENIED,
            aster_enrollment_error_t::ASTER_ENROLLMENT_ERROR_DENIED,
        ),
        AdmissionError::ClockSkew { .. } => (
            aster_status_t::ASTER_STATUS_INTERNAL,
            aster_enrollment_error_t::ASTER_ENROLLMENT_ERROR_CLOCK_SKEW,
        ),
        AdmissionError::Unavailable(_) => (
            aster_status_t::ASTER_STATUS_INTERNAL,
            aster_enrollment_error_t::ASTER_ENROLLMENT_ERROR_UNAVAILABLE,
        ),
    };
    OpError {
        status,
        code: status as i32,
        flags: kind as u32,
        message: error.to_string(),
    }
}

fn other_error(error: impl std::fmt::Display) -> OpError {
    OpError {
        status: aster_status_t::ASTER_STATUS_INTERNAL,
        code: aster_status_t::ASTER_STATUS_INTERNAL as i32,
        flags: 0,
        message: format!("{error:#}"),
    }
}

/// An `anyhow` error from Enrollment: an `AdmissionError` keeps its kind.
fn anyhow_error(error: anyhow::Error) -> OpError {
    match error.downcast::<AdmissionError>() {
        Ok(admission) => admission_error(admission),
        Err(other) => other_error(other),
    }
}

fn result(handle: u64, value: Value) -> Done {
    Done::ok(aster_event_kind_t::ASTER_EVENT_ENROLLMENT_RESULT, handle)
        .data(value.to_string().into_bytes())
}

unsafe fn prepare(out: *mut aster_bytes_t) -> Result<(), i32> {
    if out.is_null() {
        return Err(invalid("NULL output"));
    }
    ptr::write(
        out,
        aster_bytes_t {
            ptr: ptr::null(),
            len: 0,
        },
    );
    Ok(())
}

unsafe fn output(out: *mut aster_bytes_t, value: Value) {
    ptr::write(out, alloc_owned_bytes(value.to_string().into_bytes()));
}

fn live(bridge: &BridgeRuntime) -> Result<(), i32> {
    if bridge.closing.load(Ordering::Acquire) {
        set_last_error("runtime is closing");
        return Err(aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32);
    }
    Ok(())
}

fn enrollment_state(
    bridge: &BridgeRuntime,
    enrollment: aster_enrollment_t,
) -> Result<Arc<EnrollmentState>, i32> {
    live(bridge)?;
    bridge
        .enrollments
        .get(enrollment)
        .ok_or_else(|| crate::data::not_found(format!("unknown enrollment {enrollment}")))
}

macro_rules! attempt {
    ($value:expr) => {
        match $value {
            Ok(value) => value,
            Err(status) => return status,
        }
    };
}

// ── Enrolled node ───────────────────────────────────────────────────────────

/// Install Enrollment on `node` from `config_json` and write its handle.
///
/// ```json
/// {
///   "root": "<hex32 root public key>",
///   "audience": "<fleet audience>",
///   "fleet_namespace": "<hex32 fleet policy namespace>",
///   "state_path": "/var/lib/app/enrollment.json",
///   "credential": "aster-chain-v1~…",          // optional: a pure verifier has none
///   "policy": {"all_of": [{"scope": "session"}, "tolerate_stale_policy"]},
///   "rotation": {"next_commitment": "<hex32>", "seq": 0},   // optional
///   "applied_rotations": [],                   // optional
///   "require_synchronized_clock": false,       // optional
///   "activation_leeway_secs": 60,              // optional
///   "limits": {"handshake_timeout_ms": 10000, "step_timeout_ms": 3000,
///              "policy_timeout_ms": 2000, "reconciliation_interval_ms": 1000,
///              "max_admitted_peers": 1024}       // optional, each optional
/// }
/// ```
///
/// Policy nodes: `all_of` / `any_of` / `none_of` (non-empty arrays),
/// `source_cidr`, `audience`, `scope`, `tag` (strings), `attribute_equals`
/// (`{"key", "value": {"type": "bool"|"int"|"float"|"string", "value"}}`),
/// `issued_by` (hex32 key on the chain's issuer path),
/// `max_credential_age_secs`, `policy_synchronized_within`
/// (`{"max_age_secs", "require_root"}`), and the bare strings
/// `"tolerate_stale_policy"` and `"require_registration"`. Admission never
/// grants operation access; map claims to roles with
/// `aster_enrollment_peer_claims`.
///
/// Returns `INVALID_ARGUMENT` for a malformed configuration or one Enrollment
/// refuses (for example a credential that does not verify for this node),
/// `NOT_FOUND` for an unknown node, `UNSUPPORTED` for a node without hooks or
/// with a gate attached, and `ALREADY_EXISTS` when Enrollment is already
/// installed on it.
///
/// # Safety
/// `config_json` must be readable; `out_enrollment` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_install(
    runtime: aster_runtime_t,
    node: aster_node_t,
    config_json: aster_bytes_t,
    out_enrollment: *mut aster_enrollment_t,
) -> i32 {
    if out_enrollment.is_null() {
        return invalid("NULL enrollment output");
    }
    ptr::write(out_enrollment, 0);
    let bridge = attempt!(bridge_for(runtime));
    attempt!(live(&bridge));
    let config: InstallConfig = attempt!(parse_json(
        attempt!(in_slice(&config_json, "config_json")),
        "enrollment config"
    ));
    let Some(core) = bridge.nodes.get(node) else {
        return crate::data::not_found(format!("unknown node {node}"));
    };
    let Some(slot) = node::admission_slot(&bridge, node) else {
        set_last_error("Enrollment needs a node started with enable_hooks and fail-closed hooks");
        return aster_status_t::ASTER_STATUS_UNSUPPORTED as i32;
    };
    let root = attempt!(hex32(&config.root, "root").map_err(invalid));
    let namespace = attempt!(hex32(&config.fleet_namespace, "fleet_namespace").map_err(invalid));
    if config.state_path.is_empty() {
        return invalid("state_path: required (Enrollment keeps denials durably)");
    }
    let policy = attempt!(config.policy.lower().map_err(invalid));
    if let Err(error) = policy.validate() {
        return invalid(format!("policy: {error}"));
    }
    let mut store = match EnrollmentPolicyStore::open(
        &config.state_path,
        root,
        namespace,
        config.audience.clone(),
    ) {
        Ok(store) => store,
        Err(error) => return invalid(format!("state_path: {error}")),
    };
    let mut verifier = match JwtEnrollmentVerifier::new(root, config.audience.clone()) {
        Ok(verifier) => verifier,
        Err(error) => return invalid(format!("root/audience: {error}")),
    };
    if let Some(leeway) = config.activation_leeway_secs {
        store = match store.with_activation_leeway(leeway) {
            Ok(store) => store,
            Err(error) => return invalid(format!("activation_leeway_secs: {error}")),
        };
        verifier = match verifier.with_activation_leeway(leeway) {
            Ok(verifier) => verifier,
            Err(error) => return invalid(format!("activation_leeway_secs: {error}")),
        };
    }
    let mut options = EnrollmentOptions {
        require_synchronized_clock: config.require_synchronized_clock,
        applied_rotations: config.applied_rotations,
        ..Default::default()
    };
    if let Some(rotation) = config.rotation {
        options.rotation = Some(RotationPin {
            next_commitment: attempt!(
                hex32(&rotation.next_commitment, "rotation.next_commitment").map_err(invalid)
            ),
            seq: rotation.seq,
        });
    }
    let limits = config.limits;
    let ms = Duration::from_millis;
    if let Some(value) = limits.handshake_timeout_ms {
        options.handshake_timeout = ms(value);
    }
    if let Some(value) = limits.step_timeout_ms {
        options.step_timeout = ms(value);
    }
    if let Some(value) = limits.policy_timeout_ms {
        options.policy_timeout = ms(value);
    }
    if let Some(value) = limits.reconciliation_interval_ms {
        options.reconciliation_interval = ms(value);
    }
    if let Some(value) = limits.max_admitted_peers {
        options.max_admitted_peers = value;
    }
    let credential = config
        .credential
        .map(|chain| chain.trim().to_owned())
        .filter(|chain| !chain.is_empty());

    // Holding the slot across install keeps a concurrent set_gate out.
    let mut state = slot.write().unwrap_or_else(|e| e.into_inner());
    if state.gate.is_some() {
        set_last_error("detach the node's gate before installing Enrollment");
        return aster_status_t::ASTER_STATUS_UNSUPPORTED as i32;
    }
    if state.enrollment.is_some() {
        set_last_error("Enrollment is already installed on this node");
        return aster_status_t::ASTER_STATUS_ALREADY_EXISTS as i32;
    }
    let (before_connect, before_connect_rx) = tokio::sync::mpsc::channel(64);
    let (after_handshake, after_handshake_rx) = tokio::sync::mpsc::channel(64);
    let hooks = CoreHookReceiver {
        before_connect_rx,
        after_handshake_rx,
    };
    let _context = bridge.runtime.enter();
    let facade = aster::Node::from_core_node((*core).clone());
    let enrollment = match Enrollment::install_with_hooks(
        &facade,
        hooks,
        Arc::new(store),
        verifier,
        credential,
        Arc::new(policy),
        options,
    ) {
        Ok(enrollment) => enrollment,
        Err(error) => return invalid(format!("{error:#}")),
    };
    state.enrollment = Some(node::HookForward {
        before_connect,
        after_handshake,
    });
    drop(state);
    let revocations = tokio::sync::Mutex::new(enrollment.revocations());
    let id = bridge.enrollments.insert(EnrollmentState {
        enrollment,
        node: facade,
        revocations,
        released: AtomicBool::new(false),
        released_notify: tokio::sync::Notify::new(),
    });
    ptr::write(out_enrollment, id);
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Release an enrollment handle. Enrollment keeps deciding the node's
/// admission while a server or issuer still holds it; once nothing does, the
/// node rejects ordinary inbound connections (fail closed); close the node.
/// Pending `aster_enrollment_next_revocation` waits end with `ALREADY_CLOSED`.
#[no_mangle]
pub extern "C" fn aster_enrollment_free(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
) -> i32 {
    let bridge = attempt!(bridge_for(runtime));
    match bridge.enrollments.remove(enrollment) {
        Some(state) => {
            state.release();
            aster_status_t::ASTER_STATUS_OK as i32
        }
        None => aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    }
}

/// Enroll reciprocally with the first of `replicas_json` that completes the
/// handshake (tried in order; at most 16). `replicas_json` is an array of
/// `{"node_id": "<hex32>", "relay_url": "…"?, "direct_addresses": ["ip:port"]?}`.
/// Completes with `ASTER_EVENT_ENROLLMENT_RESULT`:
/// `{"readiness": {…}, "policy_ticket": "<doc ticket>"|null}`. Readiness is
/// `{"peer", "source": "chain"|"registration", "observed_source", "policy_sync",
/// "local_clock"}`; pass `peer` and the ticket to `aster_enrollment_synchronize`.
///
/// # Safety
/// `replicas_json` must be readable; `out_operation` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_connect(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    replicas_json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL operation output");
    }
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let replicas: Vec<AddrJson> = attempt!(parse_json(
        attempt!(in_slice(&replicas_json, "replicas_json")),
        "replicas"
    ));
    if replicas.is_empty() || replicas.len() > MAX_REPLICAS {
        return invalid(format!("replicas: 1 to {MAX_REPLICAS} addresses"));
    }
    let replicas: Vec<aster::NodeAddr> = attempt!(replicas
        .into_iter()
        .map(AddrJson::lower)
        .collect::<Result<_, _>>()
        .map_err(invalid));
    spawn_op_cancellable(&bridge, user_data, out_operation, move |_| async move {
        let (readiness, ticket) = state
            .enrollment
            .connect_replicas(replicas)
            .await
            .map_err(anyhow_error)?;
        Ok(result(
            enrollment,
            json!({"readiness": readiness_json(&readiness), "policy_ticket": ticket}),
        ))
    })
}

/// Synchronize the fleet policy namespace from an admitted `peer` with a
/// read-only `ticket` (the one `aster_enrollment_connect` returned, or one the
/// consumer was provisioned with), then every bound namespace the peer offered
/// a ticket for, waiting up to `timeout_ms` (0 = 15 s) for reconciliation.
/// Completes with `ASTER_EVENT_ENROLLMENT_RESULT`: `{"namespace": "<hex32>"}`.
///
/// # Safety
/// `peer` and `ticket` must be readable; `out_operation` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_synchronize(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    peer: aster_bytes_t,
    ticket: aster_bytes_t,
    timeout_ms: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL operation output");
    }
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let peer = attempt!(in_key(&peer, "peer"));
    let ticket = attempt!(in_string(&ticket, "ticket")).trim().to_owned();
    if ticket.is_empty() {
        return invalid("ticket: required");
    }
    let timeout = if timeout_ms == 0 {
        Duration::from_secs(15)
    } else {
        Duration::from_millis(timeout_ms)
    };
    spawn_op_cancellable(&bridge, user_data, out_operation, move |_| async move {
        let document = state
            .enrollment
            .synchronize(peer, ticket, timeout)
            .await
            .map_err(anyhow_error)?;
        let namespace = document.id().map_err(other_error)?.to_string();
        Ok(result(enrollment, json!({ "namespace": namespace })))
    })
}

/// Recheck an admitted peer against current policy (the call to make before
/// a sensitive operation). Completes with `ASTER_EVENT_ENROLLMENT_RESULT`
/// (readiness JSON, as `aster_enrollment_connect`), or `ASTER_EVENT_ERROR`
/// whose `flags` is an `aster_enrollment_error_t`. Only `DENIED` revokes.
///
/// # Safety
/// `peer` must be readable; `out_operation` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_check_peer(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    peer: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL operation output");
    }
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let peer = attempt!(in_key(&peer, "peer"));
    spawn_op_cancellable(&bridge, user_data, out_operation, move |_| async move {
        let readiness = state
            .enrollment
            .check_peer(&peer)
            .await
            .map_err(admission_error)?;
        Ok(result(enrollment, readiness_json(&readiness)))
    })
}

/// The verified evidence a peer's current admission session rests on, for
/// mapping its claims to application roles (for example with
/// `aster_server_set_peer_attributes`). Writes owned JSON to `*out_json`:
///
/// ```json
/// {"subject": "<hex32>", "root": "<hex32>", "issuer": "<hex32 leaf signer>",
///  "issuers": ["<leaf signer>", …, "<root>"], "source": "chain"|"registration",
///  "chain_digest": "<hex32>"|null, "document_id": "<hex32>"|null,
///  "revision": 3|null, "credential_id": "…"|null, "audience": ["…"],
///  "scopes": ["…"]|null, "tags": ["…"]|null,
///  "attributes": {"k": {"type": "string", "value": "v"}}|null,
///  "issued_at": 1700000000, "not_before": null, "expires_at": 1700003600|null,
///  "registration": "ephemeral"|"persistent"|null}
/// ```
///
/// `null` scopes/tags/attributes are unknown, not empty. The session reflects
/// the last admission or check; `aster_enrollment_check_peer` re-evaluates it.
/// Returns `NOT_FOUND` when the peer has no session.
///
/// # Safety
/// `peer` must be readable; `out_json` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_peer_claims(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    peer: aster_bytes_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    attempt!(prepare(out_json));
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let peer = attempt!(in_key(&peer, "peer"));
    let Some(evidence) = state.enrollment.peer_evidence(&peer) else {
        return crate::data::not_found("peer has no enrollment session");
    };
    output(out_json, evidence_json(&evidence));
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Verify `chain` as a credential for `subject` under this node's trust (root,
/// audience, signed validity and the root's delegate revocations; root
/// records are not consulted). Writes the evidence JSON (as
/// `aster_enrollment_peer_claims`) to `*out_json`, or returns
/// `VERIFICATION_FAILED` with the reason in the last error.
///
/// # Safety
/// `subject` and `chain` must be readable; `out_json` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_verify_credential(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    subject: aster_bytes_t,
    chain: aster_bytes_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    attempt!(prepare(out_json));
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let subject = attempt!(in_key(&subject, "subject"));
    let chain = attempt!(in_string(&chain, "chain"));
    match state.enrollment.verify_credential(&subject, chain.trim()) {
        Ok(evidence) => {
            output(out_json, evidence_json(&evidence));
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(error) => {
            set_last_error(error);
            aster_status_t::ASTER_STATUS_VERIFICATION_FAILED as i32
        }
    }
}

/// This node's Enrollment state as owned JSON:
/// `{"trusted_root", "fleet_namespace", "audience", "credential": <evidence>|null,
/// "root_state": {"root", "next_commitment", "seq"}|null, "root_rotations": []}`.
/// `trusted_root` moves on after an accepted root rotation.
///
/// # Safety
/// `out_json` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_status(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    attempt!(prepare(out_json));
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let enrollment = &state.enrollment;
    let root_state = enrollment.root_state().map(|root| {
        json!({
            "root": hex::encode(root.root),
            "next_commitment": hex::encode(root.next_commitment),
            "seq": root.seq,
        })
    });
    output(
        out_json,
        json!({
            "trusted_root": hex::encode(enrollment.trusted_root()),
            "fleet_namespace": hex::encode(enrollment.fleet_namespace()),
            "audience": enrollment.audience(),
            "credential": enrollment.credential().map(|(_, evidence)| evidence_json(&evidence)),
            "root_state": root_state,
            "root_rotations": enrollment.root_rotations(),
        }),
    );
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Replace this node's credential (it must verify for this node) and present
/// it again to every peer it has a session with. Completes with
/// `ASTER_EVENT_ENROLLMENT_RESULT`: `{"unreached": [{"peer", "error"}]}`.
///
/// # Safety
/// `chain` must be readable; `out_operation` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_replace_credential(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    chain: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL operation output");
    }
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let chain = attempt!(in_string(&chain, "chain")).trim().to_owned();
    if chain.is_empty() {
        return invalid("chain: required");
    }
    spawn_op_cancellable(&bridge, user_data, out_operation, move |_| async move {
        let unreached = state
            .enrollment
            .replace_credential(chain)
            .await
            .map_err(anyhow_error)?;
        let unreached: Vec<Value> = unreached
            .into_iter()
            .map(|(peer, error)| json!({"peer": hex::encode(peer), "error": error}))
            .collect();
        Ok(result(enrollment, json!({ "unreached": unreached })))
    })
}

/// Wait for the next revocation of a formerly admitted peer (a definite
/// denial; every connection of that peer is already closed). Completes with
/// `ASTER_EVENT_ENROLLMENT_RESULT`: `{"peer": "<hex32>", "reason": {"code",
/// "definite", "detail"}}`, or `{"lagged": n}` when `n` revocations were
/// missed (recheck every peer relied on). Revocations are buffered from
/// install; receives on one enrollment are serialized. Cancelling the
/// operation consumes nothing; `aster_enrollment_free` ends a pending wait
/// with `ALREADY_CLOSED`.
///
/// # Safety
/// `out_operation` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_next_revocation(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL operation output");
    }
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let closed = OpError {
        status: aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
        code: aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32,
        flags: 0,
        message: "enrollment released".into(),
    };
    spawn_op_cancellable(
        &bridge,
        user_data,
        out_operation,
        move |cancel| async move {
            let mut receiver = tokio::select! {
                biased;
                () = cancel.wait() => return Err(other_error("cancelled")),
                () = state.wait_released() => return Err(closed),
                receiver = state.revocations.lock() => receiver,
            };
            let received = tokio::select! {
                biased;
                () = cancel.wait() => return Err(other_error("cancelled")),
                () = state.wait_released() => return Err(closed),
                received = receiver.recv() => received,
            };
            match received {
                Ok(revoked) => Ok(result(
                    enrollment,
                    json!({"peer": hex::encode(revoked.peer), "reason": denial_json(&revoked.reason)}),
                )),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    Ok(result(enrollment, json!({ "lagged": missed })))
                }
                Err(broadcast::error::RecvError::Closed) => Err(closed),
            }
        },
    )
}

// ── Renewal ─────────────────────────────────────────────────────────────────

/// Start renewing this node's credential from the issuer that signed it,
/// whenever a third of its lifetime remains, until the root registers the
/// node, the issuer refuses for good, or the credential expires. Renewed
/// credentials are presented to every admitted peer. `config_json`:
/// `{"issuer": {"node_id", "relay_url"?, "direct_addresses"?}, "retry_ms"?,
/// "max_retry_ms"?, "call_timeout_ms"?}`. Free the handle to stop renewing.
///
/// # Safety
/// `config_json` must be readable; `out_renewal` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_start_renewal(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    config_json: aster_bytes_t,
    out_renewal: *mut aster_enrollment_renewal_t,
) -> i32 {
    if out_renewal.is_null() {
        return invalid("NULL renewal output");
    }
    ptr::write(out_renewal, 0);
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let config: RenewalJson = attempt!(parse_json(
        attempt!(in_slice(&config_json, "config_json")),
        "renewal config"
    ));
    let mut renewal = RenewalConfig::new(attempt!(config.issuer.lower().map_err(invalid)));
    let ms = Duration::from_millis;
    if let Some(value) = config.retry_ms.filter(|value| *value > 0) {
        renewal.retry = ms(value);
    }
    if let Some(value) = config.max_retry_ms.filter(|value| *value > 0) {
        renewal.max_retry = ms(value);
    }
    if let Some(value) = config.call_timeout_ms.filter(|value| *value > 0) {
        renewal.call_timeout = ms(value);
    }
    let _context = bridge.runtime.enter();
    let handle = state.enrollment.start_renewal(renewal);
    let (sender, outcome) = watch::channel(None);
    let task = bridge.runtime.spawn(async move {
        let finished = handle.finished().await;
        let _ = sender.send(Some(finished));
    });
    let id = bridge.enrollment_renewals.insert(RenewalState {
        outcome,
        task: task.abort_handle(),
    });
    ptr::write(out_renewal, id);
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Wait for renewal to stop. Completes with `ASTER_EVENT_ENROLLMENT_RESULT`:
/// `{"outcome": "registered"|"refused"|"expired"|"no_credential",
/// "refusal": {"code", "message"}|null}` (`code` as `aster.enrollment.IssuerV1`
/// reports it, e.g. `subject_denied`). May be awaited more than once.
///
/// # Safety
/// `out_operation` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_renewal_wait(
    runtime: aster_runtime_t,
    renewal: aster_enrollment_renewal_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL operation output");
    }
    let bridge = attempt!(bridge_for(runtime));
    attempt!(live(&bridge));
    let Some(state) = bridge.enrollment_renewals.get(renewal) else {
        return crate::data::not_found(format!("unknown renewal {renewal}"));
    };
    let mut outcome = state.outcome.clone();
    drop(state);
    spawn_op_cancellable(
        &bridge,
        user_data,
        out_operation,
        move |cancel| async move {
            let finished = tokio::select! {
                biased;
                () = cancel.wait() => return Err(other_error("cancelled")),
                finished = outcome.wait_for(Option::is_some) => finished.map(|value| value.clone()),
            };
            let Ok(Some(finished)) = finished else {
                return Err(OpError {
                    status: aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
                    code: aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32,
                    flags: 0,
                    message: "renewal released".into(),
                });
            };
            let (name, refusal) = match finished {
                RenewalOutcome::Registered => ("registered", Value::Null),
                RenewalOutcome::Refused(refusal) => ("refused", refusal_json(&refusal)),
                RenewalOutcome::Expired => ("expired", Value::Null),
                RenewalOutcome::NoCredential => ("no_credential", Value::Null),
            };
            Ok(result(
                renewal,
                json!({"outcome": name, "refusal": refusal}),
            ))
        },
    )
}

/// Stop renewing and release the handle.
#[no_mangle]
pub extern "C" fn aster_enrollment_renewal_free(
    runtime: aster_runtime_t,
    renewal: aster_enrollment_renewal_t,
) -> i32 {
    let bridge = attempt!(bridge_for(runtime));
    match bridge.enrollment_renewals.remove(renewal) {
        Some(_) => aster_status_t::ASTER_STATUS_OK as i32,
        None => aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    }
}

fn refusal_json(refusal: &IssuerErrorV1) -> Value {
    json!({
        "code": refusal.code,
        "message": refusal.message,
        "retryable": refusal.retryable(),
    })
}

// ── Delegated issuer ────────────────────────────────────────────────────────

/// Make this enrolled node a delegated issuer. `delegation` is the node's
/// delegation chain from the root (`aster-chain-v1~…`; its leaf names this
/// node's key). Credentials it signs last `lifetime_secs` (0 = 24 h),
/// shortened to fit the delegation's `max_child_lifetime` and expiry;
/// renewals use the same lifetime. Serve renewals by passing the issuer to
/// `aster_server_set_enrollment`.
///
/// # Safety
/// `delegation` must be readable; `out_issuer` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_issuer_new(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    delegation: aster_bytes_t,
    lifetime_secs: u64,
    out_issuer: *mut aster_enrollment_issuer_t,
) -> i32 {
    if out_issuer.is_null() {
        return invalid("NULL issuer output");
    }
    ptr::write(out_issuer, 0);
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let text = attempt!(in_string(&delegation, "delegation"));
    let chain = match Chain::from_text(text.trim()) {
        Ok(chain) => chain,
        Err(error) => return invalid(format!("delegation: {error:?}")),
    };
    let lifetime = if lifetime_secs == 0 {
        DEFAULT_ISSUED_LIFETIME
    } else {
        Duration::from_secs(lifetime_secs)
    };
    let issuer = match Issuer::new(&state.node, state.enrollment.clone(), chain, lifetime) {
        Ok(issuer) => issuer,
        Err(error) => return invalid(format!("delegation: {error:#}")),
    };
    let id = bridge.enrollment_issuers.insert(IssuerState {
        issuer,
        enrollment: state.enrollment.clone(),
    });
    ptr::write(out_issuer, id);
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Release an issuer handle (a server it was set on keeps serving it).
#[no_mangle]
pub extern "C" fn aster_enrollment_issuer_free(
    runtime: aster_runtime_t,
    issuer: aster_enrollment_issuer_t,
) -> i32 {
    let bridge = attempt!(bridge_for(runtime));
    match bridge.enrollment_issuers.remove(issuer) {
        Some(_) => aster_status_t::ASTER_STATUS_OK as i32,
        None => aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    }
}

/// `{"public_key": "<hex32>", "delegation_expires_at": 1700000000}` as owned
/// JSON. Only the root renews the delegation.
///
/// # Safety
/// `out_json` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_issuer_info(
    runtime: aster_runtime_t,
    issuer: aster_enrollment_issuer_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    attempt!(prepare(out_json));
    let bridge = attempt!(bridge_for(runtime));
    attempt!(live(&bridge));
    let Some(state) = bridge.enrollment_issuers.get(issuer) else {
        return crate::data::not_found(format!("unknown issuer {issuer}"));
    };
    output(
        out_json,
        json!({
            "public_key": hex::encode(state.issuer.public_key()),
            "delegation_expires_at": state.issuer.delegation_expires_at(),
        }),
    );
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Sign a credential for `subject` after the orchestrator has authenticated
/// it out of band. `request_json`: `{"subject": "<hex32 node id>",
/// "claims": {"scopes": [], "tags": [], "attributes": {}}, "registration":
/// "ephemeral"|"persistent"}` (claims default to unknown; registration to
/// ephemeral; a set of `null` is unknown). On success writes
/// `{"chain", "credential_id", "expires_at"}`. When the issuer refuses
/// (a subject the root denied or registered, claims beyond the delegation,
/// a revoked issuer) returns `PERMISSION_DENIED` and writes
/// `{"code", "message", "retryable"}` instead. Both outputs are owned JSON.
///
/// # Safety
/// `request_json` must be readable; `out_json` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_issue(
    runtime: aster_runtime_t,
    issuer: aster_enrollment_issuer_t,
    request_json: aster_bytes_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    attempt!(prepare(out_json));
    let bridge = attempt!(bridge_for(runtime));
    attempt!(live(&bridge));
    let Some(state) = bridge.enrollment_issuers.get(issuer) else {
        return crate::data::not_found(format!("unknown issuer {issuer}"));
    };
    let request: IssueJson = attempt!(parse_json(
        attempt!(in_slice(&request_json, "request_json")),
        "issue request"
    ));
    let subject = attempt!(hex32(&request.subject, "subject").map_err(invalid));
    match state
        .issuer
        .issue_for(&subject, request.claims, request.registration)
    {
        Ok(issued) => {
            let expires_at = state
                .enrollment
                .verify_credential(&subject, issued.chain())
                .ok()
                .and_then(|evidence| evidence.expires_at());
            output(
                out_json,
                json!({
                    "chain": issued.chain(),
                    "credential_id": issued.credential_id(),
                    "expires_at": expires_at,
                }),
            );
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(error) => {
            let refusal = IssuerErrorV1::from(error);
            set_last_error(&refusal);
            output(out_json, refusal_json(&refusal));
            aster_status_t::ASTER_STATUS_PERMISSION_DENIED as i32
        }
    }
}

// ── Server integration ──────────────────────────────────────────────────────

/// What `aster_server_set_enrollment` attaches to a server builder.
pub(crate) struct ServerEnrollment {
    pub(crate) enrollment: Enrollment,
    pub(crate) issuer: Option<Issuer>,
}

impl ServerEnrollment {
    /// Route `aster.enrollment/1` connections into Enrollment and register
    /// the issuer's renewal service. Each accepted connection is held until
    /// the peer closes it or `stopped` ends (the server was destroyed).
    pub(crate) fn apply(
        self,
        mut server: aster::rpc::Server,
        stopped: watch::Receiver<()>,
    ) -> aster::rpc::Server {
        if let Some(issuer) = self.issuer {
            server = server.register(issuer);
        }
        let enrollment = self.enrollment;
        server.route_alpn(aster::enrollment::ENROLLMENT_ALPN, move |connection| {
            let enrollment = enrollment.clone();
            let mut stopped = stopped.clone();
            tokio::spawn(async move {
                let accepted = tokio::select! {
                    accepted = enrollment.accept(&connection) => accepted.is_ok(),
                    _ = stopped.changed() => false,
                };
                drop(enrollment);
                if accepted {
                    tokio::select! {
                        _ = connection.closed() => {}
                        _ = stopped.changed() => {}
                    }
                }
            });
        })
    }
}

/// Resolve the handles `aster_server_set_enrollment` names.
pub(crate) fn server_enrollment(
    bridge: &BridgeRuntime,
    enrollment: aster_enrollment_t,
    issuer: aster_enrollment_issuer_t,
) -> Result<ServerEnrollment, i32> {
    let state = enrollment_state(bridge, enrollment)?;
    let issuer = if issuer == 0 {
        None
    } else {
        let issued = bridge
            .enrollment_issuers
            .get(issuer)
            .ok_or_else(|| crate::data::not_found(format!("unknown issuer {issuer}")))?;
        Some(issued.issuer.clone())
    };
    Ok(ServerEnrollment {
        enrollment: state.enrollment.clone(),
        issuer,
    })
}
