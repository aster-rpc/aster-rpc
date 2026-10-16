//! Gate 0 (connection-level peer admission) over the Rust implementation.
//!
//! Python shape (plan P3):
//!
//! ```python
//! gate = GatePolicy()                      # PROTECTED, default admission ALPNs
//! node = await Node.start(config, gate=gate)
//! gate.admit(peer_id)                      # after a credential check
//!
//! # or decide yourself:
//! node = await Node.start(AsterConfig(enable_hooks=True))
//! async for req in node.admission():
//!     if ok(req.peer, req.alpn):
//!         req.accept()
//!     else:
//!         req.reject(403, b"no")
//! ```
//!
//! `GatePolicy`, `PeerAdmissionStore` and `TrustMode` wrap the transport-core
//! types the `aster` crate re-exports; `Admission` wraps the node's
//! `aster::Admission` receiver. With a gate attached, every inbound decision
//! is made in Rust and nothing reaches Python.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pyo3::exceptions::{PyRuntimeError, PyStopAsyncIteration, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::tokio::future_into_py;
use tokio::sync::{mpsc, oneshot, Mutex};

use aster::{AdmissionSource, GateDecision, GatePolicy as CoreGatePolicy};
use aster_transport_core::{
    CoreAfterHandshakeDecision, CoreHookConnectInfo, CoreHookHandshakeInfo, CoreHookReceiver,
    CoreNetClient,
};

use crate::ids::{NodeId, NodeIdArg};

// ============================================================================
// TrustMode
// ============================================================================

/// Gate-0 trust posture.
///
/// `PROTECTED`: un-admitted peers reach only the admission ALPNs.
/// `OPEN_DEV`: every peer reaches every ALPN (local / dev only).
#[pyclass(eq, eq_int, frozen, from_py_object, module = "aster._aster")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrustMode {
    #[pyo3(name = "OPEN_DEV")]
    OpenDev,
    #[pyo3(name = "PROTECTED")]
    Protected,
}

impl From<TrustMode> for aster::TrustMode {
    fn from(m: TrustMode) -> Self {
        match m {
            TrustMode::OpenDev => aster::TrustMode::OpenDev,
            TrustMode::Protected => aster::TrustMode::Protected,
        }
    }
}

impl From<aster::TrustMode> for TrustMode {
    fn from(m: aster::TrustMode) -> Self {
        match m {
            aster::TrustMode::OpenDev => TrustMode::OpenDev,
            aster::TrustMode::Protected => TrustMode::Protected,
        }
    }
}

// ============================================================================
// PeerAdmission
// ============================================================================

fn epoch_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn from_epoch_secs(secs: f64, what: &str) -> PyResult<SystemTime> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(PyValueError::new_err(format!(
            "{what} must be a non-negative epoch time in seconds"
        )));
    }
    Ok(UNIX_EPOCH + Duration::from_secs_f64(secs))
}

fn source_to_str(s: &AdmissionSource) -> String {
    match s {
        AdmissionSource::Manual => "manual".into(),
        AdmissionSource::Credential => "credential".into(),
        AdmissionSource::Attestation => "attestation".into(),
        AdmissionSource::RootPolicyDoc => "root_policy_doc".into(),
        AdmissionSource::WorkloadIdentity => "workload_identity".into(),
        AdmissionSource::Custom(c) => c.clone(),
    }
}

fn source_from_str(s: &str) -> AdmissionSource {
    match s {
        "manual" => AdmissionSource::Manual,
        "credential" => AdmissionSource::Credential,
        "attestation" => AdmissionSource::Attestation,
        "root_policy_doc" => AdmissionSource::RootPolicyDoc,
        "workload_identity" => AdmissionSource::WorkloadIdentity,
        other => AdmissionSource::Custom(other.to_string()),
    }
}

/// A stored peer admission (read-only snapshot).
///
/// Times are epoch seconds (floats). `source` is one of `"manual"`,
/// `"credential"`, `"attestation"`, `"root_policy_doc"`,
/// `"workload_identity"`, or an application-defined string.
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
#[derive(Clone)]
pub struct PeerAdmission {
    inner: aster::PeerAdmission,
}

#[pymethods]
impl PeerAdmission {
    #[getter]
    fn peer(&self) -> NodeId {
        NodeId::from_trusted_hex(self.inner.endpoint_id.clone())
    }

