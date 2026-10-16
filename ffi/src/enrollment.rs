//! ABI 5.1: Enrollment driven by the resolved `AsterConfig`, the C ABI mirror
//! of `AsterServer` / `AsterClient` (design
//! `docs/_internal/working_ideas/aster-enrollment.md` §15).
//!
//! The binding never re-implements Enrollment: it hands this module the
//! `[trust]` + `[enrollment]` settings as JSON and the facade's
//! `binding_support::enrollment::plan` / `Plan::install` and the server and
//! client sequences do the rest.
//!
//! ```text
//!   aster_config_enrollment_from_env / _from_toml   resolve [trust]/[enrollment]
//!                                                   (and ASTER_ENROLLMENT_*) → JSON
//!   aster_node_start_enrolled(rt, cfg, json, side)   validate (fail closed), start
//!                                                   the node with fail-closed hooks
//!                                                   and aster.enrollment/1, install
//!                                                   → NODE_CREATED, related = enrollment
//!   aster_server_builder_new / _start                a server on that node routes
//!                                                   the handshake, serves IssuerV1
//!                                                   when configured, maps claims to
//!                                                   Gate-3 roles per call, attaches
//!                                                   policy and renews (§15.4)
//!   aster_enrollment_enroll(rt, e, server_addr)      client: handshake, sync,
//!                                                   renewal (§15.7)
//! ```
//!
//! Escape hatches on an installed Enrollment: `aster_enrollment_status`,
//! `_peer_claims`, `_check_peer`, `_next_revocation`, and on a server with a
//! configured issuer `_issuer_info` and `_issue`. Root operations (namespace
//! create/bind, promote, revoke, reauthorize, rotate, delegate) stay in the
//! Rust CLI.
//!
//! # Conventions
//!
//! Configuration and results are UTF-8 JSON (`aster_bytes_t`, borrowed for the
//! call). Synchronous outputs are owned byte spans freed with
//! `aster_string_release`. Asynchronous operations complete with
//! `ASTER_EVENT_ENROLLMENT_RESULT` (JSON payload, `handle` = the enrollment)
//! or `ASTER_EVENT_ERROR`. Admission errors carry their kind in the event's
//! `flags` (see `aster_enrollment_error_t`). Keys and node ids are 64 hex
//! characters; outputs are lowercase.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use aster::binding_support::enrollment::{plan, serve_inbound_enrollment, Overrides, Plan, Side};
use aster::enrollment::{
    AdmissionClaims, AdmissionError, AdmissionEvidenceSource, DenialReason, Enrollment,
    EnrollmentReadiness, EnrollmentRevoked, Issuer, IssuerErrorV1, Registration, RenewalHandle,
    SynchronizationSource, VerifiedAdmissionEvidence,
};
use aster::{AsterConfig, EnrollmentConfig, EnrollmentLimits, PublicKey, RoleMappingConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

use crate::data::{bridge_for, in_slice, in_string, invalid, spawn_op_cancellable, Done, OpError};
use crate::*;

/// An Enrollment installed by `aster_node_start_enrolled`, scoped to a
/// runtime. It lives as long as its node (closing the node releases it);
/// `aster_enrollment_free` releases it earlier.
pub type aster_enrollment_t = u64;

/// `side` of `aster_node_start_enrolled`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_enrollment_side_t {
    /// A server: durable state required; may run a delegated issuer;
    /// `aster_server_start` on the node attaches policy and renews.
    ASTER_ENROLLMENT_SIDE_SERVER = 0,
    /// A client: state may be temporary; enrolls per server with
    /// `aster_enrollment_enroll`; serves inbound enrollment handshakes.
    ASTER_ENROLLMENT_SIDE_CLIENT = 1,
}

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

/// Configuration and request JSON is bounded before parsing.
const MAX_CONFIG_BYTES: usize = 256 * 1024;

/// The registry entry behind an `aster_enrollment_t`.
pub(crate) struct EnrollmentState {
    pub(crate) enrollment: Enrollment,
    pub(crate) issuer: Option<Issuer>,
    pub(crate) node: aster::Node,
    /// The validated plan (keeps a client's temporary state directory alive).
    pub(crate) plan: Plan,
    /// This node's credential renewal, once started (at most one).
    renewal: Mutex<Option<RenewalHandle>>,
    /// A client's inbound enrollment acceptor.
    acceptor: Option<tokio::task::JoinHandle<()>>,
    /// Subscribed at install, so no revocation after install is missed.
    revocations: tokio::sync::Mutex<broadcast::Receiver<EnrollmentRevoked>>,
    /// Set by `aster_enrollment_free`; ends pending revocation waits.
    released: AtomicBool,
    released_notify: tokio::sync::Notify,
    /// ABI 7.7: the issuer-key publisher (`[identity.publisher]`).
    pub(crate) publisher: Mutex<crate::identity::PublisherSlot>,
}

