//! iroh_python - Python bindings for iroh using iroh_transport_core.
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
//! All wrappers now use iroh_transport_core as the backend.

use pyo3::prelude::*;

mod blobs;
mod docs;
mod error;
mod gossip;
mod hooks;
mod monitor;
mod net;
mod node;

/// Initialize the async runtime for tokio.
fn init_tokio_runtime() {
    use pyo3_asyncio::tokio::init as pyo3_asyncio_init;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    pyo3_asyncio_init(builder);
}

#[pymodule]
fn _iroh_python(py: Python<'_>, m: &PyModule) -> PyResult<()> {
    // Initialize tokio runtime for async operations
    init_tokio_runtime();

    // Register error types first (needed by other modules)
    error::register(py, m)?;

    // Register all module types
    node::register(py, m)?;
    net::register(py, m)?;
    blobs::register(py, m)?;
    docs::register(py, m)?;
    gossip::register(py, m)?;
    hooks::register(py, m)?;
    monitor::register(py, m)?;

    Ok(())
}
