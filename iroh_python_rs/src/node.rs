use pyo3::prelude::*;
use pyo3_asyncio::tokio::future_into_py;

use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{presets, Endpoint};
use iroh::protocol::Router;
use iroh_blobs::{
    api::Store as BlobStore, store::fs::FsStore, store::mem::MemStore, BlobsProtocol,
    ALPN as BLOBS_ALPN,
};
use iroh_docs::{protocol::Docs, ALPN as DOCS_ALPN};
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};

/// Wraps an error type that implements Display into a PyErr via IrohError.
fn err_to_py(e: impl std::fmt::Display) -> PyErr {
    crate::error::IrohError::new_err(e.to_string())
}

/// IrohNode – composite wrapper for Endpoint + Router + protocol handlers.
#[pyclass]
pub struct IrohNode {
    pub(crate) endpoint: Endpoint,
    #[allow(dead_code)]
    pub(crate) router: Router,
    pub(crate) blobs: BlobsProtocol,
    pub(crate) docs: Docs,
    pub(crate) gossip: Gossip,
    pub(crate) store: BlobStore,
}

#[pymethods]
impl IrohNode {
    /// Create an in-memory Iroh node with all protocols.
    #[staticmethod]
    fn memory<'py>(py: Python<'py>) -> PyResult<&'py PyAny> {
        future_into_py(py, async move {
            let endpoint = Endpoint::bind(presets::N0).await.map_err(err_to_py)?;
            endpoint.online().await;

            let mem_store = MemStore::new();
            let store: BlobStore = (*mem_store).clone();
            let blobs = BlobsProtocol::new(&store, None);
            let gossip = Gossip::builder().spawn(endpoint.clone());
            let docs = Docs::memory()
                .spawn(endpoint.clone(), store.clone(), gossip.clone())
                .await
                .map_err(err_to_py)?;

            let router = Router::builder(endpoint.clone())
                .accept(BLOBS_ALPN, blobs.clone())
                .accept(GOSSIP_ALPN, gossip.clone())
                .accept(DOCS_ALPN, docs.clone())
                .spawn();

            Ok(IrohNode {
                endpoint,
                router,
                blobs,
                docs,
                gossip,
                store,
            })
        })
    }

    /// Create a persistent Iroh node backed by an FsStore at the given path.
    #[staticmethod]
    fn persistent<'py>(py: Python<'py>, path: String) -> PyResult<&'py PyAny> {
        future_into_py(py, async move {
            let endpoint = Endpoint::bind(presets::N0).await.map_err(err_to_py)?;
            endpoint.online().await;

            let fs_store = FsStore::load(path).await.map_err(err_to_py)?;
            let store: BlobStore = fs_store.into();
            let blobs = BlobsProtocol::new(&store, None);
            let gossip = Gossip::builder().spawn(endpoint.clone());
            let docs = Docs::memory()
                .spawn(endpoint.clone(), store.clone(), gossip.clone())
                .await
                .map_err(err_to_py)?;

            let router = Router::builder(endpoint.clone())
                .accept(BLOBS_ALPN, blobs.clone())
                .accept(GOSSIP_ALPN, gossip.clone())
                .accept(DOCS_ALPN, docs.clone())
                .spawn();

            Ok(IrohNode {
                endpoint,
                router,
                blobs,
                docs,
                gossip,
                store,
            })
        })
    }

    /// Return this node's EndpointId as a hex string.
    fn node_id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Return the node's address info.
    fn node_addr(&self) -> String {
        let addr = self.endpoint.addr();
        format!("{addr:?}")
    }

    /// Gracefully shut down the node.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let endpoint = self.endpoint.clone();
        future_into_py(py, async move {
            endpoint.close().await;
            Ok(())
        })
    }

    /// Alias for close() — gracefully shut down the node.
    fn shutdown<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        self.close(py)
    }

    /// Add another node's address info so this node can connect to it.
    /// Used for peer discovery in testing/local scenarios.
    fn add_node_addr(&self, other: &IrohNode) -> PyResult<()> {
        let addr = other.endpoint.addr();
        let memory_lookup = MemoryLookup::new();
        memory_lookup.add_endpoint_info(addr);
        self.endpoint
            .address_lookup()
            .map_err(err_to_py)?
            .add(memory_lookup);
        Ok(())
    }
}

/// Register the node types with the Python module
pub fn register(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<IrohNode>()?;
    Ok(())
}
