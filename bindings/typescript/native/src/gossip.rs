//! Gossip — wraps the `aster` crate's [`aster::Gossip`] / [`aster::GossipTopic`].
//! Topic events are typed and the topic handle is an `AsyncIterable`.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::NodeId;

use crate::config::aster_err;

#[napi]
pub struct GossipClient {
    pub(crate) inner: aster::Gossip,
}

impl From<aster::Gossip> for GossipClient {
    fn from(inner: aster::Gossip) -> Self {
        Self { inner }
    }
}

#[napi]
impl GossipClient {
    /// Subscribe to a 32-byte topic, bootstrapping from `peers` (node ids).
    #[napi]
    pub async fn subscribe(
        &self,
        topic: Uint8Array,
        peers: Vec<String>,
    ) -> Result<GossipTopicHandle> {
        let topic: [u8; 32] = topic
            .to_vec()
            .try_into()
            .map_err(|_| Error::new(Status::InvalidArg, "gossip topic must be 32 bytes"))?;
        let peers: Vec<NodeId> = peers.into_iter().map(NodeId::from_hex).collect();
        let inner = self
            .inner
            .subscribe(topic, &peers)
            .await
            .map_err(aster_err)?;
        Ok(GossipTopicHandle { inner })
    }
}

/// An event on a gossip topic.
#[napi(object)]
pub struct GossipEvent {
    #[napi(ts_type = "'received' | 'neighbor_up' | 'neighbor_down' | 'lagged'")]
    pub kind: String,
    /// `received`: the message.
    pub data: Option<Buffer>,
    /// `neighbor_up` / `neighbor_down`: the peer.
    pub peer: Option<String>,
}

impl From<aster::GossipEvent> for GossipEvent {
    fn from(e: aster::GossipEvent) -> Self {
        match e {
            aster::GossipEvent::Received { data } => Self {
                kind: "received".into(),
                data: Some(Buffer::from(data)),
                peer: None,
            },
            aster::GossipEvent::NeighborUp { peer } => Self {
                kind: "neighbor_up".into(),
                data: None,
                peer: Some(peer.into_string()),
            },
            aster::GossipEvent::NeighborDown { peer } => Self {
                kind: "neighbor_down".into(),
                data: None,
                peer: Some(peer.into_string()),
            },
            aster::GossipEvent::Lagged => Self {
                kind: "lagged".into(),
                data: None,
                peer: None,
            },
        }
    }
}

/// A subscribed topic: `broadcast()` messages, and iterate its events with
/// `for await (const ev of topic)` (or call `recv()`).
#[napi(async_iterator)]
pub struct GossipTopicHandle {
    inner: aster::GossipTopic,
}

#[napi]
impl AsyncGenerator for GossipTopicHandle {
    type Yield = GossipEvent;
    type Next = ();
    type Return = ();

    fn next(
        &mut self,
        _value: Option<Self::Next>,
    ) -> impl std::future::Future<Output = Result<Option<Self::Yield>>> + Send + 'static {
        let inner = self.inner.clone();
        async move { Ok(Some(inner.recv().await.map_err(aster_err)?.into())) }
    }
}

#[napi]
impl GossipTopicHandle {
    /// Broadcast a message to the topic.
    #[napi]
    pub async fn broadcast(&self, data: Uint8Array) -> Result<()> {
        self.inner.broadcast(data.to_vec()).await.map_err(aster_err)
    }

    /// The next topic event.
    #[napi]
    pub async fn recv(&self) -> Result<GossipEvent> {
        Ok(self.inner.recv().await.map_err(aster_err)?.into())
    }
}
