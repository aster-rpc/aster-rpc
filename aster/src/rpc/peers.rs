//! Which peers are connected to this server, and when that changes
//! ([`AsterServer::peer_events`](super::AsterServer::peer_events)).
//!
//! A peer counts as connected from its first `aster/1` connection that got
//! past the node's admission gates (Gate 0, enrollment) and was not refused
//! by a drain, until its last such connection ends. A peer the gate refuses
//! never reaches the server, so it produces no event. Several connections
//! from one peer (a client's pool, its session connections) give one
//! [`Connected`](PeerEvent::Connected) and one
//! [`Disconnected`](PeerEvent::Disconnected).
//!
//! Only `aster/1` connections are tracked: a peer that is connected over a
//! custom ALPN alone is not seen.
//!
//! The snapshot and the subscription are taken under the same lock the
//! publisher sends under, so every peer is in the snapshot or in the stream
//! after it, never both and never neither. The stream is a
//! [`tokio::sync::broadcast`] channel of [`PEER_EVENT_CAPACITY`] events: a
//! consumer that falls behind gets
//! [`RecvError::Lagged(n)`](tokio::sync::broadcast::error::RecvError::Lagged)
//! with the number it missed, and connections are never held up by it. After
//! `Lagged` the consumer's state is stale; its recovery is to call
//! `peer_events()` again and rebuild from the new snapshot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aster_transport_core::reactor::{ConnectionObserver, DRAIN_CLOSE_CODE};
use aster_transport_core::{CloseCause, CoreClosedInfo};
use tokio::sync::broadcast;

use super::auth::{AttributeStore, ROLE_ATTRIBUTE};

/// How many events a receiver may fall behind before it gets `Lagged`. A
/// peer's connect and disconnect are one event each, so this covers a burst
/// of about 500 peers coming and going between two reads of the consumer.
pub const PEER_EVENT_CAPACITY: usize = 1024;

/// A receiver of [`PeerEvent`]s from the moment it was returned by
/// [`AsterServer::peer_events`](super::AsterServer::peer_events).
pub type PeerEventReceiver = broadcast::Receiver<PeerEvent>;

/// Who a connected peer is, as verified when its first connection was
/// admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    /// The peer's verified endpoint id (hex), proven by the QUIC handshake.
    pub endpoint_id: String,
    /// The principal its credential names, present only when the peer has a
    /// live enrollment session (an enrolled peer's principal is its endpoint
    /// id). A call's [`principal`](super::CallContext::principal) may differ
    /// when an [`Authenticator`](super::Authenticator) resolves one per call.
    pub principal: Option<String>,
    /// The roles its credential grants (sorted), as Gate 3 sees them at the
    /// time it connected; empty without enrollment.
    pub roles: Vec<String>,
}

/// A peer connected now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerSnapshot {
    /// The peer's endpoint id.
    pub peer: String,
    /// The ALPN of its first admitted connection.
    pub alpn: Vec<u8>,
    pub identity: PeerIdentity,
    /// How many admitted connections it has now.
    pub connections: usize,
}

/// Why a peer's last connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DisconnectReason {
    /// The peer, or this server, closed it normally.
    Closed,
    /// The connection hit its idle timeout (no traffic, no keep-alive).
    TimedOut,
    /// The peer's credential expired or was revoked and the server closed it.
    Revoked,
    /// The server's drain closed it ([`DRAIN_CLOSE_CODE`]).
    Drained,
    /// Anything else: a transport error or reset.
    Error,
}

/// A change in which peers are connected. Per peer, not per connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerEvent {
    /// `peer`'s first admitted connection opened.
    Connected {
        peer: String,
        alpn: Vec<u8>,
        identity: PeerIdentity,
    },
    /// `peer`'s last connection ended. `reason` is that connection's.
    Disconnected {
        peer: String,
        reason: DisconnectReason,
    },
}

struct Entry {
    alpn: Vec<u8>,
    identity: PeerIdentity,
    connections: usize,
}

