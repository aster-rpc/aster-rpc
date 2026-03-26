use std::sync::Arc;
use tokio::sync::Mutex;

use bytes::Bytes;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_asyncio::tokio::future_into_py;

use iroh::EndpointId;
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use futures_lite::StreamExt;

use crate::error::err_to_py;
use crate::node::IrohNode;

// ---------------------------------------------------------------------------
// GossipTopicHandle — Python wrapper for a subscribed gossip topic
// ---------------------------------------------------------------------------

#[pyclass]
pub struct GossipTopicHandle {
    sender: GossipSender,
    receiver: Arc<Mutex<GossipReceiver>>,
}

#[pymethods]
impl GossipTopicHandle {
    /// Broadcast a message to all peers on this topic.
    fn broadcast<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<&'py PyAny> {
        let sender = self.sender.clone();
        future_into_py(py, async move {
            sender
                .broadcast(Bytes::from(data))
                .await
                .map_err(err_to_py)?;
            Ok(())
        })
    }

    /// Receive the next event from this topic.
    ///
    /// Returns a tuple (event_type: str, data: bytes | None).
    /// event_type is one of: "received", "neighbor_up", "neighbor_down", "lagged".
    /// data is the message content for "received" events, None otherwise.
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let receiver = self.receiver.clone();
        future_into_py(py, async move {
            let mut rx = receiver.lock().await;
            use std::pin::Pin;
            let event = Pin::new(&mut *rx)
                .next()
                .await
                .ok_or_else(|| err_to_py("gossip topic closed"))?
                .map_err(err_to_py)?;
            match event {
                Event::Received(msg) => {
                    let content: PyObject =
                        Python::with_gil(|py| PyBytes::new(py, &msg.content).into_py(py));
                    Ok(("received".to_string(), Some(content)))
                }
                Event::NeighborUp(id) => Ok(("neighbor_up".to_string(), Some(
                    Python::with_gil(|py| {
                        PyBytes::new(py, id.to_string().as_bytes()).into_py(py)
                    }),
                ))),
                Event::NeighborDown(id) => Ok(("neighbor_down".to_string(), Some(
                    Python::with_gil(|py| {
                        PyBytes::new(py, id.to_string().as_bytes()).into_py(py)
                    }),
                ))),
                Event::Lagged => {
                    let none: Option<PyObject> = None;
                    Ok(("lagged".to_string(), none))
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// GossipClient — wraps a Gossip instance
// ---------------------------------------------------------------------------

#[pyclass]
pub struct GossipClient {
    pub(crate) inner: Gossip,
}

#[pymethods]
impl GossipClient {
    /// Subscribe to a gossip topic and wait for at least one peer connection.
    ///
    /// Args:
    ///     topic_bytes: 32-byte topic identifier
    ///     bootstrap_peers: list of endpoint ID strings to bootstrap from
    ///
    /// Returns:
    ///     GossipTopicHandle for broadcasting and receiving messages
    fn subscribe<'py>(
        &self,
        py: Python<'py>,
        topic_bytes: Vec<u8>,
        bootstrap_peers: Vec<String>,
    ) -> PyResult<&'py PyAny> {
        let gossip = self.inner.clone();
        future_into_py(py, async move {
            // Parse topic
            let topic_arr: [u8; 32] = topic_bytes
                .try_into()
                .map_err(|_| err_to_py("topic_bytes must be exactly 32 bytes"))?;
            let topic_id = TopicId::from_bytes(topic_arr);

            // Parse bootstrap peers
            let peers: Vec<EndpointId> = bootstrap_peers
                .iter()
                .map(|s| s.parse::<EndpointId>().map_err(err_to_py))
                .collect::<Result<Vec<_>, _>>()?;

            let topic = gossip
                .subscribe_and_join(topic_id, peers)
                .await
                .map_err(err_to_py)?;

            let (sender, receiver) = topic.split();
            Ok(GossipTopicHandle {
                sender,
                receiver: Arc::new(Mutex::new(receiver)),
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Factory function
// ---------------------------------------------------------------------------

/// Extract a GossipClient from an IrohNode.
#[pyfunction]
pub fn gossip_client(node: &IrohNode) -> GossipClient {
    GossipClient {
        inner: node.gossip.clone(),
    }
}

pub fn register(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<GossipClient>()?;
    m.add_class::<GossipTopicHandle>()?;
    m.add_function(wrap_pyfunction!(gossip_client, m)?)?;
    Ok(())
}