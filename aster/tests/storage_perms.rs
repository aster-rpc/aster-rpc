#![cfg(all(unix, feature = "docs"))]
//! A persistent node's data directory and secret-bearing store files are owner-
//! private: the data dir is `0700` and `docs.redb` (which holds the node
//! identity secret in its authors table) is `0600`. This is the on-disk
//! protection portal-sync relies on when treating `~/.aster` as a root trust
//! store.

use std::os::unix::fs::PermissionsExt;

use aster::{AsterConfig, Node, RelayMode, SecretKey};

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn persistent_store_is_owner_private() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("policy-node");

    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .persistent(data.clone())
            .secret_key(SecretKey::from_bytes([5u8; 32]))
            .build(),
    )
    .await
    .unwrap();
    node.shutdown().await;

    assert_eq!(mode(&data), 0o700, "data dir must be 0700");

    let redb = data.join("docs.redb");
    assert!(redb.exists(), "docs.redb should have been created");
    assert_eq!(
        mode(&redb),
        0o600,
        "docs.redb holds the node secret and must be 0600"
    );

    let default_author = data.join("default-author");
    if default_author.exists() {
        assert_eq!(mode(&default_author), 0o600, "default-author must be 0600");
    }
}
