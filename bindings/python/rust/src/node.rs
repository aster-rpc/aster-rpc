//! `Node` -- a running `aster::Node`, built from an `AsterConfig`.
//!
//! Python usage (see `docs/aster-python-getstarted.md`)::
//!
//!     node = await Node.start(AsterConfig(storage_path="/var/lib/app"))
//!     ...
//!     await node.shutdown()
//!
//!     # or, closed on exit:
//!     async with await Node.start(config) as node:
//!         ...
//!
//! The protocol clients (`blobs_client(node)`, `docs_client(node)`,
//! `gossip_client(node)`, `net_client(node)`) and the RPC reactor still drive
//! the transport-core node, reached through `aster::binding_support`.

use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::tokio::future_into_py;

use aster::binding_support::CoreNode;

use crate::config::NativeAsterConfig;
use crate::ensure_tokio_runtime;
use crate::error::{aster_err_to_py, err_to_py};
use crate::hooks::NodeHookReceiver;
use crate::ids::{NodeId, SecretKey};
use crate::net::{IrohConnection, NodeAddr};

/// A running Aster node (blobs + docs + gossip, plus any extra ALPNs).
#[pyclass(module = "aster._aster")]
pub struct Node {
    pub(crate) inner: aster::Node,
}

impl Node {
    /// The transport-core node, for the protocol clients and the reactor.
    pub(crate) fn inner(&self) -> &CoreNode {
        self.inner.core_node()
    }
}

/// Accept `None`, a `NativeAsterConfig`, or anything with a `_native()`
/// method returning one (the Python `aster.AsterConfig` dataclass).
fn extract_config(config: Option<&Bound<'_, PyAny>>) -> PyResult<aster::AsterConfig> {
    let Some(obj) = config.filter(|c| !c.is_none()) else {
        return Ok(aster::AsterConfig::default());
    };
    if let Ok(native) = obj.cast::<NativeAsterConfig>() {
        return Ok(native.get().inner.clone());
    }
    if obj.hasattr("_native")? {
        let native = obj.call_method0("_native")?;
        if let Ok(native) = native.cast::<NativeAsterConfig>() {
            return Ok(native.get().inner.clone());
        }
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "Node.start: config must be an aster.AsterConfig",
    ))
}

#[pymethods]
impl Node {
    /// Start a node from `config` (an `aster.AsterConfig`; default: an
    /// in-memory node with default relays). `alpns` adds inbound ALPNs on top
    /// of `config.alpns`; poll them with `accept_aster()`.
    ///
    /// The node is persistent when `config.storage_path` is set. A
    /// `config.secret_key_file` is loaded, or created (mode 0600) from
    /// `config.secret_key` or a fresh key; a malformed file, or one that
    /// disagrees with `secret_key`, raises `ValueError`.
    #[staticmethod]
    #[pyo3(signature = (config=None, *, alpns=None))]
    fn start<'py>(
        py: Python<'py>,
        config: Option<&Bound<'py, PyAny>>,
        alpns: Option<Vec<Vec<u8>>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        ensure_tokio_runtime();
        let config = extract_config(config)?;
        let alpns = alpns.unwrap_or_default();
        future_into_py(py, async move {
            aster::Node::start_with_alpns(config, alpns)
                .await
                .map(|inner| Node { inner })
                .map_err(aster_err_to_py)
        })
    }

    /// This node's id.
    #[getter]
    fn id(&self) -> NodeId {
        NodeId::from_trusted_hex(self.inner.id().into_string())
    }

    /// This node's id as a hex string (same value as `str(node.id)`).
    fn node_id(&self) -> String {
        self.inner.id().into_string()
    }

    /// Return the node's address info (debug format).
    fn node_addr(&self) -> String {
        format!("{:?}", self.inner().node_addr_info())
    }

    /// Return the node's structured address info.
    fn node_addr_info(&self) -> NodeAddr {
        NodeAddr::from(self.inner().node_addr_info())
    }

    /// The node's secret key (restore the identity via
    /// `AsterConfig(secret_key=...)` or persist it with `write_file`).
    fn secret_key(&self) -> PyResult<SecretKey> {
        self.inner
            .export_secret_key()
            .map(|inner| SecretKey { inner })
            .map_err(aster_err_to_py)
    }

    /// The node's secret key as 32 raw bytes.
    fn export_secret_key<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner().export_secret_key())
    }

    /// Await the next incoming connection on a custom ALPN. Returns
    /// `(alpn: bytes, IrohConnection)`. Raises when the node closes.
    fn accept_aster<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let node = self.inner().clone();
        future_into_py(py, async move {
            let (alpn, conn) = node.accept_aster().await.map_err(err_to_py)?;
            let conn_wrapper = IrohConnection::from(conn);
            Python::attach(|py| {
                let alpn_bytes: Py<PyAny> = PyBytes::new(py, &alpn).into();
                let conn_py: Py<PyAny> = Py::new(py, conn_wrapper)?.into_any();
                let tup = pyo3::types::PyTuple::new(py, &[alpn_bytes, conn_py])?;
                Ok::<Py<PyAny>, PyErr>(tup.unbind().into())
            })
        })
    }

    /// Take this node's admission hook receiver (one-shot). `None` unless the
    /// node was started with `enable_hooks=True`, or if already taken.
    fn take_hook_receiver<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let node = self.inner().clone();
        future_into_py(py, async move {
            match node.take_hook_receiver() {
                Some(core_rx) => Ok(Some(NodeHookReceiver::from_core(core_rx))),
                None => Ok(None::<NodeHookReceiver>),
            }
        })
    }

    /// Whether this node was started with `enable_hooks=True`.
    fn has_hooks(&self) -> bool {
        self.inner().has_hooks()
    }

    /// Teach this node how to reach `other` directly (tests / local setups).
    fn add_node_addr(&self, other: &Node) -> PyResult<()> {
        self.inner.add_peer(&other.inner).map_err(aster_err_to_py)
    }

    /// Transport metrics in Prometheus text exposition format.
    fn transport_metrics_prometheus(&self) -> String {
        self.inner.metrics_prometheus()
    }

    /// Gracefully shut down the node (flushes persistent state first).
    fn shutdown<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let node = self.inner.clone();
        future_into_py(py, async move {
            node.shutdown().await;
            Ok(())
        })
    }

    /// Alias for `shutdown()`.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.shutdown(py)
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let obj: Py<PyAny> = slf.into_any().unbind();
        future_into_py(py, async move { Ok(obj) })
    }

    #[pyo3(signature = (_exc_type=None, _exc=None, _tb=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Bound<'py, PyAny>>,
        _exc: Option<Bound<'py, PyAny>>,
        _tb: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let node = self.inner.clone();
        future_into_py(py, async move {
            node.shutdown().await;
            Ok(false)
        })
    }

    fn __repr__(&self) -> String {
        format!("Node(id='{}')", self.inner.id())
    }
}

/// Register the node types with the Python module
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Node>()?;
    Ok(())
}
