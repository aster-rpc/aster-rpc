//! Gate 0 — connection-level peer admission, decided in Rust (plan T3).
//!
//! Wraps the `aster` crate's [`GatePolicy`] (trust mode, always-open
//! admission ALPNs, reject code/reason) and [`PeerAdmissionStore`] (admitted
//! peers with attributes and expiry). A node started with a gate
//! (`Node.start(config, gate)` / `node.setGate(gate)`) decides inbound
//! admission without calling into JavaScript; a node started with
//! `enableHooks` and no gate hands each decision to `node.admission()`, an
//! `AsyncIterable` of [`AdmissionRequest`]s answered with `accept()` /
//! `reject(code, reason)` — or, instead of the iterator, by upcalls
//! (`node.serveAdmissionUpcalls(upcalls)`, kinds 64 / 65: one queue and one
//! respond path shared with the other JavaScript extension points; the
//! upcall policy's failure mode decides when JavaScript gives no answer).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::sync::{mpsc, oneshot};

use aster::binding_support::core::{
    CoreAfterHandshakeDecision, CoreHookConnectInfo, CoreHookHandshakeInfo,
};
use aster::binding_support::upcall::{self as up, codec as ucodec, UpcallKind};
use aster::binding_support::CoreHookReceiver;
use aster::{
    AdmissionSource, GateDecision as RsGateDecision, GatePolicy as RsGatePolicy,
    PeerAdmission as RsPeerAdmission, PeerAdmissionStore as RsPeerAdmissionStore,
    TrustMode as RsTrustMode,
};

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

/// Validate a peer id: 64 hex characters (case-insensitive), lowercased.
pub(crate) fn peer_id(value: &str) -> Result<String> {
    let v = value.trim();
    if v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "peer must be a 64-character hex node id, got {value:?}"
        )));
    }
    Ok(v.to_ascii_lowercase())
}

fn alpn_bytes(alpn: Either<String, Uint8Array>) -> Vec<u8> {
    match alpn {
        Either::A(s) => s.into_bytes(),
        Either::B(b) => b.to_vec(),
    }
}

fn to_ms(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn from_ms(field: &str, ms: f64) -> Result<SystemTime> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(invalid(format!(
            "{field} must be a non-negative number of ms"
        )));
    }
    Ok(UNIX_EPOCH + Duration::from_millis(ms as u64))
}

fn trust_mode_in(mode: &str) -> Result<RsTrustMode> {
    match mode {
        "protected" => Ok(RsTrustMode::Protected),
        "open_dev" => Ok(RsTrustMode::OpenDev),
        other => Err(invalid(format!(
            "unknown trust mode {other:?} (expected 'protected' or 'open_dev')"
        ))),
    }
}

fn trust_mode_out(mode: RsTrustMode) -> String {
    match mode {
        RsTrustMode::Protected => "protected".into(),
        RsTrustMode::OpenDev => "open_dev".into(),
    }
}

fn source_in(source: &str) -> AdmissionSource {
    match source {
        "manual" => AdmissionSource::Manual,
        "credential" => AdmissionSource::Credential,
        "attestation" => AdmissionSource::Attestation,
        "root_policy_doc" => AdmissionSource::RootPolicyDoc,
        "workload_identity" => AdmissionSource::WorkloadIdentity,
        other => {
            AdmissionSource::Custom(other.strip_prefix("custom:").unwrap_or(other).to_string())
        }
    }
}

fn source_out(source: &AdmissionSource) -> String {
    match source {
        AdmissionSource::Manual => "manual".into(),
        AdmissionSource::Credential => "credential".into(),
        AdmissionSource::Attestation => "attestation".into(),
        AdmissionSource::RootPolicyDoc => "root_policy_doc".into(),
        AdmissionSource::WorkloadIdentity => "workload_identity".into(),
        AdmissionSource::Custom(name) => format!("custom:{name}"),
    }
}

// ── PeerAdmissionStore ──────────────────────────────────────────────────────