    #[getter]
    fn owner_id(&self) -> Option<String> {
        self.inner.owner_id.clone()
    }

    #[getter]
    fn attributes(&self) -> HashMap<String, String> {
        self.inner.attributes.clone()
    }

    #[getter]
    fn admitted_at(&self) -> f64 {
        epoch_secs(self.inner.admitted_at)
    }

    #[getter]
    fn expires_at(&self) -> Option<f64> {
        self.inner.expires_at.map(epoch_secs)
    }

    #[getter]
    fn source(&self) -> String {
        source_to_str(&self.inner.source)
    }

    #[getter]
    fn proof_hash<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.inner.proof_hash.as_ref().map(|h| PyBytes::new(py, h))
    }

    #[getter]
    fn policy_epoch(&self) -> Option<u64> {
        self.inner.policy_epoch
    }

    fn __repr__(&self) -> String {
        format!(
            "PeerAdmission(peer='{}', source='{}', expires_at={:?})",
            self.inner.endpoint_id,
            source_to_str(&self.inner.source),
            self.inner.expires_at.map(epoch_secs)
        )
    }
}

/// Build a core admission record from the keyword arguments shared by
/// `PeerAdmissionStore.admit` and `GatePolicy.admit`.
#[allow(clippy::too_many_arguments)]
fn build_admission(
    peer: String,
    attributes: Option<HashMap<String, String>>,
    expires_at: Option<f64>,
    ttl: Option<f64>,
    source: &str,
    owner_id: Option<String>,
    proof_hash: Option<Vec<u8>>,
    policy_epoch: Option<u64>,
) -> PyResult<aster::PeerAdmission> {
    if expires_at.is_some() && ttl.is_some() {
        return Err(PyValueError::new_err(
            "pass at most one of expires_at and ttl",
        ));
    }
    let mut a = aster::PeerAdmission::from_source(peer, source_from_str(source));
    if let Some(attrs) = attributes {
        a = a.with_attributes(attrs);
    }
    if let Some(e) = expires_at {
        a = a.with_expires_at(from_epoch_secs(e, "expires_at")?);
    }
    if let Some(t) = ttl {
        if !t.is_finite() || t < 0.0 {
            return Err(PyValueError::new_err(
                "ttl must be a non-negative number of seconds",
            ));
        }
        a = a.with_ttl(Duration::from_secs_f64(t));
    }
    if let Some(o) = owner_id {
        a = a.with_owner_id(o);
    }
    if let Some(h) = proof_hash {
        a = a.with_proof_hash(h);
    }
    if let Some(e) = policy_epoch {
        a = a.with_policy_epoch(e);
    }
    Ok(a)
}

fn wrap(a: Option<aster::PeerAdmission>) -> Option<PeerAdmission> {
    a.map(|inner| PeerAdmission { inner })
}

// ============================================================================
// PeerAdmissionStore
// ============================================================================

/// Shared, expiry-aware store of admitted peers (the Rust
/// `PeerAdmissionStore`). Copies share state: a store taken from a
/// `GatePolicy` is that gate's live store.
#[pyclass(frozen, from_py_object, module = "aster._aster")]
#[derive(Clone)]
pub struct PeerAdmissionStore {
    pub(crate) inner: aster::PeerAdmissionStore,
}

#[pymethods]
impl PeerAdmissionStore {
    #[new]
    fn new() -> Self {
        Self {
            inner: aster::PeerAdmissionStore::new(),
        }
    }

    /// Admit `peer` (a `NodeId` or 64-hex string). Returns the admission it
    /// replaced, if any. `expires_at` (epoch seconds) or `ttl` (seconds from
    /// now) bound it; expired admissions are ignored by every lookup.
    #[pyo3(signature = (peer, *, attributes=None, expires_at=None, ttl=None, source="manual", owner_id=None, proof_hash=None, policy_epoch=None))]
    #[allow(clippy::too_many_arguments)]
    fn admit(
        &self,
        peer: NodeIdArg,
        attributes: Option<HashMap<String, String>>,
        expires_at: Option<f64>,
        ttl: Option<f64>,
        source: &str,
        owner_id: Option<String>,
        proof_hash: Option<Vec<u8>>,
        policy_epoch: Option<u64>,
    ) -> PyResult<Option<PeerAdmission>> {
        let a = build_admission(
            peer.0,
            attributes,
            expires_at,
            ttl,
            source,
            owner_id,
            proof_hash,
            policy_epoch,
        )?;
        Ok(wrap(self.inner.admit(a)))
    }

