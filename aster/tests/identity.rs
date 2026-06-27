#![cfg(feature = "docs")]
//! A persistent node restored from the same secret key + data dir keeps its
//! identity. This is the restart-stable-identity contract portal-sync relies on.

use aster::{AsterConfig, Node, RelayMode, SecretKey};

#[tokio::test]
async fn persistent_node_identity_is_restart_stable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();

    // First start: capture id + secret key, then shut down cleanly.
    let n1 = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .persistent(path.clone())
            .build(),
    )
    .await
    .expect("first start");
    let id1 = n1.id();
    let secret = n1.export_secret_key().expect("export secret");
    n1.shutdown().await;

    // Second start: same data dir + same secret key → same node id.
    let n2 = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .persistent(path.clone())
            .secret_key(secret)
            .build(),
    )
    .await
    .expect("second start");
    let id2 = n2.id();
    n2.shutdown().await;

    assert_eq!(
        id1, id2,
        "node id must survive restart with the same secret"
    );
}

/// `SecretKey::generate()` yields a random, round-trippable identity that a node
/// honors when pinned — the keypair-creation path the get-started doc shows.
#[tokio::test]
async fn generated_secret_key_pins_node_identity() {
    let a = SecretKey::generate();
    let b = SecretKey::generate();
    assert_ne!(a.to_bytes(), b.to_bytes(), "generate() must be random");
    assert_eq!(
        SecretKey::from_bytes(a.to_bytes()).to_bytes(),
        a.to_bytes(),
        "round-trips through from_bytes/to_bytes"
    );

    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .secret_key(SecretKey::from_bytes(a.to_bytes()))
            .build(),
    )
    .await
    .unwrap();
    // The node accepted the generated key as its identity, and the offline
    // public-key helper derives the matching identity.
    assert_eq!(node.export_secret_key().unwrap().to_bytes(), a.to_bytes());
    assert_eq!(
        node.id().to_string(),
        aster::attestation::public_key(&a).to_hex()
    );
    node.shutdown().await;
}

/// The default docs author IS the node identity (`AuthorId == NodeId`): a doc
/// entry's author is exactly the node that wrote it, so attribution is direct.
#[tokio::test]
async fn default_docs_author_equals_node_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let secret = SecretKey::from_bytes([7u8; 32]);

    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .persistent(path.clone())
            .secret_key(secret.clone())
            .build(),
    )
    .await
    .unwrap();
    let author = node.docs().default_author().await.unwrap();

    // Equals the node id, and the offline helper agrees.
    assert_eq!(author.to_string(), node.id().to_string());
    assert_eq!(author, aster::default_author_id(&secret));
    node.shutdown().await;
}
