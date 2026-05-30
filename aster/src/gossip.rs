//! Gossip pub-sub client.

use crate::error::{Error, Result};
use crate::id::NodeId;
use aster_transport_core::{CoreGossipClient, CoreGossipTopic};

/// An event received on a gossip topic.
#[derive(Clone, Debug)]
pub enum GossipEvent {
    /// A broadcast message was received.
    Received { data: Vec<u8> },
    /// A neighbour joined the topic swarm.
    NeighborUp { peer: NodeId },
    /// A neighbour left the topic swarm.
    NeighborDown { peer: NodeId },
    /// The receiver lagged and dropped messages.
    Lagged,
}

/// A handle to a node's gossip client. Cheap to clone.
#[derive(Clone)]
pub struct Gossip {
    inner: CoreGossipClient,
}

impl Gossip {
    pub(crate) fn new(inner: CoreGossipClient) -> Self {
        Self { inner }
    }

    /// Subscribe to a 32-byte topic, bootstrapping from `peers`.
    pub async fn subscribe(&self, topic: [u8; 32], peers: &[NodeId]) -> Result<GossipTopic> {
        let peers = peers.iter().map(|p| p.as_str().to_string()).collect();
        Ok(GossipTopic {
            inner: self.inner.subscribe(topic.to_vec(), peers).await?,
        })
    }
}

/// A subscribed gossip topic: broadcast and receive messages.
#[derive(Clone)]
pub struct GossipTopic {
    inner: CoreGossipTopic,
}

impl GossipTopic {
    /// Broadcast `data` to the topic.
    pub async fn broadcast(&self, data: impl Into<Vec<u8>>) -> Result<()> {
        Ok(self.inner.broadcast(data.into()).await?)
    }

    /// Receive the next topic event.
    pub async fn recv(&self) -> Result<GossipEvent> {
        let ev = self.inner.recv().await?;
        let peer = || -> Result<NodeId> {
            let bytes = ev.data.clone().ok_or_else(|| {
                Error::Transport(anyhow::anyhow!("gossip neighbour event missing peer id"))
            })?;
            let s = String::from_utf8(bytes)
                .map_err(|e| Error::Transport(anyhow::anyhow!("invalid peer id utf8: {e}")))?;
            Ok(NodeId::from_hex(s))
        };
        Ok(match ev.event_type.as_str() {
            "received" => GossipEvent::Received {
                data: ev.data.unwrap_or_default(),
            },
            "neighbor_up" => GossipEvent::NeighborUp { peer: peer()? },
            "neighbor_down" => GossipEvent::NeighborDown { peer: peer()? },
            "lagged" => GossipEvent::Lagged,
            other => {
                return Err(Error::Transport(anyhow::anyhow!(
                    "unknown gossip event type: {other}"
                )))
            }
        })
    }
}
