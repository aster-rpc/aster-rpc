//! Ownership-attestation facade: mint a root↔node chain and verify it offline,
//! including the reject paths a Gate-0 admission check relies on.

use aster::attestation::{attest_root_node, public_key, verify_chain, AttestOptions, Chain};
use aster::{AsterConfig, Node, RelayMode, SecretKey};

fn keys() -> (SecretKey, SecretKey) {
    (
        SecretKey::from_bytes([1u8; 32]),
        SecretKey::from_bytes([2u8; 32]),
    )
}

#[test]
fn mint_and_verify_offline() {
    let (root, node) = keys();
    let root_pk = public_key(&root);
    let node_pk = public_key(&node);

    let chain = attest_root_node(&root, &node, &AttestOptions::default()).unwrap();
    let v = verify_chain(&chain, &[root_pk], &node_pk).unwrap();

    assert_eq!(v.node, node_pk);
    assert_eq!(v.anchor, root_pk);
    assert_eq!(v.depth, 1);
}

#[test]
fn text_round_trip() {
    let (root, node) = keys();
    let chain = attest_root_node(&root, &node, &AttestOptions::default()).unwrap();

    let text = chain.to_text();
    assert!(text.starts_with("aster.attestation.chain.v1:"));

    let parsed = Chain::from_text(&text).unwrap();
    assert_eq!(parsed, chain);
    verify_chain(&parsed, &[public_key(&root)], &public_key(&node)).unwrap();
}

#[test]
fn rejects_untrusted_anchor_wrong_node_and_expired() {
    let (root, node) = keys();
    let stranger = SecretKey::from_bytes([9u8; 32]);
    let root_pk = public_key(&root);
    let node_pk = public_key(&node);

    let chain = attest_root_node(&root, &node, &AttestOptions::default()).unwrap();

    // Anchor not trusted.
    assert!(verify_chain(&chain, &[public_key(&stranger)], &node_pk).is_err());
    // Bound to a different node than expected.
    assert!(verify_chain(&chain, &[root_pk], &public_key(&stranger)).is_err());

    // Expired chain (not_after far in the past).
    let expired = attest_root_node(
        &root,
        &node,
        &AttestOptions {
            epoch: 0,
            not_before: 0,
            not_after: 1,
        },
    )
    .unwrap();
    assert!(verify_chain(&expired, &[root_pk], &node_pk).is_err());
}

/// portal-sync relies on a node's attestation identity being the *same* key as
/// its transport identity: `public_key(node.export_secret_key()) == node.id()`.
#[tokio::test]
async fn node_identity_equals_attestation_public_key() {
    let node = Node::start(AsterConfig::builder().relay(RelayMode::Disabled).build())
        .await
        .unwrap();
    let secret = node.export_secret_key().unwrap();
    let pk = public_key(&secret);
    assert_eq!(
        pk.to_hex(),
        node.id().to_string(),
        "attestation public key must equal the node's transport id"
    );
    node.shutdown().await;
}

#[test]
fn verified_chain_classifies_roles() {
    use aster::attestation::Role;
    let (root, node) = keys();
    let root_pk = public_key(&root);
    let node_pk = public_key(&node);
    let stranger = public_key(&SecretKey::from_bytes([9u8; 32]));

    let chain = attest_root_node(&root, &node, &AttestOptions::default()).unwrap();
    let v = verify_chain(&chain, &[root_pk], &node_pk).unwrap();

    assert!(v.is_node(&node_pk));
    assert!(v.is_anchor(&root_pk));
    assert!(v.intermediates.is_empty());
    assert_eq!(v.role_of(&node_pk), Some(Role::Node));
    assert_eq!(v.role_of(&root_pk), Some(Role::Anchor));
    assert_eq!(v.role_of(&stranger), None);
}

#[test]
fn tampering_is_detected() {
    let (root, node) = keys();
    let mut bytes = attest_root_node(&root, &node, &AttestOptions::default())
        .unwrap()
        .into_bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    let tampered = Chain::from_bytes(bytes);
    assert!(verify_chain(&tampered, &[public_key(&root)], &public_key(&node)).is_err());
}