    /// Revoke `peer`; returns the removed admission, if any. Open connections
    /// are not closed; new ones are gated.
    fn revoke(&self, peer: NodeIdArg) -> Option<PeerAdmission> {
        wrap(self.inner.revoke(&peer.0))
    }

    /// The unexpired admission for `peer`, or `None`.
    fn get(&self, peer: NodeIdArg) -> Option<PeerAdmission> {
        wrap(self.inner.get(&peer.0))
    }

    /// Whether `peer` holds an unexpired admission.
    fn is_admitted(&self, peer: NodeIdArg) -> bool {
        self.inner.is_admitted(&peer.0)
    }

    /// Admission attributes for an unexpired peer (empty if none).
    fn attributes(&self, peer: NodeIdArg) -> HashMap<String, String> {
        self.inner.attributes(&peer.0)
    }

    /// Remove expired records; returns how many were removed.
    fn prune_expired(&self) -> usize {
        self.inner.prune_expired()
    }

    fn __contains__(&self, peer: NodeIdArg) -> bool {
        self.inner.is_admitted(&peer.0)
    }

    /// Raw record count, including expired records not yet pruned.
    fn __len__(&self) -> usize {
        self.inner.len()
    }
}

// ============================================================================
// GatePolicy
// ============================================================================

/// The Gate-0 policy (the Rust `GatePolicy`).
///
/// Admission ALPNs are always reachable, so an unknown peer can present a
/// credential; in `PROTECTED` mode every other ALPN needs an unexpired
/// admission. Rejected peers are closed with `reject_code` / `reject_reason`.
///
/// Attach it with `Node.start(config, gate=policy)`; setters and admissions
/// take effect on the next connection.
#[pyclass(frozen, module = "aster._aster")]
pub struct GatePolicy {
    pub(crate) shared: Arc<RwLock<CoreGatePolicy>>,
    reject: RwLock<(u32, Vec<u8>)>,
}

impl GatePolicy {
    fn read(&self) -> CoreGatePolicy {
        self.shared
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn update(&self, f: impl FnOnce(CoreGatePolicy) -> CoreGatePolicy) {
        let mut g = self.shared.write().unwrap_or_else(|e| e.into_inner());
        *g = f(g.clone());
    }
}

#[pymethods]
impl GatePolicy {
    #[new]
    #[pyo3(signature = (mode=TrustMode::Protected, *, store=None, admission_alpns=None, reject_code=403, reject_reason=b"peer not admitted".to_vec()))]
    fn new(
        mode: TrustMode,
        store: Option<PeerAdmissionStore>,
        admission_alpns: Option<Vec<Vec<u8>>>,
        reject_code: u32,
        reject_reason: Vec<u8>,
    ) -> PyResult<Self> {
        let mut p = CoreGatePolicy::with_mode(mode.into());
        if let Some(s) = store {
            p = p.with_store(s.inner);
        }
        if let Some(alpns) = admission_alpns {
            p = p.with_admission_alpns(check_alpns(alpns)?);
        }
        p = p.with_reject(reject_code, reject_reason.clone());
        Ok(Self {
            shared: Arc::new(RwLock::new(p)),
            reject: RwLock::new((reject_code, reject_reason)),
        })
    }

    /// A `PROTECTED` policy with the defaults.
    #[staticmethod]
    fn protected() -> PyResult<Self> {
        Self::new(
            TrustMode::Protected,
            None,
            None,
            403,
            b"peer not admitted".to_vec(),
        )
    }

    /// An `OPEN_DEV` policy: every peer reaches every ALPN.
    #[staticmethod]
    fn open_dev() -> PyResult<Self> {
        Self::new(
            TrustMode::OpenDev,
            None,
            None,
            403,
            b"peer not admitted".to_vec(),
        )
    }

    #[getter]
    fn get_trust_mode(&self) -> TrustMode {
        self.read().mode().into()
    }