/// Options for `admit(peer, options)`.
#[napi(object)]
#[derive(Default)]
pub struct AdmitOptions {
    /// Attributes later RPC / capability checks read (e.g. `aster.role`).
    pub attributes: Option<HashMap<String, String>>,
    /// Absolute expiry, ms since the Unix epoch. Expired admissions are
    /// ignored by lookups and by Gate 0.
    pub expires_at_ms: Option<f64>,
    /// Expiry relative to now, in ms (the earlier of this and
    /// `expiresAtMs` wins when both are set).
    pub ttl_ms: Option<f64>,
    /// Root / owner id responsible for this peer.
    pub owner_id: Option<String>,
    /// Where the admission came from: `'manual'` (default), `'credential'`,
    /// `'attestation'`, `'root_policy_doc'`, `'workload_identity'`, or any
    /// other string for an application-defined source (`'custom:<name>'`).
    #[napi(
        ts_type = "'manual' | 'credential' | 'attestation' | 'root_policy_doc' | 'workload_identity' | (string & {})"
    )]
    pub source: Option<String>,
}

/// A stored admission record (an unexpired one, from `get`).
#[napi(object)]
pub struct PeerAdmission {
    /// The admitted peer's node id.
    pub peer: String,
    pub owner_id: Option<String>,
    pub attributes: HashMap<String, String>,
    /// When the peer was admitted (ms since the Unix epoch).
    pub admitted_at_ms: f64,
    /// Expiry (ms since the Unix epoch), or `null` for none.
    pub expires_at_ms: Option<f64>,
    pub source: String,
}

impl From<RsPeerAdmission> for PeerAdmission {
    fn from(a: RsPeerAdmission) -> Self {
        Self {
            peer: a.endpoint_id,
            owner_id: a.owner_id,
            attributes: a.attributes,
            admitted_at_ms: to_ms(a.admitted_at),
            expires_at_ms: a.expires_at.map(to_ms),
            source: source_out(&a.source),
        }
    }
}

fn build_admission(peer: &str, options: Option<AdmitOptions>) -> Result<RsPeerAdmission> {
    let peer = peer_id(peer)?;
    let o = options.unwrap_or_default();
    let mut a = RsPeerAdmission::from_source(
        peer,
        o.source
            .as_deref()
            .map(source_in)
            .unwrap_or(AdmissionSource::Manual),
    );
    if let Some(owner) = o.owner_id {
        a = a.with_owner_id(owner);
    }
    if let Some(attrs) = o.attributes {
        a = a.with_attributes(attrs);
    }
    let mut expiry: Option<SystemTime> = None;
    if let Some(ms) = o.expires_at_ms {
        expiry = Some(from_ms("expiresAtMs", ms)?);
    }
    if let Some(ttl) = o.ttl_ms {
        if !ttl.is_finite() || ttl < 0.0 {
            return Err(invalid("ttlMs must be a non-negative number"));
        }
        let t = a.admitted_at + Duration::from_millis(ttl as u64);
        expiry = Some(expiry.map_or(t, |e| e.min(t)));
    }
    if let Some(e) = expiry {
        a = a.with_expires_at(e);
    }
    Ok(a)
}

/// Shared in-memory store of admitted peers (the Rust `PeerAdmissionStore`).
///
/// Clone-cheap and shared: the store of a {@link GatePolicy} (`gate.store`)
/// is the one the gate consults, so admitting a peer here opens Gate 0 to it.
#[napi]
pub struct PeerAdmissionStore {
    pub(crate) inner: RsPeerAdmissionStore,
}

#[napi]
impl PeerAdmissionStore {
    /// An empty store.
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            inner: RsPeerAdmissionStore::new(),
        }
    }

    /// Admit `peer` (64-hex node id), replacing any previous record.
    /// Returns whether a previous record was replaced.
    #[napi]
    pub fn admit(&self, peer: String, options: Option<AdmitOptions>) -> Result<bool> {
        Ok(self.inner.admit(build_admission(&peer, options)?).is_some())
    }

    /// Revoke `peer`'s admission. Returns whether a record was removed.
    /// Existing connections stay open; new ones are gated.
    #[napi]
    pub fn revoke(&self, peer: String) -> Result<bool> {
        Ok(self.inner.revoke(&peer_id(&peer)?).is_some())
    }

    /// Whether `peer` holds an unexpired admission.
    #[napi]
    pub fn is_admitted(&self, peer: String) -> Result<bool> {
        Ok(self.inner.is_admitted(&peer_id(&peer)?))
    }

    /// The unexpired admission record for `peer`, or `null`.
    #[napi]
    pub fn get(&self, peer: String) -> Result<Option<PeerAdmission>> {
        Ok(self.inner.get(&peer_id(&peer)?).map(PeerAdmission::from))
    }

    /// Attributes of an unexpired admission (empty when not admitted).
    #[napi]
    pub fn attributes(&self, peer: String) -> Result<HashMap<String, String>> {
        Ok(self.inner.attributes(&peer_id(&peer)?))
    }

    /// Raw record count, including expired records not yet pruned.
    #[napi(getter)]
    pub fn size(&self) -> u32 {
        self.inner.len() as u32
    }

    /// Remove expired records; returns how many were removed.
    #[napi]
    pub fn prune_expired(&self) -> u32 {
        self.inner.prune_expired() as u32
    }
}

