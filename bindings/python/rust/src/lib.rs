//! aster - Python bindings for iroh using aster_transport_core.
//!
//! Phase 2: This module is now registration-only.
//! All actual wrapper logic has been moved to individual modules:
//! - node.rs: IrohNode wrapper
//! - net.rs: NetClient, IrohConnection, streams, monitoring types
//! - blobs.rs: BlobsClient wrapper
//! - docs.rs: DocsClient, DocHandle wrappers
//! - gossip.rs: GossipClient, GossipTopicHandle wrappers
//! - monitor.rs: Phase 1b monitoring utilities
//! - hooks.rs: Phase 1b hooks utilities
//! - error.rs: Exception types
//!
//! All wrappers now use aster_transport_core as the backend.

use pyo3::prelude::*;

mod blobs;
mod call;
mod contract;
mod docs;
mod error;
mod gossip;
mod hooks;
mod monitor;
mod net;
mod node;
mod ticket;

/// Wrapper to convert Vec<u8> to Python bytes via IntoPyObject.
/// In pyo3 0.28, Vec<u8> converts to list[int], but we want bytes.
pub(crate) struct PyBytesResult(pub Vec<u8>);

impl<'py> IntoPyObject<'py> for PyBytesResult {
    type Target = pyo3::types::PyBytes;
    type Output = Bound<'py, pyo3::types::PyBytes>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        Ok(pyo3::types::PyBytes::new(py, &self.0))
    }
}

/// Initialize the tokio runtime. Called lazily on first node/endpoint creation,
/// not at module import time — keeps `import _aster` fast.
pub(crate) fn ensure_tokio_runtime() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        use pyo3_async_runtimes::tokio::init as pyo3_asyncio_init;
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        pyo3_asyncio_init(builder);
    });
}

#[pymodule]
fn _aster(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();

    // Register error types first (needed by other modules)
    error::register(py, m)?;

    // Register all module types
    node::register(py, m)?;
    net::register(py, m)?;
    call::register(py, m)?;
    blobs::register(py, m)?;
    docs::register(py, m)?;
    gossip::register(py, m)?;
    hooks::register(py, m)?;
    monitor::register(py, m)?;
    contract::register(py, m)?;
    ticket::register(py, m)?;

    Ok(())
}
