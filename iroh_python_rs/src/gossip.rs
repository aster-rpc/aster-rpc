//! Gossip module - wraps CoreGossipClient, CoreGossipTopic from iroh_transport_core.
//!
//! Phase 2: Now wraps iroh_transport_core types instead of iroh_gossip types directly.

use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_asyncio::tokio::future_into_py;

use iroh_transport_core::{CoreGossipClient, CoreGossipTopic};

use crate::error::err_to_py;
use crate::node::IrohNode;

// ============================================================================
// GossipTopicHandle
// ============================================================================

#[pyclass]
pub struct GossipTopicHandle {
    inner: CoreGossipTopic,
}

impl From<CoreGossipTopic> for GossipTopicHandle {
    fn from(inner: CoreGossipTopic) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl GossipTopicHandle {
    /// Broadcast a message to all peers on this topic.
    fn broadcast<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<&'py PyAny> {
        let topic = self.inner.clone();
        future_into_py(py, async move {
            topic.broadcast(data).await.map_err(err_to_py)?;
            Ok(())
        })
    }

    /// Receive the next event from this topic.
    ///
    /// Returns a tuple (event_type: str, data: bytes | None).
    /// event_type is one of: "received", "neighbor_up", "neighbor_down", "lagged".
    /// data is the message content for "received" events, None otherwise.
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let topic = self.inner.clone();
        future_into_py(py, async move {
            let event = topic.recv().await.map_err(err_to_py)?;
            let (event_type, data): (String, Option<PyObject>) = match event.event_type.as_str() {
                "received" => (
                    "received".to_string(),
                    event.data.map(|d| {
                        Python::with_gil(|py| PyBytes::new(py, &d).into_py(py))
                    }),
                ),
                "neighbor_up" | "neighbor_down" => {
                    let data: Option<PyObject> = event.data.map(|d| {
                        Python::with_gil(|py| PyBytes::new(py, &d).into_py(py))
                    });
                    (event.event_type, data)
                }
                "lagged" => ("lagged".to_string(), None),
                _ => (event.event_type, None),
            };
            Ok((event_type, data))
        })
    }
}

// ============================================================================
// GossipClient
// ============================================================================

#[pyclass]
pub struct GossipClient {
    inner: CoreGossipClient,
}

impl From<CoreGossipClient> for GossipClient {
    fn from(inner: CoreGossipClient) -> Self {
        Self { inner }
    }
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
        let client = self.inner.clone();
        future_into_py(py, async move {
            let topic = client
                .subscribe(topic_bytes, bootstrap_peers)
                .await
                .map_err(err_to_py)?;
            Ok(GossipTopicHandle::from(topic))
        })
    }
}

// ============================================================================
// Factory function
// ============================================================================

/// Extract a GossipClient from an IrohNode.
#[pyfunction]
pub fn gossip_client(node: &IrohNode) -> GossipClient {
    GossipClient::from(node.inner().gossip_client())
}

/// Register the gossip types with the Python module.
pub fn register(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<GossipClient>()?;
    m.add_class::<GossipTopicHandle>()?;
    m.add_function(wrap_pyfunction!(gossip_client, m)?)?;
    Ok(())
}