impl Default for PeerAdmissionStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── GatePolicy ──────────────────────────────────────────────────────────────

/// The Gate-0 decision for a `(peer, alpn)` pair.
#[napi(object)]
pub struct GateDecision {
    pub allow: bool,
    /// QUIC application close code sent on reject.
    pub error_code: Option<u32>,
    /// Close reason sent on reject.
    pub reason: Option<String>,
}

/// Options for `new GatePolicy(options)`.
#[napi(object)]
#[derive(Default)]
pub struct GatePolicyOptions {
    /// `'protected'` (default): un-admitted peers may only use the admission
    /// ALPNs. `'open_dev'`: anyone may use any ALPN (local / dev only).
    #[napi(ts_type = "'protected' | 'open_dev'")]
    pub mode: Option<String>,
    /// ALPNs open to un-admitted peers. Default: `aster.producer_admission`,
    /// `aster.consumer_admission`, `aster.admission`.
    pub admission_alpns: Option<Vec<Either<String, Uint8Array>>>,
    /// Close code for rejected peers (default 403).
    pub reject_code: Option<u32>,
    /// Close reason for rejected peers (default `"peer not admitted"`).
    pub reject_reason: Option<String>,
}

/// Gate-0 policy (the Rust `GatePolicy`): admission ALPNs are open to
/// everyone; any other ALPN needs an admitted peer (or `'open_dev'` mode);
/// rejected peers are closed with the reject code and reason.
///
/// Attach it to a node (`Node.start(config, gate)` / `node.setGate(gate)`)
/// and inbound admission is decided in Rust. Changes (mode, ALPNs, reject,
/// admissions) apply to the next decision.
#[napi]
pub struct GatePolicy {
    pub(crate) inner: Arc<RwLock<RsGatePolicy>>,
}

impl GatePolicy {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, RsGatePolicy> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(RsGatePolicy) -> RsGatePolicy) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let current = guard.clone();
        *guard = f(current);
    }
}

#[napi]
impl GatePolicy {
    /// A gate (protected by default) over `store`, or over a fresh store.
    #[napi(constructor)]
    pub fn new(
        options: Option<GatePolicyOptions>,
        store: Option<&PeerAdmissionStore>,
    ) -> Result<Self> {
        let o = options.unwrap_or_default();
        let mode = match o.mode.as_deref() {
            Some(m) => trust_mode_in(m)?,
            None => RsTrustMode::Protected,
        };
        let mut policy = RsGatePolicy::with_mode(mode);
        if let Some(store) = store {
            policy = policy.with_store(store.inner.clone());
        }
        if let Some(alpns) = o.admission_alpns {
            policy = policy.with_admission_alpns(alpns.into_iter().map(alpn_bytes).collect());
        }
        if o.reject_code.is_some() || o.reject_reason.is_some() {
            policy = policy.with_reject(
                o.reject_code.unwrap_or(403),
                o.reject_reason
                    .unwrap_or_else(|| "peer not admitted".into())
                    .into_bytes(),
            );
        }
        Ok(Self {
            inner: Arc::new(RwLock::new(policy)),
        })
    }

    /// The trust mode.
    #[napi(getter, ts_return_type = "'protected' | 'open_dev'")]
    pub fn get_mode(&self) -> String {
        trust_mode_out(self.read().mode())
    }

    #[napi(setter, js_name = "mode")]
    pub fn set_mode(
        &self,
        #[napi(ts_arg_type = "'protected' | 'open_dev'")] mode: String,
    ) -> Result<()> {
        let mode = trust_mode_in(&mode)?;
        self.update(|p| p.set_mode(mode));
        Ok(())
    }

    /// The admitted-peer store this gate consults (shared, not a copy).
    #[napi(getter)]
    pub fn store(&self) -> PeerAdmissionStore {
        PeerAdmissionStore {
            inner: self.read().store(),
        }
    }

    /// ALPNs open to un-admitted peers (UTF-8, lossy).
    #[napi(getter)]
    pub fn get_admission_alpns(&self) -> Vec<String> {
        self.read()
            .admission_alpns()
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }

