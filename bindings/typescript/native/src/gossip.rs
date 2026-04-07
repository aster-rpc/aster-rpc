//! Gossip module — wraps CoreGossipClient and CoreGossipTopic.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::{CoreGossipClient, CoreGossipTopic};

use crate::error::to_napi_err;

#[napi]
pub struct GossipClient {
    pub(crate) inner: CoreGossipClient,
}

impl From<CoreGossipClient> for GossipClient {
    fn from(inner: CoreGossipClient) -> Self {
        Self { inner }
    }
}

#[napi]
impl GossipClient {
    /// Subscribe to a gossip topic with initial peer IDs.
    #[napi]
    pub async fn subscribe(
        &self,
        topic: Buffer,
        peers: Vec<String>,
    ) -> Result<GossipTopicHandle> {
        let inner = self
            .inner
            .clone()
            .subscribe(topic.to_vec(), peers)
            .await
            .map_err(to_napi_err)?;
        Ok(GossipTopicHandle { inner })
    }
}

#[napi]
pub struct GossipTopicHandle {
    inner: CoreGossipTopic,
}

#[napi(object)]
pub struct GossipEvent {
    pub event_type: String,
    pub data: Option<Buffer>,
}

#[napi]
impl GossipTopicHandle {
    /// Broadcast a message to the topic.
    #[napi]
    pub async fn broadcast(&self, data: Buffer) -> Result<()> {
        self.inner
            .clone()
            .broadcast(data.to_vec())
            .await
            .map_err(to_napi_err)
    }

    /// Receive the next event from the topic.
    #[napi]
    pub async fn recv(&self) -> Result<GossipEvent> {
        let event = self.inner.clone().recv().await.map_err(to_napi_err)?;
        Ok(GossipEvent {
            event_type: event.event_type,
            data: event.data.map(Buffer::from),
        })
    }
}
