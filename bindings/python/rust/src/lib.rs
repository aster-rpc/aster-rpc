//! aster - Python bindings over the `aster` crate (plan D1).
//!
//! This module is registration-only; the wrappers live in submodules:
//! - node.rs: `Node` over `aster::Node`
//! - config.rs: `NativeAsterConfig` over `aster::AsterConfig`
//! - ids.rs: `SecretKey`, `PublicKey`, `NodeId`, `Hash`
//! - net.rs: NetClient, IrohConnection, streams, monitoring types
//! - blobs.rs: BlobsClient wrapper
//! - docs.rs: DocsClient, DocHandle wrappers
//! - gossip.rs: GossipClient, GossipTopicHandle wrappers
//! - monitor.rs: Phase 1b monitoring utilities
//! - admission.rs: Gate 0 (`GatePolicy`, `PeerAdmissionStore`, `Admission`)
//! - attestation.rs, grants.rs, lease.rs, topology.rs: plan P7 (values are
//!   the frozen dataclasses of the matching `aster.*` Python modules)
//! - coordination.rs: plan P9, the wrapped coordination client (+ the
//!   placement manager with Python providers)
//! - upcall.rs: plan R6, the upcall runtime (Rust asking Python: foreign
//!   lease serializers / fenced resources / authorizers, the topology
//!   `admitted` filter, placement providers, expose authorize, Gate 0)
//! - rpc.rs: plan P4, `DispatchServer` / `ServerCall` (Python services on
//!   `aster::rpc` dynamic dispatch) and `RpcClient` (pooled `RpcConnection`)
//! - expose.rs: `Expose` (socket targets) + `relay_request`, minimal, for
//!   the R6 expose authorize hook
//! - observability.rs: `aster-otel` pipeline, log bridge drain, instruments,
//!   spans and the `HealthServer` ops listener (observability O6)
//! - error.rs: Exception types
//!
//! The protocol clients and bare endpoints still drive the transport
//! core, reached from `aster` handles via `aster::binding_support`.

use pyo3::prelude::*;

mod admission;
mod attestation;
mod audit_outbox;
mod blob_custody;
mod blobs;
mod config;
mod contract;
mod coordination;
mod crypto;
mod docs;
mod error;
mod expose;
mod gossip;
mod grants;
mod http;
mod ids;
mod keys;
mod lease;
mod monitor;
mod net;
mod node;
mod observability;
mod provisioning;
mod response_queue;
mod rpc;
mod ticket;
mod topology;
mod upcall;
mod wire;

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
    // Opt-in Rust log sink (`ASTER_LOG_STDERR=1`, filter `ASTER_LOG`).
    aster_otel::init_stderr_from_env();
    let py = m.py();

    // This comes from core/build.rs and therefore describes the source commit,
    // not merely the static Cargo package fallback.
    m.add("VERSION", aster_transport_core::VERSION)?;

    // Register error types first (needed by other modules)
    error::register(py, m)?;

    // Register all module types
    ids::register(py, m)?;
    config::register(py, m)?;
    node::register(py, m)?;
    net::register(py, m)?;
    blobs::register(py, m)?;
    docs::register(py, m)?;
    gossip::register(py, m)?;
    admission::register(py, m)?;
    monitor::register(py, m)?;
    contract::register(py, m)?;
    ticket::register(py, m)?;
    crypto::register(py, m)?;
    wire::register(py, m)?;
    attestation::register(py, m)?;
    audit_outbox::register(m)?;
    grants::register(py, m)?;
    lease::register(py, m)?;
    topology::register(py, m)?;
    coordination::register(py, m)?;
    provisioning::register(m)?;
    rpc::register(py, m)?;
    upcall::register(py, m)?;
    observability::register(py, m)?;
    expose::register(py, m)?;

    Ok(())
}