    #[napi(setter, js_name = "admissionAlpns")]
    pub fn set_admission_alpns(&self, alpns: Vec<Either<String, Uint8Array>>) {
        let list: Vec<Vec<u8>> = alpns.into_iter().map(alpn_bytes).collect();
        self.update(|p| p.with_admission_alpns(list));
    }

    /// Set the close code and reason sent to rejected peers.
    #[napi]
    pub fn set_reject(&self, code: u32, reason: String) {
        self.update(|p| p.with_reject(code, reason.into_bytes()));
    }

    /// Admit `peer` into this gate's store (see `PeerAdmissionStore.admit`).
    #[napi]
    pub fn admit(&self, peer: String, options: Option<AdmitOptions>) -> Result<bool> {
        Ok(self
            .read()
            .admit(build_admission(&peer, options)?)
            .is_some())
    }

    /// Revoke `peer`. Existing connections stay open; new ones are gated.
    #[napi]
    pub fn revoke(&self, peer: String) -> Result<bool> {
        Ok(self.read().revoke(&peer_id(&peer)?).is_some())
    }

    /// Whether `peer` holds an unexpired admission.
    #[napi]
    pub fn is_admitted(&self, peer: String) -> Result<bool> {
        Ok(self.read().is_admitted(&peer_id(&peer)?))
    }

    /// Evaluate the gate for `(peer, alpn)` without connecting.
    #[napi]
    pub fn should_allow(
        &self,
        peer: String,
        alpn: Either<String, Uint8Array>,
    ) -> Result<GateDecision> {
        let peer = peer_id(&peer)?;
        Ok(match self.read().should_allow(&peer, &alpn_bytes(alpn)) {
            RsGateDecision::Allow => GateDecision {
                allow: true,
                error_code: None,
                reason: None,
            },
            RsGateDecision::Reject { error_code, reason } => GateDecision {
                allow: false,
                error_code: Some(error_code),
                reason: Some(String::from_utf8_lossy(&reason).into_owned()),
            },
        })
    }
}

// ── Node admission plumbing ─────────────────────────────────────────────────

enum Reply {
    Inbound(oneshot::Sender<CoreAfterHandshakeDecision>),
    Outbound(oneshot::Sender<bool>),
}

impl Reply {
    fn is_closed(&self) -> bool {
        match self {
            Reply::Inbound(tx) => tx.is_closed(),
            Reply::Outbound(tx) => tx.is_closed(),
        }
    }
}

struct Pending {
    peer: String,
    alpn: Vec<u8>,
    is_alive: bool,
    reply: Reply,
}

/// Per-node admission state, shared with the hook drainers.
pub(crate) struct AdmissionHub {
    gate: RwLock<Option<Arc<RwLock<RsGatePolicy>>>>,
    listener: Mutex<Option<mpsc::UnboundedSender<Pending>>>,
    /// Answer by upcall instead of the iterator.
    upcalls: RwLock<Option<up::Upcalls>>,
    taken: AtomicBool,
}

/// Answer one admission request by upcall. Gives up (cancelling the upcall)
/// if the node stops waiting first.
async fn answer_by_upcall(upcalls: up::Upcalls, p: Pending) {
    match p.reply {
        Reply::Outbound(mut tx) => {
            let call = upcalls.call(
                UpcallKind::AdmissionConnect,
                ucodec::encode_admission_connect_request(&p.peer, &p.alpn),
            );
            let answer = tokio::select! {
                a = call => a,
                _ = tx.closed() => return,
            };
            let allow = upcalls.decide(
                UpcallKind::AdmissionConnect,
                answer,
                ucodec::decode_decision,
            );
            let _ = tx.send(allow);
        }
        Reply::Inbound(mut tx) => {
            let call = upcalls.call(
                UpcallKind::AdmissionHandshake,
                ucodec::encode_admission_handshake_request(&p.peer, &p.alpn, p.is_alive),
            );
            let answer = tokio::select! {
                a = call => a,
                _ = tx.closed() => return,
            };
            let _ = tx.send(match up::admission::handshake_decision(&upcalls, answer) {
                ucodec::HandshakeDecision::Accept => CoreAfterHandshakeDecision::Accept,
                ucodec::HandshakeDecision::Reject { code, reason } => {
                    CoreAfterHandshakeDecision::Reject {
                        error_code: code,
                        reason,
                    }
                }
            });
        }
    }
}

