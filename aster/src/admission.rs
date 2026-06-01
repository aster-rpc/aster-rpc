//! Peer admission (Gate 0).
//!
//! Obtain an [`Admission`] from [`Node::take_admission`](crate::Node::take_admission)
//! (the node must be started with [`hooks(true)`](crate::AsterConfigBuilder::hooks)).
//!
//! Inbound peers are admitted at the **post-handshake** stage — drive
//! [`Admission::next_handshake`] and `accept`/`reject` each request. The
//! pre-connect stage ([`Admission::next_connect`]) gates this node's own
//! **outbound** dials.

use crate::id::NodeId;
use aster_transport_core::{
    CoreAfterHandshakeDecision, CoreHookConnectInfo, CoreHookHandshakeInfo, CoreHookReceiver,
};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use tokio::sync::oneshot;

/// Well-known admission ALPNs — connections on these are let through to present
/// a credential, even from peers not yet admitted (see [`Gate0`]).
pub mod alpns {
    /// Producer (mesh-join) admission.
    pub const PRODUCER_ADMISSION: &[u8] = b"aster.producer_admission";
    /// Consumer admission.
    pub const CONSUMER_ADMISSION: &[u8] = b"aster.consumer_admission";
    /// Delegated (@aster-issued token) admission.
    pub const DELEGATED_ADMISSION: &[u8] = b"aster.admission";
}

/// Gate-0 admission policy: the rule an [`Admission`] handler applies per
/// inbound connection. **Admission ALPNs are always allowed** (so an unknown
/// peer can connect to present a credential); any other ALPN is allowed only
/// once the peer has been [`admit`](Gate0::admit)ted (e.g. after a successful
/// admission handshake).
///
/// Clone-cheap and internally shared, so the same `Gate0` can be held by both
/// the [`Admission::next_handshake`] loop (which calls [`should_allow`](Gate0::should_allow))
/// and the admission accept loop (which calls [`admit`](Gate0::admit)).
#[derive(Clone)]
pub struct Gate0 {
    admitted: Arc<RwLock<HashSet<NodeId>>>,
    admission_alpns: Arc<Vec<Vec<u8>>>,
    allow_unadmitted: bool,
}

impl Default for Gate0 {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate0 {
    /// A gate seeded with the well-known admission ALPNs ([`alpns`]).
    pub fn new() -> Self {
        Self::with_admission_alpns(vec![
            alpns::PRODUCER_ADMISSION.to_vec(),
            alpns::CONSUMER_ADMISSION.to_vec(),
            alpns::DELEGATED_ADMISSION.to_vec(),
        ])
    }

    /// A gate whose only always-open ALPNs are the ones given.
    pub fn with_admission_alpns(admission_alpns: Vec<Vec<u8>>) -> Self {
        Self {
            admitted: Arc::new(RwLock::new(HashSet::new())),
            admission_alpns: Arc::new(admission_alpns),
            allow_unadmitted: false,
        }
    }

    /// Allow connections from un-admitted peers on any ALPN (dev/local only).
    pub fn allow_unadmitted(mut self, yes: bool) -> Self {
        self.allow_unadmitted = yes;
        self
    }

    /// Mark a peer admitted (e.g. after verifying its attestation chain on an
    /// admission ALPN). Subsequent connections from it on normal ALPNs pass.
    pub fn admit(&self, peer: NodeId) {
        self.admitted.write().unwrap().insert(peer);
    }

    /// Revoke a peer's admission.
    pub fn revoke(&self, peer: &NodeId) {
        self.admitted.write().unwrap().remove(peer);
    }

    /// Whether `peer` is currently admitted.
    pub fn is_admitted(&self, peer: &NodeId) -> bool {
        self.admitted.read().unwrap().contains(peer)
    }

    /// The Gate-0 decision for a `(peer, alpn)` pair: admission ALPNs are always
    /// allowed; otherwise the peer must be admitted (or `allow_unadmitted`).
    pub fn should_allow(&self, peer: &NodeId, alpn: &[u8]) -> bool {
        if self.admission_alpns.iter().any(|a| a.as_slice() == alpn) {
            return true;
        }
        if self.is_admitted(peer) {
            return true;
        }
        self.allow_unadmitted
    }
}

/// Admission handle. Not clonable — there is a single decision stream per node.
pub struct Admission {
    rx: CoreHookReceiver,
}

impl Admission {
    pub(crate) fn new(rx: CoreHookReceiver) -> Self {
        Self { rx }
    }

    /// Await the next **outbound** connect attempt by this node, or `None` when
    /// the node is closing.
    pub async fn next_connect(&mut self) -> Option<ConnectRequest> {
        let (info, reply) = self.rx.before_connect_rx.recv().await?;
        Some(ConnectRequest::new(info, reply))
    }

    /// Await the next **inbound** post-handshake admission request, or `None`
    /// when the node is closing. This is the config-doc / Gate 0 boundary.
    pub async fn next_handshake(&mut self) -> Option<HandshakeRequest> {
        let (info, reply) = self.rx.after_handshake_rx.recv().await?;
        Some(HandshakeRequest::new(info, reply))
    }
}

/// A pending outbound-connect decision.
pub struct ConnectRequest {
    /// The peer being dialled.
    pub peer: NodeId,
    /// The ALPN of the attempt.
    pub alpn: Vec<u8>,
    reply: oneshot::Sender<bool>,
}

impl ConnectRequest {
    fn new(info: CoreHookConnectInfo, reply: oneshot::Sender<bool>) -> Self {
        Self {
            peer: NodeId::from_hex(info.remote_endpoint_id),
            alpn: info.alpn,
            reply,
        }
    }

    /// Allow the connection.
    pub fn accept(self) {
        let _ = self.reply.send(true);
    }

    /// Block the connection.
    pub fn reject(self) {
        let _ = self.reply.send(false);
    }
}

/// A pending inbound post-handshake admission decision.
pub struct HandshakeRequest {
    /// The remote peer.
    pub peer: NodeId,
    /// The negotiated ALPN.
    pub alpn: Vec<u8>,
    /// Whether the connection is still alive at decision time.
    pub is_alive: bool,
    reply: oneshot::Sender<CoreAfterHandshakeDecision>,
}

impl HandshakeRequest {
    fn new(
        info: CoreHookHandshakeInfo,
        reply: oneshot::Sender<CoreAfterHandshakeDecision>,
    ) -> Self {
        Self {
            peer: NodeId::from_hex(info.remote_endpoint_id),
            alpn: info.alpn,
            is_alive: info.is_alive,
            reply,
        }
    }

    /// Admit the peer.
    pub fn accept(self) {
        let _ = self.reply.send(CoreAfterHandshakeDecision::Accept);
    }

    /// Reject the peer, closing the connection with `code` and `reason`.
    pub fn reject(self, code: u32, reason: impl Into<Vec<u8>>) {
        let _ = self.reply.send(CoreAfterHandshakeDecision::Reject {
            error_code: code,
            reason: reason.into(),
        });
    }
}
