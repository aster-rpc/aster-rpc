//! A node's HPKE recipient keypair, derived from its Aster (Ed25519) identity,
//! interoperates with `hpke_seal` / `hpke_open`. This is the portal-sync
//! contract: `portalctl add node <ticket>` derives the recipient public key
//! from the ticket's node id, and the node opens grants with its identity
//! secret — no separately published encryption key.

use aster::{
    hpke_open, hpke_public_key_from_private, hpke_seal, hpke_x25519_public_from_identity,
    hpke_x25519_public_from_node_id, hpke_x25519_secret_from_identity, AsterConfig, Node, NodeId,
    PublicKey, RelayMode, SecretKey,
};

#[tokio::test]
async fn real_node_identity_seals_and_opens() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .persistent(dir.path().to_path_buf())
            .secret_key(SecretKey::from_bytes([13u8; 32]))
            .build(),
    )
    .await
    .unwrap();

    let node_id = node.id();
    let secret = node.export_secret_key().unwrap();
    node.shutdown().await;

    // Root derives the recipient public key from the node id alone; the node
    // derives its recipient secret from its identity secret.
    let recipient_public = hpke_x25519_public_from_node_id(&node_id).unwrap();
    let recipient_secret = hpke_x25519_secret_from_identity(&secret);

    // Invariant: the two derivations agree.
    let public_from_secret = hpke_public_key_from_private(&recipient_secret).unwrap();
    assert_eq!(recipient_public, public_from_secret);
    // ...and the PublicKey-typed path matches the NodeId path.
    let via_public_key =
        hpke_x25519_public_from_identity(&PublicKey::from_hex(node_id.as_str()).unwrap()).unwrap();
    assert_eq!(recipient_public, via_public_key);

    // Seal to the node-id-derived public key; open with the identity-derived
    // secret.
    let aad = b"root_ns/root_node/path/recipient/generation/role/v1";
    let plaintext = b"namespace capability for this node";
    let envelope = hpke_seal(&recipient_public, aad, plaintext).unwrap();
    let opened = hpke_open(&recipient_secret, aad, &envelope).unwrap();
    assert_eq!(opened.expose_secret(), plaintext);
}

#[tokio::test]
async fn from_node_id_rejects_bad_input() {
    // Not valid hex.
    assert!(hpke_x25519_public_from_node_id(&NodeId::from_hex("nothex")).is_err());
    // Valid hex, wrong length.
    assert!(hpke_x25519_public_from_node_id(&NodeId::from_hex("00ff")).is_err());
}