impl AdmissionHub {
    /// Take the node's hook receiver and start deciding: with a gate in Rust,
    /// otherwise through the `admission()` iterator once one is open. With
    /// neither, a request is dropped unanswered so the node's hook failure
    /// mode applies at once.
    pub(crate) fn spawn(receiver: CoreHookReceiver, gate: Option<&GatePolicy>) -> Arc<Self> {
        let hub = Arc::new(Self {
            gate: RwLock::new(gate.map(|g| g.inner.clone())),
            listener: Mutex::new(None),
            upcalls: RwLock::new(None),
            taken: AtomicBool::new(false),
        });
        let CoreHookReceiver {
            mut before_connect_rx,
            mut after_handshake_rx,
        } = receiver;

        let h = hub.clone();
        tokio::spawn(async move {
            while let Some((info, reply)) = before_connect_rx.recv().await {
                h.on_connect(info, reply);
            }
        });
        let h = hub.clone();
        tokio::spawn(async move {
            while let Some((info, reply)) = after_handshake_rx.recv().await {
                h.on_handshake(info, reply);
            }
        });
        hub
    }

    fn current_gate(&self) -> Option<Arc<RwLock<RsGatePolicy>>> {
        self.gate.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn set_gate(&self, gate: Option<&GatePolicy>) {
        *self.gate.write().unwrap_or_else(|e| e.into_inner()) = gate.map(|g| g.inner.clone());
    }

    pub(crate) fn has_gate(&self) -> bool {
        self.current_gate().is_some()
    }

    fn forward(&self, pending: Pending) {
        let upcalls = self
            .upcalls
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(upcalls) = upcalls {
            tokio::spawn(answer_by_upcall(upcalls, pending));
            return;
        }
        let mut guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = guard.as_ref() {
            if tx.send(pending).is_err() {
                // Iterator gone: stop forwarding; the request's reply is
                // dropped with it, so the failure mode applies.
                *guard = None;
            }
        }
    }

    fn on_connect(&self, info: CoreHookConnectInfo, reply: oneshot::Sender<bool>) {
        // Outbound dials are this node's own; a gate never blocks them.
        if self.has_gate() {
            let _ = reply.send(true);
            return;
        }
        self.forward(Pending {
            peer: info.remote_endpoint_id,
            alpn: info.alpn,
            is_alive: true,
            reply: Reply::Outbound(reply),
        });
    }

    fn on_handshake(
        &self,
        info: CoreHookHandshakeInfo,
        reply: oneshot::Sender<CoreAfterHandshakeDecision>,
    ) {
        if let Some(gate) = self.current_gate() {
            let decision = gate
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .should_allow(&info.remote_endpoint_id, &info.alpn);
            let _ = reply.send(match decision {
                RsGateDecision::Allow => CoreAfterHandshakeDecision::Accept,
                RsGateDecision::Reject { error_code, reason } => {
                    CoreAfterHandshakeDecision::Reject { error_code, reason }
                }
            });
            return;
        }
        self.forward(Pending {
            peer: info.remote_endpoint_id,
            alpn: info.alpn,
            is_alive: info.is_alive,
            reply: Reply::Inbound(reply),
        });
    }

    /// Answer admission by upcall from now on (instead of the iterator).
    pub(crate) fn serve_upcalls(&self, upcalls: up::Upcalls) -> Result<()> {
        if self.taken.swap(true, Ordering::SeqCst) {
            return Err(Error::from_reason(
                "admission is already being answered on this node (admission() or serveAdmissionUpcalls())",
            ));
        }
        *self.upcalls.write().unwrap_or_else(|e| e.into_inner()) = Some(upcalls);
        Ok(())
    }

    /// Open the (one) admission iterator.
    pub(crate) fn open(&self) -> Result<AdmissionRequests> {
        if self.taken.swap(true, Ordering::SeqCst) {
            return Err(Error::from_reason(
                "admission is already being answered on this node (admission() or serveAdmissionUpcalls(); one per node)",
            ));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *self.listener.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        Ok(AdmissionRequests {
            rx: Arc::new(tokio::sync::Mutex::new(Some(rx))),
        })
    }
}

/// The admission requests of a node with hooks and no gate: an
/// `AsyncIterable<AdmissionRequest>`. Ends when the node closes; `break`ing
/// out of the loop stops delivery (later requests get the hook failure mode).
#[napi(async_iterator)]
pub struct AdmissionRequests {
    rx: Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<Pending>>>>,
}

#[napi]
impl AsyncGenerator for AdmissionRequests {
    type Yield = AdmissionRequest;
    type Next = ();
    type Return = ();

    fn next(
        &mut self,
        _value: Option<Self::Next>,
    ) -> impl std::future::Future<Output = Result<Option<Self::Yield>>> + Send + 'static {
        let rx = self.rx.clone();
        async move {
            let mut guard = rx.lock().await;
            let Some(rx) = guard.as_mut() else {
                return Ok(None);
            };
            while let Some(p) = rx.recv().await {
                // Skip requests whose hook already timed out.
                if p.reply.is_closed() {
                    continue;
                }
                return Ok(Some(AdmissionRequest::from(p)));
            }
            *guard = None;
            Ok(None)
        }
    }

    fn complete(
        &mut self,
        _value: Option<Self::Return>,
    ) -> impl std::future::Future<Output = Result<Option<Self::Yield>>> + Send + 'static {
        let rx = self.rx.clone();
        async move {
            rx.lock().await.take();
            Ok(None)
        }
    }
}

