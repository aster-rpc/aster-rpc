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

use crate::admission::{spawn_gate_drainer, Admission, GatePolicy};
use crate::config::NativeAsterConfig;
use crate::ensure_tokio_runtime;
use crate::error::{aster_err_to_py, err_to_py};
use crate::ids::{NodeId, SecretKey};
use crate::net::{IrohConnection, NodeAddr};

/// A running Aster node (blobs + docs + gossip, plus any extra ALPNs).
#[pyclass(module = "aster._aster")]
pub struct Node {
    pub(crate) inner: aster::Node,
    /// The Gate-0 policy deciding inbound admission in Rust, if attached.
    gate: Option<Py<GatePolicy>>,
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
    /// `gate` attaches a `GatePolicy` (and turns admission hooks on): every
    /// inbound peer is then admitted or rejected in Rust by that policy.
    ///
    /// The node is persistent when `config.storage_path` is set. A
    /// `config.secret_key_file` is loaded, or created (mode 0600) from
    /// `config.secret_key` or a fresh key; a malformed file, or one that
    /// disagrees with `secret_key`, raises `ValueError`.
    #[staticmethod]
    #[pyo3(signature = (config=None, *, alpns=None, gate=None))]
    fn start<'py>(
        py: Python<'py>,
        config: Option<&Bound<'py, PyAny>>,
        alpns: Option<Vec<Vec<u8>>>,
        gate: Option<Bound<'py, GatePolicy>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        ensure_tokio_runtime();
        let mut config = extract_config(config)?;
        let alpns = alpns.unwrap_or_default();
        let gate_shared = gate.as_ref().map(|g| g.get().shared.clone());
        if gate_shared.is_some() {
            // The gate decides at the post-handshake hook.
            config.enable_hooks = true;
        }
        let gate = gate.map(Bound::unbind);
        future_into_py(py, async move {
            let inner = aster::Node::start_with_alpns(config, alpns)
                .await
                .map_err(aster_err_to_py)?;
            if let Some(shared) = gate_shared {
                let Some(admission) = inner.take_admission() else {
                    inner.shutdown().await;
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(
                        "Node.start: the node has no admission hooks to attach the gate to",
                    ));
                };
                spawn_gate_drainer(admission.into_core_receiver(), shared);
            }
            Ok(Node { inner, gate })
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

    /// This node's admission requests as an async iterator (`async for req
    /// in node.admission()`). Needs `enable_hooks=True` and no `gate`; can be
    /// taken once. With `outbound=True` this node's own dials are yielded too
    /// (as `ConnectRequest`); otherwise they are allowed.
    #[pyo3(signature = (*, outbound=false))]
    fn admission(&self, outbound: bool) -> PyResult<Admission> {
        if self.gate.is_some() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "admission is decided by the attached gate",
            ));
        }
        if !self.inner().has_hooks() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "admission needs a node started with enable_hooks=True",
            ));
        }
        let admission = self.inner.take_admission().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("admission was already taken")
        })?;
        Ok(Admission::new(
            admission.into_core_receiver(),
            self.inner().net_client(),
            outbound,
        ))
    }

    /// The attached `GatePolicy`, if any.
    #[getter]
    fn gate(&self, py: Python<'_>) -> Option<Py<GatePolicy>> {
        self.gate.as_ref().map(|g| g.clone_ref(py))
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
