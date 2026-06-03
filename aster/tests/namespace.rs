#![cfg(feature = "docs")]
//! Deterministic namespaces from a 32-byte secret, idempotent import, and the
//! read→write capability upgrade.

use aster::{
    hpke_generate_keypair, hpke_open, hpke_public_key_from_private, hpke_seal, AsterConfig,
    NamespaceCapability, NamespaceSecret, Node, RelayMode, HPKE_ENVELOPE_ALG,
};

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

#[test]
fn namespace_capability_uses_fory_and_redacts_capability_material() {
    let secret = NamespaceSecret::from_bytes([0x33u8; 32]);
    let write = NamespaceCapability::write(secret.clone());
    let read = NamespaceCapability::read(secret.id());

    assert!(write.can_write());
    assert!(!read.can_write());
    assert_eq!(write.namespace_id(), read.namespace_id());

    let decoded_write = NamespaceCapability::decode_fory(&write.encode_fory()).unwrap();
    assert!(decoded_write.can_write());
    assert_eq!(decoded_write.namespace_id(), secret.id());

    let text = format!("{decoded_write:?}");
    assert!(text.contains("<redacted>"));
    assert!(!text.contains("333333"));

    let decoded_read = NamespaceCapability::decode_fory(&read.encode_fory()).unwrap();
    assert!(!decoded_read.can_write());
    assert_eq!(decoded_read.namespace_id(), secret.id());

    let read_text = format!("{decoded_read:?}");
    assert!(read_text.contains("<redacted>"));
    assert!(!read_text.contains(&secret.id().to_hex()[..12]));
}

#[test]
fn hpke_envelope_round_trips_with_fory_and_ad() {
    let keypair = hpke_generate_keypair();
    assert_eq!(
        hpke_public_key_from_private(&keypair.private_key()).unwrap(),
        keypair.public_key()
    );

    let aad = b"root-namespace/root-node/path/recipient/epoch/role/v1";
    let plaintext =
        NamespaceCapability::read(NamespaceSecret::from_bytes([0x44u8; 32]).id()).encode_fory();

    let envelope = hpke_seal(&keypair.public_key(), aad, &plaintext).unwrap();
    assert_eq!(envelope.alg(), HPKE_ENVELOPE_ALG);

    let decoded = aster::HpkeEnvelope::decode_fory(&envelope.encode_fory()).unwrap();
    let opened = hpke_open(&keypair.private_key(), aad, &decoded).unwrap();
    assert_eq!(opened.expose_secret(), plaintext);

    assert!(hpke_open(&keypair.private_key(), b"wrong-ad", &decoded).is_err());
}
