use pyo3::prelude::*;

mod blobs;
mod docs;
mod error;
mod gossip;
mod net;
mod node;

/// The native Iroh Python module implemented in Rust.
#[pymodule]
fn _iroh_python(_py: Python, m: &PyModule) -> PyResult<()> {
    // Initialize the tokio runtime for pyo3-asyncio
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    pyo3_asyncio::tokio::init(builder);

    // Register the custom exception
    m.add("IrohError", _py.get_type::<error::IrohError>())?;

    // Register classes
    m.add_class::<node::IrohNode>()?;
    m.add_class::<blobs::BlobsClient>()?;
    m.add_class::<docs::DocsClient>()?;
    m.add_class::<docs::DocHandle>()?;
    m.add_class::<gossip::GossipClient>()?;
    m.add_class::<gossip::GossipTopicHandle>()?;
    m.add_class::<net::NetClient>()?;
    m.add_class::<net::IrohConnection>()?;
    m.add_class::<net::IrohSendStream>()?;
    m.add_class::<net::IrohRecvStream>()?;

    // Register factory functions
    m.add_function(wrap_pyfunction!(blobs::blobs_client, m)?)?;
    m.add_function(wrap_pyfunction!(docs::docs_client, m)?)?;
    m.add_function(wrap_pyfunction!(gossip::gossip_client, m)?)?;
    m.add_function(wrap_pyfunction!(net::net_client, m)?)?;
    m.add_function(wrap_pyfunction!(net::create_endpoint, m)?)?;

    Ok(())
}
