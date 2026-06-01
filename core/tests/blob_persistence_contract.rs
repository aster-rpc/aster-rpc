//! Contract tests for the portal-sync persistence surface:
//!
//! * `CoreNode::persistent_with_alpns` keeps iroh-docs state (namespace +
//!   author + entry) across a close/reopen cycle — not just blobs.
//! * `CoreBlobsClient::add_path` imports a file (Copy mode) and the blob
//!   survives `CoreNode::close()` + reopen, proving `close()` flushes the store.
//! * `CoreBlobsClient::add_path_with_named_tag` sets a deterministic, caller-keyed
//!   tag that also survives reopen.
//!
//! All nodes run with relay disabled so the test stays offline and fast.

use std::path::Path;

use aster_transport_core::{CoreEndpointConfig, CoreNode};

/// A persistent node with relay/discovery off, loaded from `dir`.
async fn open_node(dir: &Path) -> CoreNode {
    let cfg = CoreEndpointConfig {
        relay_mode: Some("disabled".to_string()),
        ..Default::default()
    };
    CoreNode::persistent_with_alpns(dir.to_string_lossy().into_owned(), Vec::new(), Some(cfg))
        .await
        .expect("open persistent node")
}

/// Unique temp directory for one test run; removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aster-persist-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_docs_and_blobs_survive_close_and_reopen() {
    let dir = TempDir::new("docs-blobs");

    let key = b"portal-sync/entry".to_vec();
    let value = b"slice-4 payload".to_vec();
    let file_contents = b"file imported via add_path".to_vec();

    let file_path = dir.path().join("import-me.bin");
    std::fs::write(&file_path, &file_contents).expect("write source file");
    let file_path_str = file_path.to_string_lossy().into_owned();

    // ── Phase 1: write blob + docs state, then close cleanly. ──────────────
    let (namespace_id, author, blob_hash) = {
        let node = open_node(dir.path()).await;

        let blobs = node.blobs_client();
        let blob_hash = blobs
            .add_path(file_path_str.clone())
            .await
            .expect("add_path");

        let docs = node.docs_client();
        let author = docs.create_author().await.expect("create_author");
        let doc = docs.create().await.expect("create doc");
        let namespace_id = doc.doc_id();
        doc.set_bytes(author.clone(), key.clone(), value.clone())
            .await
            .expect("set_bytes");

        // Clean shutdown: must flush the redb metadata + blob store to disk.
        node.close().await;
        (namespace_id, author, blob_hash)
    };

    // The persistent docs store should have been written to disk.
    assert!(
        dir.path().join("docs.redb").exists(),
        "Docs::persistent should write docs.redb under the node directory"
    );

    // ── Phase 2: reopen the same directory and assert everything survived. ─
    let node = open_node(dir.path()).await;

    // Blob: present and byte-identical (proves add_path durability + close() flush).
    let blobs = node.blobs_client();
    assert!(
        blobs.blob_has(blob_hash.clone()).await.expect("blob_has"),
        "imported blob should survive close/reopen"
    );
    let read_back = blobs
        .read_to_bytes(blob_hash.clone())
        .await
        .expect("read_to_bytes");
    assert_eq!(
        read_back, file_contents,
        "blob contents changed across reopen"
    );

    // Docs: namespace + author + entry all reloaded from docs.redb.
    let docs = node.docs_client();
    let doc = docs
        .open(namespace_id.clone())
        .await
        .expect("open namespace")
        .expect("namespace should still exist after reopen");
    let got = doc
        .get_exact(author, key)
        .await
        .expect("get_exact")
        .expect("entry should still exist after reopen");
    assert_eq!(got, value, "doc entry value changed across reopen");

    // Opening an unknown namespace must return Ok(None), not an error — the
    // documented contract (upstream DocsApi::open errors "Replica not found").
    let unknown_ns = "ff".repeat(32);
    let missing = docs
        .open(unknown_ns)
        .await
        .expect("open of an unknown namespace must not error");
    assert!(missing.is_none(), "unknown namespace should open to None");

    node.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_path_with_named_tag_sets_deterministic_tag_that_persists() {
    let dir = TempDir::new("named-tag");

    let file_contents = b"tagged file".to_vec();
    let file_path = dir.path().join("tagged.bin");
    std::fs::write(&file_path, &file_contents).expect("write source file");
    let file_path_str = file_path.to_string_lossy().into_owned();

    // Phase 1: import with a caller-chosen tag keyed like portal-sync would.
    let (blob_hash, tag_name) = {
        let node = open_node(dir.path()).await;
        let blobs = node.blobs_client();
        let blob_hash = blobs
            .add_path(file_path_str.clone())
            .await
            .expect("add_path (for hash)");
        // delete the anonymous auto-tag add_path created so only our named tag remains
        let tag_name = format!("portal-sync/t1/{blob_hash}");
        let tagged_hash = blobs
            .add_path_with_named_tag(file_path_str.clone(), tag_name.clone())
            .await
            .expect("add_path_with_named_tag");
        assert_eq!(tagged_hash, blob_hash, "same bytes must hash identically");
        node.close().await;
        (blob_hash, tag_name)
    };

    // Phase 2: reopen — the named tag and its target must still be there.
    let node = open_node(dir.path()).await;
    let blobs = node.blobs_client();
    let tag = blobs
        .tag_get(tag_name.clone())
        .await
        .expect("tag_get")
        .expect("named tag should survive reopen");
    assert_eq!(tag.name, tag_name);
    assert_eq!(tag.hash, blob_hash);
    assert!(blobs.blob_has(blob_hash).await.expect("blob_has"));

    node.close().await;
}