/// The server's record of connected peers; the reactor's connection observer.
pub(crate) struct PeerLedger {
    peers: Mutex<HashMap<String, Entry>>,
    events: broadcast::Sender<PeerEvent>,
    attributes: AttributeStore,
}

impl PeerLedger {
    pub(crate) fn new(attributes: AttributeStore) -> Arc<Self> {
        Arc::new(Self {
            peers: Mutex::new(HashMap::new()),
            events: broadcast::channel(PEER_EVENT_CAPACITY).0,
            attributes,
        })
    }

    /// The peers now, and a receiver of every change after them.
    pub(crate) fn subscribe(&self) -> (Vec<PeerSnapshot>, PeerEventReceiver) {
        // Subscribed under the lock `opened` and `closed` send under.
        let peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        let receiver = self.events.subscribe();
        let mut snapshot: Vec<PeerSnapshot> = peers
            .iter()
            .map(|(peer, entry)| PeerSnapshot {
                peer: peer.clone(),
                alpn: entry.alpn.clone(),
                identity: entry.identity.clone(),
                connections: entry.connections,
            })
            .collect();
        drop(peers);
        snapshot.sort_by(|a, b| a.peer.cmp(&b.peer));
        (snapshot, receiver)
    }

    fn identity(&self, peer: &str) -> PeerIdentity {
        let (attributes, credentialed) = self.attributes.resolve(peer);
        let mut roles: Vec<String> = attributes
            .get(ROLE_ATTRIBUTE)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|role| !role.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        roles.sort();
        PeerIdentity {
            endpoint_id: peer.to_string(),
            principal: credentialed.then(|| peer.to_string()),
            roles,
        }
    }

    /// Why a connection ended, from the transport's close error. A local
    /// close reports no code, so what this side knows decides it: the
    /// reactor's drain, or an enrollment session that is gone (revocation and
    /// expiry remove the session, then close its connections).
    fn reason(&self, peer: &str, info: &CoreClosedInfo, drained: bool) -> DisconnectReason {
        match info.cause {
            CloseCause::TimedOut => DisconnectReason::TimedOut,
            CloseCause::LocallyClosed if drained => DisconnectReason::Drained,
            CloseCause::LocallyClosed
                if self.attributes.has_source() && !self.attributes.resolve(peer).1 =>
            {
                DisconnectReason::Revoked
            }
            CloseCause::LocallyClosed => DisconnectReason::Closed,
            CloseCause::PeerClosed if info.code == Some(DRAIN_CLOSE_CODE) => {
                DisconnectReason::Drained
            }
            CloseCause::PeerClosed => DisconnectReason::Closed,
            CloseCause::Other => DisconnectReason::Error,
        }
    }
}

impl ConnectionObserver for PeerLedger {
    fn opened(&self, _connection_id: u64, peer: &str, alpn: &[u8]) {
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = peers.get_mut(peer) {
            entry.connections += 1;
            return;
        }
        let identity = self.identity(peer);
        peers.insert(
            peer.to_string(),
            Entry {
                alpn: alpn.to_vec(),
                identity: identity.clone(),
                connections: 1,
            },
        );
        // No receiver is not an error.
        let _ = self.events.send(PeerEvent::Connected {
            peer: peer.to_string(),
            alpn: alpn.to_vec(),
            identity,
        });
    }