impl EnrollmentState {
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.released_notify.notify_waiters();
        if let Some(acceptor) = &self.acceptor {
            acceptor.abort();
        }
        self.renewal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release();
    }

    /// The node's issuer-key publisher, when it runs one.
    pub(crate) fn publisher_handle(&self) -> Option<aster::rpc::jwks_publisher::PublisherHandle> {
        self.publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handle()
    }

    async fn wait_released(&self) {
        let notified = self.released_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.released.load(Ordering::Acquire) {
            notified.await;
        }
    }

    fn renewing(&self) -> bool {
        self.renewal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Start renewal unless one runs already (§15.4 step 6, §15.7 step 6).
    fn start_renewal(&self, server: Option<&aster::NodeAddr>) -> bool {
        let mut renewal = self.renewal.lock().unwrap_or_else(|e| e.into_inner());
        if renewal.is_none() {
            *renewal = self.plan.start_renewal(&self.enrollment, server);
        }
        renewal.is_some()
    }

    /// Server sequence after serving (§15.4 steps 5 and 6): hold or
    /// synchronize the fleet namespace, then renew from `renew_from`.
    pub(crate) async fn attach_server(&self) -> aster::Result<()> {
        self.plan
            .attach_policy(&self.node, &self.enrollment)
            .await?;
        self.start_renewal(None);
        Ok(())
    }

    /// Wire a server (§15.4 step 4): route `aster.enrollment/1` into
    /// Enrollment, serve `aster.enrollment.IssuerV1` when an issuer is
    /// configured, and feed Gate 3 each peer's roles per call. Accepted
    /// enrollment connections are held until the peer closes them or
    /// `stopped` ends (the server was destroyed).
    pub(crate) fn apply(
        &self,
        mut server: aster::rpc::Server,
        attributes: &aster::rpc::AttributeStore,
        stopped: watch::Receiver<()>,
    ) -> aster::rpc::Server {
        attributes.set_source(self.plan.attribute_source(&self.enrollment));
        if let Some(issuer) = &self.issuer {
            server = server.register(issuer.clone());
        }
        let enrollment = self.enrollment.clone();
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

impl Drop for EnrollmentState {
    fn drop(&mut self) {
        if let Some(acceptor) = &self.acceptor {
            acceptor.abort();
        }
    }
}

// ── Configuration JSON ──────────────────────────────────────────────────────

/// `[trust]` (root, rotation pin, this node's credential) and `[enrollment]`
/// as JSON: the field names of `aster::AsterConfig` / `EnrollmentConfig`
/// (design §15.3). Every field is optional; unknown fields are refused.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TrustJson {
    root_pubkey: Option<String>,
    root_pubkey_file: Option<String>,
    next_root_commitment: Option<String>,
    root_seq: u64,
    root_rotations: Vec<String>,
    enrollment_credential_file: Option<String>,
    enrollment_credential_chain: Option<String>,
    enrollment: EnrollmentJson,
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct EnrollmentJson {
    enabled: Option<bool>,
    audience: Option<String>,
    fleet_namespace: Option<String>,
    state_path: Option<String>,
    replicas: Vec<String>,
    policy: Option<Value>,
    require_synchronized_clock: bool,
    activation_leeway_secs: Option<u64>,
    limits: LimitsJson,
    roles: RolesJson,
    issuer_delegation_file: Option<String>,
    issuer_lifetime_secs: Option<u64>,
    renew_from: Option<String>,
    renew: Option<bool>,
    synchronize: Option<bool>,
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct LimitsJson {
    handshake_timeout_ms: Option<u64>,
    step_timeout_ms: Option<u64>,
    policy_timeout_ms: Option<u64>,
    reconciliation_interval_ms: Option<u64>,
    max_admitted_peers: Option<u64>,
    sync_timeout_ms: Option<u64>,
}

/// `[enrollment.roles]`; an absent `attribute` / `include_attributes` keeps
/// the default (`"aster.role"` / `true`), and `""` disables the attribute.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct RolesJson {
    attribute: Option<String>,
    scopes_as_roles: bool,
    scopes: BTreeMap<String, Vec<String>>,
    tags: BTreeMap<String, Vec<String>>,
    include_attributes: Option<bool>,
}

fn opt_path(path: &Option<String>) -> Option<PathBuf> {
    path.as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

fn opt_text(text: &Option<String>) -> Option<String> {
    text.as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn path_text(path: &Option<PathBuf>) -> Option<String> {
    path.as_ref().map(|p| p.display().to_string())
}

impl TrustJson {
    pub(crate) fn from_config(config: &AsterConfig) -> Self {
        let e = &config.enrollment;
        let l = &e.limits;
        Self {
            root_pubkey: config.root_pubkey.as_ref().map(PublicKey::to_hex),
            root_pubkey_file: path_text(&config.root_pubkey_file),
            next_root_commitment: config.next_root_commitment.clone(),
            root_seq: config.root_seq,
            root_rotations: config.root_rotations.clone(),
            enrollment_credential_file: path_text(&config.enrollment_credential_file),
            enrollment_credential_chain: config.enrollment_credential_chain.clone(),
            enrollment: EnrollmentJson {
                enabled: e.enabled,
                audience: e.audience.clone(),
                fleet_namespace: e.fleet_namespace.clone(),
                state_path: path_text(&e.state_path),
                replicas: e.replicas.clone(),
                policy: e.policy.clone(),
                require_synchronized_clock: e.require_synchronized_clock,
                activation_leeway_secs: e.activation_leeway_secs,
                limits: LimitsJson {
                    handshake_timeout_ms: l.handshake_timeout_ms,
                    step_timeout_ms: l.step_timeout_ms,
                    policy_timeout_ms: l.policy_timeout_ms,
                    reconciliation_interval_ms: l.reconciliation_interval_ms,
                    max_admitted_peers: l.max_admitted_peers,
                    sync_timeout_ms: l.sync_timeout_ms,
                },
                roles: RolesJson {
                    attribute: Some(e.roles.attribute.clone()),
                    scopes_as_roles: e.roles.scopes_as_roles,
                    scopes: e.roles.scopes.clone(),
                    tags: e.roles.tags.clone(),
                    include_attributes: Some(e.roles.include_attributes),
                },
                issuer_delegation_file: path_text(&e.issuer_delegation_file),
                issuer_lifetime_secs: e.issuer_lifetime_secs,
                renew_from: e.renew_from.clone(),
                renew: e.renew,
                synchronize: e.synchronize,
            },
        }
    }

    /// Replace `config`'s trust and enrollment settings with these.
    pub(crate) fn apply(self, config: &mut AsterConfig) -> Result<(), String> {
        config.root_pubkey = match opt_text(&self.root_pubkey) {
            None => None,
            Some(hex) => Some(
                PublicKey::from_hex(&hex)
                    .map_err(|_| "root_pubkey: expected 64 hex characters".to_string())?,
            ),
        };
        config.root_pubkey_file = opt_path(&self.root_pubkey_file);
        config.next_root_commitment = opt_text(&self.next_root_commitment);
        config.root_seq = self.root_seq;
        config.root_rotations = self.root_rotations;
        config.enrollment_credential_file = opt_path(&self.enrollment_credential_file);
        // Not trimmed to empty-means-unset: plan() refuses a non-chain
        // value with a hint, and an empty one as "no credential".
        config.enrollment_credential_chain = self
            .enrollment_credential_chain
            .filter(|chain| !chain.trim().is_empty());
        let e = self.enrollment;
        let l = e.limits;
        let defaults = RoleMappingConfig::default();
        config.enrollment = EnrollmentConfig {
            enabled: e.enabled,
            audience: opt_text(&e.audience),
            fleet_namespace: opt_text(&e.fleet_namespace),
            state_path: opt_path(&e.state_path),
            replicas: e
                .replicas
                .into_iter()
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
                .collect(),
            policy: e.policy.filter(|p| !p.is_null()),
            require_synchronized_clock: e.require_synchronized_clock,
            activation_leeway_secs: e.activation_leeway_secs,
            limits: EnrollmentLimits {
                handshake_timeout_ms: l.handshake_timeout_ms,
                step_timeout_ms: l.step_timeout_ms,
                policy_timeout_ms: l.policy_timeout_ms,
                reconciliation_interval_ms: l.reconciliation_interval_ms,
                max_admitted_peers: l.max_admitted_peers,
                sync_timeout_ms: l.sync_timeout_ms,
            },
            roles: RoleMappingConfig {
                attribute: e
                    .roles
                    .attribute
                    .map(|a| a.trim().to_string())
                    .unwrap_or(defaults.attribute),
                scopes_as_roles: e.roles.scopes_as_roles,
                scopes: e.roles.scopes,
                tags: e.roles.tags,
                include_attributes: e
                    .roles
                    .include_attributes
                    .unwrap_or(defaults.include_attributes),
            },
            issuer_delegation_file: opt_path(&e.issuer_delegation_file),
            issuer_lifetime_secs: e.issuer_lifetime_secs,
            renew_from: opt_text(&e.renew_from),
            renew: e.renew,
            synchronize: e.synchronize,
        };
        Ok(())
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T, i32> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(invalid(format!(
            "{what}: larger than {MAX_CONFIG_BYTES} bytes"
        )));
    }
    serde_json::from_slice(bytes).map_err(|error| invalid(format!("{what}: {error}")))
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

macro_rules! attempt {
    ($value:expr) => {
        match $value {
            Ok(value) => value,
            Err(status) => return status,
        }
    };
}

/// Resolve `[trust]` and `[enrollment]` from the environment
/// (`AsterConfig::from_env`: `ASTER_ROOT_PUBKEY(_FILE)`,
/// `ASTER_NEXT_ROOT_COMMITMENT`, `ASTER_ROOT_SEQ`, `ASTER_ROOT_ROTATIONS`,
/// `ASTER_ENROLLMENT_CREDENTIAL(_CHAIN)`, `ASTER_ENROLLMENT_*`) and write the
/// trust JSON `aster_node_start_enrolled` takes as owned JSON:
///
/// ```json
/// {"root_pubkey": "<hex>"|null, "root_pubkey_file": "…"|null,
///  "next_root_commitment": "<hex>"|null, "root_seq": 0, "root_rotations": [],
///  "enrollment_credential_file": "…"|null, "enrollment_credential_chain": "…"|null,
///  "enrollment": {"enabled": null, "audience": "…", "fleet_namespace": "<hex>",
///    "state_path": "…", "replicas": [], "policy": <PolicyConfig>|null,
///    "require_synchronized_clock": false, "activation_leeway_secs": null,
///    "limits": {"handshake_timeout_ms", "step_timeout_ms", "policy_timeout_ms",
///               "reconciliation_interval_ms", "max_admitted_peers", "sync_timeout_ms"},
///    "roles": {"attribute": "aster.role", "scopes_as_roles": false,
///              "scopes": {"scope": ["role"]}, "tags": {}, "include_attributes": true},
///    "issuer_delegation_file": "…"|null, "issuer_lifetime_secs": null,
///    "renew_from": "…"|null, "renew": null, "synchronize": null}}
/// ```
///
/// A malformed variable is `INVALID_ARGUMENT`.
///
/// # Safety
/// `out_json` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_config_enrollment_from_env(out_json: *mut aster_bytes_t) -> i32 {
    attempt!(prepare(out_json));
    match AsterConfig::from_env() {
        Ok(config) => {
            output(out_json, json!(TrustJson::from_config(&config)));
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(error) => invalid(error),
    }
}

/// [`aster_config_enrollment_from_env`] for a TOML file
/// (`AsterConfig::from_file`: defaults < file < environment).
///
/// # Safety
/// `path` must be readable; `out_json` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_config_enrollment_from_toml(
    path: aster_bytes_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    attempt!(prepare(out_json));
    let path = attempt!(in_string(&path, "path"));
    match AsterConfig::from_file(&path) {
        Ok(config) => {
            output(out_json, json!(TrustJson::from_config(&config)));
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(error) => invalid(error),
    }
}

// ── Node start ──────────────────────────────────────────────────────────────

/// Start a node from `config` with Enrollment from `trust_json` (the JSON of
/// `aster_config_enrollment_from_env`), as `AsterServer` / `AsterClient` do.
///
/// When the trust JSON does not enable enrollment (design §15.3: `enabled`,
/// else any of audience, fleet namespace, issuer delegation or credential),
/// this is exactly `aster_node_start(runtime, config, 0, …)`. Otherwise the
/// whole plan is validated before anything starts (any error is
/// `INVALID_ARGUMENT` naming the setting, and nothing runs open); the node
/// starts with fail-closed hooks and `aster.enrollment/1`, and Enrollment is
/// installed from the credential before the node is handed out (an
/// install failure, such as a credential issued for another key, closes the
/// node and completes with `ASTER_EVENT_ERROR`).
///
/// Completes with `ASTER_EVENT_NODE_CREATED`: `handle` = the node, `related`
/// = its `aster_enrollment_t` (0 when enrollment is off). A server
/// (`ASTER_ENROLLMENT_SIDE_SERVER`) then builds its dispatch server on the
/// node; `aster_server_start` wires and attaches Enrollment. A client
/// (`ASTER_ENROLLMENT_SIDE_CLIENT`) enrolls with each server through
/// `aster_enrollment_enroll` before its first RPC connection, and serves
/// inbound enrollment handshakes on the node (do not `aster_accept` on it).
/// The node accepts no gate.
///
/// # Safety
/// `config` is NULL or a valid `aster_config_t`; `trust_json` readable;
/// `out_operation` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_node_start_enrolled(
    runtime: aster_runtime_t,
    config: *const node::aster_config_t,
    trust_json: aster_bytes_t,
    side: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_node_start_enrolled: out_operation is NULL");
    }
    let side = match side {
        x if x == aster_enrollment_side_t::ASTER_ENROLLMENT_SIDE_SERVER as u32 => Side::Server,
        x if x == aster_enrollment_side_t::ASTER_ENROLLMENT_SIDE_CLIENT as u32 => Side::Client,
        other => return invalid(format!("unknown enrollment side {other}")),
    };
    let mut cfg = match node::lower_config_ptr(config) {
        Ok(cfg) => cfg,
        Err(e) => return invalid(e),
    };
    let trust: TrustJson = attempt!(parse_json(
        attempt!(in_slice(&trust_json, "trust_json")),
        "trust_json"
    ));
    if let Err(error) = trust.apply(&mut cfg) {
        return invalid(error);
    }
    let bridge = attempt!(bridge_for(runtime));
    let plan = match plan(&mut cfg, side, Overrides::default()) {
        Ok(Some(plan)) => plan,
        Ok(None) => return node::aster_node_start(runtime, config, 0, user_data, out_operation),
        Err(error) => return invalid(error),
    };
    // A client runs `[identity.publisher]` itself (validated here, as
    // `AsterClient` does); a server's is validated when it is built.
    let publisher = match side {
        Side::Client => match aster::binding_support::identity::client_publisher_plan(&cfg, true) {
            Ok(config) => {
                crate::identity::PublisherSlot::client(config, cfg.identity_max_key_set_age_secs)
            }
            Err(error) => return invalid(error),
        },
        Side::Server => crate::identity::PublisherSlot::default(),
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    *out_operation = op_id;
    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        let health = cfg.health_config();
        let peer_labels = crate::health::peer_label_cap(&cfg);
        let identity = cfg.clone();
        let node = match aster::Node::start_with_alpns(
            cfg,
            vec![aster::enrollment::ENROLLMENT_ALPN.to_vec()],
        )
        .await
        {
            Ok(node) => node,
            Err(error) => return bridge2.emit_error(op_id, user_data, &error.to_string()),
        };
        let installed = match plan.install(&node) {
            Ok(installed) => installed,
            Err(error) => {
                node.shutdown().await;
                bridge2.emit_error_coded(
                    op_id,
                    user_data,
                    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
                    &error.to_string(),
                );
                return;
            }
        };
        let acceptor = (plan.side() == Side::Client)
            .then(|| serve_inbound_enrollment(&node, &installed.enrollment));
        let revocations = tokio::sync::Mutex::new(installed.enrollment.revocations());
        let handle = bridge2.nodes.insert(node.core_node().clone());
        crate::health::remember_node_config(handle, health, peer_labels);
        crate::identity::remember_node_identity(handle, &identity);
        let id = bridge2.enrollments.insert(EnrollmentState {
            enrollment: installed.enrollment,
            issuer: installed.issuer,
            node: node.clone(),
            plan,
            renewal: Mutex::new(None),
            acceptor,
            revocations,
            released: AtomicBool::new(false),
            released_notify: tokio::sync::Notify::new(),
            publisher: Mutex::new(publisher),
        });
        bridge2
            .node_enrollments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle, id);
        let completed = bridge2.emit_simple(
            aster_event_kind_t::ASTER_EVENT_NODE_CREATED,
            aster_status_t::ASTER_STATUS_OK,
            op_id,
            handle,
            id,
            user_data,
            0,
        );
        if !completed {
            node::forget_node(&bridge2, handle);
            bridge2.nodes.remove(handle);
            node.shutdown().await;
        }
    });
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Release the Enrollment a node was started with (called when the node is
/// closed or freed).
pub(crate) fn forget_node(bridge: &BridgeRuntime, node: aster_node_t) {
    let id = bridge
        .node_enrollments
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&node);
    if let Some(state) = id.and_then(|id| bridge.enrollments.remove(id)) {
        state.release();
    }
}

/// The server-side Enrollment of `node`, for `aster_server_builder_new`.
/// `Ok(None)` when the node was started without enrollment.
pub(crate) fn server_enrollment_for_node(
    bridge: &BridgeRuntime,
    node: aster_node_t,
) -> Result<Option<Arc<EnrollmentState>>, i32> {
    let id = bridge
        .node_enrollments
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&node)
        .copied();
    let Some(id) = id else { return Ok(None) };
    let Some(state) = bridge.enrollments.get(id) else {
        set_last_error("the node's enrollment was released; close the node");
        return Err(aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32);
    };
    if state.plan.side() != Side::Server {
        return Err(invalid(
            "the node was started with a client enrollment; a server needs \
             ASTER_ENROLLMENT_SIDE_SERVER",
        ));
    }
    Ok(Some(state))
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
        DenialReason::RefusedByPeer => "refused_by_peer",
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
            aster_status_t::ASTER_STATUS_UNAVAILABLE,
            aster_enrollment_error_t::ASTER_ENROLLMENT_ERROR_CLOCK_SKEW,
        ),
        AdmissionError::Unavailable(_) => (
            aster_status_t::ASTER_STATUS_UNAVAILABLE,
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

/// `aster_enrollment_enroll`'s failure: the admission decision of either side
/// keeps its kind (and status) from [`admission_error`]; a server that could
/// not be reached or did not finish the handshake is `UNAVAILABLE`.
fn enroll_error(error: aster::Error) -> OpError {
    match error {
        aster::Error::Transport(error) => match error.downcast_ref::<AdmissionError>() {
            Some(admission) => OpError {
                message: error.to_string(),
                ..admission_error(admission.clone())
            },
            None => other_error(error),
        },
        aster::Error::Connection(message) => OpError {
            status: aster_status_t::ASTER_STATUS_UNAVAILABLE,
            code: aster_status_t::ASTER_STATUS_UNAVAILABLE as i32,
            flags: 0,
            message,
        },
        other => other_error(other),
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

fn result(handle: u64, value: Value) -> Done {
    Done::ok(aster_event_kind_t::ASTER_EVENT_ENROLLMENT_RESULT, handle)
        .data(value.to_string().into_bytes())
}

fn live(bridge: &BridgeRuntime) -> Result<(), i32> {
    if bridge.closing.load(Ordering::Acquire) {
        set_last_error("runtime is closing");
        return Err(aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32);
    }
    Ok(())
}

pub(crate) fn enrollment_state(
    bridge: &BridgeRuntime,
    enrollment: aster_enrollment_t,
) -> Result<Arc<EnrollmentState>, i32> {
    live(bridge)?;
    bridge
        .enrollments
        .get(enrollment)
        .ok_or_else(|| crate::data::not_found(format!("unknown enrollment {enrollment}")))
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

unsafe fn in_key(b: &aster_bytes_t, what: &str) -> Result<[u8; 32], i32> {
    let text = in_string(b, what)?;
    hex32(text.trim(), what).map_err(invalid)
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
        let node_id = hex::encode(hex32(self.node_id.trim(), "node_id")?);
        Ok(aster::NodeAddr {
            node_id: aster::NodeId::from_hex(node_id),
            relay_url: self.relay_url.filter(|url| !url.is_empty()),
            direct_addresses: self.direct_addresses,
        })
    }
}

// ── Client ──────────────────────────────────────────────────────────────────

/// Client sequence against one server (design §15.7, steps 3, 4 and 6):
/// enroll over the reciprocal handshake (the server verifies this node's
/// credential and this node verifies the server's) within
/// `limits.sync_timeout_ms`, synchronize the fleet namespace the server
/// offers unless `synchronize = false`, then start renewal from `renew_from`,
/// or from this server when it signed this node's credential, unless
/// `renew = false` (at most one renewal runs per node). Call it before the
/// first `aster/1` connection to the server; again after a refused call to
/// re-enroll.
///
/// `server_json`: `{"node_id": "<hex32>", "relay_url": "…"?,
/// "direct_addresses": ["ip:port"]?}`. Completes with
/// `ASTER_EVENT_ENROLLMENT_RESULT`: `{"readiness": {"peer", "source":
/// "chain"|"registration", "observed_source", "policy_sync", "local_clock"},
/// "renewing": bool}`, or `ASTER_EVENT_ERROR`. An admission decision by
/// either side keeps its `aster_enrollment_error_t` in `flags`, with status
/// `PERMISSION_DENIED` (`DENIED`: the server refused this node, or this node
/// refused the server), `UNAUTHENTICATED` (`NOT_ADMITTED`) or `UNAVAILABLE`
/// (`CLOCK_SKEW`, `UNAVAILABLE`: retry). A server that could not be reached or
/// did not finish the handshake in time is `UNAVAILABLE` with `flags` 0.
/// `INVALID_ARGUMENT` for a server enrollment.
///
/// # Safety
/// `server_json` must be readable; `out_operation` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_enroll(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    server_json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL operation output");
    }
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    if state.plan.side() != Side::Client {
        return invalid(
            "aster_enrollment_enroll is the client sequence; this is a server enrollment",
        );
    }
    let server: AddrJson = attempt!(parse_json(
        attempt!(in_slice(&server_json, "server_json")),
        "server address"
    ));
    let server = attempt!(server.lower().map_err(invalid));
    spawn_op_cancellable(&bridge, user_data, out_operation, move |_| async move {
        let readiness = state
            .plan
            .enroll_with_server(&state.enrollment, &server)
            .await
            .map_err(enroll_error)?;
        let renewing = state.start_renewal(Some(&server));
        // `[identity.publisher]` on a client delivers to every server it
        // enrolled with (Rust `AsterClient` delivers to its one server).
        state
            .publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enrolled(&state.node, &state.enrollment, server.node_id.clone())
            .map_err(other_error)?;
        Ok(result(
            enrollment,
            json!({"readiness": readiness_json(&readiness), "renewing": renewing}),
        ))
    })
}

// ── Escape hatches ──────────────────────────────────────────────────────────

/// Release an enrollment handle early (closing the node releases it too).
/// Pending `aster_enrollment_next_revocation` waits end with
/// `ALREADY_CLOSED`, a client stops renewing and answering inbound
/// handshakes. A server built on the node keeps its Enrollment until it is
/// destroyed; with nothing holding it the node rejects ordinary inbound
/// connections (fail closed).
#[no_mangle]
pub extern "C" fn aster_enrollment_free(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
) -> i32 {
    let bridge = attempt!(bridge_for(runtime));
    match bridge.enrollments.remove(enrollment) {
        Some(state) => {
            bridge
                .node_enrollments
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, id| *id != enrollment);
            state.release();
            aster_status_t::ASTER_STATUS_OK as i32
        }
        None => aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    }
}

/// Recheck an admitted peer against current policy (the call to make before
/// a sensitive operation). Completes with `ASTER_EVENT_ENROLLMENT_RESULT`
/// (readiness JSON, as `aster_enrollment_enroll`'s `readiness`), or
/// `ASTER_EVENT_ERROR` whose `flags` is an `aster_enrollment_error_t`. Only
/// `DENIED` revokes.
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
/// audit and diagnostics. A server already maps it to Gate-3 roles on every
/// call (`[enrollment.roles]`); do not copy it into peer attributes. Writes
/// owned JSON to `*out_json`:
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
/// `null` scopes/tags/attributes are unknown, not empty. Returns `NOT_FOUND`
/// when the peer has no session (never enrolled, revoked or evicted).
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

/// This node's Enrollment state as owned JSON:
/// `{"side": "server"|"client", "trusted_root", "fleet_namespace",
/// "audience", "credential": <evidence>|null, "root_state": {"root",
/// "next_commitment", "seq"}|null, "root_rotations": [], "renewing": bool,
/// "issuer": "<hex32>"|null}`. `credential` follows renewals;
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
    let e = &state.enrollment;
    let root_state = e.root_state().map(|root| {
        json!({
            "root": hex::encode(root.root),
            "next_commitment": hex::encode(root.next_commitment),
            "seq": root.seq,
        })
    });
    let side = match state.plan.side() {
        Side::Server => "server",
        Side::Client => "client",
    };
    output(
        out_json,
        json!({
            "side": side,
            "trusted_root": hex::encode(e.trusted_root()),
            "fleet_namespace": hex::encode(e.fleet_namespace()),
            "audience": e.audience(),
            "credential": e.credential().map(|(_, evidence)| evidence_json(&evidence)),
            "root_state": root_state,
            "root_rotations": e.root_rotations(),
            "renewing": state.renewing(),
            "issuer": state.issuer.as_ref().map(|issuer| hex::encode(issuer.public_key())),
        }),
    );
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Wait for the next revocation of a formerly admitted peer (a definite
/// denial; every connection of that peer is already closed and its roles are
/// gone from the next call). Completes with `ASTER_EVENT_ENROLLMENT_RESULT`:
/// `{"peer": "<hex32>", "reason": {"code", "definite", "detail"}}`, or
/// `{"lagged": n}` when `n` revocations were missed (recheck every peer
/// relied on). Revocations are buffered from install; receives on one
/// enrollment are serialized. Cancelling the operation consumes nothing;
/// releasing the enrollment ends a pending wait with `ALREADY_CLOSED`.
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

// ── Configured issuer ───────────────────────────────────────────────────────

fn issuer_of(state: &EnrollmentState) -> Result<&Issuer, i32> {
    state.issuer.as_ref().ok_or_else(|| {
        crate::data::not_found(
            "no issuer: set [enrollment.issuer] delegation_file \
             (ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE) on the server",
        )
    })
}

/// The configured delegated issuer of a server enrollment
/// (`[enrollment.issuer] delegation_file`), as owned JSON:
/// `{"public_key": "<hex32>", "delegation_expires_at": 1700000000}`.
/// `NOT_FOUND` when none is configured.
///
/// # Safety
/// `out_json` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_issuer_info(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    attempt!(prepare(out_json));
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let issuer = attempt!(issuer_of(&state));
    output(
        out_json,
        json!({
            "public_key": hex::encode(issuer.public_key()),
            "delegation_expires_at": issuer.delegation_expires_at(),
        }),
    );
    aster_status_t::ASTER_STATUS_OK as i32
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

fn refusal_json(refusal: &IssuerErrorV1) -> Value {
    json!({
        "code": refusal.code,
        "message": refusal.message,
        "retryable": refusal.retryable(),
    })
}

/// Sign a credential with the server's configured issuer for a subject the
/// orchestrator has authenticated out of band (renewals are served
/// automatically). `request_json`: `{"subject": "<hex32 node id>",
/// "claims": {"scopes": [], "tags": [], "attributes": {}}, "registration":
/// "ephemeral"|"persistent"}` (claims default to unknown; registration to
/// ephemeral). On success writes `{"chain", "credential_id", "expires_at"}`.
/// When the issuer refuses (a subject the root denied or registered, claims
/// beyond the delegation, a revoked issuer) returns `PERMISSION_DENIED` and
/// writes `{"code", "message", "retryable"}` instead. Both outputs are owned
/// JSON. `NOT_FOUND` when no issuer is configured.
///
/// # Safety
/// `request_json` must be readable; `out_json` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_issue(
    runtime: aster_runtime_t,
    enrollment: aster_enrollment_t,
    request_json: aster_bytes_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    attempt!(prepare(out_json));
    let bridge = attempt!(bridge_for(runtime));
    let state = attempt!(enrollment_state(&bridge, enrollment));
    let issuer = attempt!(issuer_of(&state));
    let request: IssueJson = attempt!(parse_json(
        attempt!(in_slice(&request_json, "request_json")),
        "issue request"
    ));
    let subject = attempt!(hex32(request.subject.trim(), "subject").map_err(invalid));
    match issuer.issue_for(&subject, request.claims, request.registration) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_json_round_trips_the_resolved_config() {
        let text = r#"
[trust]
root_pubkey = "1111111111111111111111111111111111111111111111111111111111111111"
next_root_commitment = "2222222222222222222222222222222222222222222222222222222222222222"
root_seq = 2
enrollment_credential = "node.cred"

[enrollment]
audience = "mc"
fleet_namespace = "33"
replicas = ["aa"]
policy = { all_of = [{ scope = "ops" }, "tolerate_stale_policy"] }
renew = false

[enrollment.limits]
sync_timeout_ms = 2000

[enrollment.roles]
attribute = ""
[enrollment.roles.tags]
admin = "ops.admin, ops.logs"

[enrollment.issuer]
delegation_file = "issuer.chain"
lifetime = "6h"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        std::fs::write(&path, text).unwrap();
        let config = AsterConfig::from_file(&path).unwrap();
        let json = TrustJson::from_config(&config);
        assert_eq!(json.enrollment.roles.attribute.as_deref(), Some(""));
        assert_eq!(json.enrollment.issuer_lifetime_secs, Some(6 * 3600));
        let encoded = serde_json::to_string(&json).unwrap();
        let decoded: TrustJson = serde_json::from_str(&encoded).unwrap();
        let mut back = AsterConfig::default();
        decoded.apply(&mut back).unwrap();
        assert_eq!(back.enrollment, config.enrollment);
        assert_eq!(back.root_pubkey, config.root_pubkey);
        assert_eq!(back.root_seq, 2);
        assert!(back.is_enrollment_enabled());

        // Absent role settings keep the defaults; unknown fields are refused.
        let mut minimal = AsterConfig::default();
        serde_json::from_str::<TrustJson>(r#"{"enrollment": {"audience": "mc"}}"#)
            .unwrap()
            .apply(&mut minimal)
            .unwrap();
        assert_eq!(minimal.enrollment.roles, RoleMappingConfig::default());
        assert!(serde_json::from_str::<TrustJson>(r#"{"allow_all_consumers": true}"#).is_err());
    }
}