    #[setter]
    fn set_trust_mode(&self, mode: TrustMode) {
        self.update(|p| p.set_mode(mode.into()));
    }

    /// The always-open admission ALPNs.
    #[getter]
    fn get_admission_alpns<'py>(&self, py: Python<'py>) -> Vec<Bound<'py, PyBytes>> {
        self.read()
            .admission_alpns()
            .iter()
            .map(|a| PyBytes::new(py, a))
            .collect()
    }

    #[setter]
    fn set_admission_alpns(&self, alpns: Vec<Vec<u8>>) -> PyResult<()> {
        let alpns = check_alpns(alpns)?;
        self.update(|p| p.with_admission_alpns(alpns));
        Ok(())
    }

    #[getter]
    fn reject_code(&self) -> u32 {
        self.reject.read().unwrap_or_else(|e| e.into_inner()).0
    }

    #[getter]
    fn reject_reason<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let g = self.reject.read().unwrap_or_else(|e| e.into_inner());
        PyBytes::new(py, &g.1)
    }

    /// Set the QUIC close code and reason sent to rejected peers.
    fn set_reject(&self, code: u32, reason: Vec<u8>) {
        *self.reject.write().unwrap_or_else(|e| e.into_inner()) = (code, reason.clone());
        self.update(|p| p.with_reject(code, reason));
    }

    /// The live admitted-peer store.
    #[getter]
    fn store(&self) -> PeerAdmissionStore {
        PeerAdmissionStore {
            inner: self.read().store(),
        }
    }

    /// Admit `peer`; see `PeerAdmissionStore.admit`.
    #[pyo3(signature = (peer, *, attributes=None, expires_at=None, ttl=None, source="manual", owner_id=None, proof_hash=None, policy_epoch=None))]
    #[allow(clippy::too_many_arguments)]
    fn admit(
        &self,
        peer: NodeIdArg,
        attributes: Option<HashMap<String, String>>,
        expires_at: Option<f64>,
        ttl: Option<f64>,
        source: &str,
        owner_id: Option<String>,
        proof_hash: Option<Vec<u8>>,
        policy_epoch: Option<u64>,
    ) -> PyResult<Option<PeerAdmission>> {
        let a = build_admission(
            peer.0,
            attributes,
            expires_at,
            ttl,
            source,
            owner_id,
            proof_hash,
            policy_epoch,
        )?;
        Ok(wrap(self.read().admit(a)))
    }

    /// Revoke `peer`; returns the removed admission, if any.
    fn revoke(&self, peer: NodeIdArg) -> Option<PeerAdmission> {
        wrap(self.read().revoke(&peer.0))
    }

    /// Whether `peer` holds an unexpired admission.
    fn is_admitted(&self, peer: NodeIdArg) -> bool {
        self.read().is_admitted(&peer.0)
    }

    /// Admission attributes for an unexpired peer (empty if none).
    fn attributes(&self, peer: NodeIdArg) -> HashMap<String, String> {
        self.read().attributes(&peer.0)
    }

    /// The Gate-0 decision for `(peer, alpn)`, without connecting.
    fn should_allow(&self, peer: NodeIdArg, alpn: Vec<u8>) -> bool {
        self.read().should_allow(&peer.0, &alpn).is_allowed()
    }

    fn __repr__(&self) -> String {
        format!("GatePolicy(mode={:?})", TrustMode::from(self.read().mode()))
    }
}

fn check_alpns(alpns: Vec<Vec<u8>>) -> PyResult<Vec<Vec<u8>>> {
    if alpns.iter().any(|a| a.is_empty()) {
        return Err(PyValueError::new_err("admission ALPNs must be non-empty"));
    }
    Ok(alpns)
}

