//! Temporary multi-root custody, exact retained imports and durability.
use aster_transport_core::{CoreBlobCustodyError, CoreEndpointConfig, CoreNode};
use iroh_blobs::Hash;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

async fn node() -> CoreNode {
    CoreNode::memory_with_alpns_and_gc(
        Vec::new(),
        Some(CoreEndpointConfig {
            relay_mode: Some("disabled".into()),
            ..Default::default()
        }),
        Some(Duration::from_secs(3600)),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn multiple_roots_discovery_and_active_close_keep_guards_without_tags() {
    let node = node().await;
    let blobs = node.blobs_client();
    let scope = blobs.custody();
    let first = Hash::new(b"before fetch");
    scope
        .protect(first.to_hex().to_string(), "raw".into())
        .await
        .unwrap();
    let first_tag = blobs
        .store
        .blobs()
        .add_slice(b"before fetch")
        .temp_tag()
        .await
        .unwrap();
    drop(first_tag);
    let second = scope
        .add_bytes(b"discovered during verification".to_vec(), "raw".into())
        .await
        .unwrap();
    assert!(blobs.tag_list().await.unwrap().is_empty());
    blobs.gc_run_once().await.unwrap();
    assert!(blobs.blob_has(first.to_hex().to_string()).await.unwrap());
    assert!(blobs.blob_has(second.clone()).await.unwrap());
    let lease = scope.lease().unwrap();
    scope.close();
    assert!(scope
        .lease()
        .err()
        .unwrap()
        .downcast_ref::<CoreBlobCustodyError>()
        .is_some());
    // New roots acquired by previously admitted work remain protected too.
    let third = lease
        .add_bytes(b"active work after close".to_vec(), "raw".into())
        .await
        .unwrap();
    blobs.gc_run_once().await.unwrap();
    assert_eq!(
        lease.read(second.clone()).await.unwrap(),
        b"discovered during verification"
    );
    assert!(blobs.blob_has(third.clone()).await.unwrap());
    drop(lease);
    scope.close_and_wait().await;
    blobs.gc_run_once().await.unwrap();
    for hash in [first.to_hex().to_string(), second, third] {
        assert!(!blobs.blob_has(hash).await.unwrap());
    }
    node.close().await;
}

#[tokio::test]
async fn hashseq_custody_and_exact_named_retention_do_not_require_collection_schema() {
    let node = node().await;
    let blobs = node.blobs_client();
    let scope = blobs.custody();
    let child = scope
        .add_bytes(b"child".to_vec(), "raw".into())
        .await
        .unwrap();
    let hash: Hash = child.parse().unwrap();
    let sequence = hash.as_bytes().to_vec();
    let root = scope
        .add_bytes(sequence.clone(), "hash_seq".into())
        .await
        .unwrap();
    // A HashSeq with one child is intentionally not a Collection.
    assert_eq!(scope.read(root.clone()).await.unwrap(), sequence);
    scope
        .retain(root.clone(), "hash_seq".into(), "retained/seq".into(), None)
        .await
        .unwrap();
    scope.close_and_wait().await;
    blobs.gc_run_once().await.unwrap();
    assert!(blobs.blob_has(child.clone()).await.unwrap());
    let tag = blobs.tag_get("retained/seq".into()).await.unwrap().unwrap();
    assert_eq!(tag.hash, root);
    assert_eq!(tag.format, "hash_seq");
    assert_eq!(blobs.tag_list().await.unwrap().len(), 1);
    blobs.tag_delete("retained/seq".into()).await.unwrap();
    blobs.gc_run_once().await.unwrap();
    assert!(!blobs.blob_has(child).await.unwrap());
    node.close().await;
}

#[tokio::test]
async fn verified_bytes_mismatch_cancellation_and_replacement_preserve_exact_tag_contract() {
    let node = node().await;
    let blobs = node.blobs_client();
    let original = Hash::new(b"original").to_hex().to_string();
    assert_eq!(
        blobs
            .add_bytes_verified(
                b"original".to_vec(),
                original.clone(),
                "raw".into(),
                "stable".into(),
                None
            )
            .await
            .unwrap(),
        original
    );
    let mismatch = blobs
        .add_bytes_verified(
            b"wrong".to_vec(),
            original.clone(),
            "raw".into(),
            "stable".into(),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        mismatch.downcast_ref::<CoreBlobCustodyError>(),
        Some(CoreBlobCustodyError::HashMismatch { .. })
    ));
    let flag = Arc::new(AtomicBool::new(true));
    let cancelled = blobs
        .add_bytes_verified(
            b"cancelled".to_vec(),
            Hash::new(b"cancelled").to_hex().to_string(),
            "raw".into(),
            "stable".into(),
            Some(flag),
        )
        .await
        .unwrap_err();
    assert_eq!(
        cancelled.downcast_ref::<CoreBlobCustodyError>(),
        Some(&CoreBlobCustodyError::Cancelled)
    );
    assert_eq!(
        blobs.tag_get("stable".into()).await.unwrap().unwrap().hash,
        original
    );
    blobs.gc_run_once().await.unwrap();
    assert!(!blobs
        .blob_has(Hash::new(b"wrong").to_hex().to_string())
        .await
        .unwrap());
    let replacement = Hash::new(b"replacement").to_hex().to_string();
    blobs
        .add_bytes_verified(
            b"replacement".to_vec(),
            replacement.clone(),
            "raw".into(),
            "stable".into(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(blobs.tag_list().await.unwrap().len(), 1);
    blobs.gc_run_once().await.unwrap();
    assert!(!blobs.blob_has(original).await.unwrap());
    assert_eq!(
        blobs.tag_get("stable".into()).await.unwrap().unwrap().hash,
        replacement
    );
    node.close().await;
}

#[tokio::test]
async fn verified_hashseq_import_retains_child_and_rejects_bad_format_without_tag_mutation() {
    let node = node().await;
    let blobs = node.blobs_client();
    let child = blobs
        .store
        .blobs()
        .add_slice(b"sequence child")
        .temp_tag()
        .await
        .unwrap();
    let bytes = child.hash().as_bytes().to_vec();
    let hash = Hash::new(&bytes).to_hex().to_string();
    blobs
        .add_bytes_verified(bytes, hash.clone(), "hash_seq".into(), "seq".into(), None)
        .await
        .unwrap();
    drop(child);
    blobs.gc_run_once().await.unwrap();
    let tag = blobs.tag_get("seq".into()).await.unwrap().unwrap();
    assert_eq!(tag.format, "hash_seq");
    let bad = blobs
        .add_bytes_verified(
            b"not a hash sequence".to_vec(),
            Hash::new(b"not a hash sequence").to_hex().to_string(),
            "hash_seq".into(),
            "seq".into(),
            None,
        )
        .await;
    assert!(bad.is_err());
    assert_eq!(
        blobs.tag_get("seq".into()).await.unwrap().unwrap().hash,
        hash
    );
    assert!(blobs
        .add_bytes_verified(
            Vec::new(),
            Hash::EMPTY.to_hex().to_string(),
            "typo".into(),
            "invalid".into(),
            None
        )
        .await
        .is_err());
    assert!(blobs
        .add_bytes_verified(
            Vec::new(),
            Hash::EMPTY.to_hex().to_string(),
            "raw".into(),
            "".into(),
            None
        )
        .await
        .is_err());
    node.close().await;
}

#[tokio::test]
async fn verified_copy_path_persists_and_source_changes_cannot_corrupt_it() {
    let dir = TempDir::new();
    let path = dir.path().join("source");
    let store = dir.path().join("store");
    std::fs::write(&path, b"copied verified bytes").unwrap();
    let hash = Hash::new(b"copied verified bytes").to_hex().to_string();
    let config = CoreEndpointConfig {
        relay_mode: Some("disabled".into()),
        ..Default::default()
    };
    let node = CoreNode::persistent_with_alpns(
        store.to_string_lossy().into_owned(),
        Vec::new(),
        Some(config.clone()),
    )
    .await
    .unwrap();
    let blobs = node.blobs_client();
    blobs
        .add_path_verified(
            path.to_string_lossy().into_owned(),
            hash.clone(),
            "raw".into(),
            "copy".into(),
            None,
        )
        .await
        .unwrap();
    std::fs::write(&path, b"mutated").unwrap();
    assert_eq!(
        blobs.read_to_bytes(hash.clone()).await.unwrap(),
        b"copied verified bytes"
    );
    node.close().await;
    let node = CoreNode::persistent_with_alpns(
        store.to_string_lossy().into_owned(),
        Vec::new(),
        Some(config),
    )
    .await
    .unwrap();
    assert_eq!(
        node.blobs_client()
            .read_to_bytes(hash.clone())
            .await
            .unwrap(),
        b"copied verified bytes"
    );
    assert_eq!(
        node.blobs_client()
            .tag_get("copy".into())
            .await
            .unwrap()
            .unwrap()
            .hash,
        hash
    );
    assert_eq!(node.blobs_client().tag_list().await.unwrap().len(), 1);
    node.close().await;
}

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("aster-custody-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
