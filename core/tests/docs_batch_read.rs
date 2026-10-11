//! Completeness-aware local Docs reads: order, absence, validation and I/O errors.
use aster_transport_core::{CoreBlobStatus, CoreEndpointConfig, CoreNode};
use iroh_blobs::Hash;
use std::num::NonZeroU64;
use std::time::Duration;

fn config() -> CoreEndpointConfig {
    CoreEndpointConfig {
        relay_mode: Some("disabled".into()),
        ..Default::default()
    }
}
async fn memory() -> CoreNode {
    CoreNode::memory_with_alpns_and_gc(Vec::new(), Some(config()), Some(Duration::from_secs(3600)))
        .await
        .unwrap()
}

#[tokio::test]
async fn ordered_optional_values_include_duplicates_empty_missing_and_partial() {
    let node = memory().await;
    let doc = node.docs_client().create().await.unwrap();
    let blobs = node.blobs_client();
    let hash = blobs.add_bytes(b"local value".to_vec()).await.unwrap();
    let partial = Hash::new(b"unfinished import");
    let import = doc
        .store
        .blobs()
        .import_bao(partial, NonZeroU64::new(1024 * 1024).unwrap(), 8)
        .await
        .unwrap();
    drop(import.tx);
    import.rx.await.unwrap().unwrap();
    assert!(matches!(
        doc.entry_content_status(partial.to_hex().to_string())
            .await
            .unwrap(),
        CoreBlobStatus::Partial { .. }
    ));
    let missing = Hash::new(b"absent").to_hex().to_string();
    let empty = blobs.add_bytes(Vec::new()).await.unwrap();
    let values = doc
        .read_entry_contents_if_complete(vec![
            missing,
            hash.clone(),
            empty,
            partial.to_hex().to_string(),
            hash,
        ])
        .await
        .unwrap();
    assert_eq!(
        values,
        vec![
            None,
            Some(b"local value".to_vec()),
            Some(Vec::new()),
            None,
            Some(b"local value".to_vec())
        ]
    );
    assert!(doc
        .read_entry_contents_if_complete(Vec::new())
        .await
        .unwrap()
        .is_empty());
    node.close().await;
}

#[tokio::test]
async fn validates_entire_input_before_closed_store_read_and_never_returns_partial_results() {
    let node = memory().await;
    let doc = node.docs_client().create().await.unwrap();
    node.close().await;
    for invalid in [
        "invalid".into(),
        "a".repeat(52),
        "g".repeat(64),
        "é".repeat(32),
    ] {
        let error = doc
            .read_entry_contents_if_complete(vec![Hash::EMPTY.to_hex().to_string(), invalid])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "content hash must contain exactly 64 ASCII hexadecimal characters"
        );
    }
    assert!(doc
        .read_entry_contents_if_complete(vec![Hash::new(b"valid but closed").to_hex().to_string()])
        .await
        .is_err());
}

#[tokio::test]
async fn local_batch_does_not_fetch_from_an_available_provider_or_create_retention() {
    let provider = memory().await;
    let consumer = memory().await;
    consumer.add_node_addr(&provider).unwrap();
    let hash = provider
        .blobs_client()
        .add_bytes(b"provider only".to_vec())
        .await
        .unwrap();
    let doc = consumer.docs_client().create().await.unwrap();
    let before = consumer.blobs_client().tag_list().await.unwrap();
    assert_eq!(
        doc.read_entry_contents_if_complete(vec![hash.clone(), hash.clone()])
            .await
            .unwrap(),
        vec![None, None]
    );
    assert!(!consumer.blobs_client().blob_has(hash).await.unwrap());
    assert_eq!(
        consumer.blobs_client().tag_list().await.unwrap().len(),
        before.len()
    );
    consumer.close().await;
    provider.close().await;
}

#[tokio::test]
async fn custody_keeps_large_and_inline_values_through_gc_then_release_makes_them_absent() {
    let dir = TempDir::new();
    let node = CoreNode::persistent_with_alpns_and_gc(
        dir.path().to_string_lossy().into_owned(),
        Vec::new(),
        Some(config()),
        Some(Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    let doc = node.docs_client().create().await.unwrap();
    let blobs = node.blobs_client();
    let scope = blobs.custody();
    let large = vec![19u8; 1024 * 1024 + 17];
    let large_hash = scope.add_bytes(large.clone(), "raw".into()).await.unwrap();
    let small_hash = scope
        .add_bytes(b"inline".to_vec(), "raw".into())
        .await
        .unwrap();
    let (values, gc) = tokio::join!(
        doc.read_entry_contents_if_complete(vec![
            large_hash.clone(),
            small_hash.clone(),
            large_hash.clone()
        ]),
        blobs.gc_run_once()
    );
    gc.unwrap();
    assert_eq!(
        values.unwrap(),
        vec![Some(large.clone()), Some(b"inline".to_vec()), Some(large)]
    );
    assert!(blobs.tag_list().await.unwrap().is_empty());
    scope.close_and_wait().await;
    blobs.gc_run_once().await.unwrap();
    assert_eq!(
        doc.read_entry_contents_if_complete(vec![large_hash, small_hash])
            .await
            .unwrap(),
        vec![None, None]
    );
    node.close().await;
}

#[tokio::test]
async fn complete_metadata_with_corrupted_large_export_fails_batch_not_none() {
    let dir = TempDir::new();
    let node = CoreNode::persistent_with_alpns(
        dir.path().to_string_lossy().into_owned(),
        Vec::new(),
        Some(config()),
    )
    .await
    .unwrap();
    let doc = node.docs_client().create().await.unwrap();
    let data = vec![41u8; 1024 * 1024 + 17];
    let hash = node.blobs_client().add_bytes(data.clone()).await.unwrap();
    let path = iroh_blobs::store::fs::options::PathOptions::new(dir.path())
        .data_path(&hash.parse::<Hash>().unwrap());
    assert!(
        path.exists(),
        "large content must use validated file export"
    );
    std::fs::write(path, vec![99u8; data.len()]).unwrap();
    assert!(matches!(
        doc.entry_content_status(hash.clone()).await.unwrap(),
        CoreBlobStatus::Complete { .. }
    ));
    assert!(doc
        .read_entry_contents_if_complete(vec![Hash::EMPTY.to_hex().to_string(), hash])
        .await
        .is_err());
    node.close().await;
}

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aster-doc-batch-{}-{id}", std::process::id()));
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
