//! Node module - wraps CoreNode from iroh_transport_core.
//!
//! Phase 2: Now wraps iroh_transport_core::CoreNode instead of iroh types directly.

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use iroh_transport_core::CoreNode;

use crate::error::err_to_py;
use crate::net::NodeAddr;

/// IrohNode – wrapper for CoreNode with all protocols enabled.
#[pyclass]
pub struct IrohNode {
    pub(crate) inner: CoreNode,
}

impl IrohNode {
    /// Get the inner CoreNode for use by other modules.
    pub(crate) fn inner(&self) -> &CoreNode {
        &self.inner
    }
}

#[pymethods]
impl IrohNode {
    /// Create an in-memory Iroh node with all protocols.
    #[staticmethod]
    fn memory<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            CoreNode::memory()
                .await
                .map(IrohNode::from)
                .map_err(err_to_py)
        })
    }

    /// Create a persistent Iroh node backed by an FsStore at the given path.
    #[staticmethod]
    fn persistent<'py>(py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            CoreNode::persistent(path)
                .await
                .map(IrohNode::from)
                .map_err(err_to_py)
        })
    }

    /// Return this node's EndpointId as a hex string.
    fn node_id(&self) -> String {
        self.inner.node_id()
    }

    /// Return the node's address info (debug format).
    fn node_addr(&self) -> String {
        format!("{:?}", self.inner.node_addr_info())
    }

    /// Return the node's structured address info.
    fn node_addr_info(&self) -> NodeAddr {
        let addr = self.inner.node_addr_info();
        NodeAddr {
            endpoint_id: addr.endpoint_id,
            relay_url: addr.relay_url,
            direct_addresses: addr.direct_addresses,
        }
    }

    /// Gracefully shut down the node.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let node = self.inner.clone();
        future_into_py(py, async move {
            node.close().await;
            Ok(())
        })
    }

    /// Alias for close() — gracefully shut down the node.
    fn shutdown<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }

    /// Add another node's address info so this node can connect to it.
    /// Used for peer discovery in testing/local scenarios.
    fn add_node_addr(&self, other: &IrohNode) -> PyResult<()> {
        self.inner.add_node_addr(&other.inner).map_err(err_to_py)
    }

    /// Export the node's secret key as 32 bytes.
    fn export_secret_key(&self) -> Vec<u8> {
        self.inner.export_secret_key()
    }
}

impl From<CoreNode> for IrohNode {
    fn from(inner: CoreNode) -> Self {
        Self { inner }
    }
}

/// Register the node types with the Python module
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<IrohNode>()?;
    Ok(())
}