/// Decide every admission with `gate` in Rust: inbound peers at the
/// handshake, and this node's own dials at `before_connect` (outbound
/// handshakes bypass `after_handshake`; the dialed id is what TLS verifies).
/// Ends when the node closes.
pub(crate) fn spawn_gate_drainer(receiver: CoreHookReceiver, gate: Arc<RwLock<CoreGatePolicy>>) {
    let CoreHookReceiver {
        mut before_connect_rx,
        mut after_handshake_rx,
        ..
    } = receiver;
    let dial_gate = Arc::clone(&gate);
    pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
        while let Some((info, reply)) = before_connect_rx.recv().await {
            let allowed = dial_gate
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .should_allow(&info.remote_endpoint_id, &info.alpn)
                .is_allowed();
            let _ = reply.send(allowed);
        }
    });
    pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
        while let Some((info, reply)) = after_handshake_rx.recv().await {
            let decision = gate
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .should_allow(&info.remote_endpoint_id, &info.alpn);
            let _ = reply.send(match decision {
                GateDecision::Allow => CoreAfterHandshakeDecision::Accept,
                GateDecision::Reject { error_code, reason } => {
                    CoreAfterHandshakeDecision::Reject { error_code, reason }
                }
            });
        }
    });
}

// ============================================================================
// Admission requests
// ============================================================================

type HandshakeReply = oneshot::Sender<CoreAfterHandshakeDecision>;
type ConnectReply = oneshot::Sender<bool>;

fn take<T>(slot: &std::sync::Mutex<Option<T>>) -> PyResult<T> {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .ok_or_else(|| PyRuntimeError::new_err("admission request already answered"))
}

/// An inbound peer, after the QUIC handshake (its id is verified). Answer
/// with `accept()` or `reject(code, reason)`; an unanswered request falls
/// back to the node's `hook_failure_mode` after `hook_timeout_ms`.
#[pyclass(frozen, module = "aster._aster")]
pub struct HandshakeRequest {
    peer: String,
    alpn: Vec<u8>,
    is_alive: bool,
    reply: std::sync::Mutex<Option<HandshakeReply>>,
}

#[pymethods]
impl HandshakeRequest {
    /// Always `"handshake"`.
    #[getter]
    fn kind(&self) -> &'static str {
        "handshake"
    }

    #[getter]
    fn peer(&self) -> NodeId {
        NodeId::from_trusted_hex(self.peer.clone())
    }

    #[getter]
    fn alpn<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.alpn)
    }

    /// Whether the connection was still alive when the request was raised.
    #[getter]
    fn is_alive(&self) -> bool {
        self.is_alive
    }

    /// Admit the connection.
    fn accept(&self) -> PyResult<()> {
        let _ = take(&self.reply)?.send(CoreAfterHandshakeDecision::Accept);
        Ok(())
    }

    /// Reject, closing the connection with QUIC application `code` and `reason`.
    #[pyo3(signature = (code=403, reason=b"peer not admitted".to_vec()))]
    fn reject(&self, code: u32, reason: Vec<u8>) -> PyResult<()> {
        let _ = take(&self.reply)?.send(CoreAfterHandshakeDecision::Reject {
            error_code: code,
            reason,
        });
        Ok(())
    }

    fn __repr__(&self) -> String {
        format!(
            "HandshakeRequest(peer='{}', alpn={:?})",
            self.peer,
            String::from_utf8_lossy(&self.alpn)
        )
    }
}

/// An outbound dial by this node (only yielded by `node.admission(outbound=True)`).
#[pyclass(frozen, module = "aster._aster")]
pub struct ConnectRequest {
    peer: String,
    alpn: Vec<u8>,
    reply: std::sync::Mutex<Option<ConnectReply>>,
}

#[pymethods]
impl ConnectRequest {
    /// Always `"connect"`.
    #[getter]
    fn kind(&self) -> &'static str {
        "connect"
    }

    #[getter]
    fn peer(&self) -> NodeId {
        NodeId::from_trusted_hex(self.peer.clone())
    }

    #[getter]
    fn alpn<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.alpn)
    }

    /// Let the dial proceed.
    fn accept(&self) -> PyResult<()> {
        let _ = take(&self.reply)?.send(true);
        Ok(())
    }

    /// Block the dial. Outbound dials carry no close code, so `code` and
    /// `reason` are accepted for symmetry and ignored.
    #[pyo3(signature = (code=0, reason=Vec::new()))]
    fn reject(&self, code: u32, reason: Vec<u8>) -> PyResult<()> {
        let _ = (code, reason);
        let _ = take(&self.reply)?.send(false);
        Ok(())
    }

    fn __repr__(&self) -> String {
        format!(
            "ConnectRequest(peer='{}', alpn={:?})",
            self.peer,
            String::from_utf8_lossy(&self.alpn)
        )
    }
}

