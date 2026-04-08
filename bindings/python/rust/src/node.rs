//! Node module - wraps CoreNode from aster_transport_core.
//!
//! Phase 2: Now wraps aster_transport_core::CoreNode instead of iroh types directly.

use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::tokio::future_into_py;

use aster_transport_core::CoreNode;

use crate::ensure_tokio_runtime;
use crate::error::err_to_py;
use crate::hooks::NodeHookReceiver;
use crate::net::{EndpointConfig, IrohConnection, NodeAddr};

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
        ensure_tokio_runtime();
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
        ensure_tokio_runtime();
        future_into_py(py, async move {
            CoreNode::persistent(path)
                .await
                .map(IrohNode::from)
                .map_err(err_to_py)
        })
    }

    /// Create an in-memory Iroh node that serves blobs + docs + gossip AND
    /// accepts connections on each entry in `aster_alpns` (custom Aster
    /// protocols). Poll incoming aster-ALPN connections via `accept_aster()`.
    /// `endpoint_config` (optional) applies the same `enable_hooks` /
    /// `enable_monitoring` / `secret_key` / `relay_mode` / `bind_addr`
    /// surface as `create_endpoint_with_config`.
    #[staticmethod]
    #[pyo3(signature = (aster_alpns, endpoint_config=None))]
    fn memory_with_alpns<'py>(
        py: Python<'py>,
        aster_alpns: Vec<Vec<u8>>,
        endpoint_config: Option<&EndpointConfig>,
    ) -> PyResult<Bound<'py, PyAny>> {
        ensure_tokio_runtime();
        let core_cfg = endpoint_config.map(|c| c.into());
        future_into_py(py, async move {
            CoreNode::memory_with_alpns(aster_alpns, core_cfg)
                .await
                .map(IrohNode::from)
                .map_err(err_to_py)
        })
    }

    /// Persistent (FsStore-backed) counterpart to `memory_with_alpns`.
    #[staticmethod]
    #[pyo3(signature = (path, aster_alpns, endpoint_config=None))]
    fn persistent_with_alpns<'py>(
        py: Python<'py>,
        path: String,
        aster_alpns: Vec<Vec<u8>>,
        endpoint_config: Option<&EndpointConfig>,
    ) -> PyResult<Bound<'py, PyAny>> {
        ensure_tokio_runtime();
        let core_cfg = endpoint_config.map(|c| c.into());
        future_into_py(py, async move {
            CoreNode::persistent_with_alpns(path, aster_alpns, core_cfg)
                .await
                .map(IrohNode::from)
                .map_err(err_to_py)
        })
    }

    /// Await the next incoming aster-ALPN connection. Returns
    /// `(bytes, IrohConnection)` tuple. Raises when the node closes.
    fn accept_aster<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let node = self.inner.clone();
        future_into_py(py, async move {
            let (alpn, conn) = node.accept_aster().await.map_err(err_to_py)?;
            let conn_wrapper = IrohConnection::from(conn);
            // Wrap the alpn bytes in a Python `bytes` object (Vec<u8> would
            // otherwise come across as list[int]).
            Python::attach(|py| {
                let alpn_bytes: Py<PyAny> = PyBytes::new(py, &alpn).into();
                let conn_py: Py<PyAny> = Py::new(py, conn_wrapper)?.into_any();
                let tup = pyo3::types::PyTuple::new(py, &[alpn_bytes, conn_py])?;
                Ok::<Py<PyAny>, PyErr>(tup.unbind().into())
            })
        })
    }

    /// Take this node's Phase 1b hook receiver (one-shot). Returns `None`
    /// if hooks weren't enabled at construction or the receiver was taken.
    /// Must be awaited because the receiver spawns a background tokio task.
    fn take_hook_receiver<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let node = self.inner.clone();
        future_into_py(py, async move {
            match node.take_hook_receiver() {
                Some(core_rx) => Ok(Some(NodeHookReceiver::from_core(core_rx))),
                None => Ok(None::<NodeHookReceiver>),
            }
        })
    }

    /// Whether this node was built with `enable_hooks=true`.
    fn has_hooks(&self) -> bool {
        self.inner.has_hooks()
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

    /// Export all transport-level metrics in Prometheus text exposition format.
    ///
    /// Covers socket I/O, path counts, holepunching, relay, and net report
    /// counters. Merge with Aster RPC metrics for a single scrape target.
    fn transport_metrics_prometheus(&self) -> String {
        self.inner.transport_metrics_prometheus()
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