#[napi]
impl AdmissionRequests {
    /// Stop receiving requests (as `break` in `for await` does).
    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.rx.lock().await.take();
        Ok(())
    }
}

/// One admission decision to make. Answer exactly once with `accept()` or
/// `reject(code, reason)`; an unanswered request gets the node's hook
/// failure mode after `hookTimeoutMs`.
#[napi]
pub struct AdmissionRequest {
    peer: String,
    alpn: Vec<u8>,
    is_alive: bool,
    inbound: bool,
    reply: Mutex<Option<Reply>>,
}

impl From<Pending> for AdmissionRequest {
    fn from(p: Pending) -> Self {
        Self {
            inbound: matches!(p.reply, Reply::Inbound(_)),
            peer: p.peer,
            alpn: p.alpn,
            is_alive: p.is_alive,
            reply: Mutex::new(Some(p.reply)),
        }
    }
}

impl AdmissionRequest {
    fn take(&self) -> Result<Reply> {
        self.reply
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| Error::from_reason("admission request already answered"))
    }
}

#[napi]
impl AdmissionRequest {
    /// The remote peer's node id.
    #[napi(getter)]
    pub fn peer(&self) -> String {
        self.peer.clone()
    }

    /// The negotiated ALPN (UTF-8, lossy).
    #[napi(getter)]
    pub fn alpn(&self) -> String {
        String::from_utf8_lossy(&self.alpn).into_owned()
    }

    /// The negotiated ALPN bytes.
    #[napi(getter)]
    pub fn alpn_bytes(&self) -> Buffer {
        Buffer::from(self.alpn.clone())
    }

    /// `'inbound'`: a peer connecting to this node (post-handshake).
    /// `'outbound'`: this node dialling a peer (code/reason are ignored).
    #[napi(getter, ts_return_type = "'inbound' | 'outbound'")]
    pub fn direction(&self) -> String {
        if self.inbound { "inbound" } else { "outbound" }.into()
    }

    /// Whether the connection was still alive when the request was raised.
    #[napi(getter)]
    pub fn is_alive(&self) -> bool {
        self.is_alive
    }

    /// Whether `accept` / `reject` has been called.
    #[napi(getter)]
    pub fn answered(&self) -> bool {
        self.reply
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    }

    /// Admit the connection.
    #[napi]
    pub fn accept(&self) -> Result<()> {
        match self.take()? {
            Reply::Inbound(tx) => {
                let _ = tx.send(CoreAfterHandshakeDecision::Accept);
            }
            Reply::Outbound(tx) => {
                let _ = tx.send(true);
            }
        }
        Ok(())
    }

    /// Reject the connection; an inbound peer sees QUIC close `code`
    /// (default 403) and `reason` (default `"peer not admitted"`).
    #[napi]
    pub fn reject(&self, code: Option<u32>, reason: Option<String>) -> Result<()> {
        match self.take()? {
            Reply::Inbound(tx) => {
                let _ = tx.send(CoreAfterHandshakeDecision::Reject {
                    error_code: code.unwrap_or(403),
                    reason: reason
                        .unwrap_or_else(|| "peer not admitted".into())
                        .into_bytes(),
                });
            }
            Reply::Outbound(tx) => {
                let _ = tx.send(false);
            }
        }
        Ok(())
    }
}