enum Request {
    Connect(CoreHookConnectInfo, ConnectReply),
    Handshake(CoreHookHandshakeInfo, HandshakeReply),
}

struct Channels {
    before: Option<mpsc::Receiver<(CoreHookConnectInfo, ConnectReply)>>,
    after: mpsc::Receiver<(CoreHookHandshakeInfo, HandshakeReply)>,
    /// The node's endpoint: the hook channels outlive a closed endpoint, so
    /// its close is what ends the iterator.
    net: CoreNetClient,
}

impl Channels {
    async fn next(&mut self) -> Option<Request> {
        loop {
            let net = self.net.clone();
            let before = async {
                match self.before.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                r = self.after.recv() => {
                    return r.map(|(i, tx)| Request::Handshake(i, tx));
                }
                r = before => match r {
                    Some((i, tx)) => return Some(Request::Connect(i, tx)),
                    // Outbound side closed: keep serving inbound requests.
                    None => self.before = None,
                },
                _ = net.closed() => return None,
            }
        }
    }
}

/// The node's admission requests, as an async iterator (`async for req in
/// node.admission()`). Yields `HandshakeRequest` for inbound peers and, with
/// `outbound=True`, `ConnectRequest` for this node's own dials. Ends when the
/// node closes.
#[pyclass(frozen, module = "aster._aster")]
pub struct Admission {
    rx: Arc<Mutex<Channels>>,
}

impl Admission {
    pub(crate) fn new(receiver: CoreHookReceiver, net: CoreNetClient, outbound: bool) -> Self {
        let CoreHookReceiver {
            mut before_connect_rx,
            after_handshake_rx,
            ..
        } = receiver;
        let before = if outbound {
            Some(before_connect_rx)
        } else {
            // The host loop asked for inbound requests only, so let this
            // node's own dials go. (A dialed id is TLS-verified, so a host
            // that wants to gate its dials passes outbound=True.)
            pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                while let Some((_info, reply)) = before_connect_rx.recv().await {
                    let _ = reply.send(true);
                }
            });
            None
        };
        Self {
            rx: Arc::new(Mutex::new(Channels {
                before,
                after: after_handshake_rx,
                net,
            })),
        }
    }
}

fn request_to_py(py: Python<'_>, req: Request) -> PyResult<Py<PyAny>> {
    Ok(match req {
        Request::Connect(info, reply) => Py::new(
            py,
            ConnectRequest {
                peer: info.remote_endpoint_id.to_ascii_lowercase(),
                alpn: info.alpn,
                reply: std::sync::Mutex::new(Some(reply)),
            },
        )?
        .into_any(),
        Request::Handshake(info, reply) => Py::new(
            py,
            HandshakeRequest {
                peer: info.remote_endpoint_id.to_ascii_lowercase(),
                alpn: info.alpn,
                is_alive: info.is_alive,
                reply: std::sync::Mutex::new(Some(reply)),
            },
        )?
        .into_any(),
    })
}

#[pymethods]
impl Admission {
    /// Await the next request, or `None` once the node has closed.
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.rx.clone();
        future_into_py(py, async move {
            let next = rx.lock().await.next().await;
            Python::attach(|py| match next {
                Some(req) => request_to_py(py, req),
                None => Ok(py.None()),
            })
        })
    }

    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.rx.clone();
        future_into_py(py, async move {
            let next = rx.lock().await.next().await;
            match next {
                Some(req) => Python::attach(|py| request_to_py(py, req)),
                None => Err(PyStopAsyncIteration::new_err(())),
            }
        })
    }
}

/// Register the Gate-0 types and the well-known admission ALPNs.
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<TrustMode>()?;
    m.add_class::<PeerAdmission>()?;
    m.add_class::<PeerAdmissionStore>()?;
    m.add_class::<GatePolicy>()?;
    m.add_class::<Admission>()?;
    m.add_class::<HandshakeRequest>()?;
    m.add_class::<ConnectRequest>()?;
    let py = m.py();
    m.add(
        "ALPN_PRODUCER_ADMISSION",
        PyBytes::new(py, aster::alpns::PRODUCER_ADMISSION),
    )?;
    m.add(
        "ALPN_DELEGATED_ADMISSION",
        PyBytes::new(py, aster::alpns::DELEGATED_ADMISSION),
    )?;
    Ok(())
}