    fn closed(&self, _connection_id: u64, peer: &str, info: &CoreClosedInfo, drained: bool) {
        let reason = self.reason(peer, info, drained);
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = peers.get_mut(peer) else {
            return;
        };
        entry.connections -= 1;
        if entry.connections > 0 {
            return;
        }
        peers.remove(peer);
        let _ = self.events.send(PeerEvent::Disconnected {
            peer: peer.to_string(),
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    fn closed(cause: CloseCause, code: Option<u64>) -> CoreClosedInfo {
        CoreClosedInfo {
            kind: String::new(),
            code,
            reason: None,
            cause,
        }
    }

    fn peer(n: usize) -> String {
        format!("peer-{n}")
    }

    #[test]
    fn a_receiver_that_stops_reading_gets_lagged_with_the_count_and_the_newest_events() {
        let ledger = PeerLedger::new(AttributeStore::new());
        let (_, mut events) = ledger.subscribe();
        // Never read: every peer connects and leaves, two events each.
        let peers = PEER_EVENT_CAPACITY;
        for n in 0..peers {
            ledger.opened(n as u64, &peer(n), b"aster/1");
            ledger.closed(
                n as u64,
                &peer(n),
                &closed(CloseCause::PeerClosed, Some(0)),
                false,
            );
        }
        let sent = 2 * peers;
        let missed = (sent - PEER_EVENT_CAPACITY) as u64;
        assert!(matches!(events.try_recv(), Err(TryRecvError::Lagged(n)) if n == missed));
        // After the signal the receiver reads on from the oldest event kept,
        // so nothing is silently skipped past it.
        let mut rest = 0;
        while events.try_recv().is_ok() {
            rest += 1;
        }
        assert_eq!(rest, PEER_EVENT_CAPACITY);
        // The recovery: a fresh snapshot is current.
        let (snapshot, _) = ledger.subscribe();
        assert!(snapshot.is_empty());
    }

    #[tokio::test]
    async fn lagged_is_what_recv_returns() {
        let ledger = PeerLedger::new(AttributeStore::new());
        let (_, mut events) = ledger.subscribe();
        for n in 0..=PEER_EVENT_CAPACITY {
            ledger.opened(n as u64, &peer(n), b"aster/1");
        }
        assert!(matches!(events.recv().await, Err(RecvError::Lagged(1))));
        assert!(matches!(
            events.recv().await,
            Ok(PeerEvent::Connected { .. })
        ));
    }

    /// Threads connect while others take snapshots: each peer is in every
    /// snapshot-plus-stream exactly once, never twice and never missing.
    #[test]
    fn snapshot_and_stream_cover_each_peer_exactly_once_under_contention() {
        const PEERS: usize = 64;
        let ledger = PeerLedger::new(AttributeStore::new());
        let connecting = {
            let ledger = ledger.clone();
            std::thread::spawn(move || {
                for n in 0..PEERS {
                    ledger.opened(n as u64, &peer(n), b"aster/1");
                    std::thread::yield_now();
                }
            })
        };
        let watchers: Vec<_> = (0..4)
            .map(|_| {
                let ledger = ledger.clone();
                std::thread::spawn(move || {
                    let (snapshot, mut events) = ledger.subscribe();
                    let mut seen: Vec<String> = snapshot.into_iter().map(|p| p.peer).collect();
                    while seen.len() < PEERS {
                        match events.try_recv() {
                            Ok(PeerEvent::Connected { peer, .. }) => seen.push(peer),
                            Ok(other) => panic!("unexpected {other:?}"),
                            Err(TryRecvError::Empty) => std::thread::yield_now(),
                            Err(other) => panic!("unexpected {other:?}"),
                        }
                    }
                    seen
                })
            })
            .collect();
        connecting.join().unwrap();
        for watcher in watchers {
            let mut seen = watcher.join().unwrap();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), PEERS, "each peer exactly once");
        }
    }

    #[test]
    fn close_causes_map_to_reasons() {
        let ledger = PeerLedger::new(AttributeStore::new());
        let reason = |info: CoreClosedInfo, drained| ledger.reason("p", &info, drained);
        assert_eq!(
            reason(closed(CloseCause::TimedOut, None), false),
            DisconnectReason::TimedOut
        );
        assert_eq!(
            reason(closed(CloseCause::LocallyClosed, None), true),
            DisconnectReason::Drained
        );
        assert_eq!(
            reason(closed(CloseCause::LocallyClosed, None), false),
            DisconnectReason::Closed
        );
        assert_eq!(
            reason(closed(CloseCause::PeerClosed, Some(0)), false),
            DisconnectReason::Closed
        );
        assert_eq!(
            reason(
                closed(CloseCause::PeerClosed, Some(DRAIN_CLOSE_CODE)),
                false
            ),
            DisconnectReason::Drained
        );
        assert_eq!(
            reason(closed(CloseCause::Other, None), false),
            DisconnectReason::Error
        );
    }
}
