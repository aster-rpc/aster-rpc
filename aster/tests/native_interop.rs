//! Compile and lifetime smoke test for the supported native escape hatch.

use aster::{AsterConfig, Node, RelayMode};

#[tokio::test]
async fn native_handles_are_the_running_nodes_handles() -> aster::Result<()> {
    let node = Node::start(AsterConfig::builder().relay(RelayMode::Disabled).build()).await?;

    let endpoint: aster::native::iroh::Endpoint = node.native_endpoint();
    assert_eq!(endpoint.id().to_string(), node.id().to_string());

    let _: aster::native::iroh_blobs::api::Store = node.blobs().native_store();
    let _: aster::native::iroh::Endpoint = node.blobs().native_endpoint();
    let _: aster::native::iroh_docs::protocol::Docs = node.docs().native_docs();
    let _: aster::native::iroh_blobs::api::Store = node.docs().native_store();
    let _: aster::native::iroh::Endpoint = node.docs().native_endpoint();
    let _: aster::native::iroh_gossip::net::Gossip = node.gossip().native_gossip();

    node.shutdown().await;
    Ok(())
}
