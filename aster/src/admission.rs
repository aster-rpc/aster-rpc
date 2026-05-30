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
use tokio::sync::oneshot;

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
