//! Deterministic namespaces from a 32-byte secret, idempotent import, and the
//! read→write capability upgrade.

use aster::{AsterConfig, NamespaceSecret, Node, RelayMode};

async fn mem_node() -> Node {
    Node::start(AsterConfig::builder().relay(RelayMode::Disabled).build())
        .await
        .expect("start")
}

#[tokio::test]
async fn same_secret_yields_same_namespace_on_two_nodes() {
    let seed = [0xABu8; 32];
    let secret = NamespaceSecret::from_bytes(seed);
    let expected = secret.id();

    let n1 = mem_node().await;
    let n2 = mem_node().await;

    let d1 = n1
        .docs()
        .open_or_import_write_namespace(secret.clone())
        .await
        .unwrap();
    let d2 = n2
        .docs()
        .open_or_import_write_namespace(secret.clone())
        .await
        .unwrap();

    assert_eq!(d1.id().unwrap(), d2.id().unwrap());
    assert_eq!(d1.id().unwrap(), expected, "doc id must equal secret.id()");

    n1.shutdown().await;
    n2.shutdown().await;
}

#[tokio::test]
async fn write_import_is_idempotent() {
    let secret = NamespaceSecret::from_bytes([0x11u8; 32]);
    let node = mem_node().await;

    let d1 = node
        .docs()
        .open_or_import_write_namespace(secret.clone())
        .await
        .unwrap();
    // Second call reattaches rather than erroring.
    let d1b = node
        .docs()
        .open_or_import_write_namespace(secret.clone())
        .await
        .expect("idempotent reattach");
    assert_eq!(d1.id().unwrap(), d1b.id().unwrap());

    node.shutdown().await;
}

#[tokio::test]
async fn read_import_then_write_import_upgrades_to_writable() {
    let secret = NamespaceSecret::from_bytes([0x22u8; 32]);
    let id = secret.id();
    let node = mem_node().await;

    // First import read-only by namespace id.
    let _ro = node
        .docs()
        .open_or_import_read_namespace(id)
        .await
        .expect("read import");

    // Now import the write capability for the same namespace — must upgrade.
    let rw = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .expect("write import (upgrade)");

    // Writing must now succeed, proving the capability was upgraded in place.
    let author = node.docs().default_author().await.unwrap();
    let hash = rw
        .set_bytes(&author, b"k".to_vec(), b"v".to_vec())
        .await
        .expect("write must succeed after upgrade");

    let val = rw.read_entry_content(&hash).await.unwrap();
    assert_eq!(val, b"v");

    node.shutdown().await;
}
